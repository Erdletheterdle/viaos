//! The normalized deployment manifest (spec section 16).
//!
//! What a release is, in a form people and the health gate can read. It comes
//! from `docker compose config` during `via build`. What a device runs is the
//! compose file itself, under the official `docker compose`.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::{Digest, Platform, ServiceName};

/// Bumped when the manifest shape changes incompatibly. An agent refuses a
/// manifest whose version it does not understand rather than guessing.
pub const DEPLOYMENT_SCHEMA_VERSION: u32 = 2;

/// The oldest shape still read. Version 1 releases have a compose file too; an
/// agent too old to run one refuses version 2 rather than starting containers
/// from a manifest that no longer lists their commands.
pub const OLDEST_DEPLOYMENT_SCHEMA: u32 = 1;

/// A fully resolved application definition for one platform.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Deployment {
    pub schema_version: u32,

    /// The Compose project name. Docker object names are derived from this, so
    /// it must not change between releases or every volume would orphan
    /// (spec section 19).
    pub project: String,

    pub platform: Platform,

    /// Ordered by name for deterministic output; start order comes from
    /// `depends_on`, not from map order.
    pub services: BTreeMap<String, Service>,

    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub networks: BTreeMap<String, Network>,

    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub volumes: BTreeMap<String, Volume>,

    /// When a deploy of this release counts as successful (spec section 22).
    #[serde(default)]
    pub health_gate: HealthGate,

    /// What the compose file's top-level `x-via` says about this release's
    /// variables. Lives here rather than on [`Service`] so rewording a
    /// description never changes a container's config hash.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub variables: Vec<VariableSpec>,
}

/// One variable a release describes: for people setting it, and for deciding
/// whether a device is missing it.
///
/// A default is not carried from here to a container. The normalizer writes it
/// into the environment of each service that declares the name, so it reaches
/// the device inside the release and applies with no network.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VariableSpec {
    pub key: String,
    /// The services that receive it, sorted.
    #[serde(default)]
    pub services: Vec<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub default: Option<String>,
    /// Asks whoever sets it to mark it secret. A release holds no secrets, so
    /// this never comes with a default.
    #[serde(default)]
    pub secret: bool,
    #[serde(default = "required_by_default")]
    pub required: bool,
}

fn required_by_default() -> bool {
    true
}

impl Deployment {
    pub fn new(project: impl Into<String>, platform: Platform) -> Self {
        Self {
            schema_version: DEPLOYMENT_SCHEMA_VERSION,
            project: project.into(),
            platform,
            services: BTreeMap::new(),
            networks: BTreeMap::new(),
            volumes: BTreeMap::new(),
            health_gate: HealthGate::default(),
            variables: Vec::new(),
        }
    }

    /// Every variable this release takes: what it describes, plus any bare
    /// `- KEY` it does not, which counts as required. Sorted by name.
    pub fn variable_schema(&self) -> Vec<VariableSpec> {
        let mut schema: BTreeMap<String, VariableSpec> = self
            .variables
            .iter()
            .map(|v| (v.key.clone(), v.clone()))
            .collect();
        for (name, service) in &self.services {
            for key in &service.runtime_env {
                let spec = schema.entry(key.clone()).or_insert_with(|| VariableSpec {
                    key: key.clone(),
                    services: Vec::new(),
                    description: None,
                    default: None,
                    secret: false,
                    required: true,
                });
                if !spec.services.contains(name) {
                    spec.services.push(name.clone());
                }
            }
        }
        schema
            .into_values()
            .map(|mut v| {
                v.services.sort();
                // Whatever built this bundle, a secret's value is not shown.
                if v.secret {
                    v.default = None;
                }
                v
            })
            .collect()
    }

    /// The name Docker knows a declared volume by.
    pub fn volume_name(&self, declared: &str) -> String {
        self.volumes
            .get(declared)
            .and_then(|v| v.name.clone())
            .unwrap_or_else(|| format!("{}_{declared}", self.project))
    }

    /// Whether the release says it can run without this name.
    pub fn variable_is_optional(&self, key: &str) -> bool {
        self.variables.iter().any(|v| v.key == key && !v.required)
    }

