//! Container runtime abstraction for the Via agent.
//!
//! The agent applies a [`via_core::Deployment`] through the [`Runtime`] trait
//! (spec section 59). Docker is the only implementation today; containerd or
//! Podman would slot in behind the same trait without touching the agent.
//!
//! This crate deliberately does not depend on `via-compose`: the runtime
//! contract is the normalized deployment manifest, and Compose is an authoring
//! format that never reaches a device.
//!
//! # Reconciliation
//!
//! [`Runtime::apply`] is idempotent. Every container carries a hash of the
//! configuration it was created from, so applying the same deployment twice
//! leaves a healthy stack untouched, and applying a changed one replaces only
//! the services that actually changed. That is what makes it safe to run on a
//! timer, after a reboot, or after a partial failure, which the spec's
//! straight-line deploy pipeline had no answer for.

mod docker;
mod gate;
mod logs;
mod plan;
mod types;

use std::path::Path;

use async_trait::async_trait;
use thiserror::Error as ThisError;
use via_core::Deployment;

pub use docker::DockerRuntime;
pub use gate::{GateReport, Verdict, evaluate, wait_for_ready};
pub use plan::{
    LABEL_CONFIG_HASH, LABEL_PROJECT, LABEL_RELEASE, LABEL_SERVICE, config_hash, container_name,
};
pub use types::{
    ApplyContext, ApplyReport, ContainerStatus, Exec, ExecChunk, ExecOptions, HealthStatus,
    LogLine, LogOptions, LogSource, Logs, Naming, RuntimeInfo, ServiceState, needs_default_network,
    resolved_networks,
};

/// What the agent needs from a container engine.
#[async_trait]
pub trait Runtime: Send + Sync {
    /// Engine identity and version, for device inventory.
    async fn info(&self) -> Result<RuntimeInfo, Error>;

    /// Load images from an archive on disk, returning the references loaded.
    ///
    /// Takes a path rather than bytes because a release's images routinely run
    /// to gigabytes and must never be held in memory on a small device.
    async fn load_images(&self, archive: &Path) -> Result<Vec<String>, Error>;

    async fn image_present(&self, reference: &str) -> Result<bool, Error>;

    /// Converge the engine onto `deployment`. Idempotent.
    async fn apply(
        &self,
        deployment: &Deployment,
        ctx: &ApplyContext,
    ) -> Result<ApplyReport, Error>;

    /// Observed state of every service in the deployment.
    async fn service_states(&self, deployment: &Deployment) -> Result<Vec<ServiceState>, Error>;

    /// Read container output (spec section 46).
    ///
    /// Returns a stream rather than a buffer because following logs has no
    /// end, and because the reader dropping the stream has to be what stops
    /// the engine being read -- Via forwards nothing when nobody is watching.
    async fn logs(&self, deployment: &Deployment, options: &LogOptions) -> Result<Logs, Error>;

    /// Run a command inside a running container.
    ///
    /// Deliberately narrower than `docker exec`: one service, argv already
    /// split, no TTY and no stdin. What an operator debugging a device needs
    /// is to look -- `ls /data`, `cat /etc/resolv.conf`, `ps` -- and an
    /// interactive shell is a much larger surface for a capability that is
    /// already the widest one Via grants.
    async fn exec(&self, deployment: &Deployment, options: &ExecOptions) -> Result<Exec, Error>;

    /// Stop the application, leaving containers and data in place.
    async fn stop(&self, deployment: &Deployment) -> Result<(), Error>;

    /// Remove the application's containers, and optionally its volumes.
    async fn remove(&self, deployment: &Deployment, remove_volumes: bool) -> Result<(), Error>;
}

#[derive(Debug, ThisError)]
pub enum Error {
    #[error("docker: {0}")]
    Docker(#[source] bollard::errors::Error),

    #[error("{path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },

    #[error(
        "image {reference:?} is not loaded. A bundle is self-contained; \
         Via does not pull images at deploy time"
    )]
    ImageNotLoaded { reference: String },

    #[error("external network {name:?} does not exist on this device and Via will not create it")]
    MissingExternalNetwork { name: String },

    #[error("external volume {name:?} does not exist on this device and Via will not create it")]
    MissingExternalVolume { name: String },

    #[error("timed out waiting for {service:?} to satisfy condition {condition}")]
    DependencyTimeout { service: String, condition: String },

    #[error("dependency {service:?} failed: {reason}")]
    DependencyFailed { service: String, reason: String },

    #[error("{path}")]
    DependencyCycle { path: String },

    #[error("{service:?} is not a service of the release this device is running")]
    NoSuchService { service: String },

    /// The request was malformed, or the engine answered in a shape this code
    /// cannot use.
    #[error("{reason}")]
    Invalid { reason: String },
}

impl Error {
    pub(crate) fn docker(e: bollard::errors::Error) -> Self {
        Error::Docker(e)
    }

    pub(crate) fn io(path: impl AsRef<Path>, source: std::io::Error) -> Self {
        Error::Io {
            path: path.as_ref().display().to_string(),
            source,
        }
    }
}
