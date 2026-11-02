//! Reading a project with the official `docker compose`.
//!
//! Nothing here parses Compose. `docker compose config` resolves the project
//! and says what it means, and a device runs `docker compose up` on the result,
//! so a file means here what it means anywhere else. What this crate adds is
//! the part Compose has no opinion on: which local files a release has to carry
//! and what to show about it.

mod metadata;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::Value;
use thiserror::Error as ThisError;
use via_core::{Deployment, Platform};

pub use metadata::{VariableRef, references};

/// Names this crate looks for, in Compose's own order of preference.
pub const COMPOSE_FILES: [&str; 4] = [
    "compose.yaml",
    "compose.yml",
    "docker-compose.yaml",
    "docker-compose.yml",
];

/// What would stay on a device without anyone asking for it.
const NEVER_SHIPPED: [&str; 1] = [".git"];

#[derive(Debug, ThisError)]
pub enum Error {
    #[error("{0}")]
    Compose(String),

    #[error("running docker compose: {0}")]
    Spawn(#[source] std::io::Error),

    #[error("{path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },

    #[error("reading docker compose's answer: {0}")]
    Output(String),

    #[error("service {service:?}: {reason}")]
    Service { service: String, reason: String },

    #[error("x-via variable {key:?}: {reason}")]
    Variable { key: String, reason: String },

    #[error(
        "{path} is outside the project, so it cannot travel with the release. \
         Move it under {project}, or use an absolute path that exists on the device"
    )]
    OutsideProject { path: String, project: String },
}

/// A thing worth telling whoever is building the release.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Warning {
    pub service: Option<String>,
    pub message: String,
}

impl std::fmt::Display for Warning {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.service {
            Some(s) => write!(f, "{s}: {}", self.message),
            None => f.write_str(&self.message),
        }
    }
}

/// A local file a release carries, relative to the project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectFile {
    pub path: String,
    pub source: PathBuf,
}

/// The first compose file in `dir`, by Compose's own rule.
pub fn discover(dir: impl AsRef<Path>) -> Option<PathBuf> {
    COMPOSE_FILES
        .iter()
        .map(|name| dir.as_ref().join(name))
        .find(|p| p.is_file())
}

/// A project as `docker compose` reads it.
pub struct Project {
    file: PathBuf,
    dir: PathBuf,
    name: String,
    platform: Platform,
    profiles: Vec<String>,
    /// Gives each built service a tag of its release's own. Without one every
    /// release builds the same `<project>-<service>:latest`, and a rollback
    /// would find the newer image under that name.
    tags: tempfile::NamedTempFile,
    /// The services these profiles turn on, by Compose's own rule. Empty until
    /// `open` has asked.
    active: BTreeSet<String>,
    flat: Value,
    resolved: Value,
}

impl Project {
    pub fn open(
        file: impl AsRef<Path>,
        name: impl Into<String>,
        platform: Platform,
        profiles: Vec<String>,
        tag: &str,
    ) -> Result<Self, Error> {
        let file = std::path::absolute(file.as_ref()).map_err(|source| Error::Io {
            path: file.as_ref().display().to_string(),
            source,
        })?;
        let dir = file
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("/"));
        let name = name.into();
        let tags = tempfile::Builder::new()
            .prefix("via-tags-")
            .suffix(".yaml")
            .tempfile()
            .map_err(|source| Error::Io {
                path: "temporary file".into(),
                source,
            })?;
        std::fs::write(tags.path(), "{}\n").map_err(|source| Error::Io {
            path: tags.path().display().to_string(),
            source,
        })?;
        let mut project = Self {
            file,
            dir,
            name,
            platform,
            profiles,
            tags,
            active: BTreeSet::new(),
            flat: Value::Null,
            resolved: Value::Null,
        };

        let untagged = project.config_flat()?;
        let services: serde_json::Map<String, Value> = untagged["services"]
            .as_object()
            .into_iter()
            .flatten()
            .filter(|(_, s)| s.get("build").is_some() && s.get("image").is_none())
            .map(|(service, _)| {
                let image = format!("{}/{service}:{tag}", project.name);
                (service.clone(), serde_json::json!({ "image": image }))
            })
            .collect();
        std::fs::write(
            project.tags.path(),
            serde_json::json!({ "services": services }).to_string(),
        )
        .map_err(|source| Error::Io {
            path: project.tags.path().display().to_string(),
            source,
        })?;

