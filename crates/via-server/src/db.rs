//! The registry: fleets, devices, enrollment tokens, API tokens, and the
//! audit log.
//!
//! One SQLite file, one connection behind a mutex. A control plane for a few
//! thousand devices does a handful of writes a second at most; a pool would
//! be machinery for a load that does not exist.

use std::path::Path;
use std::sync::Mutex;

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use sha2::{Digest, Sha256};

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS fleets (
    id    INTEGER PRIMARY KEY,
    name  TEXT NOT NULL UNIQUE
);
CREATE TABLE IF NOT EXISTS devices (
    id              TEXT PRIMARY KEY,
    name            TEXT NOT NULL,
    fleet_id        INTEGER NOT NULL REFERENCES fleets(id),
    credential_hash TEXT NOT NULL UNIQUE,
    arch            TEXT,
    os_version      TEXT,
    agent_version   TEXT,
    enrolled_at     INTEGER NOT NULL,
    last_seen       INTEGER
);
CREATE TABLE IF NOT EXISTS enrollment_tokens (
    token_hash  TEXT PRIMARY KEY,
    fleet_id    INTEGER NOT NULL REFERENCES fleets(id),
    hostname    TEXT,
    created_at  INTEGER NOT NULL,
    expires_at  INTEGER NOT NULL,
    used_by     TEXT,
    image_id    TEXT
);
CREATE TABLE IF NOT EXISTS audit (
    id     INTEGER PRIMARY KEY,
    at     INTEGER NOT NULL,
    tool   TEXT NOT NULL,
    args   TEXT NOT NULL,
    ok     INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS api_tokens (
    name        TEXT PRIMARY KEY,
    role        TEXT NOT NULL,
    token_hash  TEXT NOT NULL UNIQUE,
    created_at  INTEGER NOT NULL,
    last_used   INTEGER
);
";

/// What a person (or their Claude) may do through the API.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, serde::Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// Everything: fleet changes, uploads, tunnels into devices.
    Admin,
    /// Read-only tools: health, logs, lists. Changes nothing.
    Viewer,
}

impl Role {
    fn as_str(self) -> &'static str {
        match self {
            Role::Admin => "admin",
            Role::Viewer => "viewer",
        }
    }

    fn parse(s: &str) -> Option<Self> {
        match s {
            "admin" => Some(Role::Admin),
            "viewer" => Some(Role::Viewer),
            _ => None,
        }
    }
}

/// Whoever an API request authenticated as.
#[derive(Debug, Clone)]
pub struct Caller {
    pub name: String,
    pub role: Role,
}

#[derive(Debug, Clone, Serialize)]
pub struct ApiToken {
    pub name: String,
    pub role: Role,
    pub created_at: i64,
    pub last_used: Option<i64>,
}

