//! The Via bundle: a complete, portable, immutable application release.
//!
//! A bundle carries everything a device needs to run one release of one
//! application on one platform: the normalized deployment manifest, the
//! original compose source for inspection, and every container image, stored
//! as content-addressed OCI blobs. If the device has the bundle, it has
//! everything (spec sections 10, 11).
//!
//! ```text
//! demo-stack-44-linux-arm64.bundle.zst   = zstd(tar)
//! ├── via-bundle.json        manifest: images, blobs, digests   (written first)
//! ├── deployment.json        the runtime contract
//! ├── compose.yaml           original authoring form, optional
//! └── blobs/sha256-<hex>     OCI image manifests, configs, layers
//! ```
//!
//! # Why content-addressed blobs rather than one `images.tar`
//!
//! Storing blobs individually costs nothing at build time and buys two things.
//! Releases that share a base image share its layer blobs, so a bundle is not
//! the sum of its images. And because [`BundleManifest::missing_blobs`] can
//! diff a manifest against what a device already holds, only the changed
//! blobs need to cross the network, which is most of the benefit of a delta
//! protocol without the machinery (spec sections 24, 67).
//!
//! # Verification
//!
//! Nothing reaches Docker unverified. [`UnpackedBundle::unpack`] digests every
//! blob as it is written and refuses a bundle whose contents do not match its
//! manifest, before the LOAD step (spec section 21).

mod manifest;
mod oci;
mod reader;
mod signing;
mod writer;

use std::path::Path;

use thiserror::Error as ThisError;

pub use manifest::{
    BLOB_PREFIX, BUNDLE_SCHEMA_VERSION, BlobEntry, BundleImage, BundleManifest, COMPOSE_PATH,
    DEPLOYMENT_PATH, MANIFEST_PATH,
};
pub use oci::{IngestedImage, blob_path, ingest_docker_archive};
pub use reader::{UnpackedBundle, peek_manifest, peek_signature};
pub use signing::{
    ALGORITHM_ED25519, BundleSignature, KeyId, PublicKey, SIGNATURE_PATH, SigningKey, TrustPolicy,
    Verified,
};
pub use writer::{BundleWriter, DEFAULT_COMPRESSION};

#[derive(Debug, ThisError)]
pub enum Error {
    #[error("{path}: {source}")]
    Io { path: String, #[source] source: std::io::Error },

    #[error("malformed archive: {0}")]
    Tar(#[source] std::io::Error),

    #[error("malformed JSON: {0}")]
    Json(#[source] serde_json::Error),

    #[error("bundle schema version {found} is not supported by this build (supports {supported})")]
    UnsupportedSchema { found: u32, supported: u32 },

    #[error("deployment schema version {found} is not supported by this build (supports {supported})")]
    UnsupportedDeploymentSchema { found: u32, supported: u32 },

    #[error("bundle contains no {}", MANIFEST_PATH)]
    MissingManifest,

    #[error("bundle contains no {}", DEPLOYMENT_PATH)]
    MissingDeployment,

    #[error("blob path {path:?} is not a valid digest")]
    MalformedBlobPath { path: String },

    #[error("blob digest mismatch: expected {expected}, got {actual}")]
    BlobDigestMismatch { expected: via_core::Digest, actual: via_core::Digest },

    #[error("deployment.json digest mismatch: expected {expected}, got {actual}")]
    DeploymentDigestMismatch { expected: via_core::Digest, actual: via_core::Digest },

    #[error("compose.yaml digest mismatch: expected {expected}, got {actual}")]
    ComposeDigestMismatch { expected: via_core::Digest, actual: via_core::Digest },

    #[error("bundle is incomplete: {missing} blob(s) missing, starting with {first}")]
    IncompleteBundle { missing: usize, first: via_core::Digest },

    #[error("archive is not a docker image archive (no index.json)")]
    NotAnImageArchive,

    #[error("archive contains no image for platform {target}")]
    NoMatchingPlatform { target: String },

    #[error(
        "deployment requires image(s) not present in the bundle: {}. \
         A bundle must be self-contained; nothing is pulled at deploy time",
        references.join(", ")
    )]
    MissingImages { references: Vec<String> },

    #[error("bundle is not signed, and this device requires a signed release")]
    BundleUnsigned,

    #[error("signature algorithm {algorithm:?} is not supported by this build")]
    UnsupportedSignatureAlgorithm { algorithm: String },

    #[error(
        "bundle is signed by key {key_id}, which this device does not trust (trusted: {})",
        if trusted.is_empty() { "none".to_string() }
        else { trusted.iter().map(|k| k.to_string()).collect::<Vec<_>>().join(", ") }
    )]
    UntrustedSigner { key_id: KeyId, trusted: Vec<KeyId> },

    #[error("signature by key {key_id} does not match the manifest; the bundle has been altered")]
    SignatureVerificationFailed { key_id: KeyId },

    #[error("malformed signature: {reason}")]
    MalformedSignature { reason: String },

    #[error("malformed {kind} key: {reason}")]
    MalformedKey { kind: &'static str, reason: String },

    #[error(transparent)]
    Core(#[from] via_core::Error),
}

impl Error {
    pub(crate) fn io(path: impl AsRef<Path>, source: std::io::Error) -> Self {
        Error::Io { path: path.as_ref().display().to_string(), source }
    }

    pub(crate) fn tar(source: std::io::Error) -> Self {
        Error::Tar(source)
    }

    pub(crate) fn json(source: serde_json::Error) -> Self {
        Error::Json(source)
    }
}