    /// Every distinct image required to run this deployment. This is the set a
    /// bundle must contain in full (spec section 11).
    pub fn required_images(&self) -> Vec<&ImageRef> {
        let mut seen: Vec<&ImageRef> = Vec::new();
        for svc in self.services.values() {
            if !seen.iter().any(|i| i.reference == svc.image.reference) {
                seen.push(&svc.image);
            }
        }
        seen
    }
}

/// One container definition.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Service {
    pub name: String,
    pub image: ImageRef,

    /// How many containers Compose runs for it.
    #[serde(default = "one")]
    pub replicas: u32,

    /// Values the release itself sets, for showing. Runtime variables and
    /// secrets are never stored here (spec sections 35, 40).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub environment: BTreeMap<String, String>,

    /// Environment keys this service declares but does not define -- Compose's
    /// bare `- KEY` form. These are the names it expects the device to supply.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub runtime_env: Vec<String>,

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mounts: Vec<Mount>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ports: Vec<PortMapping>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub networks: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub depends_on: Vec<Dependency>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub healthcheck: Option<HealthCheck>,
    #[serde(default)]
    pub restart: RestartPolicy,

    /// How this service contributes to the deploy health gate.
    #[serde(default)]
    pub readiness: Readiness,

    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub privileged: bool,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network_mode: Option<String>,
}

fn one() -> u32 {
    1
}

impl Service {
    pub fn new(name: impl Into<String>, image: ImageRef) -> Self {
        Self {
            name: name.into(),
            image,
            replicas: 1,
            environment: BTreeMap::new(),
            runtime_env: Vec::new(),
            mounts: Vec::new(),
            ports: Vec::new(),
            networks: Vec::new(),
            depends_on: Vec::new(),
            healthcheck: None,
            restart: RestartPolicy::default(),
            readiness: Readiness::default(),
            privileged: false,
            network_mode: None,
        }
    }

    /// Whether this service asked for a runtime value called `key`.
    ///
    /// A service that never mentions a name does not receive it, so rotating a
    /// credential replaces the containers that read it and leaves the rest
    /// running.
    pub fn wants_runtime_key(&self, key: &str) -> bool {
        self.runtime_env.iter().any(|k| k == key) || self.environment.contains_key(key)
    }

    /// Typed name, or an error if the manifest carries something unusable.
    pub fn service_name(&self) -> Result<ServiceName, crate::Error> {
        ServiceName::parse(self.name.clone())
    }
}

/// An image required by a service.
///
/// `reference` is the human-facing tag (`postgres:17`). `digest` is the
/// immutable identity and is what the agent actually verifies and runs, so a
/// mutable upstream tag cannot change what a release means (spec section 11).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageRef {
    pub reference: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub digest: Option<Digest>,
}

impl ImageRef {
    pub fn new(reference: impl Into<String>) -> Self {
        Self {
            reference: reference.into(),
            digest: None,
        }
    }

    pub fn with_digest(mut self, digest: Digest) -> Self {
        self.digest = Some(digest);
        self
    }
}

