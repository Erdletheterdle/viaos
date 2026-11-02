//! App variables and secrets, as the control plane resolved them for this
//! device. Kept on /var so a release keeps working with no network: a reboot,
//! a rollback and a recovery all apply the same values.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::Result;
use via_core::Deployment;

/// App name to variable name to value.
pub type Env = BTreeMap<String, BTreeMap<String, String>>;

fn path(data_dir: &Path) -> PathBuf {
    data_dir.join("env.json")
}

/// What is stored, or nothing: a device that never heard from a control plane
/// has no variables.
pub fn load(data_dir: &Path) -> Env {
    std::fs::read(path(data_dir))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

pub fn save(data_dir: &Path, env: &Env) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let tmp = path(data_dir).with_extension("tmp");
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)?;
    f.write_all(&serde_json::to_vec(env)?)?;
    f.sync_all()?;
    std::fs::rename(tmp, path(data_dir))?;
    Ok(())
}

/// Names the running release's services declared and nothing has supplied,
/// by service.
pub fn missing(deployment: &Deployment, env: &Env) -> BTreeMap<String, Vec<String>> {
    let empty = BTreeMap::new();
    let have = env.get(&deployment.project).unwrap_or(&empty);
    deployment
        .services
        .iter()
        .filter_map(|(name, s)| {
            let keys: Vec<String> = s
                .runtime_env
                .iter()
                .filter(|k| !have.contains_key(*k))
                .cloned()
                .collect();
            (!keys.is_empty()).then(|| (name.clone(), keys))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stored_values_come_back_and_are_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        assert!(load(dir.path()).is_empty());
        let env = Env::from([(
            "hello".to_string(),
            BTreeMap::from([("TOKEN".to_string(), "s3cret".to_string())]),
        )]);
        save(dir.path(), &env).unwrap();
        assert_eq!(load(dir.path()), env);
        let mode = std::fs::metadata(path(dir.path()))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }
}