        // Everything, until the next question has said what's active.
        project.flat = project.config_flat()?;
        let mut services = project.command();
        services.args(["config", "--services"]);
        project.stand_ins(&mut services);
        project.active = capture(&mut services)?
            .lines()
            .map(str::to_string)
            .collect();
        project.flat = project.only_active(project.flat.clone())?;
        project.resolved = project.config_resolved()?;
        Ok(project)
    }

    fn command(&self) -> Command {
        let mut cmd = Command::new(via_core::docker_program());
        cmd.arg("compose")
            .arg("-p")
            .arg(&self.name)
            .arg("-f")
            .arg(&self.file)
            .arg("-f")
            .arg(self.tags.path())
            .arg("--project-directory")
            .arg(&self.dir)
            .current_dir(&self.dir)
            .env("DOCKER_DEFAULT_PLATFORM", self.platform.to_string());
        for profile in &self.profiles {
            cmd.arg("--profile").arg(profile);
        }
        cmd
    }

    fn config_flat(&self) -> Result<Value, Error> {
        let text = capture(self.command().args([
            "config",
            "--no-interpolate",
            "--no-path-resolution",
            "--format",
            "json",
        ]))?;
        self.only_active(serde_json::from_str(&text).map_err(|e| Error::Output(e.to_string()))?)
    }

    fn only_active(&self, mut doc: Value) -> Result<Value, Error> {
        if !self.active.is_empty()
            && let Some(services) = doc["services"].as_object_mut()
        {
            services.retain(|name, _| self.active.contains(name));
            for service in services.values_mut() {
                if let Some(service) = service.as_object_mut() {
                    service.remove("profiles");
                }
            }
        }
        Ok(doc)
    }

    /// A name marked required that only a device will have gets a stand-in:
    /// nothing built depends on it, and without one Compose refuses to read the
    /// file at all.
    fn stand_ins(&self, cmd: &mut Command) {
        let dotenv = self.dotenv_keys();
        for r in metadata::references(&self.flat) {
            if r.required
                && std::env::var_os(&r.name).is_none()
                && !dotenv.contains(r.name.as_str())
            {
                cmd.env(&r.name, "via-set-on-the-device");
            }
        }
    }

    /// What the project means here.
    fn config_resolved(&self) -> Result<Value, Error> {
        let mut cmd = self.command();
        cmd.args(["config", "--format", "json"]);
        self.stand_ins(&mut cmd);
        let text = capture(&mut cmd)?;
        self.only_active(serde_json::from_str(&text).map_err(|e| Error::Output(e.to_string()))?)
    }

    fn dotenv_keys(&self) -> BTreeSet<String> {
        std::fs::read_to_string(self.dir.join(".env"))
            .unwrap_or_default()
            .lines()
            .map(str::trim)
            .filter(|l| !l.starts_with('#'))
            .filter_map(|l| l.trim_start_matches("export ").split_once('='))
            .map(|(k, _)| k.trim().to_string())
            .collect()
    }

    /// Build every service that has a `build:`.
    pub fn build(&self) -> Result<(), Error> {
        run(self.command().arg("build"))
    }

    /// Fetch what is not here yet, as `docker compose up` would.
    pub fn pull_missing(&self) -> Result<(), Error> {
        run(self
            .command()
            .args(["pull", "--ignore-buildable", "--policy", "missing"]))
    }

    /// What the release shows, and what its images are.
    pub fn deployment(&self) -> Result<(Deployment, Vec<Warning>), Error> {
        metadata::deployment(
            &self.name,
            self.platform.clone(),
            &self.flat,
            &self.resolved,
            &self.dotenv_keys(),
        )
    }

    /// The compose file a device runs: the project flattened by Compose, with
    /// nothing left to build. Variables are still unresolved, for the device.
    pub fn device_compose(&self) -> Result<String, Error> {
        let text = capture(self.command().args([
            "config",
            "--no-interpolate",
            "--no-path-resolution",
            "--format",
            "yaml",
        ]))?;
        let mut doc: serde_yaml_ng::Value =
            serde_yaml_ng::from_str(&text).map_err(|e| Error::Output(e.to_string()))?;
        if let Some(services) = doc
            .get_mut("services")
            .and_then(serde_yaml_ng::Value::as_mapping_mut)
        {
            // The profiles were decided here, so the device has none to pick.
            services.retain(|name, _| name.as_str().is_some_and(|n| self.active.contains(n)));
            for (_, service) in services.iter_mut() {
                if let Some(service) = service.as_mapping_mut() {
                    service.remove("build");
                    service.remove("develop");
                    service.remove("profiles");
                }
            }
        }
        serde_yaml_ng::to_string(&doc).map_err(|e| Error::Output(e.to_string()))
    }

    /// Every local file the project reads when it runs: bind mount sources,
    /// `env_file`s, `configs` and `secrets` from files, and `.env`.
    pub fn files(&self) -> Result<(Vec<ProjectFile>, Vec<Warning>), Error> {
        let mut wanted = BTreeSet::new();
        let mut warnings = Vec::new();
        let dotenv = self.dir.join(".env");
        if dotenv.is_file() {
            wanted.insert(".env".to_string());
            warnings.push(Warning {
                service: None,
                message: "`.env` travels with the release, as `docker compose` would read it. \
                          Use a Via secret for anything that must stay off the device's disk"
                    .into(),
            });
        }
        for (name, service) in self.flat["services"].as_object().into_iter().flatten() {
            for mount in service["volumes"].as_array().into_iter().flatten() {
                if mount["type"] == "bind"
                    && let Some(source) = mount["source"].as_str()
                {
                    self.want(source, Some(name), &mut wanted, &mut warnings)?;
                }
            }
            for entry in service["env_file"].as_array().into_iter().flatten() {
                let path = entry["path"].as_str().or(entry.as_str());
                if let Some(path) = path {
                    self.want(path, Some(name), &mut wanted, &mut warnings)?;
                }
            }
        }
        for section in ["configs", "secrets"] {
            for (_, item) in self.flat[section].as_object().into_iter().flatten() {
                if let Some(path) = item["file"].as_str() {
                    self.want(path, None, &mut wanted, &mut warnings)?;
                }
            }
        }

        let mut files = BTreeMap::new();
        for path in wanted {
            collect(&self.dir, &self.dir.join(&path), &mut files)?;
        }
        let files = files
            .into_iter()
            .map(|(path, source)| ProjectFile { path, source })
            .collect();
        Ok((files, warnings))
    }

    /// Note a path the project names. An absolute one means a path on the
    /// device, and one that is not here is for Docker to create there.
    fn want(
        &self,
        path: &str,
        service: Option<&str>,
        wanted: &mut BTreeSet<String>,
        warnings: &mut Vec<Warning>,
    ) -> Result<(), Error> {
        if path.starts_with('/') || path.starts_with('~') {
            return Ok(());
        }
        if path.contains('$') {
            warnings.push(Warning {
                service: service.map(str::to_string),
                message: format!(
                    "{path} depends on a variable, so it can't be known at build time and \
                     nothing at that path travels with the release"
                ),
            });
            return Ok(());
        }
        let relative = tidy(Path::new(path)).ok_or_else(|| Error::OutsideProject {
            path: path.to_string(),
            project: self.dir.display().to_string(),
        })?;
        if self.dir.join(&relative).exists() {
            wanted.insert(relative);
        }
        Ok(())
    }
}

