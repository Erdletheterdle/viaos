//! App variables and secrets, as the control plane resolved them for this
//! device. Kept on /var so a release keeps working with no network: a reboot,
//! a rollback and a recovery all apply the same values.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::{Deserialize, Serialize};
use via_core::Deployment;
use via_deploy::{AppliedEnv, Deployer};
use via_runtime::ApplyContext;

/// App name to variable name to value.
pub type Apps = BTreeMap<String, BTreeMap<String, String>>;
/// App name to service name to variable name to value.
pub type ServiceApps = BTreeMap<String, BTreeMap<String, BTreeMap<String, String>>>;

/// What this device holds. A value in `services` wins for that service; the
/// control plane has already dropped any that a stronger value beats.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Env {
    #[serde(default)]
    pub apps: Apps,
    #[serde(default)]
    pub services: ServiceApps,
}

const FORMAT: u32 = 2;

fn path(data_dir: &Path) -> PathBuf {
    data_dir.join("env.json")
}

/// What is stored, or nothing: a device that never heard from a control plane
/// has no variables.
///
/// The first format was the bare `apps` map. Its top-level values are always
/// objects, so a number under `version` tells the two apart.
pub fn load(data_dir: &Path) -> Env {
    let Some(stored) = std::fs::read(path(data_dir))
        .ok()
        .and_then(|b| serde_json::from_slice::<serde_json::Value>(&b).ok())
    else {
        return Env::default();
    };
    if stored.get("version").is_some_and(|v| v.is_number()) {
        serde_json::from_value(stored).unwrap_or_default()
    } else {
        Env {
            apps: serde_json::from_value(stored).unwrap_or_default(),
            services: ServiceApps::new(),
        }
    }
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
    let stored = serde_json::json!({
        "version": FORMAT, "apps": env.apps, "services": env.services,
    });
    f.write_all(&serde_json::to_vec(&stored)?)?;
    f.sync_all()?;
    std::fs::rename(tmp, path(data_dir))?;
    Ok(())
}

impl Env {
    /// One app's share of these values.
    pub fn for_app(&self, app: &str) -> AppliedEnv {
        AppliedEnv {
            global: self.apps.get(app).cloned().unwrap_or_default(),
            services: self.services.get(app).cloned().unwrap_or_default(),
        }
    }

    /// Have a deployer apply these values.
    pub fn configure<'a>(&self, deployer: Deployer<'a>) -> Deployer<'a> {
        deployer
            .with_env(self.apps.clone())
            .with_service_env(self.services.clone())
    }

    /// Whether moving to `other` changes what any service of `deployment`
    /// sees. If not, there is nothing to re-apply.
    pub fn changes_for(&self, other: &Env, deployment: &Deployment) -> bool {
        let context = |env: &Env| -> ApplyContext {
            via_deploy::runtime_context(deployment, &env.for_app(&deployment.project), None)
        };
        let (before, after) = (context(self), context(other));
        deployment
            .services
            .values()
            .any(|s| before.effective_env(s) != after.effective_env(s))
    }
}

