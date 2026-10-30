//! Reading and re-emitting Docker's image archive format.
//!
//! Docker 25 and later already write an OCI image layout when you
//! `docker save`: content-addressed `blobs/sha256/<hex>` alongside a legacy
//! `manifest.json` kept for compatibility. Via stores those blobs directly, so
//! a bundle is content-addressed for free and two releases sharing a base
//! image share its layer blobs (spec section 24).
//!
//! Loading back is not symmetric. Docker's classic image store rejects a pure
//! OCI layout and requires the legacy `manifest.json`, so
//! [`write_docker_load_archive`] synthesizes one. Writing it ourselves is also
//! what lets Via choose the tag an image loads under rather than inheriting
//! whatever it was built as.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use via_core::{Digest, Platform};

use crate::Error;

const MEDIA_OCI_INDEX: &str = "application/vnd.oci.image.index.v1+json";
const MEDIA_OCI_MANIFEST: &str = "application/vnd.oci.image.manifest.v1+json";
const MEDIA_DOCKER_LIST: &str = "application/vnd.docker.distribution.manifest.list.v2+json";
const MEDIA_DOCKER_MANIFEST: &str = "application/vnd.docker.distribution.manifest.v2+json";

/// Annotation Docker uses to record an image's name in `index.json`.
const ANNOTATION_IMAGE_NAME: &str = "io.containerd.image.name";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OciIndex {
    #[serde(default)]
    pub schema_version: u32,
    #[serde(default, rename = "mediaType", skip_serializing_if = "Option::is_none")]
    pub media_type: Option<String>,
    pub manifests: Vec<OciDescriptor>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OciDescriptor {
    #[serde(rename = "mediaType")]
    pub media_type: String,
    pub digest: Digest,
    pub size: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub platform: Option<OciPlatform>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub annotations: BTreeMap<String, String>,
}

impl OciDescriptor {
    fn image_name(&self) -> Option<&str> {
        self.annotations
            .get(ANNOTATION_IMAGE_NAME)
            .map(|s| s.as_str())
            .filter(|s| !s.is_empty())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OciPlatform {
    pub os: String,
    pub architecture: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub variant: Option<String>,
}

impl OciPlatform {
    fn matches(&self, target: &Platform) -> bool {
        let p = Platform {
            os: self.os.clone(),
            arch: self.architecture.clone(),
            variant: self.variant.clone(),
        };
        p.is_compatible_with(target)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OciManifest {
    #[serde(default)]
    pub schema_version: u32,
    #[serde(default, rename = "mediaType", skip_serializing_if = "Option::is_none")]
    pub media_type: Option<String>,
    pub config: OciDescriptor,
    pub layers: Vec<OciDescriptor>,
}

/// An image found inside a `docker save` archive.
#[derive(Debug, Clone)]
pub struct IngestedImage {
    /// Name recorded in the archive, if any.
    pub reference: Option<String>,
    pub manifest_digest: Digest,
    pub config_digest: Digest,
    pub layers: Vec<Digest>,
    pub media_type: String,
}

/// Blobs extracted from an archive, keyed by digest, with their sizes.
#[derive(Debug, Default)]
pub struct BlobSet {
    pub sizes: BTreeMap<Digest, u64>,
    pub media_types: BTreeMap<Digest, String>,
}

/// Result of exploding a `docker save` archive.
#[derive(Debug)]
pub struct Ingested {
    pub images: Vec<IngestedImage>,
    pub blobs: BlobSet,
}

/// Explode a `docker save` archive into a flat content-addressed blob
/// directory, returning the images it describes.
///
/// Only blobs reachable from the selected images are kept. `docker save` of a
/// multi-platform image carries every architecture, and shipping the ones the
/// device cannot execute would defeat the point of per-platform bundles
/// (spec section 12).
pub fn ingest_docker_archive(
    archive: impl Read,
    blob_dir: &Path,
    target: &Platform,
) -> Result<Ingested, Error> {
    std::fs::create_dir_all(blob_dir).map_err(|e| Error::io(blob_dir, e))?;

    // Entry order in a tar is not guaranteed, and index.json may follow the
    // blobs it refers to, so extract first and interpret afterwards.
    let mut archive = tar::Archive::new(archive);
    let mut index_bytes: Option<Vec<u8>> = None;
    let mut present: BTreeMap<Digest, u64> = BTreeMap::new();

    for entry in archive.entries().map_err(Error::tar)? {
        let mut entry = entry.map_err(Error::tar)?;
        let path = entry.path().map_err(Error::tar)?.into_owned();
        let name = path.to_string_lossy().trim_start_matches("./").to_string();

        if name == "index.json" {
            let mut buf = Vec::new();
            entry.read_to_end(&mut buf).map_err(|e| Error::io(&path, e))?;
            index_bytes = Some(buf);
            continue;
        }
        // `blobs/sha256/<hex>`; the algorithm directory is the digest's.
        let Some(rest) = name.strip_prefix("blobs/") else { continue };
        let Some((algo, hex)) = rest.split_once('/') else { continue };
        let Ok(digest) = Digest::parse(format!("{algo}:{hex}")) else { continue };

        let dest = blob_path(blob_dir, &digest);
        let size = if dest.exists() {
            // Already have it: content-addressed, so identical by definition.
            std::fs::metadata(&dest).map_err(|e| Error::io(&dest, e))?.len()
        } else {
            let mut out = std::fs::File::create(&dest).map_err(|e| Error::io(&dest, e))?;
            let n = std::io::copy(&mut entry, &mut out).map_err(|e| Error::io(&dest, e))?;
            out.flush().map_err(|e| Error::io(&dest, e))?;
            n
        };
        present.insert(digest, size);
    }

    let index_bytes = index_bytes.ok_or(Error::NotAnImageArchive)?;
    let index: OciIndex = serde_json::from_slice(&index_bytes).map_err(Error::json)?;

    let mut images = Vec::new();
    let mut blobs = BlobSet::default();

    for desc in &index.manifests {
        let name = desc.image_name().map(|s| s.to_string());
        let resolved = resolve_descriptor(desc, blob_dir, target)?;
        let Some((manifest_digest, manifest)) = resolved else { continue };

        let mut record = |d: &Digest, media: &str| {
            if let Some(size) = present.get(d) {
                blobs.sizes.insert(d.clone(), *size);
                blobs.media_types.insert(d.clone(), media.to_string());
            }
        };
        record(&manifest_digest, manifest.media_type.as_deref().unwrap_or(MEDIA_OCI_MANIFEST));
        record(&manifest.config.digest, &manifest.config.media_type);
        for layer in &manifest.layers {
            record(&layer.digest, &layer.media_type);
        }

        images.push(IngestedImage {
            reference: name,
            manifest_digest,
            config_digest: manifest.config.digest.clone(),
            layers: manifest.layers.iter().map(|l| l.digest.clone()).collect(),
            media_type: manifest.media_type.clone().unwrap_or_else(|| MEDIA_OCI_MANIFEST.into()),
        });
    }

    if images.is_empty() {
        return Err(Error::NoMatchingPlatform { target: target.to_string() });
    }

    // Drop blobs no selected image references: other architectures, and the
    // index blobs of multi-platform images.
    for digest in present.keys() {
        if !blobs.sizes.contains_key(digest) {
            let _ = std::fs::remove_file(blob_path(blob_dir, digest));
        }
    }

    Ok(Ingested { images, blobs })
}

/// Follow a descriptor to a concrete image manifest, descending through a
/// multi-platform index if necessary.
fn resolve_descriptor(
    desc: &OciDescriptor,
    blob_dir: &Path,
    target: &Platform,
) -> Result<Option<(Digest, OciManifest)>, Error> {
    match desc.media_type.as_str() {
        MEDIA_OCI_MANIFEST | MEDIA_DOCKER_MANIFEST => {
            // A descriptor that states its platform must match. A single-image
            // `docker save` omits the field entirely, and there the archive's
            // one image is what the caller asked to save, so absence is
            // accepted rather than treated as a mismatch.
            if let Some(p) = &desc.platform {
                if !p.matches(target) {
                    return Ok(None);
                }
            }
            let manifest: OciManifest = read_json_blob(blob_dir, &desc.digest)?;
            Ok(Some((desc.digest.clone(), manifest)))
        }
        MEDIA_OCI_INDEX | MEDIA_DOCKER_LIST => {
            let index: OciIndex = read_json_blob(blob_dir, &desc.digest)?;
            for child in &index.manifests {
                let matches = match &child.platform {
                    Some(p) => p.matches(target),
                    // A child with no platform in a multi-platform index is
                    // usually an attestation, not something runnable.
                    None => false,
                };
                if matches {
                    return resolve_descriptor(child, blob_dir, target);
                }
            }
            Ok(None)
        }
        _ => Ok(None),
    }
}

fn read_json_blob<T: for<'de> Deserialize<'de>>(blob_dir: &Path, digest: &Digest) -> Result<T, Error> {
    let path = blob_path(blob_dir, digest);
    let bytes = std::fs::read(&path).map_err(|e| Error::io(&path, e))?;
    serde_json::from_slice(&bytes).map_err(Error::json)
}

/// Path of a blob in a flat content-addressed directory.
pub fn blob_path(dir: &Path, digest: &Digest) -> PathBuf {
    dir.join(digest.to_path_segment())
}

/// `nginx:1.29` as `docker.io/library/nginx:1.29`.
///
/// Docker's containerd image store keys images by their fully qualified name
/// and stores whatever `io.containerd.image.name` says verbatim. Given the
/// short form, it creates an image that no lookup by name ever finds, so the
/// annotation must carry the name the way Docker itself would spell it.
fn qualified_reference(reference: &str) -> String {
    let (first, _) = reference.split_once('/').unwrap_or(("", reference));
    let has_domain = first.contains('.') || first.contains(':') || first == "localhost";
    let mut name = if has_domain {
        reference.to_string()
    } else if reference.contains('/') {
        format!("docker.io/{reference}")
    } else {
        format!("docker.io/library/{reference}")
    };
    let last = name.rsplit('/').next().unwrap_or_default();
    if !last.contains(':') && !last.contains('@') {
        name.push_str(":latest");
    }
    name
}

/// Legacy `manifest.json` entry, which Docker's classic image store requires.
#[derive(Debug, Serialize)]
struct LegacyManifestEntry {
    #[serde(rename = "Config")]
    config: String,
    #[serde(rename = "RepoTags")]
    repo_tags: Vec<String>,
    #[serde(rename = "Layers")]
    layers: Vec<String>,
}

/// What to load, and under which tag.
pub struct LoadImage<'a> {
    pub reference: &'a str,
    pub manifest_digest: &'a Digest,
    pub config_digest: &'a Digest,
    pub layers: &'a [Digest],
    pub media_type: &'a str,
}

/// Write a tar archive Docker's `/images/load` accepts, pulling blob content
/// from a flat content-addressed directory.
///
/// The archive is a hybrid on purpose: an OCI layout for correctness, plus the
/// legacy `manifest.json` because Docker's classic image store will not load
/// without it.
pub fn write_docker_load_archive<W: Write>(
    out: W,
    blob_dir: &Path,
    images: &[LoadImage<'_>],
) -> Result<W, Error> {
    let mut builder = tar::Builder::new(out);

    let layout = br#"{"imageLayoutVersion": "1.0.0"}"#;
    append_bytes(&mut builder, "oci-layout", layout)?;

    let mut index_manifests = Vec::new();
    let mut legacy = Vec::new();
    let mut written: BTreeMap<Digest, ()> = BTreeMap::new();

    for image in images {
        let manifest_path = blob_path(blob_dir, image.manifest_digest);
        let size = std::fs::metadata(&manifest_path).map_err(|e| Error::io(&manifest_path, e))?.len();

        let mut annotations = BTreeMap::new();
        annotations.insert(ANNOTATION_IMAGE_NAME.to_string(), qualified_reference(image.reference));
        if let Some(tag) = image.reference.rsplit_once(':').map(|(_, t)| t) {
            annotations.insert("org.opencontainers.image.ref.name".to_string(), tag.to_string());
        }

        index_manifests.push(OciDescriptor {
            media_type: image.media_type.to_string(),
            digest: image.manifest_digest.clone(),
            size,
            platform: None,
            annotations,
        });

        legacy.push(LegacyManifestEntry {
            config: oci_blob_name(image.config_digest),
            repo_tags: vec![image.reference.to_string()],
            layers: image.layers.iter().map(oci_blob_name).collect(),
        });

        let mut blobs = vec![image.manifest_digest.clone(), image.config_digest.clone()];
        blobs.extend(image.layers.iter().cloned());
        for digest in blobs {
            if written.insert(digest.clone(), ()).is_some() {
                continue;
            }
            let src = blob_path(blob_dir, &digest);
            let mut file = std::fs::File::open(&src).map_err(|e| Error::io(&src, e))?;
            let meta = file.metadata().map_err(|e| Error::io(&src, e))?;
            let mut header = tar::Header::new_gnu();
            header.set_size(meta.len());
            header.set_mode(0o644);
            header.set_mtime(0);
            header.set_cksum();
            builder
                .append_data(&mut header, oci_blob_name(&digest), &mut file)
                .map_err(Error::tar)?;
        }
    }

    let index = OciIndex {
        schema_version: 2,
        media_type: Some(MEDIA_OCI_INDEX.to_string()),
        manifests: index_manifests,
    };
    append_bytes(&mut builder, "index.json", &serde_json::to_vec(&index).map_err(Error::json)?)?;
    append_bytes(&mut builder, "manifest.json", &serde_json::to_vec(&legacy).map_err(Error::json)?)?;

    builder.into_inner().map_err(Error::tar)
}

/// Blob path inside an OCI layout: `blobs/<algo>/<hex>`.
fn oci_blob_name(digest: &Digest) -> String {
    format!("blobs/{}/{}", digest.algorithm(), digest.hex())
}

fn append_bytes<W: Write>(builder: &mut tar::Builder<W>, name: &str, data: &[u8]) -> Result<(), Error> {
    let mut header = tar::Header::new_gnu();
    header.set_size(data.len() as u64);
    header.set_mode(0o644);
    header.set_mtime(0);
    header.set_cksum();
    builder.append_data(&mut header, name, data).map_err(Error::tar)
}

#[cfg(test)]
mod reference_tests {
    use super::qualified_reference;

    #[test]
    fn short_names_are_qualified_the_way_docker_spells_them() {
        assert_eq!(qualified_reference("nginx:1.29-alpine"), "docker.io/library/nginx:1.29-alpine");
        assert_eq!(qualified_reference("nginx"), "docker.io/library/nginx:latest");
        assert_eq!(qualified_reference("grafana/grafana:11"), "docker.io/grafana/grafana:11");
        assert_eq!(qualified_reference("ghcr.io/acme/app:1"), "ghcr.io/acme/app:1");
        assert_eq!(qualified_reference("localhost:5000/app"), "localhost:5000/app:latest");
        assert_eq!(qualified_reference("localhost/app:2"), "localhost/app:2");
        assert_eq!(qualified_reference("redis@sha256:abc"), "docker.io/library/redis@sha256:abc");
    }
}
