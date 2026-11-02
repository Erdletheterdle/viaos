//! Reading, verifying and unpacking a bundle.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use via_core::{Deployment, Digest};

use crate::Error;
use crate::manifest::*;
use crate::oci::{self, blob_path};
use crate::signing::{BundleSignature, SIGNATURE_PATH, TrustPolicy, Verified};

/// Read just the manifest from the front of a bundle stream.
///
/// The manifest is the first tar entry, so this stops as soon as it has it
/// rather than decompressing the blobs behind it. That makes it cheap enough
/// to identify a partially transferred bundle.
pub fn peek_manifest(source: impl Read) -> Result<BundleManifest, Error> {
    let decoder = zstd::Decoder::new(source).map_err(|e| Error::io(Path::new("<zstd>"), e))?;
    let mut archive = tar::Archive::new(decoder);
    for entry in archive.entries().map_err(Error::tar)? {
        let mut entry = entry.map_err(Error::tar)?;
        let path = entry.path().map_err(Error::tar)?.into_owned();
        if path.to_string_lossy().trim_start_matches("./") == MANIFEST_PATH {
            let mut buf = Vec::new();
            entry
                .read_to_end(&mut buf)
                .map_err(|e| Error::io(&path, e))?;
            let manifest: BundleManifest = serde_json::from_slice(&buf).map_err(Error::json)?;
            manifest.check_supported()?;
            return Ok(manifest);
        }
    }
    Err(Error::MissingManifest)
}

/// The most any one text file or image config may unpack to when reading a
/// bundle's contents. A real one is kilobytes; this bounds a hand-made one.
const CONTENTS_TEXT_LIMIT: u64 = 16 * 1024 * 1024;

