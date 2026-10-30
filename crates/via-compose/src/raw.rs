//! Serde shapes mirroring `compose.yaml` as written by humans.
//!
//! Compose accepts many shorthand spellings for the same thing. Everything in
//! this module exists to accept those spellings faithfully; deciding what they
//! *mean* is [`crate::normalize`]'s job. Keeping the two apart means the
//! normalizer never has to reason about YAML shape.

use indexmap::IndexMap;
use serde::Deserialize;

/// A scalar that may be written as a string, number, or bool.
///
/// YAML will happily hand back an integer for `PORT: 8080` or a bool for
/// `DEBUG: true`, and both must survive as strings.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum Scalar {
    String(String),
    Int(i64),
    Float(f64),
    Bool(bool),
}

impl Scalar {
    pub fn as_string(&self) -> String {
        match self {
            Scalar::String(s) => s.clone(),
            Scalar::Int(i) => i.to_string(),
            Scalar::Float(f) => f.to_string(),
            Scalar::Bool(b) => b.to_string(),
        }
    }
}

/// `command: echo hi`, `command: ["echo", "hi"]`, or a bare scalar.
///
/// The scalar case is not hypothetical: `command: true` is a YAML boolean but
/// means the `true` binary, and `entrypoint: 0` appears in the wild too.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum StringOrList {
    List(Vec<Scalar>),
    Scalar(Scalar),
}