/// Names the running release's services declared and nothing has supplied,
/// by service. A name the release gave a default for, or called optional, is
/// not missing.
pub fn missing(deployment: &Deployment, env: &Env) -> BTreeMap<String, Vec<String>> {
    let values = env.for_app(&deployment.project);
    deployment
        .services
        .iter()
        .filter_map(|(name, s)| {
            let own = values.services.get(name);
            // A bare `- KEY`, and a `${KEY}` anywhere in the service.
            let interpolated = deployment
                .variables
                .iter()
                .filter(|v| v.required && v.services.contains(name))
                .map(|v| &v.key);
            let wanted: std::collections::BTreeSet<&String> =
                s.runtime_env.iter().chain(interpolated).collect();
            let keys: Vec<String> = wanted
                .into_iter()
                .filter(|k| {
                    !values.global.contains_key(*k)
                        && !own.is_some_and(|o| o.contains_key(*k))
                        && !deployment.variable_is_optional(k)
                })
                .cloned()
                .collect();
            (!keys.is_empty()).then(|| (name.clone(), keys))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use via_core::{ImageRef, Platform, Service, VariableSpec};

    fn hello(key: &str, value: &str) -> BTreeMap<String, BTreeMap<String, String>> {
        BTreeMap::from([(
            "hello".to_string(),
            BTreeMap::from([(key.to_string(), value.to_string())]),
        )])
    }

    fn deployment() -> Deployment {
        let mut d = Deployment::new("hello", Platform::new("linux", "amd64"));
        for name in ["web", "db"] {
            d.services
                .insert(name.into(), Service::new(name, ImageRef::new("x:1")));
        }
        d.services.get_mut("web").unwrap().runtime_env = vec!["TOKEN".into()];
        d
    }

    #[test]
    fn stored_values_come_back_and_are_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load(dir.path()), Env::default());
        let env = Env {
            apps: hello("TOKEN", "s3cret"),
            services: BTreeMap::from([("hello".to_string(), hello("TOKEN", "just-web"))]),
        };
        save(dir.path(), &env).unwrap();
        assert_eq!(load(dir.path()), env);
        let mode = std::fs::metadata(path(dir.path()))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn a_file_from_before_service_values_still_loads() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(path(dir.path()), r#"{"hello":{"TOKEN":"s3cret"}}"#).unwrap();
        let env = load(dir.path());
        assert_eq!(env.apps, hello("TOKEN", "s3cret"));
        assert!(env.services.is_empty());
    }

    #[test]
    fn an_app_can_be_called_version_or_apps_in_the_old_format() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            path(dir.path()),
            r#"{"version":{"A":"1"},"apps":{"B":"2"}}"#,
        )
        .unwrap();
        let env = load(dir.path());
        assert_eq!(env.apps["version"]["A"], "1");
        assert_eq!(env.apps["apps"]["B"], "2");
    }

    #[test]
    fn missing_counts_each_services_own_values() {
        let d = deployment();
        let nothing = Env::default();
        assert_eq!(missing(&d, &nothing)["web"], vec!["TOKEN".to_string()]);

        let aimed_at_web = Env {
            apps: Apps::new(),
            services: BTreeMap::from([(
                "hello".to_string(),
                BTreeMap::from([(
                    "web".to_string(),
                    BTreeMap::from([("TOKEN".to_string(), "x".to_string())]),
                )]),
            )]),
        };
        assert!(missing(&d, &aimed_at_web).is_empty());

        let aimed_elsewhere = Env {
            apps: Apps::new(),
            services: BTreeMap::from([(
                "hello".to_string(),
                BTreeMap::from([(
                    "db".to_string(),
                    BTreeMap::from([("TOKEN".to_string(), "x".to_string())]),
                )]),
            )]),
        };
        assert_eq!(
            missing(&d, &aimed_elsewhere)["web"],
            vec!["TOKEN".to_string()]
        );
    }

    #[test]
    fn an_optional_name_is_not_missing() {
        let mut d = deployment();
        d.variables.push(VariableSpec {
            key: "TOKEN".into(),
            services: vec!["web".into()],
            description: None,
            default: None,
            secret: false,
            required: false,
        });
        assert!(missing(&d, &Env::default()).is_empty());
    }

    #[test]
    fn a_change_no_service_declared_needs_no_re_apply() {
        let d = deployment();
        let before = Env::default();
        let unrelated = Env {
            apps: hello("NOT_DECLARED", "x"),
            services: ServiceApps::new(),
        };
        assert!(!before.changes_for(&unrelated, &d));
        let declared = Env {
            apps: hello("TOKEN", "x"),
            services: ServiceApps::new(),
        };
        assert!(before.changes_for(&declared, &d));
    }
}
