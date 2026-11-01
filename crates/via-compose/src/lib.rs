//! Compose is Via's authoring format, not its runtime contract.
//!
//! This crate exists to turn a `compose.yaml` into a [`via_core::Deployment`]
//! at build time. Nothing downstream depends on it: the agent consumes the
//! normalized manifest and has never heard of Compose. That is what lets Via
//! keep a familiar authoring experience (spec section 15) without every device
//! carrying a Compose parser (spec section 16), and what would let a second
//! authoring format be added without touching the runtime.
//!
//! ```no_run
//! use via_compose::{Loader, NormalizeOptions};
//! use via_core::Platform;
//!
//! let loaded = Loader::new("compose.yaml").load()?;
//! let opts = NormalizeOptions::new("demo-stack", Platform::parse("linux/arm64")?)
//!     .base_dir(loaded.base_dir.clone())
//!     .build_tag("44");
//! let normalized = via_compose::normalize(&loaded.file, &opts)?;
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

mod interpolate;
mod normalize;
mod raw;
mod units;

use std::path::{Path, PathBuf};

use thiserror::Error as ThisError;

pub use interpolate::{InterpolationError, Vars, interpolate, parse_dotenv};
pub use normalize::{BuildSpec, NormalizeOptions, Normalized, normalize};
pub use raw::ComposeFile;
pub use units::{parse_duration_secs, parse_size_bytes};

/// Something accepted but not honored.
///
/// Warnings are collected rather than printed so the caller decides where they
/// go, and so a build can be made to fail on them in CI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Warning {
    pub service: Option<String>,
    pub message: String,
}

impl Warning {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            service: None,
            message: message.into(),
        }
    }

    pub fn for_service(service: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            service: Some(service.into()),
            message: message.into(),
        }
    }
}

impl std::fmt::Display for Warning {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.service {
            Some(s) => write!(f, "{s}: {}", self.message),
            None => f.write_str(&self.message),
        }
    }
}

#[derive(Debug, ThisError)]
pub enum Error {
    #[error("reading {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },

    #[error("parsing {path}: {source}")]
    Yaml {
        path: String,
        #[source]
        source: serde_yaml_ng::Error,
    },

    #[error(transparent)]
    Interpolation(#[from] InterpolationError),

    #[error("compose file defines no services to deploy")]
    NoServices,

    #[error("service {service:?} has neither `image:` nor `build:`")]
    ServiceHasNoImage { service: String },

    #[error(
        "service {service:?} declares platform {declared:?} but this bundle targets {target:?}"
    )]
    PlatformConflict {
        service: String,
        declared: String,
        target: String,
    },

    #[error("service {service:?}: invalid mount {value:?}: {reason}")]
    InvalidMount {
        service: String,
        value: String,
        reason: String,
    },

    #[error("service {service:?}: invalid port {value:?}: {reason}")]
    InvalidPort {
        service: String,
        value: String,
        reason: String,
    },

    #[error("invalid device mapping {value:?}: expected host[:container[:permissions]]")]
    InvalidDevice { value: String },

    #[error("invalid restart policy {value:?}: {reason}")]
    InvalidRestartPolicy { value: String, reason: String },

    #[error("invalid duration {value:?}: expected a form like 30s, 1m30s or 2h")]
    InvalidDuration { value: String },

    #[error("invalid size {value:?}: expected a form like 64m or 1g")]
    InvalidSize { value: String },

    #[error("unbalanced quote in {value:?}")]
    UnbalancedQuote { value: String },

    #[error("service {service:?} references unknown {kind} {referenced:?}")]
    UnknownReference {
        service: String,
        kind: &'static str,
        referenced: String,
    },

    #[error("{path}")]
    DependencyCycle { path: String },

    #[error("service {service:?}: {feature} is not supported: {reason}")]
    Unsupported {
        service: String,
        feature: String,
        reason: String,
    },

    #[error(transparent)]
    Core(#[from] via_core::Error),
}

/// A compose file read from disk, interpolated and parsed.
#[derive(Debug, Clone)]
pub struct Loaded {
    pub file: ComposeFile,
    /// The interpolated YAML text. Kept so the bundle can carry the original
    /// authoring form for inspection and debugging (spec section 16).
    pub source: String,
    /// Directory the file came from; relative paths resolve against it.
    pub base_dir: PathBuf,
}

/// Reads a compose file and its `.env`, in that order, then interpolates.
pub struct Loader {
    path: PathBuf,
    vars: Option<Vars>,
}

impl Loader {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            vars: None,
        }
    }

    /// Override the interpolation environment. Defaults to the process
    /// environment overlaid with a `.env` beside the compose file.
    pub fn vars(mut self, vars: Vars) -> Self {
        self.vars = Some(vars);
        self
    }

    /// Find a compose file using Compose's own preference order.
    pub fn discover(dir: impl AsRef<Path>) -> Option<PathBuf> {
        let dir = dir.as_ref();
        [
            "compose.yaml",
            "compose.yml",
            "docker-compose.yaml",
            "docker-compose.yml",
        ]
        .iter()
        .map(|f| dir.join(f))
        .find(|p| p.is_file())
    }

    pub fn load(self) -> Result<Loaded, Error> {
        let raw = std::fs::read_to_string(&self.path).map_err(|e| Error::Io {
            path: self.path.display().to_string(),
            source: e,
        })?;
        let base_dir = self
            .path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."))
            .to_path_buf();

        let vars = match self.vars {
            Some(v) => v,
            None => {
                let dotenv = base_dir.join(".env");
                let mut v = Vars::from_env();
                if let Ok(body) = std::fs::read_to_string(&dotenv) {
                    v = v.with_dotenv(&body);
                }
                v
            }
        };

        // Interpolate the text before parsing, as Compose does. Doing it after
        // parsing would miss `${...}` appearing in keys.
        let source = interpolate(&raw, &vars)?;
        let file: ComposeFile = serde_yaml_ng::from_str(&source).map_err(|e| Error::Yaml {
            path: self.path.display().to_string(),
            source: e,
        })?;

        Ok(Loaded {
            file,
            source,
            base_dir,
        })
    }
}

/// Parse an already-interpolated compose document.
pub fn parse_str(yaml: &str) -> Result<ComposeFile, Error> {
    serde_yaml_ng::from_str(yaml).map_err(|e| Error::Yaml {
        path: "<string>".into(),
        source: e,
    })
}