/// A relative path with `.` removed, unless it climbs out of its directory.
fn tidy(path: &Path) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    for part in path.components() {
        match part {
            Component::CurDir => {}
            Component::Normal(p) => parts.push(p.to_string_lossy().into_owned()),
            Component::ParentDir => {
                parts.pop()?;
            }
            _ => return None,
        }
    }
    Some(if parts.is_empty() {
        ".".into()
    } else {
        parts.join("/")
    })
}

/// Files at or under `path`, by their path relative to `root`.
fn collect(root: &Path, path: &Path, out: &mut BTreeMap<String, PathBuf>) -> Result<(), Error> {
    let io = |source| Error::Io {
        path: path.display().to_string(),
        source,
    };
    let meta = std::fs::metadata(path).map_err(io)?;
    if meta.is_dir() {
        for entry in std::fs::read_dir(path).map_err(io)? {
            let entry = entry.map_err(io)?;
            if NEVER_SHIPPED.iter().any(|n| entry.file_name() == *n) {
                continue;
            }
            collect(root, &entry.path(), out)?;
        }
    } else if meta.is_file() {
        let relative = path.strip_prefix(root).unwrap_or(path);
        out.insert(relative.to_string_lossy().into_owned(), path.to_path_buf());
    }
    Ok(())
}

fn capture(cmd: &mut Command) -> Result<String, Error> {
    let out = cmd.stdin(Stdio::null()).output().map_err(Error::Spawn)?;
    if !out.status.success() {
        return Err(Error::Compose(
            String::from_utf8_lossy(&out.stderr).trim().to_string(),
        ));
    }
    String::from_utf8(out.stdout).map_err(|e| Error::Output(e.to_string()))
}

fn run(cmd: &mut Command) -> Result<(), Error> {
    let status = cmd.stdin(Stdio::null()).status().map_err(Error::Spawn)?;
    if status.success() {
        Ok(())
    } else {
        Err(Error::Compose(format!("docker compose failed: {status}")))
    }
}
