//! Assembling a bundle.
//!
//! A bundle is a zstd-compressed tar. The manifest is written first so a
//! reader can identify the bundle from a stream prefix without unpacking the
//! blobs behind it.

use std::io::Write;
use std::path::{Path, PathBuf};

use via_core::{AppName, Deployment, Digest, ImageRef, Platform, ReleaseId};

use crate::Error;
use crate::manifest::*;
use crate::oci::{self, blob_path};
use crate::signing::{SIGNATURE_PATH, SigningKey};

/// Default zstd level.
///
/// Container layers are already gzip-compressed inside the archive, so the
/// bundle wrapper mostly compresses JSON and slack. Level 3 costs seconds and
/// higher levels buy almost nothing on this input.
pub const DEFAULT_COMPRESSION: i32 = 3;

/// Builds a bundle from a deployment and the images it needs.
///
/// Blobs are staged in a content-addressed directory as they are ingested, so
/// two images sharing a base layer stage that layer once (spec section 24).
pub struct BundleWriter {
    app: AppName,
    release: ReleaseId,
    platform: Platform,
    staging: PathBuf,
    _owned_staging: Option<tempfile::TempDir>,
    images: Vec<BundleImage>,
    blobs: Vec<BlobEntry>,
    project_files: Vec<(ProjectFileEntry, PathBuf)>,
    created_at: Option<String>,
    builder: Option<String>,
    compression: i32,
    signing_key: Option<SigningKey>,
}

impl BundleWriter {
    /// Stage into a temporary directory, removed when the writer is dropped.
    pub fn new(app: AppName, release: ReleaseId, platform: Platform) -> Result<Self, Error> {
        let dir = tempfile::TempDir::new().map_err(|e| Error::io(Path::new("<tempdir>"), e))?;
        let staging = dir.path().to_path_buf();
        Ok(Self {
            app,
            release,
            platform,
            staging,
            _owned_staging: Some(dir),
            images: Vec::new(),
            blobs: Vec::new(),
            project_files: Vec::new(),
            created_at: None,
            builder: None,
            compression: DEFAULT_COMPRESSION,
            signing_key: None,
        })
    }

    /// Stage into a caller-owned directory.
    ///
    /// Reusing one across builds turns the blob directory into a build cache:
    /// unchanged base layers are already present and are not re-extracted.
    pub fn with_staging(
        app: AppName,
        release: ReleaseId,
        platform: Platform,
        staging: impl Into<PathBuf>,
    ) -> Result<Self, Error> {
        let staging = staging.into();
        std::fs::create_dir_all(&staging).map_err(|e| Error::io(&staging, e))?;
        Ok(Self {
            app,
            release,
            platform,
            staging,
            _owned_staging: None,
            images: Vec::new(),
            blobs: Vec::new(),
            project_files: Vec::new(),
            created_at: None,
            builder: None,
            compression: DEFAULT_COMPRESSION,
            signing_key: None,
        })
    }

    /// Carry a file the compose file reads, at its path in the project.
    pub fn add_project_file(&mut self, path: &str, source: &Path) -> Result<(), Error> {
        if !crate::manifest::is_safe_project_path(path) {
            return Err(Error::ProjectFile {
                path: path.to_string(),
                reason: "not a path inside the project".into(),
            });
        }
        let bytes = std::fs::read(source).map_err(|e| Error::io(source, e))?;
        let meta = std::fs::metadata(source).map_err(|e| Error::io(source, e))?;
        self.project_files.push((
            ProjectFileEntry {
                path: path.to_string(),
                digest: Digest::sha256_of(&bytes),
                size: bytes.len() as u64,
                mode: std::os::unix::fs::PermissionsExt::mode(&meta.permissions()) & 0o777,
            },
            source.to_path_buf(),
        ));
        Ok(())
    }

    /// RFC 3339 build timestamp. Passed in rather than read from a clock so
    /// callers that want reproducible bundles can leave it out.
    pub fn created_at(mut self, ts: impl Into<String>) -> Self {
        self.created_at = Some(ts.into());
        self
    }

    pub fn builder(mut self, b: impl Into<String>) -> Self {
        self.builder = Some(b.into());
        self
    }

    pub fn compression(mut self, level: i32) -> Self {
        self.compression = level;
        self
    }

    /// Sign the manifest with `key`.
    ///
    /// The manifest carries the digest of every blob and of the deployment, so
    /// this signature covers the whole release. It is written as the second
    /// entry in the archive, immediately after the manifest, so a verifier can
    /// reject a forgery after reading a kilobyte instead of a gigabyte.
    pub fn sign_with(mut self, key: SigningKey) -> Self {
        self.signing_key = Some(key);
        self
    }

    pub fn blob_dir(&self) -> &Path {
        &self.staging
    }

    /// Ingest a `docker save` archive, adding every image it contains that
    /// matches the target platform.
    ///
    /// `reference_for` maps an image's recorded name to the tag Via should
    /// load it under, letting the caller reconcile what Docker called an image
    /// with what the deployment manifest expects.
    pub fn add_docker_archive(
        &mut self,
        archive: impl std::io::Read,
        reference_for: impl Fn(Option<&str>) -> Option<String>,
    ) -> Result<usize, Error> {
        let ingested = oci::ingest_docker_archive(archive, &self.staging, &self.platform)?;

        let mut added = 0;
        for image in ingested.images {
            let Some(reference) = reference_for(image.reference.as_deref()) else {
                continue;
            };
            if self.images.iter().any(|i| i.reference == reference) {
                continue;
            }
            self.images.push(BundleImage {
                reference,
                manifest_digest: image.manifest_digest,
                config_digest: image.config_digest,
                layers: image.layers,
                media_type: image.media_type,
            });
            added += 1;
        }

        for (digest, size) in ingested.blobs.sizes {
            if self.blobs.iter().any(|b| b.digest == digest) {
                continue;
            }
            self.blobs.push(BlobEntry {
                digest: digest.clone(),
                size,
                media_type: ingested.blobs.media_types.get(&digest).cloned(),
            });
        }
        Ok(added)
    }