/// `environment:` and `labels:`, which accept a mapping or a `KEY=value` list.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum MapOrList {
    Map(IndexMap<String, Option<Scalar>>),
    List(Vec<String>),
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ComposeFile {
    /// Explicit project name. Ignored in favor of the Via application name,
    /// but accepted so real-world files parse.
    #[serde(default)]
    pub name: Option<String>,
    /// Legacy top-level `version:`. Obsolete in the Compose spec; accepted and
    /// ignored so existing files do not need editing.
    #[serde(default)]
    pub version: Option<Scalar>,
    #[serde(default)]
    pub services: IndexMap<String, RawService>,
    #[serde(default)]
    pub networks: IndexMap<String, Option<RawNetwork>>,
    #[serde(default)]
    pub volumes: IndexMap<String, Option<RawVolume>>,
    #[serde(default)]
    pub configs: IndexMap<String, serde_yaml_ng::Value>,
    #[serde(default)]
    pub secrets: IndexMap<String, serde_yaml_ng::Value>,
    /// `x-` extension fields, collected so `deny_unknown_fields` still catches
    /// genuine typos like `servics:`.
    #[serde(flatten)]
    pub extensions: IndexMap<String, serde_yaml_ng::Value>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct RawService {
    #[serde(default)]
    pub image: Option<String>,
    #[serde(default)]
    pub build: Option<RawBuild>,
    #[serde(default)]
    pub command: Option<StringOrList>,
    #[serde(default)]
    pub entrypoint: Option<StringOrList>,
    #[serde(default)]
    pub environment: Option<MapOrList>,
    #[serde(default)]
    pub env_file: Option<StringOrList>,
    #[serde(default)]
    pub volumes: Vec<RawVolumeMount>,
    #[serde(default)]
    pub tmpfs: Option<StringOrList>,
    #[serde(default)]
    pub ports: Vec<RawPort>,
    #[serde(default)]
    pub expose: Vec<Scalar>,
    #[serde(default)]
    pub networks: Option<RawServiceNetworks>,
    #[serde(default)]
    pub network_mode: Option<String>,
    #[serde(default)]
    pub depends_on: Option<RawDependsOn>,
    #[serde(default)]
    pub healthcheck: Option<RawHealthcheck>,
    #[serde(default)]
    pub restart: Option<String>,
    #[serde(default)]
    pub labels: Option<MapOrList>,
    #[serde(default)]
    pub devices: Vec<String>,
    #[serde(default)]
    pub cap_add: Vec<String>,
    #[serde(default)]
    pub cap_drop: Vec<String>,
    #[serde(default)]
    pub privileged: Option<bool>,
    #[serde(default)]
    pub user: Option<Scalar>,
    #[serde(default)]
    pub working_dir: Option<String>,
    #[serde(default)]
    pub hostname: Option<String>,
    #[serde(default)]
    pub container_name: Option<String>,
    #[serde(default)]
    pub init: Option<bool>,
    #[serde(default)]
    pub pid: Option<String>,
    #[serde(default)]
    pub ipc: Option<String>,
    #[serde(default)]
    pub shm_size: Option<Scalar>,
    #[serde(default)]
    pub stop_grace_period: Option<String>,
    #[serde(default)]
    pub stop_signal: Option<String>,
    #[serde(default)]
    pub tty: Option<bool>,
    #[serde(default)]
    pub stdin_open: Option<bool>,
    #[serde(default)]
    pub read_only: Option<bool>,
    #[serde(default)]
    pub extra_hosts: Option<MapOrList>,
    #[serde(default)]
    pub dns: Option<StringOrList>,
    #[serde(default)]
    pub group_add: Vec<Scalar>,
    #[serde(default)]
    pub sysctls: Option<MapOrList>,
    #[serde(default)]
    pub profiles: Vec<String>,
    #[serde(default)]
    pub platform: Option<String>,
    #[serde(default)]
    pub pull_policy: Option<String>,
    /// Swarm-only. Captured so it can be rejected with a clear message rather
    /// than silently ignored, since `deploy.replicas` changes what a user
    /// expects to happen.
    #[serde(default)]
    pub deploy: Option<serde_yaml_ng::Value>,
    /// Via-specific per-service settings under `x-via:`.
    #[serde(default, rename = "x-via")]
    pub x_via: Option<RawViaService>,
}

/// `x-via:` on a service. Uses the Compose extension namespace so a Via
/// compose file stays a valid Compose file and still runs under plain
/// `docker compose up` during local development.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawViaService {
    /// `healthy` | `running` | `completed` | `ignored`.
    #[serde(default)]
    pub readiness: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum RawBuild {
    /// `build: ./app`
    Context(String),
    Long(RawBuildLong),
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct RawBuildLong {
    #[serde(default)]
    pub context: Option<String>,
    #[serde(default)]
    pub dockerfile: Option<String>,
    #[serde(default)]
    pub args: Option<MapOrList>,
    #[serde(default)]
    pub target: Option<String>,
    #[serde(default)]
    pub cache_from: Vec<String>,
    #[serde(default)]
    pub platforms: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum RawVolumeMount {
    /// `- db-data:/var/lib/postgresql/data:ro`
    Short(String),
    Long(RawVolumeMountLong),
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct RawVolumeMountLong {
    #[serde(rename = "type", default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub target: Option<String>,
    #[serde(default)]
    pub read_only: Option<bool>,
    #[serde(default)]
    pub tmpfs: Option<RawTmpfsOpts>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct RawTmpfsOpts {
    #[serde(default)]
    pub size: Option<Scalar>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum RawPort {
    /// `- 8080` or `- "127.0.0.1:8080:80/udp"`
    Short(Scalar),
    Long(RawPortLong),
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct RawPortLong {
    #[serde(default)]
    pub target: Option<Scalar>,
    #[serde(default)]
    pub published: Option<Scalar>,
    #[serde(default)]
    pub host_ip: Option<String>,
    #[serde(default)]
    pub protocol: Option<String>,
    #[serde(default)]
    pub mode: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum RawServiceNetworks {
    List(Vec<String>),
    Map(IndexMap<String, Option<serde_yaml_ng::Value>>),
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum RawDependsOn {
    /// `depends_on: [db]`
    List(Vec<String>),
    /// `depends_on: {db: {condition: service_healthy}}`
    Map(IndexMap<String, RawDependsOnEntry>),
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct RawDependsOnEntry {
    #[serde(default)]
    pub condition: Option<String>,
    #[serde(default)]
    pub required: Option<bool>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct RawHealthcheck {
    #[serde(default)]
    pub test: Option<StringOrList>,
    #[serde(default)]
    pub interval: Option<String>,
    #[serde(default)]
    pub timeout: Option<String>,
    #[serde(default)]
    pub retries: Option<u32>,
    #[serde(default)]
    pub start_period: Option<String>,
    #[serde(default)]
    pub disable: Option<bool>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct RawNetwork {
    #[serde(default)]
    pub driver: Option<String>,
    #[serde(default)]
    pub driver_opts: IndexMap<String, Scalar>,
    #[serde(default)]
    pub external: Option<bool>,
    #[serde(default)]
    pub internal: Option<bool>,
    #[serde(default)]
    pub labels: Option<MapOrList>,
    #[serde(default)]
    pub name: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct RawVolume {
    #[serde(default)]
    pub driver: Option<String>,
    #[serde(default)]
    pub driver_opts: IndexMap<String, Scalar>,
    #[serde(default)]
    pub external: Option<bool>,
    #[serde(default)]
    pub labels: Option<MapOrList>,
    #[serde(default)]
    pub name: Option<String>,
}
