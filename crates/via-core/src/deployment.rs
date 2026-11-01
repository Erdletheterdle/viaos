//! The normalized deployment manifest (spec section 16).
//!
//! This is the runtime contract between a bundle and an agent. Compose is an
//! authoring format that is translated into this during `via build`, so the
//! agent never needs a Compose parser and Compose stays swappable
//! (spec section 15).
//!
//! Everything here is already resolved: no variable interpolation, no
//! `extends`, no profiles, no relative paths that depend on the build host.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::{Digest, Platform, ServiceName};

/// Bumped when the manifest shape changes incompatibly. An agent refuses a
/// manifest whose version it does not understand rather than guessing.
pub const DEPLOYMENT_SCHEMA_VERSION: u32 = 1;

/// A fully resolved application definition for one platform.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Deployment {
    pub schema_version: u32,

    /// Stable project identity. Docker object names are derived from this, so
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
        }
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

    /// Services in dependency order, or the cycle that prevents an order.
    ///
    /// Deterministic: ties are broken by name so two builds of the same input
    /// produce the same plan.
    pub fn start_order(&self) -> Result<Vec<&str>, DependencyCycle> {
        let mut visited: BTreeMap<&str, Mark> = BTreeMap::new();
        let mut order = Vec::new();
        let mut stack = Vec::new();
        for name in self.services.keys() {
            visit(self, name.as_str(), &mut visited, &mut order, &mut stack)?;
        }
        Ok(order)
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Mark {
    InProgress,
    Done,
}

fn visit<'a>(
    dep: &'a Deployment,
    name: &'a str,
    visited: &mut BTreeMap<&'a str, Mark>,
    order: &mut Vec<&'a str>,
    stack: &mut Vec<&'a str>,
) -> Result<(), DependencyCycle> {
    match visited.get(name) {
        Some(Mark::Done) => return Ok(()),
        Some(Mark::InProgress) => {
            let start = stack.iter().position(|s| *s == name).unwrap_or(0);
            let mut path: Vec<String> = stack[start..].iter().map(|s| s.to_string()).collect();
            path.push(name.to_string());
            return Err(DependencyCycle { path });
        }
        None => {}
    }
    // A dependency on a service that does not exist is caught during
    // validation, not here; ignore it so we still report real cycles.
    let Some(svc) = dep.services.get(name) else {
        return Ok(());
    };

    visited.insert(name, Mark::InProgress);
    stack.push(name);
    for d in &svc.depends_on {
        if let Some((key, _)) = dep.services.get_key_value(&d.service) {
            visit(dep, key.as_str(), visited, order, stack)?;
        }
    }
    stack.pop();
    visited.insert(name, Mark::Done);
    if let Some((key, _)) = dep.services.get_key_value(name) {
        order.push(key.as_str());
    }
    Ok(())
}

/// A `depends_on` cycle, reported with the full path so the message is useful.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DependencyCycle {
    pub path: Vec<String>,
}

impl std::fmt::Display for DependencyCycle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "dependency cycle: {}", self.path.join(" -> "))
    }
}

impl std::error::Error for DependencyCycle {}

/// One container definition.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Service {
    pub name: String,
    pub image: ImageRef,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entrypoint: Option<Vec<String>>,

    /// Baked-in environment. Runtime variables and secrets are layered on top
    /// of this by the agent and are deliberately not stored here
    /// (spec sections 35, 40).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub environment: BTreeMap<String, String>,

    /// Environment keys this service declares but does not define -- Compose's
    /// bare `- KEY` form.
    ///
    /// These are the service's runtime inputs: the names it expects the device
    /// to supply. Recording them rather than resolving them at build time is
    /// what lets the agent inject a variable or secret into exactly the
    /// services that asked for it, instead of into everything (spec section 40).
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

    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub devices: Vec<DeviceMapping>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cap_add: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cap_drop: Vec<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub privileged: bool,

    /// `host`, `none`, `container:<name>`, or a named network. When set, the
    /// `networks` list is ignored, matching Docker's own behavior.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network_mode: Option<String>,

    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub labels: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub working_dir: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hostname: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub init: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ipc: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shm_size_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_grace_period_secs: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_signal: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub tty: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub stdin_open: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub read_only: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extra_hosts: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dns: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub group_add: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub sysctls: BTreeMap<String, String>,
}

impl Service {
    pub fn new(name: impl Into<String>, image: ImageRef) -> Self {
        Self {
            name: name.into(),
            image,
            ..Default::default()
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

/// A host device passed into a container, e.g. a camera, serial/PLC adapter,
/// or accelerator. Central to Via's target environment, where the whole point
/// of the device is usually the hardware attached to it (spec section 53).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct DeviceMapping {
    /// Path on the host, e.g. `/dev/ttyUSB0`.
    pub host_path: String,
    /// Path inside the container. Defaults to the host path when unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub container_path: Option<String>,
    /// cgroup permissions: any combination of `r`, `w`, `m`.
    #[serde(default = "default_device_permissions")]
    pub permissions: String,
}

impl DeviceMapping {
    pub fn new(host_path: impl Into<String>) -> Self {
        Self {
            host_path: host_path.into(),
            container_path: None,
            permissions: default_device_permissions(),
        }
    }

    pub fn effective_container_path(&self) -> &str {
        self.container_path.as_deref().unwrap_or(&self.host_path)
    }
}

fn default_device_permissions() -> String {
    "rwm".to_string()
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
    fn start_order_respects_dependencies() {
        let d = deployment(vec![
            svc("app", &["db"]),
            svc("db", &[]),
            svc("proxy", &["app"]),
        ]);
        let order = d.start_order().unwrap();
        let pos = |n: &str| order.iter().position(|s| *s == n).unwrap();
        assert!(pos("db") < pos("app"));
        assert!(pos("app") < pos("proxy"));
    }

    #[test]
    fn start_order_reports_the_cycle_path() {
        let d = deployment(vec![svc("a", &["b"]), svc("b", &["a"])]);
        let err = d.start_order().unwrap_err();
        assert!(
            err.path.len() >= 2,
            "cycle path should name the participants: {err}"
        );
    }

    #[test]
    fn missing_dependency_does_not_masquerade_as_a_cycle() {
        let d = deployment(vec![svc("app", &["nope"])]);
        assert!(d.start_order().is_ok());
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
