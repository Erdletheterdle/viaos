//! The keys everything is signed with. There is one way to publish: signed.
//! They live in the config directory and are made the first time they're
//! needed, so nobody has to think about them until it's time to back them up.

use std::path::PathBuf;

use anyhow::{Context, Result, ensure};

fn config_dir() -> Result<PathBuf> {
    let dir = match std::env::var_os("XDG_CONFIG_HOME") {
        Some(d) if !d.is_empty() => PathBuf::from(d),
        _ => PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?).join(".config"),
    };
    Ok(dir.join("via"))
}

/// The key that signs OS updates and app releases: the one asked for, or the
/// default, made if there isn't one yet.
pub fn signing_key(given: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(path) = given {
        ensure!(path.exists(), "no signing key at {}", path.display());
        return Ok(path);
    }
    let path = config_dir()?.join("signing.key");
    if !path.exists() {
        std::fs::create_dir_all(config_dir()?)?;
        crate::remote::keygen(&path)?;
        eprintln!(
            "Made a signing key at {}. Back it up, and give the server {}.pub as VIA_TRUSTED_KEYS.",
            path.display(),
            path.display()
        );
    }
    Ok(path)
}

/// The directory of Secure Boot keys: the one asked for, or the default, made
/// if there isn't one yet.
pub fn secure_boot_keys(given: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(dir) = given {
        ensure!(
            dir.join("db.key").exists(),
            "no Secure Boot keys in {}",
            dir.display()
        );
        return Ok(dir);
    }
    let dir = config_dir()?.join("secure-boot");
    if !dir.join("db.key").exists() {
        crate::secureboot::keygen(&dir)?;
        eprintln!(
            "Made Secure Boot keys in {}. Back them up: devices that enrolled them run only what they sign.",
            dir.display()
        );
    }
    Ok(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_key_that_was_asked_for_is_never_made_up() {
        let d = tempfile::tempdir().unwrap();
        assert!(signing_key(Some(d.path().join("nope"))).is_err());
        assert!(secure_boot_keys(Some(d.path().join("nope"))).is_err());
    }
}
