//! App variables on their way to devices, and secrets at rest.
//!
//! A secret is sealed with a key the database does not hold: copying the
//! database, or a backup of it, gives ciphertext. Each value is bound to the
//! row it belongs to, so ciphertext copied onto another key will not open.
//! A device gets the plaintext over its own TLS connection, because its
//! containers need it, and keeps it readable by root only.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{Context, Result, ensure};
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use rmcp::model::JsonObject;
use serde_json::{Value, json};

use crate::State;
use crate::db::{Device, Variable};

const NONCE: usize = 24;

#[derive(Clone)]
pub struct Secrets {
    cipher: XChaCha20Poly1305,
}

impl Secrets {
    /// The key from `configured` (64 hex digits) or from `secret.key` in the
    /// data directory, made on first use.
    pub fn open(data_dir: &Path, configured: Option<&str>) -> Result<Self> {
        let hex_key = match configured {
            Some(k) => k.trim().to_string(),
            None => {
                let path = data_dir.join("secret.key");
                if !path.exists() {
                    use std::io::Write;
                    use std::os::unix::fs::OpenOptionsExt;
                    let mut f = std::fs::OpenOptions::new()
                        .write(true)
                        .create_new(true)
                        .mode(0o600)
                        .open(&path)
                        .with_context(|| format!("creating {}", path.display()))?;
                    writeln!(f, "{}", crate::db::random_secret())?;
                }
                std::fs::read_to_string(&path)?.trim().to_string()
            }
        };
        let bytes = hex::decode(&hex_key).context("the secret key is not hex")?;
        ensure!(bytes.len() == 32, "the secret key is not 32 bytes");
        Ok(Self {
            cipher: XChaCha20Poly1305::new_from_slice(&bytes)?,
        })
    }

    fn binding(scope: &str, target: &str, app: &str, key: &str) -> String {
        format!("{scope}\0{target}\0{app}\0{key}")
    }

    /// Nonce followed by ciphertext.
    pub fn seal(&self, scope: &str, target: &str, app: &str, key: &str, value: &str) -> Vec<u8> {
        let nonce: [u8; NONCE] = rand::random();
        let aad = Self::binding(scope, target, app, key);
        let sealed = self
            .cipher
            .encrypt(
                XNonce::from_slice(&nonce),
                Payload {
                    msg: value.as_bytes(),
                    aad: aad.as_bytes(),
                },
            )
            .expect("encrypting cannot fail for a value this size");
        [nonce.as_slice(), &sealed].concat()
    }

    fn open_value(&self, v: &Variable) -> Result<String> {
        ensure!(v.value.len() > NONCE, "stored secret is too short");
        let (nonce, sealed) = v.value.split_at(NONCE);
        let aad = Self::binding(&v.scope, &v.target, &v.app, &v.key);
        let plain = self
            .cipher
            .decrypt(
                XNonce::from_slice(nonce),
                Payload {
                    msg: sealed,
                    aad: aad.as_bytes(),
                },
            )
            .map_err(|_| anyhow::anyhow!("cannot open secret {}: wrong key or altered", v.key))?;
        Ok(String::from_utf8(plain)?)
    }
}

/// What a device should hold, by app: its fleet's values, then its own.
pub fn resolve(
    secrets: &Secrets,
    vars: &[Variable],
    device: &Device,
) -> Result<BTreeMap<String, BTreeMap<String, String>>> {
    let mut apps: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
    for scope in ["fleet", "device"] {
        let target = if scope == "fleet" {
            &device.fleet
        } else {
            &device.id
        };
        for v in vars
            .iter()
            .filter(|v| v.scope == scope && &v.target == target)
        {
            let value = if v.secret {
                secrets.open_value(v)?
            } else {
                String::from_utf8_lossy(&v.value).into_owned()
            };
            apps.entry(v.app.clone())
                .or_default()
                .insert(v.key.clone(), value);
        }
    }
    Ok(apps)
}

/// Send a device everything it should hold.
pub async fn push(state: &State, device: &Device) -> Result<Value> {
    let apps = resolve(&state.secrets, &state.db.variables()?, device)?;
    let mut args = JsonObject::new();
    args.insert("apps".into(), json!(apps));
    let res = state.hub.call(&device.id, "sync_env", args).await?;
    ensure!(
        res.is_error != Some(true),
        "{}",
        res.content
            .iter()
            .filter_map(|c| c.as_text().map(|t| t.text.clone()))
            .collect::<Vec<_>>()
            .join(" ")
    );
    Ok(res.structured_content.unwrap_or(Value::Null))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn var(key: &str, value: Vec<u8>, secret: bool) -> Variable {
        Variable {
            scope: "fleet".into(),
            target: "lab".into(),
            app: "hello".into(),
            key: key.into(),
            value,
            secret,
            updated_at: 0,
            updated_by: "t".into(),
        }
    }

    fn device() -> Device {
        Device {
            id: "d1".into(),
            name: "line-07".into(),
            fleet: "lab".into(),
            arch: None,
            os_version: None,
            agent_version: None,
            enrolled_at: 0,
            last_seen: None,
        }
    }

    #[test]
    fn a_secret_opens_only_where_it_was_sealed() {
        let dir = tempfile::tempdir().unwrap();
        let s = Secrets::open(dir.path(), None).unwrap();
        let sealed = s.seal("fleet", "lab", "hello", "TOKEN", "s3cret");
        assert!(!sealed.windows(6).any(|w| w == b"s3cret"));
        assert_eq!(
            s.open_value(&var("TOKEN", sealed.clone(), true)).unwrap(),
            "s3cret"
        );
        assert!(s.open_value(&var("OTHER", sealed.clone(), true)).is_err());

        // The key is kept, so a restart opens what was sealed before it.
        let again = Secrets::open(dir.path(), None).unwrap();
        assert_eq!(
            again.open_value(&var("TOKEN", sealed, true)).unwrap(),
            "s3cret"
        );
    }

    #[test]
    fn a_devices_value_beats_its_fleets() {
        let dir = tempfile::tempdir().unwrap();
        let s = Secrets::open(dir.path(), None).unwrap();
        let mut own = var("MODE", b"device".to_vec(), false);
        own.scope = "device".into();
        own.target = "d1".into();
        let mut other = var("MODE", b"other".to_vec(), false);
        other.target = "elsewhere".into();
        let vars = vec![
            own,
            var("MODE", b"fleet".to_vec(), false),
            other,
            var("REGION", b"us".to_vec(), false),
        ];
        let env = resolve(&s, &vars, &device()).unwrap();
        assert_eq!(env["hello"]["MODE"], "device");
        assert_eq!(env["hello"]["REGION"], "us");
    }
}