impl std::fmt::Display for ImageRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.digest {
            Some(d) => write!(
                f,
                "{} ({})",
                self.reference,
                &d.hex()[..12.min(d.hex().len())]
            ),
            None => f.write_str(&self.reference),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Mount {
    /// A named volume, scoped to the project. Persists across releases.
    Volume {
        source: String,
        target: String,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        read_only: bool,
    },
    /// A host path. Host paths are absolute in a normalized manifest; the
    /// build host's working directory is meaningless on the device.
    Bind {
        source: String,
        target: String,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        read_only: bool,
    },
    Tmpfs {
        target: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        size_bytes: Option<u64>,
    },
}

impl Mount {
    pub fn target(&self) -> &str {
        match self {
            Mount::Volume { target, .. }
            | Mount::Bind { target, .. }
            | Mount::Tmpfs { target, .. } => target,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Protocol {
    Tcp,
    Udp,
}

impl std::fmt::Display for Protocol {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Protocol::Tcp => "tcp",
            Protocol::Udp => "udp",
        })
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PortMapping {
    /// Absent means "publish on all interfaces", matching Docker.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_ip: Option<String>,
    /// Absent means Docker picks an ephemeral host port, which is what
    /// Compose's `- "80"` short form asks for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host_port: Option<u16>,
    pub container_port: u16,
    #[serde(default = "default_tcp")]
    pub protocol: Protocol,
}

fn default_tcp() -> Protocol {
    Protocol::Tcp
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Dependency {
    pub service: String,
    #[serde(default)]
    pub condition: DependencyCondition,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DependencyCondition {
    /// Container created and started. Says nothing about readiness.
    #[default]
    Started,
    /// Container's own healthcheck reports healthy.
    Healthy,
    /// Container ran to completion with exit code 0.
    CompletedSuccessfully,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HealthCheck {
    /// Argv form. A leading `CMD-SHELL` is preserved so Docker semantics are
    /// unchanged (spec section 18).
    pub test: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interval_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retries: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub start_period_secs: Option<u64>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "policy", rename_all = "kebab-case")]
pub enum RestartPolicy {
    No,
    /// The sane default for edge devices: survive reboots without operator
    /// action, which is the whole point of offline operation (spec section 3).
    #[default]
    UnlessStopped,
    Always,
    OnFailure {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max_retries: Option<u32>,
    },
}

/// How a service contributes to deciding whether a release is healthy.
///
/// The spec's deployment state machine has a HEALTH CHECK step but never
/// defined what passes it. Without an explicit per-service answer, a stack
/// containing one service with no healthcheck either blocks forever or is
/// silently ignored. This makes the decision part of the manifest.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Readiness {
    /// Wait for the service's Docker healthcheck to report healthy. Chosen
    /// automatically when the service defines a healthcheck.
    Healthy,
    /// No healthcheck exists, so the best available signal is "it started and
    /// did not immediately crash". The default for services without one.
    #[default]
    Running,
    /// No `restart:` and no healthcheck, so it may be a task: up, or exited 0.
    RunningOrFinished,
    /// A migration or init job: must exit 0.
    Completed,
    /// Excluded from the gate entirely. For known-flaky sidecars an operator
    /// does not want gating a fleet rollout.
    Ignored,
}

/// When a deployment is considered successful, and what happens if it is not
/// (spec sections 21, 22).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HealthGate {
    /// Total time allowed for all gated services to become ready. Without a
    /// bound, a candidate release sits in HEALTH CHECK forever and the device
    /// never converges to a decision.
    pub timeout_secs: u64,
    /// How long a `Running` service must stay up to count as ready. Catches
    /// crash-loops that would otherwise pass an instantaneous check.
    pub stable_secs: u64,
    /// Whether the agent restores the previous release on its own. Kept
    /// explicit rather than deferred, because an unattended edge device with no
    /// operator nearby is exactly where automatic rollback matters most.
    pub rollback_on_failure: bool,
}

impl Default for HealthGate {
    fn default() -> Self {
        Self {
            timeout_secs: 300,
            stable_secs: 10,
            rollback_on_failure: true,
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Network {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub driver: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub driver_opts: BTreeMap<String, String>,
    /// An externally managed network Via must not create or delete.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub external: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub internal: bool,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub labels: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Volume {
    /// The name Docker knows it by, which a file can set itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub driver: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub driver_opts: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub external: bool,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub labels: BTreeMap<String, String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn svc(name: &str, deps: &[&str]) -> Service {
        let mut s = Service::new(name, ImageRef::new(format!("{name}:latest")));
        s.depends_on = deps
            .iter()
            .map(|d| Dependency {
                service: d.to_string(),
                condition: DependencyCondition::Started,
            })
            .collect();
        s
    }

    fn deployment(services: Vec<Service>) -> Deployment {
        let mut d = Deployment::new("test", Platform::new("linux", "arm64"));
        for s in services {
            d.services.insert(s.name.clone(), s);
        }
        d
    }

    #[test]
    fn required_images_are_deduplicated() {
        let mut a = Service::new("a", ImageRef::new("postgres:17"));
        a.name = "a".into();
        let b = Service::new("b", ImageRef::new("postgres:17"));
        let c = Service::new("c", ImageRef::new("nats:2"));
        let d = deployment(vec![a, b, c]);
        assert_eq!(d.required_images().len(), 2);
    }

    #[test]
    fn round_trips_through_json() {
        let d = deployment(vec![svc("app", &["db"]), svc("db", &[])]);
        let json = serde_json::to_string(&d).unwrap();
        let back: Deployment = serde_json::from_str(&json).unwrap();
        assert_eq!(back.services.len(), 2);
        assert_eq!(back.project, "test");
        assert!(back.health_gate.rollback_on_failure);
    }
}