/// What is in a bundle, for showing to a person. See [`read_contents`].
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct BundleContents {
    pub manifest: BundleManifest,
    pub compose: Option<String>,
    pub deployment: String,
    /// Every entry in the archive, in order.
    pub files: Vec<BundleFile>,
    /// The text of each image's OCI manifest and config blobs, by digest.
    pub image_blobs: std::collections::BTreeMap<String, String>,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct BundleFile {
    pub path: String,
    pub size: u64,
}

/// Read everything about a bundle except its image layers, whose bytes are
/// only skipped over. This decompresses the whole stream, so a caller keeps the
/// result. Like [`peek_deployment`] it checks no signature: it is only data.
pub fn read_contents(source: impl Read) -> Result<BundleContents, Error> {
    let decoder = zstd::Decoder::new(source).map_err(|e| Error::io(Path::new("<zstd>"), e))?;
    let mut archive = tar::Archive::new(decoder);
    let mut manifest: Option<BundleManifest> = None;
    let mut compose = None;
    let mut deployment = None;
    let mut files = Vec::new();
    let mut image_blobs = std::collections::BTreeMap::new();
    let mut wanted = std::collections::BTreeSet::new();
    for entry in archive.entries().map_err(Error::tar)? {
        let entry = entry.map_err(Error::tar)?;
        let path = entry.path().map_err(Error::tar)?.into_owned();
        let name = path.to_string_lossy().trim_start_matches("./").to_string();
        let size = entry.size();
        files.push(BundleFile {
            path: name.clone(),
            size,
        });
        let blob = name
            .strip_prefix(BLOB_PREFIX)
            .map(|b| b.replacen('-', ":", 1));
        let keep = name == MANIFEST_PATH
            || name == DEPLOYMENT_PATH
            || name == crate::COMPOSE_PATH
            || blob.as_ref().is_some_and(|b| wanted.contains(b));
        if !keep {
            continue;
        }
        let mut buf = Vec::new();
        entry
            .take(CONTENTS_TEXT_LIMIT)
            .read_to_end(&mut buf)
            .map_err(|e| Error::io(&path, e))?;
        let text = String::from_utf8_lossy(&buf).into_owned();
        if name == MANIFEST_PATH {
            let m: BundleManifest = serde_json::from_slice(&buf).map_err(Error::json)?;
            m.check_supported()?;
            for image in &m.images {
                wanted.insert(image.manifest_digest.to_string());
                wanted.insert(image.config_digest.to_string());
            }
            manifest = Some(m);
        } else if name == DEPLOYMENT_PATH {
            deployment = Some(text);
        } else if name == crate::COMPOSE_PATH {
            compose = Some(text);
        } else if let Some(b) = blob {
            image_blobs.insert(b, text);
        }
    }
    Ok(BundleContents {
        manifest: manifest.ok_or(Error::MissingManifest)?,
        compose,
        deployment: deployment.ok_or(Error::MissingDeployment)?,
        files,
        image_blobs,
    })
}

/// The most a deployment manifest may unpack to when only peeking at one.
const PEEK_DEPLOYMENT_LIMIT: u64 = 16 * 1024 * 1024;

/// Read a bundle's deployment manifest from the front of its stream, checked
/// against the bundle's own manifest, without unpacking any blobs.
///
/// For a control plane that stores releases it never runs. Nothing here checks
/// a signature or touches an image: the result is only data about the release.
pub fn peek_deployment(source: impl Read) -> Result<Deployment, Error> {
    let decoder = zstd::Decoder::new(source).map_err(|e| Error::io(Path::new("<zstd>"), e))?;
    let mut archive = tar::Archive::new(decoder);
    let mut manifest: Option<BundleManifest> = None;
    for entry in archive.entries().map_err(Error::tar)? {
        let entry = entry.map_err(Error::tar)?;
        let path = entry.path().map_err(Error::tar)?.into_owned();
        let name = path.to_string_lossy().trim_start_matches("./").to_string();
        if name.starts_with(BLOB_PREFIX) {
            break;
        }
        if name != MANIFEST_PATH && name != DEPLOYMENT_PATH {
            continue;
        }
        let mut buf = Vec::new();
        entry
            .take(PEEK_DEPLOYMENT_LIMIT)
            .read_to_end(&mut buf)
            .map_err(|e| Error::io(&path, e))?;
        if name == MANIFEST_PATH {
            let m: BundleManifest = serde_json::from_slice(&buf).map_err(Error::json)?;
            m.check_supported()?;
            manifest = Some(m);
            continue;
        }
        let manifest = manifest.ok_or(Error::MissingManifest)?;
        let actual = Digest::sha256_of(&buf);
        if actual != manifest.deployment_digest {
            return Err(Error::DeploymentDigestMismatch {
                expected: manifest.deployment_digest,
                actual,
            });
        }
        return serde_json::from_slice(&buf).map_err(Error::json);
    }
    Err(Error::MissingDeployment)
}

/// Read the manifest and its signature from the front of a bundle stream, and
/// check them against a trust policy.
///
/// Both entries are written at the head of the archive, so this rejects a
/// bundle signed by an untrusted key after roughly a kilobyte rather than
/// after the whole payload has crossed the network.
pub fn peek_signature(
    source: impl Read,
    policy: &TrustPolicy,
) -> Result<(BundleManifest, Verified), Error> {
    let decoder = zstd::Decoder::new(source).map_err(|e| Error::io(Path::new("<zstd>"), e))?;
    let mut archive = tar::Archive::new(decoder);

    let mut manifest_bytes: Option<Vec<u8>> = None;
    let mut signature: Option<BundleSignature> = None;

    for entry in archive.entries().map_err(Error::tar)? {
        let mut entry = entry.map_err(Error::tar)?;
        let path = entry.path().map_err(Error::tar)?.into_owned();
        match path.to_string_lossy().trim_start_matches("./") {
            MANIFEST_PATH => {
                let mut buf = Vec::new();
                entry
                    .read_to_end(&mut buf)
                    .map_err(|e| Error::io(&path, e))?;
                manifest_bytes = Some(buf);
            }
            SIGNATURE_PATH => {
                let mut buf = Vec::new();
                entry
                    .read_to_end(&mut buf)
                    .map_err(|e| Error::io(&path, e))?;
                signature = Some(serde_json::from_slice(&buf).map_err(Error::json)?);
            }
            // Both live at the head of the archive; anything else means we
            // have passed them and can stop without reading the payload.
            _ => break,
        }
    }

    let manifest_bytes = manifest_bytes.ok_or(Error::MissingManifest)?;
    let verified = policy.verify(&manifest_bytes, signature.as_ref())?;
    let manifest: BundleManifest = serde_json::from_slice(&manifest_bytes).map_err(Error::json)?;
    manifest.check_supported()?;
    Ok((manifest, verified))
}

/// A bundle unpacked onto disk and verified.
#[derive(Debug, Clone)]
pub struct UnpackedBundle {
    pub manifest: BundleManifest,
    pub deployment: Deployment,
    /// The compose file a device runs.
    pub compose_source: Option<String>,
    /// Flat content-addressed blob directory.
    pub blob_dir: PathBuf,
    /// How this bundle was authenticated.
    pub trust: Verified,
}

impl UnpackedBundle {
    /// Unpack a bundle into `dest`, verifying every blob as it lands.
    ///
    /// Verification happens here, before anything is handed to Docker, because
    /// a bundle is the only thing a device trusts about a release
    /// (spec section 21, VERIFY before LOAD).
    pub fn unpack(source: impl Read, dest: impl AsRef<Path>) -> Result<Self, Error> {
        Self::unpack_with(source, dest, &TrustPolicy::Insecure)
    }

    /// Unpack, enforcing a trust policy.
    ///
    /// The signature is checked before any blob is written, so a bundle from
    /// an untrusted signer never reaches the disk.
    pub fn unpack_with(
        source: impl Read,
        dest: impl AsRef<Path>,
        policy: &TrustPolicy,
    ) -> Result<Self, Error> {
        let dest = dest.as_ref();
        let blob_dir = dest.join("blobs");
        std::fs::create_dir_all(&blob_dir).map_err(|e| Error::io(&blob_dir, e))?;

        let decoder = zstd::Decoder::new(source).map_err(|e| Error::io(Path::new("<zstd>"), e))?;
        let mut archive = tar::Archive::new(decoder);

        let mut manifest: Option<BundleManifest> = None;
        let mut manifest_bytes: Option<Vec<u8>> = None;
        let mut signature: Option<BundleSignature> = None;
        let mut trust: Option<Verified> = None;
        let mut deployment_bytes: Option<Vec<u8>> = None;
        let mut compose_source: Option<String> = None;
        let mut seen: Vec<Digest> = Vec::new();
        let mut project_files_seen = 0;

        for entry in archive.entries().map_err(Error::tar)? {
            let mut entry = entry.map_err(Error::tar)?;
            let raw = entry.path().map_err(Error::tar)?.into_owned();
            let name = raw.to_string_lossy().trim_start_matches("./").to_string();

            match name.as_str() {
                MANIFEST_PATH => {
                    let mut buf = Vec::new();
                    entry
                        .read_to_end(&mut buf)
                        .map_err(|e| Error::io(&raw, e))?;
                    let m: BundleManifest = serde_json::from_slice(&buf).map_err(Error::json)?;
                    m.check_supported()?;
                    manifest = Some(m);
                    manifest_bytes = Some(buf);
                    if !policy.requires_signature() {
                        // Nothing to wait for; record the outcome now.
                        trust = Some(Verified::Unsigned);
                    }
                }
                SIGNATURE_PATH => {
                    let mut buf = Vec::new();
                    entry
                        .read_to_end(&mut buf)
                        .map_err(|e| Error::io(&raw, e))?;
                    let sig: BundleSignature = serde_json::from_slice(&buf).map_err(Error::json)?;
                    let bytes = manifest_bytes.as_deref().ok_or(Error::MissingManifest)?;
                    // Verify here, before a single blob is written, so an
                    // untrusted release never touches the disk.
                    trust = Some(policy.verify(bytes, Some(&sig))?);
                    signature = Some(sig);
                }
                DEPLOYMENT_PATH => {
                    let mut buf = Vec::new();
                    entry
                        .read_to_end(&mut buf)
                        .map_err(|e| Error::io(&raw, e))?;
                    deployment_bytes = Some(buf);
                }
                COMPOSE_PATH => {
                    let mut buf = String::new();
                    entry
                        .read_to_string(&mut buf)
                        .map_err(|e| Error::io(&raw, e))?;
                    compose_source = Some(buf);
                }
                other if other.starts_with(PROJECT_PREFIX) => {
                    let path = &other[PROJECT_PREFIX.len()..];
                    if entry.header().entry_type().is_dir() {
                        continue;
                    }
                    let bad = |reason: &str| Error::ProjectFile {
                        path: path.to_string(),
                        reason: reason.to_string(),
                    };
                    // Another attacker-controlled path, and the manifest comes
                    // first, so a file it doesn't list never lands.
                    if !crate::manifest::is_safe_project_path(path) {
                        return Err(bad("not a path inside the project"));
                    }
                    let listed = manifest
                        .as_ref()
                        .ok_or(Error::MissingManifest)?
                        .project_files
                        .iter()
                        .find(|f| f.path == path)
                        .cloned()
                        .ok_or_else(|| bad("the manifest doesn't list it"))?;
                    let dest_path = dest.join("project").join(path);
                    if let Some(parent) = dest_path.parent() {
                        std::fs::create_dir_all(parent).map_err(|e| Error::io(parent, e))?;
                    }
                    let mut file =
                        std::fs::File::create(&dest_path).map_err(|e| Error::io(&dest_path, e))?;
                    let actual = copy_and_digest(&mut entry, &mut file)?;
                    file.flush().map_err(|e| Error::io(&dest_path, e))?;
                    if actual != listed.digest {
                        return Err(bad("its contents don't match the manifest"));
                    }
                    std::fs::set_permissions(
                        &dest_path,
                        std::os::unix::fs::PermissionsExt::from_mode(listed.mode & 0o777),
                    )
                    .map_err(|e| Error::io(&dest_path, e))?;
                    project_files_seen += 1;
                }
                other => {
                    let Some(segment) = other.strip_prefix(BLOB_PREFIX) else {
                        continue;
                    };
                    // A tar path is attacker-controlled input. Only a bare,
                    // well-formed digest segment is ever written, so a crafted
                    // `../../etc` path cannot escape the blob directory.
                    let digest = Digest::parse(segment.replacen('-', ":", 1)).map_err(|_| {
                        Error::MalformedBlobPath {
                            path: other.to_string(),
                        }
                    })?;

                    let dest_path = blob_dir.join(digest.to_path_segment());
                    let mut file =
                        std::fs::File::create(&dest_path).map_err(|e| Error::io(&dest_path, e))?;
                    let actual = copy_and_digest(&mut entry, &mut file)?;
                    file.flush().map_err(|e| Error::io(&dest_path, e))?;

                    if actual != digest {
                        let _ = std::fs::remove_file(&dest_path);
                        return Err(Error::BlobDigestMismatch {
                            expected: digest,
                            actual,
                        });
                    }
                    seen.push(digest);
                }
            }
        }

        let manifest = manifest.ok_or(Error::MissingManifest)?;
        let manifest_bytes = manifest_bytes.ok_or(Error::MissingManifest)?;
        // An unsigned bundle reaches here with no signature entry seen, so the
        // policy still has to be consulted before accepting it.
        let trust = match trust {
            Some(t) if signature.is_some() || !policy.requires_signature() => t,
            _ => policy.verify(&manifest_bytes, signature.as_ref())?,
        };
        let deployment_bytes = deployment_bytes.ok_or(Error::MissingDeployment)?;

        let actual = Digest::sha256_of(&deployment_bytes);
        if actual != manifest.deployment_digest {
            return Err(Error::DeploymentDigestMismatch {
                expected: manifest.deployment_digest.clone(),
                actual,
            });
        }
        // What a device runs, so the signed manifest has to vouch for it.
        match (&manifest.compose_digest, &compose_source) {
            (Some(expected), Some(src)) => {
                let actual = Digest::sha256_of(src.as_bytes());
                if &actual != expected {
                    return Err(Error::ComposeDigestMismatch {
                        expected: expected.clone(),
                        actual,
                    });
                }
            }
            (None, None) => {}
            _ => return Err(Error::ComposeNotListed),
        }
        if project_files_seen != manifest.project_files.len() {
            return Err(Error::ProjectFile {
                path: "*".into(),
                reason: format!(
                    "the manifest lists {} and the bundle holds {project_files_seen}",
                    manifest.project_files.len()
                ),
            });
        }

        // Every blob the manifest promises must actually be here. A truncated
        // transfer that happened to end on an entry boundary would otherwise
        // look like a valid bundle right up until Docker failed to load it.
        let missing: Vec<Digest> = manifest
            .blobs
            .iter()
            .filter(|b| !seen.contains(&b.digest))
            .map(|b| b.digest.clone())
            .collect();
        if !missing.is_empty() {
            return Err(Error::IncompleteBundle {
                missing: missing.len(),
                first: missing[0].clone(),
            });
        }

        let deployment: Deployment =
            serde_json::from_slice(&deployment_bytes).map_err(Error::json)?;
        if !(via_core::OLDEST_DEPLOYMENT_SCHEMA..=via_core::DEPLOYMENT_SCHEMA_VERSION)
            .contains(&deployment.schema_version)
        {
            return Err(Error::UnsupportedDeploymentSchema {
                found: deployment.schema_version,
                supported: via_core::DEPLOYMENT_SCHEMA_VERSION,
            });
        }

        Ok(Self {
            manifest,
            deployment,
            compose_source,
            blob_dir,
            trust,
        })
    }

    /// Re-verify blobs already on disk.
    ///
    /// Used after a reboot or a suspected disk problem, where the bundle was
    /// verified once but the filesystem may have changed under it.
    pub fn verify(&self) -> Result<(), Error> {
        for blob in &self.manifest.blobs {
            let path = blob_path(&self.blob_dir, &blob.digest);
            let mut file = std::fs::File::open(&path).map_err(|e| Error::io(&path, e))?;
            let actual = copy_and_digest(&mut file, &mut std::io::sink())?;
            if actual != blob.digest {
                return Err(Error::BlobDigestMismatch {
                    expected: blob.digest.clone(),
                    actual,
                });
            }
        }
        Ok(())
    }

    /// Write a tar archive suitable for Docker's `/images/load`.
    pub fn write_docker_load_archive<W: Write>(&self, out: W) -> Result<W, Error> {
        let images: Vec<oci::LoadImage<'_>> = self
            .manifest
            .images
            .iter()
            .map(|i| oci::LoadImage {
                reference: &i.reference,
                manifest_digest: &i.manifest_digest,
                config_digest: &i.config_digest,
                layers: &i.layers,
                media_type: &i.media_type,
            })
            .collect();
        oci::write_docker_load_archive(out, &self.blob_dir, &images)
    }
}

/// Copy `from` to `to`, returning the digest of what passed through.
///
/// Digesting during the copy means a large blob is read once, not once to
/// write and again to verify.
fn copy_and_digest(from: &mut impl Read, to: &mut impl Write) -> Result<Digest, Error> {
    use sha2::{Digest as _, Sha256};
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = from
            .read(&mut buf)
            .map_err(|e| Error::io(Path::new("<blob>"), e))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        to.write_all(&buf[..n])
            .map_err(|e| Error::io(Path::new("<blob>"), e))?;
    }
    Digest::parse(format!("sha256:{}", hex::encode(hasher.finalize()))).map_err(Error::Core)
}
