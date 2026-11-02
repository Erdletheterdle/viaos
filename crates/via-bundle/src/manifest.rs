//! The bundle manifest: what a release contains and how to verify it.

use serde::{Deserialize, Serialize};

use via_core::{AppName, Digest, Platform, ReleaseId};

/// Bumped when the bundle layout changes incompatibly. An agent that does not
/// recognize the version refuses the bundle instead of guessing at it.
pub const BUNDLE_SCHEMA_VERSION: u32 = 1;

/// Path of the manifest inside the bundle archive.
///
/// Written first so a reader can learn what it is holding without unpacking
/// gigabytes of blobs.
pub const MANIFEST_PATH: &str = "via-bundle.json";
pub const DEPLOYMENT_PATH: &str = "deployment.json";
pub const COMPOSE_PATH: &str = "compose.yaml";
/// Where the files the compose file reads travel, under their project paths.
pub const PROJECT_PREFIX: &str = "project/";
pub const BLOB_PREFIX: &str = "blobs/";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BundleManifest {
    pub schema_version: u32,
    pub app: AppName,
    pub release: ReleaseId,
    pub platform: Platform,

    /// RFC 3339 build timestamp, supplied by the caller. Optional because a
    /// bundle built in a hermetic or reproducible context may deliberately
    /// have no clock reading in it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<String>,

    /// Version of the tool that produced this bundle, for support triage.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub builder: Option<String>,

    /// Images this release runs, in the order they were added.
    pub images: Vec<BundleImage>,

    /// Every blob carried by the bundle, with its size and digest.
    ///
    /// This doubles as the integrity manifest and as the transfer plan: a
    /// device can compare this list against what it already has and ask for
    /// only the blobs it is missing, which is most of the value of a delta
    /// protocol for none of the machinery (spec section 24).
    pub blobs: Vec<BlobEntry>,

    /// Digest of `deployment.json`, so the runtime contract is covered by the
    /// same integrity check as the images.
    pub deployment_digest: Digest,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compose_digest: Option<Digest>,

    /// Files the compose file reads: bind mount sources, `env_file`s, configs.
    /// Listed here so the signature covers them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub project_files: Vec<ProjectFileEntry>,
}

/// One file a release carries beside its compose file.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectFileEntry {
    /// Relative to the project, with no `.` or `..`.
    pub path: String,
    pub digest: Digest,
    pub size: u64,
    pub mode: u32,
}

/// Whether a path from an archive stays inside the directory it lands in.
pub(crate) fn is_safe_project_path(path: &str) -> bool {
    !path.is_empty()
        && !path.starts_with('/')
        && path
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..")
}

impl BundleManifest {
    /// Total bytes of blob content, which is essentially the whole bundle.
    pub fn blob_bytes(&self) -> u64 {
        self.blobs.iter().map(|b| b.size).sum()
    }

    pub fn blob(&self, digest: &Digest) -> Option<&BlobEntry> {
        self.blobs.iter().find(|b| &b.digest == digest)
    }

    /// Blobs in `self` that `have` does not already contain.
    ///
    /// The transfer-planning primitive: a device reports what it holds, and
    /// only the difference goes over the wire.
    pub fn missing_blobs<'a>(&'a self, have: &[Digest]) -> Vec<&'a BlobEntry> {
        self.blobs
            .iter()
            .filter(|b| !have.contains(&b.digest))
            .collect()
    }

    /// Reject a bundle this build cannot safely interpret.
    pub fn check_supported(&self) -> Result<(), crate::Error> {
        if self.schema_version != BUNDLE_SCHEMA_VERSION {
            return Err(crate::Error::UnsupportedSchema {
                found: self.schema_version,
                supported: BUNDLE_SCHEMA_VERSION,
            });
        }
        Ok(())
    }
}

/// One image, described by the OCI blobs that constitute it.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BundleImage {
    /// Tag to load the image as on the device. This is what the deployment
    /// manifest's `ImageRef.reference` points at.
    pub reference: String,
    /// Digest of the OCI image manifest blob. The immutable identity of the
    /// image, and what makes a release reproducible.
    pub manifest_digest: Digest,
    pub config_digest: Digest,
    /// Layer blob digests, in application order.
    pub layers: Vec<Digest>,
    pub media_type: String,
}

impl BundleImage {
    /// Every blob this image needs: its manifest, its config, and its layers.
    pub fn blob_digests(&self) -> Vec<Digest> {
        let mut v = Vec::with_capacity(self.layers.len() + 2);
        v.push(self.manifest_digest.clone());
        v.push(self.config_digest.clone());
        v.extend(self.layers.iter().cloned());
        v
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BlobEntry {
    pub digest: Digest,
    pub size: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media_type: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest() -> BundleManifest {
        let d = |s: &[u8]| Digest::sha256_of(s);
        BundleManifest {
            schema_version: BUNDLE_SCHEMA_VERSION,
            app: AppName::parse("demo-stack").unwrap(),
            release: ReleaseId(44),
            platform: Platform::new("linux", "arm64"),
            created_at: None,
            builder: None,
            images: vec![],
            blobs: vec![
                BlobEntry {
                    digest: d(b"a"),
                    size: 10,
                    media_type: None,
                },
                BlobEntry {
                    digest: d(b"b"),
                    size: 20,
                    media_type: None,
                },
            ],
            deployment_digest: d(b"deployment"),
            compose_digest: None,
            project_files: Vec::new(),
        }
    }

    #[test]
    fn missing_blobs_is_the_transfer_plan() {
        let m = manifest();
        let have = vec![Digest::sha256_of(b"a")];
        let missing = m.missing_blobs(&have);
        assert_eq!(missing.len(), 1);
        assert_eq!(missing[0].size, 20);
        assert_eq!(m.blob_bytes(), 30);
    }

    #[test]
    fn a_future_schema_version_is_refused_not_guessed_at() {
        let mut m = manifest();
        m.schema_version = BUNDLE_SCHEMA_VERSION + 1;
        assert!(m.check_supported().is_err());
    }
}
