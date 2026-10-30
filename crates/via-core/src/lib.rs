//! Core domain model for Via.
//!
//! This crate is the shared vocabulary every other Via crate speaks. It holds
//! types and validation only: no I/O, no async runtime, no Docker, no
//! transport. That keeps the dependency graph one-directional.
//!
//! ```text
//! via-core
//!    ├── via-compose    compose.yaml  -> core::Deployment
//!    ├── via-bundle     core::Deployment <-> bundle file
//!    └── via-runtime    core::Deployment -> containers
//! ```
//!
//! The important consequence is that `via-runtime` does not depend on
//! `via-compose`. The agent applies a [`Deployment`], and Compose is only ever
//! an authoring format the CLI translates at build time (spec sections 15, 16).

mod deployment;
mod digest;
mod error;
mod id;
mod platform;
mod release;

pub use deployment::{
    DEPLOYMENT_SCHEMA_VERSION, Dependency, DependencyCondition, DependencyCycle, Deployment,
    HealthCheck, HealthGate, ImageRef, Mount, Network, PortMapping, Protocol, Readiness,
    RestartPolicy, Service, Volume, DeviceMapping,
};
pub use digest::Digest;
pub use error::Error;
pub use id::{AppName, DeviceId, ReleaseId, ServiceName};
pub use platform::Platform;
pub use release::{ArtifactRef, ReleaseRef, ReleaseTarget};

/// Label applied to every Docker object Via creates, carrying the project name.
///
/// The agent uses these to recognize what it owns. Anything unlabeled was not
/// created by Via and is never removed by it.
pub const LABEL_PROJECT: &str = "via.project";
/// Label carrying the service name within a project.
pub const LABEL_SERVICE: &str = "via.service";
/// Label carrying the release number a container was created for.
pub const LABEL_RELEASE: &str = "via.release";
