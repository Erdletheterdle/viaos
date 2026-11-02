//! Container runtime abstraction for the Via agent.
//!
//! The agent applies a release through the [`Runtime`] trait (spec section 59).
//! The Docker implementation hands the release's compose file to the official
//! `docker compose`, so a file behaves on a device as it does anywhere else.
//!
//! # Reconciliation
//!
//! [`Runtime::apply`] is `docker compose up`, which is idempotent: it leaves a
//! container alone unless its configuration changed, and replaces only those
//! that did and what depends on them. That is what makes it safe to run after a
//! reboot, or after a partial failure.

mod compose;
mod docker;
mod gate;
mod logs;
mod types;

use std::path::Path;

use async_trait::async_trait;
use thiserror::Error as ThisError;
use via_core::Deployment;

pub use compose::{OVERRIDE_FILE, override_document, process_env};
pub use docker::{DiskUsage, DockerRuntime, ServiceStats, VolumeUsage};
pub use gate::{GateReport, Verdict, evaluate, wait_for_ready};
pub use types::{
    ApplyContext, ApplyReport, ContainerStatus, Exec, ExecChunk, ExecOptions, HealthStatus,
    LogLine, LogOptions, LogSource, Logs, RuntimeInfo, ServiceState,
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

    /// Bring up the release laid out at `ctx.project_dir`. Idempotent.
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

    #[error("docker compose: {reason}")]
    Compose { reason: String },

    #[error(
        "this release has no compose file. Deploy a new release built with this version of via"
    )]
    NoComposeFile,

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
