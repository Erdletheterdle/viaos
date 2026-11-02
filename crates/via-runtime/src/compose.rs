//! Running the official `docker compose` on a device.
//!
//! Everything about how a file behaves is Compose's. What is decided here is
//! how a device's values reach it: names a service asked for go in an override
//! file, so Compose's own config hash replaces exactly the containers whose
//! values changed, and everything goes in the process environment for
//! `${NAME}` in the file.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Stdio;

use serde_json::{Value, json};
use tokio::process::Command;
use tokio::time::{Duration, timeout};
use via_core::Deployment;

use crate::Error;
use crate::types::ApplyContext;

/// Beside the compose file, and readable only by root: it can hold secrets.
pub const OVERRIDE_FILE: &str = ".via.override.json";

/// Compose reads `$` in a value as the start of a variable, even in an override.
fn escape(value: &str) -> String {
    value.replace('$', "$$")
}

/// Each service's own values: a default the release gave a name it asked for,
/// then what is set for the app, then what is set for that one service.
pub fn override_document(deployment: &Deployment, ctx: &ApplyContext) -> Option<Value> {
    let mut services = serde_json::Map::new();
    for (name, service) in &deployment.services {
        let mut env: BTreeMap<&str, &str> = BTreeMap::new();
        for spec in &deployment.variables {
            if let Some(default) = &spec.default
                && service.runtime_env.contains(&spec.key)
            {
                env.insert(&spec.key, default);
            }
        }
        for (k, v) in &ctx.global_environment {
            if service.wants_runtime_key(k) {
                env.insert(k, v);
            }
        }
        for (k, v) in ctx.env_for(name).into_iter().flatten() {
            env.insert(k, v);
        }
        if !env.is_empty() {
            let env: serde_json::Map<String, Value> = env
                .into_iter()
                .map(|(k, v)| (k.to_string(), Value::String(escape(v))))
                .collect();
            services.insert(name.clone(), json!({ "environment": env }));
        }
    }
    (!services.is_empty()).then(|| json!({ "services": services }))
}

/// What `${NAME}` in the file sees: the release's defaults, then the app's
/// values. Names that steer Compose itself stay out of a value's reach.
pub fn process_env(deployment: &Deployment, ctx: &ApplyContext) -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    for spec in &deployment.variables {
        if let Some(default) = &spec.default {
            env.insert(spec.key.clone(), default.clone());
        }
    }
    env.extend(ctx.global_environment.clone());
    env.retain(|k, _| k != "PATH" && !k.starts_with("COMPOSE_") && !k.starts_with("DOCKER_"));
    env
}

fn compose(project: &str) -> Command {
    let mut cmd = Command::new(via_core::docker_program());
    cmd.env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Dropping an `up` that ran out of time has to end it.
        .kill_on_drop(true)
        .args(["compose", "-p", project]);
    for key in ["PATH", "HOME"] {
        if let Some(value) = std::env::var_os(key) {
            cmd.env(key, value);
        }
    }
    cmd
}

async fn run(mut cmd: Command, limit: Duration, what: &str) -> Result<(), Error> {
    let out = match timeout(limit, cmd.output()).await {
        Ok(out) => out.map_err(|e| Error::Compose {
            reason: format!("couldn't run docker compose: {e}"),
        })?,
        Err(_) => {
            return Err(Error::Compose {
                reason: format!("{what} took longer than {}s", limit.as_secs()),
            });
        }
    };
    if out.status.success() {
        return Ok(());
    }
    Err(Error::Compose {
        reason: format!("{what}: {}", String::from_utf8_lossy(&out.stderr).trim()),
    })
}

/// `docker compose up`, nothing fetched and nothing built: the release brought
/// its images. Bounded by the release's own gate, because one service waiting
/// on another that never gets healthy would otherwise wait for good.
pub async fn up(deployment: &Deployment, ctx: &ApplyContext, dir: &Path) -> Result<(), Error> {
    let override_path = dir.join(OVERRIDE_FILE);
    let document = override_document(deployment, ctx);
    match &document {
        Some(doc) => {
            let text = serde_json::to_vec(doc).map_err(|e| Error::Invalid {
                reason: e.to_string(),
            })?;
            write_private(&override_path, &text)?;
        }
        None => match std::fs::remove_file(&override_path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(Error::io(&override_path, e)),
        },
    }

    let mut cmd = compose(&deployment.project);
    cmd.arg("--project-directory").arg(dir);
    cmd.current_dir(dir);
    cmd.arg("-f").arg(dir.join("compose.yaml"));
    if document.is_some() {
        cmd.arg("-f").arg(&override_path);
    }
    cmd.args([
        "up",
        "-d",
        "--remove-orphans",
        "--pull",
        "never",
        "--no-build",
    ]);
    cmd.envs(process_env(deployment, ctx));
    run(
        cmd,
        Duration::from_secs(deployment.health_gate.timeout_secs),
        "docker compose up",
    )
    .await
}

