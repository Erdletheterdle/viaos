//! Runtime-facing types, independent of any particular container engine.

use std::collections::BTreeMap;

use via_core::{Deployment, ReleaseId, Service};

/// What engine the agent is talking to, reported for device inventory
/// (spec section 7).
#[derive(Clone, Debug, Default)]
pub struct RuntimeInfo {
    pub engine: String,
    pub version: String,
    pub api_version: Option<String>,
    pub os: Option<String>,
    pub arch: Option<String>,
    pub kernel: Option<String>,
    /// Bytes available where the engine stores images and volumes. Checked
    /// before accepting a bundle (spec section 26).
    pub root_dir: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContainerStatus {
    /// No container exists for this service.
    Absent,
    Created,
    Running,
    Restarting,
    Paused,
    Exited {
        code: i64,
    },
    Dead,
}

impl ContainerStatus {
    pub fn is_running(&self) -> bool {
        matches!(self, ContainerStatus::Running)
    }
}

/// Health as reported by the container's own healthcheck.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HealthStatus {
    /// The container declares no healthcheck, so there is nothing to report.
    /// Deliberately distinct from `Unhealthy`: absence of a check is not a
    /// failing check.
    None,
    Starting,
    Healthy,
    Unhealthy,
}

/// Observed state of one service.
#[derive(Clone, Debug)]
pub struct ServiceState {
    pub service: String,
    pub container_id: Option<String>,
    pub container_name: Option<String>,
    pub status: ContainerStatus,
    pub health: HealthStatus,
    /// Config hash recorded when the container was created. When this differs
    /// from the desired hash, the container is stale and must be replaced.
    pub config_hash: Option<String>,
    pub release: Option<u64>,
    pub image: Option<String>,
}

impl ServiceState {
    pub fn absent(service: impl Into<String>) -> Self {
        Self {
            service: service.into(),
            container_id: None,
            container_name: None,
            status: ContainerStatus::Absent,
            health: HealthStatus::None,
            config_hash: None,
            release: None,
            image: None,
        }
    }
}

/// What `apply` did, so the agent can report it and an operator can read it.
#[derive(Clone, Debug, Default)]
pub struct ApplyReport {
    pub created: Vec<String>,
    pub replaced: Vec<String>,
    pub started: Vec<String>,
    /// Services left untouched because their configuration had not changed.
    pub unchanged: Vec<String>,
    /// Containers removed because they belonged to the project but no longer
    /// to the deployment.
    pub removed_orphans: Vec<String>,
    pub networks_created: Vec<String>,
    pub volumes_created: Vec<String>,
}

impl ApplyReport {
    pub fn changed(&self) -> bool {
        !self.created.is_empty()
            || !self.replaced.is_empty()
            || !self.removed_orphans.is_empty()
            || !self.networks_created.is_empty()
            || !self.volumes_created.is_empty()
    }

    pub fn summary(&self) -> String {
        if !self.changed() {
            return format!("{} service(s) already up to date", self.unchanged.len());
        }
        let mut parts = Vec::new();
        if !self.created.is_empty() {
            parts.push(format!("{} created", self.created.len()));
        }
        if !self.replaced.is_empty() {
            parts.push(format!("{} replaced", self.replaced.len()));
        }
        if !self.unchanged.is_empty() {
            parts.push(format!("{} unchanged", self.unchanged.len()));
        }
        if !self.removed_orphans.is_empty() {
            parts.push(format!("{} orphan(s) removed", self.removed_orphans.len()));
        }
        parts.join(", ")
    }
}

/// Inputs that vary per deploy but are not part of the release itself
/// (spec section 40).
#[derive(Clone, Debug, Default)]
pub struct ApplyContext {
    pub release: Option<ReleaseId>,
    /// Values applied to every service in the deployment.
    ///
    /// Application variables and secrets land here: they describe the device's
    /// runtime identity, not one service's configuration, and a compose stack
    /// expects them visible to whichever services read them.
    pub global_environment: BTreeMap<String, String>,
    /// Per-service overrides, layered over `global_environment`.
    ///
    /// Kept separate from [`Deployment`] so one bundle serves a whole fleet
    /// with different runtime identities.
    pub environment: BTreeMap<String, BTreeMap<String, String>>,
}

impl ApplyContext {
    pub fn for_release(release: ReleaseId) -> Self {
        Self {
            release: Some(release),
            ..Default::default()
        }
    }

    /// Overrides for one service, if any.
    pub fn env_for(&self, service: &str) -> Option<&BTreeMap<String, String>> {
        self.environment.get(service)
    }

    /// Every value applied to every service.
    pub fn with_global(mut self, env: BTreeMap<String, String>) -> Self {
        self.global_environment = env;
        self
    }

