//! `viaos.toml`: the device's configuration, on the boot partition.
//!
//! It lives on the FAT partition so anyone can edit it from a laptop after
//! writing the image. It is read on every start, so editing it and rebooting
//! is the whole reconfiguration story.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub hostname: Option<String>,
    pub control: Option<Control>,
    #[serde(default)]
    pub ssh: Ssh,
    #[serde(default)]
    pub trust: Trust,
}

/// Who the device takes OS updates and app releases from.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Trust {
    /// Public keys, hex, from `via keygen`. With any listed, an update or
    /// release must be signed by one of them; with none, signatures are not
    /// checked and the sha256 the control plane sends is all that is.
    #[serde(default)]
    pub keys: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Control {
    /// `wss://host[:port]` or, for a lab, `ws://`.
    pub url: String,
    /// One-time enrollment token. Spent on first contact; the device then
    /// authenticates with the credential it was issued.
    pub token: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Ssh {
    #[serde(default)]
    pub authorized_keys: Vec<String>,
}

impl Config {
    /// A missing file is an unconfigured device, not an error: it still runs
    /// containers and still answers MCP over SSH or the console.
    pub fn load(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(text) => toml::from_str(&text).with_context(|| format!("parsing {}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    pub fn trust_policy(&self) -> Result<via_bundle::TrustPolicy> {
        if self.trust.keys.is_empty() {
            return Ok(via_bundle::TrustPolicy::Insecure);
        }
        let keys = self
            .trust
            .keys
            .iter()
            .map(|k| via_bundle::PublicKey::from_hex(k).with_context(|| format!("[trust] key {k:?}")))
            .collect::<Result<Vec<_>>>()?;
        Ok(via_bundle::TrustPolicy::from_keys(keys))
    }

    /// Make the host match the file. Each step is independent, so one bad
    /// value does not stop the others.
    pub fn apply(&self, data_dir: &Path) {
        let name = self.hostname.clone().unwrap_or_else(default_hostname);
        if let Err(e) = set_hostname(&name) {
            tracing::warn!("hostname {name:?} not applied: {e:#}");
        }
        let keys = authorized_keys_path(data_dir);
        let body: String = self.ssh.authorized_keys.iter().map(|k| format!("{}\n", k.trim())).collect();
        if let Err(e) = std::fs::write(&keys, body) {
            tracing::warn!("{} not written: {e}", keys.display());
        }
    }
}

pub fn authorized_keys_path(data_dir: &Path) -> PathBuf {
    data_dir.join("authorized_keys")
}

/// `viaos-` and the start of the machine id: stable across reboots, and
/// different for every device on a shop floor that was never named.
fn default_hostname() -> String {
    let id = std::fs::read_to_string("/etc/machine-id").unwrap_or_default();
    match id.trim().get(..6) {
        Some(prefix) => format!("viaos-{prefix}"),
        None => "viaos".into(),
    }
}

fn set_hostname(name: &str) -> Result<()> {
    let valid = !name.is_empty()
        && name.len() <= 63
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        && !name.starts_with('-');
    anyhow::ensure!(valid, "not a valid hostname");
    let status = std::process::Command::new("hostnamectl")
        .args(["set-hostname", "--transient", name])
        .status()
        .context("running hostnamectl")?;
    anyhow::ensure!(status.success(), "hostnamectl exited with {status}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn example_file_parses() {
        let text = include_str!("../../../nix/viaos.example.toml");
        let cfg: Config = toml::from_str(text).unwrap();
        assert!(cfg.control.is_none());
    }

    #[test]
    fn full_file_parses() {
        let cfg: Config = toml::from_str(
            r#"
            hostname = "line-07"
            [control]
            url = "wss://via.example.com"
            token = "abc"
            [ssh]
            authorized_keys = ["ssh-ed25519 AAAA x"]
            "#,
        )
        .unwrap();
        assert_eq!(cfg.control.as_ref().unwrap().token.as_deref(), Some("abc"));
        assert_eq!(cfg.ssh.authorized_keys.len(), 1);
        assert!(!cfg.trust_policy().unwrap().requires_signature());
    }

    #[test]
    fn trust_keys_require_signatures() {
        let key = via_bundle::SigningKey::generate().public().to_hex();
        let cfg: Config = toml::from_str(&format!("[trust]\nkeys = [\"{key}\"]\n")).unwrap();
        assert!(cfg.trust_policy().unwrap().requires_signature());
        let bad: Config = toml::from_str("[trust]\nkeys = [\"zz\"]\n").unwrap();
        assert!(bad.trust_policy().is_err());
    }
}