    /// Every image reference staged so far.
    pub fn staged_references(&self) -> Vec<&str> {
        self.images.iter().map(|i| i.reference.as_str()).collect()
    }

    /// Write the bundle.
    ///
    /// Fails if the deployment names an image the bundle does not carry: a
    /// bundle that would have to pull at runtime is not a bundle
    /// (spec section 11).
    pub fn finish<W: Write>(
        mut self,
        out: W,
        deployment: &Deployment,
        compose_source: Option<&str>,
    ) -> Result<W, Error> {
        self.check_images_present(deployment)?;

        let deployment_bytes = serde_json::to_vec_pretty(deployment).map_err(Error::json)?;
        let deployment_digest = Digest::sha256_of(&deployment_bytes);
        let compose_digest = compose_source.map(Digest::sha256_of);

        // Deterministic order so two builds of the same input differ only by
        // whatever the caller deliberately varied.
        self.blobs.sort_by(|a, b| a.digest.cmp(&b.digest));

        let manifest = BundleManifest {
            schema_version: BUNDLE_SCHEMA_VERSION,
            app: self.app.clone(),
            release: self.release,
            platform: self.platform.clone(),
            created_at: self.created_at.clone(),
            builder: self.builder.clone(),
            images: self.images.clone(),
            blobs: self.blobs.clone(),
            deployment_digest,
            compose_digest,
            project_files: self.project_files.iter().map(|(e, _)| e.clone()).collect(),
        };
        let manifest_bytes = serde_json::to_vec_pretty(&manifest).map_err(Error::json)?;

        let encoder = zstd::Encoder::new(out, self.compression)
            .map_err(|e| Error::io(Path::new("<zstd>"), e))?;
        let mut tar = tar::Builder::new(encoder);

        append_bytes(&mut tar, MANIFEST_PATH, &manifest_bytes)?;
        if let Some(key) = &self.signing_key {
            let signature = key.sign_manifest(&manifest_bytes);
            let sig_bytes = serde_json::to_vec_pretty(&signature).map_err(Error::json)?;
            append_bytes(&mut tar, SIGNATURE_PATH, &sig_bytes)?;
        }
        append_bytes(&mut tar, DEPLOYMENT_PATH, &deployment_bytes)?;
        if let Some(src) = compose_source {
            append_bytes(&mut tar, COMPOSE_PATH, src.as_bytes())?;
        }
        for (entry, source) in &self.project_files {
            let bytes = std::fs::read(source).map_err(|e| Error::io(source, e))?;
            if Digest::sha256_of(&bytes) != entry.digest {
                return Err(Error::ProjectFile {
                    path: entry.path.clone(),
                    reason: "changed while the release was being built".into(),
                });
            }
            append_bytes(&mut tar, &format!("{PROJECT_PREFIX}{}", entry.path), &bytes)?;
        }

        for blob in &self.blobs {
            let src = blob_path(&self.staging, &blob.digest);
            let mut file = std::fs::File::open(&src).map_err(|e| Error::io(&src, e))?;
            let mut header = tar::Header::new_gnu();
            header.set_size(blob.size);
            header.set_mode(0o644);
            header.set_mtime(0);
            header.set_cksum();
            tar.append_data(
                &mut header,
                format!("{BLOB_PREFIX}{}", blob.digest.to_path_segment()),
                &mut file,
            )
            .map_err(Error::tar)?;
        }

        let encoder = tar.into_inner().map_err(Error::tar)?;
        encoder
            .finish()
            .map_err(|e| Error::io(Path::new("<zstd>"), e))
    }

    /// Refuse to build a bundle that would need a registry at deploy time.
    fn check_images_present(&self, deployment: &Deployment) -> Result<(), Error> {
        let missing: Vec<String> = deployment
            .required_images()
            .iter()
            .filter(|img| !self.images.iter().any(|i| i.reference == img.reference))
            .map(|img| img.reference.clone())
            .collect();
        if !missing.is_empty() {
            return Err(Error::MissingImages {
                references: missing,
            });
        }
        Ok(())
    }

    /// Return a copy of `deployment` with every image's digest filled in from
    /// what was actually staged.
    pub fn digest_deployment(&self, deployment: &Deployment) -> Deployment {
        let mut out = deployment.clone();
        for svc in out.services.values_mut() {
            if let Some(img) = self
                .images
                .iter()
                .find(|i| i.reference == svc.image.reference)
            {
                svc.image = ImageRef::new(svc.image.reference.clone())
                    .with_digest(img.manifest_digest.clone());
            }
        }
        out
    }
}

fn append_bytes<W: Write>(
    builder: &mut tar::Builder<W>,
    name: &str,
    data: &[u8],
) -> Result<(), Error> {
    let mut header = tar::Header::new_gnu();
    header.set_size(data.len() as u64);
    header.set_mode(0o644);
    header.set_mtime(0);
    header.set_cksum();
    builder
        .append_data(&mut header, name, data)
        .map_err(Error::tar)
}