pub struct Db {
    conn: Mutex<Connection>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Fleet {
    pub name: String,
    pub devices: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct Device {
    pub id: String,
    pub name: String,
    pub fleet: String,
    pub arch: Option<String>,
    pub os_version: Option<String>,
    pub agent_version: Option<String>,
    pub enrolled_at: i64,
    pub last_seen: Option<i64>,
}

/// What a device reports about itself when it enrolls.
#[derive(Debug, Clone, serde::Deserialize)]
pub struct Enrollment {
    pub token: String,
    pub hostname: String,
    pub arch: Option<String>,
    pub os_version: Option<String>,
    pub agent_version: Option<String>,
}

pub fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// Secrets are stored as their SHA-256. They are 256 random bits, so a slow
/// hash would add nothing.
pub fn hash(secret: &str) -> String {
    hex::encode(Sha256::digest(secret.as_bytes()))
}

pub fn random_secret() -> String {
    hex::encode(rand::random::<[u8; 32]>())
}

impl Db {
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")?;
        conn.execute_batch(SCHEMA)?;
        // Added after the first release of the schema.
        let has_actor: bool = conn
            .prepare("SELECT 1 FROM pragma_table_info('audit') WHERE name = 'actor'")?
            .exists([])?;
        if !has_actor {
            conn.execute_batch("ALTER TABLE audit ADD COLUMN actor TEXT")?;
        }
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    pub fn create_fleet(&self, name: &str) -> Result<()> {
        valid_name(name)?;
        let c = self.conn.lock().unwrap();
        c.execute(
            "INSERT INTO fleets (name) VALUES (?1) ON CONFLICT DO NOTHING",
            [name],
        )?;
        Ok(())
    }

    pub fn fleets(&self) -> Result<Vec<Fleet>> {
        let c = self.conn.lock().unwrap();
        let mut q = c.prepare(
            "SELECT f.name, COUNT(d.id) FROM fleets f
             LEFT JOIN devices d ON d.fleet_id = f.id GROUP BY f.id ORDER BY f.name",
        )?;
        let rows = q.query_map([], |r| {
            Ok(Fleet {
                name: r.get(0)?,
                devices: r.get::<_, i64>(1)? as u64,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// Mint a single-use token for `fleet`. The caller must put the token
    /// where the device will find it and nowhere else.
    pub fn mint_token(
        &self,
        fleet: &str,
        hostname: Option<&str>,
        ttl_secs: i64,
        image_id: Option<&str>,
    ) -> Result<String> {
        let c = self.conn.lock().unwrap();
        let fleet_id = fleet_id(&c, fleet)?;
        let token = random_secret();
        c.execute(
            "INSERT INTO enrollment_tokens (token_hash, fleet_id, hostname, created_at, expires_at, image_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![hash(&token), fleet_id, hostname, now(), now() + ttl_secs, image_id],
        )?;
        Ok(token)
    }

    /// Spend an enrollment token and register the device. Returns the device,
    /// its long-lived credential, and the provisioning image the token was
    /// written into, which has now served its purpose.
    ///
    /// A token is single-use, so the same image written to two cards enrolls
    /// one device. The second is refused, rather than silently becoming the
    /// first.
    pub fn enroll(&self, e: &Enrollment) -> Result<(Device, String, Option<String>)> {
        let mut c = self.conn.lock().unwrap();
        let tx = c.transaction()?;
        let row: Option<(i64, Option<String>, i64, Option<String>, Option<String>)> = tx
            .query_row(
                "SELECT fleet_id, hostname, expires_at, used_by, image_id FROM enrollment_tokens
                 WHERE token_hash = ?1",
                [hash(&e.token)],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .optional()?;
        let Some((fleet_id, name, expires_at, used_by, image_id)) = row else {
            bail!("unknown token")
        };
        if used_by.is_some() {
            bail!("token already used");
        }
        if expires_at < now() {
            bail!("token expired");
        }
        let id = random_secret()[..16].to_string();
        let name = name.unwrap_or_else(|| e.hostname.clone());
        let credential = random_secret();
        tx.execute(
            "INSERT INTO devices (id, name, fleet_id, credential_hash, arch, os_version,
                                  agent_version, enrolled_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                id,
                name,
                fleet_id,
                hash(&credential),
                e.arch,
                e.os_version,
                e.agent_version,
                now()
            ],
        )?;
        tx.execute(
            "UPDATE enrollment_tokens SET used_by = ?1 WHERE token_hash = ?2",
            params![id, hash(&e.token)],
        )?;
        tx.commit()?;
        drop(c);
        Ok((
            self.device(&id)?.expect("just inserted"),
            credential,
            image_id,
        ))
    }

    pub fn authenticate(&self, credential: &str) -> Result<Option<String>> {
        let c = self.conn.lock().unwrap();
        Ok(c.query_row(
            "SELECT id FROM devices WHERE credential_hash = ?1",
            [hash(credential)],
            |r| r.get(0),
        )
        .optional()?)
    }

    pub fn seen(
        &self,
        id: &str,
        os_version: Option<&str>,
        agent_version: Option<&str>,
    ) -> Result<()> {
        let c = self.conn.lock().unwrap();
        c.execute(
            "UPDATE devices SET last_seen = ?2,
                    os_version = COALESCE(?3, os_version),
                    agent_version = COALESCE(?4, agent_version)
             WHERE id = ?1",
            params![id, now(), os_version, agent_version],
        )?;
        Ok(())
    }

    pub fn devices(&self, fleet: Option<&str>) -> Result<Vec<Device>> {
        let c = self.conn.lock().unwrap();
        let mut q = c.prepare(&format!(
            "{DEVICE_SELECT} WHERE (?1 IS NULL OR f.name = ?1) ORDER BY f.name, d.name"
        ))?;
        let rows = q.query_map([fleet], device_row)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn device(&self, id: &str) -> Result<Option<Device>> {
        let c = self.conn.lock().unwrap();
        Ok(c.query_row(
            &format!("{DEVICE_SELECT} WHERE d.id = ?1"),
            [id],
            device_row,
        )
        .optional()?)
    }

    /// Find a device by id or by name. A name shared by two devices is
    /// ambiguous and refused, so a tool never acts on the wrong one.
    pub fn resolve(&self, key: &str) -> Result<Device> {
        if let Some(d) = self.device(key)? {
            return Ok(d);
        }
        let c = self.conn.lock().unwrap();
        let mut q = c.prepare(&format!("{DEVICE_SELECT} WHERE d.name = ?1"))?;
        let found: Vec<Device> = q.query_map([key], device_row)?.collect::<Result<_, _>>()?;
        match found.len() {
            1 => Ok(found.into_iter().next().unwrap()),
            0 => bail!("no device named {key:?}; `list_devices` shows them"),
            n => bail!("{n} devices are named {key:?}; use the device id"),
        }
    }

    /// Create an API token and return it. It is shown once, here, and
    /// stored only as a hash.
    pub fn create_api_token(&self, name: &str, role: Role) -> Result<String> {
        valid_name(name)?;
        let token = format!("via_{}", random_secret());
        let c = self.conn.lock().unwrap();
        let n = c.execute(
            "INSERT INTO api_tokens (name, role, token_hash, created_at) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT DO NOTHING",
            params![name, role.as_str(), hash(&token), now()],
        )?;
        anyhow::ensure!(n == 1, "a token named {name:?} exists; revoke it first");
        Ok(token)
    }

    pub fn api_tokens(&self) -> Result<Vec<ApiToken>> {
        let c = self.conn.lock().unwrap();
        let mut q =
            c.prepare("SELECT name, role, created_at, last_used FROM api_tokens ORDER BY name")?;
        let rows = q.query_map([], |r| {
            let role: String = r.get(1)?;
            Ok(ApiToken {
                name: r.get(0)?,
                role: Role::parse(&role).unwrap_or(Role::Viewer),
                created_at: r.get(2)?,
                last_used: r.get(3)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn revoke_api_token(&self, name: &str) -> Result<()> {
        let c = self.conn.lock().unwrap();
        let n = c.execute("DELETE FROM api_tokens WHERE name = ?1", [name])?;
        anyhow::ensure!(n == 1, "no token named {name:?}");
        Ok(())
    }

    /// Who an API token belongs to, if anyone.
    pub fn api_caller(&self, token: &str) -> Result<Option<Caller>> {
        let c = self.conn.lock().unwrap();
        let row: Option<(String, String)> = c
            .query_row(
                "UPDATE api_tokens SET last_used = ?2 WHERE token_hash = ?1 RETURNING name, role",
                params![hash(token), now()],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        Ok(row.and_then(|(name, role)| {
            Some(Caller {
                name,
                role: Role::parse(&role)?,
            })
        }))
    }

    pub fn audit(&self, actor: &str, tool: &str, args: &serde_json::Value, ok: bool) {
        let c = self.conn.lock().unwrap();
        if let Err(e) = c.execute(
            "INSERT INTO audit (at, actor, tool, args, ok) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![now(), actor, tool, args.to_string(), ok],
        ) {
            tracing::error!("audit write failed: {e}");
        }
    }
}

const DEVICE_SELECT: &str = "
    SELECT d.id, d.name, f.name, d.arch, d.os_version, d.agent_version, d.enrolled_at, d.last_seen
    FROM devices d JOIN fleets f ON f.id = d.fleet_id";

fn device_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Device> {
    Ok(Device {
        id: r.get(0)?,
        name: r.get(1)?,
        fleet: r.get(2)?,
        arch: r.get(3)?,
        os_version: r.get(4)?,
        agent_version: r.get(5)?,
        enrolled_at: r.get(6)?,
        last_seen: r.get(7)?,
    })
}

fn fleet_id(c: &Connection, name: &str) -> Result<i64> {
    c.query_row("SELECT id FROM fleets WHERE name = ?1", [name], |r| {
        r.get(0)
    })
    .optional()?
    .with_context(|| format!("no fleet named {name:?}; `list_fleets` shows them"))
}

fn valid_name(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name.len() <= 63
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    anyhow::ensure!(ok, "names are 1-63 letters, digits, '-' or '_'");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> (tempfile::TempDir, Db) {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("via.db")).unwrap();
        (dir, db)
    }

    fn enrollment(token: &str) -> Enrollment {
        Enrollment {
            token: token.into(),
            hostname: "viaos".into(),
            arch: Some("x86_64".into()),
            os_version: Some("1".into()),
            agent_version: None,
        }
    }

    #[test]
    fn token_enrolls_once() {
        let (_d, db) = db();
        db.create_fleet("line").unwrap();
        let token = db
            .mint_token("line", Some("line-07"), 3600, Some("img"))
            .unwrap();
        let (device, cred, image) = db.enroll(&enrollment(&token)).unwrap();
        assert_eq!(image.as_deref(), Some("img"));
        assert_eq!(device.name, "line-07");
        assert_eq!(device.fleet, "line");
        assert_eq!(db.authenticate(&cred).unwrap(), Some(device.id.clone()));
        assert!(db.enroll(&enrollment(&token)).is_err());
        assert_eq!(db.fleets().unwrap()[0].devices, 1);
    }

    #[test]
    fn expired_and_unknown_tokens_are_refused() {
        let (_d, db) = db();
        db.create_fleet("line").unwrap();
        let token = db.mint_token("line", None, -1, None).unwrap();
        assert!(db.enroll(&enrollment(&token)).is_err());
        assert!(db.enroll(&enrollment("nope")).is_err());
    }

    #[test]
    fn ambiguous_names_are_refused() {
        let (_d, db) = db();
        db.create_fleet("line").unwrap();
        for _ in 0..2 {
            let t = db.mint_token("line", Some("dup"), 60, None).unwrap();
            db.enroll(&enrollment(&t)).unwrap();
        }
        assert!(db.resolve("dup").is_err());
    }

    #[test]
    fn api_tokens_authenticate_until_revoked() {
        let (_d, db) = db();
        let token = db.create_api_token("ana", Role::Viewer).unwrap();
        assert!(db.create_api_token("ana", Role::Admin).is_err());
        let caller = db.api_caller(&token).unwrap().unwrap();
        assert_eq!((caller.name.as_str(), caller.role), ("ana", Role::Viewer));
        assert!(db.api_tokens().unwrap()[0].last_used.is_some());
        assert!(db.api_caller("via_nope").unwrap().is_none());
        db.revoke_api_token("ana").unwrap();
        assert!(db.api_caller(&token).unwrap().is_none());
    }

    #[test]
    fn audit_records_the_actor() {
        let (_d, db) = db();
        db.audit("ana", "list_fleets", &serde_json::json!({}), true);
        let c = db.conn.lock().unwrap();
        let actor: String = c
            .query_row("SELECT actor FROM audit", [], |r| r.get(0))
            .unwrap();
        assert_eq!(actor, "ana");
    }
}