    /// The manifest's environment with runtime values applied on top.
    ///
    /// Precedence, weakest first: what the release baked in, then values that
    /// apply to the whole device, then anything scoped to this one service.
    /// Runtime values win over the bundle, which is what makes a secret
    /// rotation take effect without a rebuild.
    pub fn effective_env(&self, service: &Service) -> BTreeMap<String, String> {
        let mut env = service.environment.clone();
        // Only services that declared a name receive it. Injecting every
        // device-wide value into every container would mean rotating one
        // credential replaced the whole stack, database included.
        for (k, v) in &self.global_environment {
            if service.wants_runtime_key(k) {
                env.insert(k.clone(), v.clone());
            }
        }
        if let Some(overrides) = self.env_for(&service.name) {
            for (k, v) in overrides {
                env.insert(k.clone(), v.clone());
            }
        }
        env
    }
}

/// Which containers to read, and how much history (spec sections 41--42).
#[derive(Clone, Debug, Default)]
pub struct LogOptions {
    /// Services to read. Empty means every service in the deployment.
    pub services: Vec<String>,
    /// Existing lines to show first, from whatever the engine's local logging
    /// driver kept. Via operates no log archive, so this is all the history
    /// there is.
    pub tail: u32,
    /// Keep reading as new output arrives. Without it the stream ends after
    /// the tail, so a one-shot read costs nothing once it has returned.
    pub follow: bool,
}

/// Which of a container's two output streams a line came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LogSource {
    Stdout,
    Stderr,
}

/// One line of container output.
#[derive(Clone, Debug)]
pub struct LogLine {
    pub service: String,
    pub source: LogSource,
    /// The engine's own timestamp, when it gave one.
    pub at: Option<String>,
    pub line: String,
}

/// A live read of container output.
///
/// Dropping `lines` is how a reader detaches, and it is the only teardown
/// there is: the stream owns the engine request, so when nobody polls it,
/// nothing is read and nothing is forwarded (spec section 41).
pub struct Logs {
    /// The services actually being read. A viewer that asked for everything
    /// finds out here what "everything" turned out to be -- a service whose
    /// container does not exist is absent rather than fatal.
    pub services: Vec<String>,
    pub lines: futures::stream::BoxStream<'static, Result<LogLine, crate::Error>>,
}

/// A command to run inside a running container.
#[derive(Debug, Clone)]
pub struct ExecOptions {
    /// Which service's container.
    pub service: String,
    /// Argv, already split. Never a string for a shell to parse: that would
    /// make quoting bugs into command injection on the device.
    pub command: Vec<String>,
}

/// A command running inside a container.
pub struct Exec {
    /// The container it is running in, so an operator sees which one answered.
    pub container: String,
    pub output: futures::stream::BoxStream<'static, Result<ExecChunk, crate::Error>>,
}

/// One piece of a running command's output, or its exit status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecChunk {
    Output {
        source: LogSource,
        data: String,
    },
    /// `None` when the engine reported no status, which is not the same as
    /// zero and must not be flattened into it.
    Exit {
        code: Option<i64>,
    },
}

/// Names Via gives to the Docker objects it owns.
///
/// Centralized so that recognizing "things Via created" is one rule rather
/// than a pattern repeated at every call site.
pub struct Naming;

impl Naming {
    pub fn container(project: &str, service: &str) -> String {
        format!("{project}-{service}")
    }

    /// Implicit network every service joins when it declares none, so services
    /// resolve each other by name exactly as they do under Compose.
    pub fn default_network(project: &str) -> String {
        format!("{project}_default")
    }

    pub fn network(project: &str, declared: &str) -> String {
        format!("{project}_{declared}")
    }

    pub fn volume(project: &str, declared: &str) -> String {
        format!("{project}_{declared}")
    }
}

/// Networks a service should actually be attached to.
///
/// Returns `None` when `network_mode` takes over, in which case Docker ignores
/// network attachment entirely.
pub fn resolved_networks(deployment: &Deployment, service: &Service) -> Option<Vec<String>> {
    if service.network_mode.is_some() {
        return None;
    }
    if service.networks.is_empty() {
        return Some(vec![Naming::default_network(&deployment.project)]);
    }
    Some(
        service
            .networks
            .iter()
            .map(|n| match deployment.networks.get(n) {
                // An external network is referenced by its real name; Via did
                // not create it and must not rename it.
                Some(net) if net.external => n.clone(),
                _ => Naming::network(&deployment.project, n),
            })
            .collect(),
    )
}

/// Whether the deployment needs the implicit default network.
pub fn needs_default_network(deployment: &Deployment) -> bool {
    deployment
        .services
        .values()
        .any(|s| s.network_mode.is_none() && s.networks.is_empty())
}