fn write_private(path: &Path, bytes: &[u8]) -> Result<(), Error> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .map_err(|e| Error::io(path, e))?;
    file.write_all(bytes).map_err(|e| Error::io(path, e))
}

pub async fn stop(project: &str) -> Result<(), Error> {
    let mut cmd = compose(project);
    cmd.arg("stop");
    run(cmd, Duration::from_secs(300), "docker compose stop").await
}

pub async fn down(project: &str, remove_volumes: bool) -> Result<(), Error> {
    let mut cmd = compose(project);
    cmd.args(["down", "--remove-orphans"]);
    if remove_volumes {
        cmd.arg("--volumes");
    }
    run(cmd, Duration::from_secs(300), "docker compose down").await
}

#[cfg(test)]
mod tests {
    use super::*;
    use via_core::{ImageRef, Platform, Service, VariableSpec};

    fn deployment() -> Deployment {
        let mut d = Deployment::new("app", Platform::new("linux", "amd64"));
        let mut web = Service::new("web", ImageRef::new("web:1"));
        web.runtime_env = vec!["GREETING".into(), "TOKEN".into()];
        web.environment.insert("LITERAL".into(), "x".into());
        d.services.insert("web".into(), web);
        d.services
            .insert("db".into(), Service::new("db", ImageRef::new("db:1")));
        d.variables.push(VariableSpec {
            key: "GREETING".into(),
            services: vec!["web".into()],
            description: None,
            default: Some("hi".into()),
            secret: false,
            required: false,
        });
        d
    }

    fn global(pairs: &[(&str, &str)]) -> ApplyContext {
        ApplyContext::default().with_global(
            pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        )
    }

    #[test]
    fn only_the_service_that_asked_gets_a_value() {
        let doc =
            override_document(&deployment(), &global(&[("TOKEN", "t"), ("OTHER", "o")])).unwrap();
        assert_eq!(doc["services"]["web"]["environment"]["TOKEN"], "t");
        assert!(doc["services"]["web"]["environment"].get("OTHER").is_none());
        assert!(doc["services"].get("db").is_none());
    }

    #[test]
    fn a_default_is_what_is_left_when_nothing_is_set() {
        let d = deployment();
        let doc = override_document(&d, &ApplyContext::default()).unwrap();
        assert_eq!(doc["services"]["web"]["environment"]["GREETING"], "hi");
        let doc = override_document(&d, &global(&[("GREETING", "yo")])).unwrap();
        assert_eq!(doc["services"]["web"]["environment"]["GREETING"], "yo");
    }

    #[test]
    fn one_services_value_beats_the_apps_and_a_literal_in_the_file() {
        let mut ctx = global(&[("LITERAL", "app"), ("TOKEN", "app")]);
        ctx.environment.insert(
            "web".into(),
            BTreeMap::from([("TOKEN".to_string(), "own".to_string())]),
        );
        let doc = override_document(&deployment(), &ctx).unwrap();
        let env = &doc["services"]["web"]["environment"];
        assert_eq!(env["TOKEN"], "own");
        assert_eq!(env["LITERAL"], "app");
    }

    #[test]
    fn a_dollar_in_a_value_stays_a_dollar() {
        let doc = override_document(&deployment(), &global(&[("TOKEN", "pa$$w0rd$x")])).unwrap();
        assert_eq!(
            doc["services"]["web"]["environment"]["TOKEN"],
            "pa$$$$w0rd$$x"
        );
    }

    #[test]
    fn a_value_cannot_steer_compose_itself() {
        let env = process_env(
            &deployment(),
            &global(&[
                ("COMPOSE_FILE", "/evil.yaml"),
                ("DOCKER_HOST", "tcp://x"),
                ("PATH", "/evil"),
                ("PORT", "8080"),
            ]),
        );
        assert_eq!(env["PORT"], "8080");
        assert_eq!(env["GREETING"], "hi");
        assert!(!env.contains_key("COMPOSE_FILE"));
        assert!(!env.contains_key("DOCKER_HOST"));
        assert!(!env.contains_key("PATH"));
    }
}
