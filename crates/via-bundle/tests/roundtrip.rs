//! Bundle round-trip tests.
//!
//! These build a synthetic `docker save`-shaped archive rather than shelling
//! out to Docker, so they run in CI without a daemon. `tests/docker.rs` covers
//! the real thing.

use std::collections::BTreeMap;
use std::io::Write;

use via_bundle::{BundleWriter, Error, UnpackedBundle, peek_manifest};
use via_core::{AppName, Deployment, Digest, ImageRef, Platform, ReleaseId, Service};

/// Build an archive shaped like `docker save` output: an OCI layout with
/// `index.json` and content-addressed blobs.
struct FakeImage {
    reference: String,
    layer_bodies: Vec<Vec<u8>>,
    platform: Option<(&'static str, &'static str)>,
}

fn tar_append(builder: &mut tar::Builder<Vec<u8>>, name: &str, data: &[u8]) {
    let mut h = tar::Header::new_gnu();
    h.set_size(data.len() as u64);
    h.set_mode(0o644);
    h.set_mtime(0);
    h.set_cksum();
    builder.append_data(&mut h, name, data).unwrap();
}

fn blob_name(d: &Digest) -> String {
    format!("blobs/{}/{}", d.algorithm(), d.hex())
}

fn make_docker_archive(images: &[FakeImage]) -> Vec<u8> {
    let mut builder = tar::Builder::new(Vec::new());
    let mut index_manifests = Vec::new();
    let mut written: BTreeMap<String, ()> = BTreeMap::new();

    for img in images {
        let mut layer_descs = Vec::new();
        for body in &img.layer_bodies {
            let d = Digest::sha256_of(body);
            if written.insert(blob_name(&d), ()).is_none() {
                tar_append(&mut builder, &blob_name(&d), body);
            }
            layer_descs.push(serde_json::json!({
                "mediaType": "application/vnd.oci.image.layer.v1.tar",
                "digest": d.to_string(),
                "size": body.len(),
            }));
        }

        let config = serde_json::json!({
            "architecture": img.platform.map(|p| p.1).unwrap_or("arm64"),
            "os": "linux",
            "rootfs": {"type": "layers", "diff_ids": []},
        });
        let config_bytes = serde_json::to_vec(&config).unwrap();
        let config_digest = Digest::sha256_of(&config_bytes);
        if written.insert(blob_name(&config_digest), ()).is_none() {
            tar_append(&mut builder, &blob_name(&config_digest), &config_bytes);
        }

        let manifest = serde_json::json!({
            "schemaVersion": 2,
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "config": {
                "mediaType": "application/vnd.oci.image.config.v1+json",
                "digest": config_digest.to_string(),
                "size": config_bytes.len(),
            },
            "layers": layer_descs,
        });
        let manifest_bytes = serde_json::to_vec(&manifest).unwrap();
        let manifest_digest = Digest::sha256_of(&manifest_bytes);
        tar_append(&mut builder, &blob_name(&manifest_digest), &manifest_bytes);

        let mut desc = serde_json::json!({
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "digest": manifest_digest.to_string(),
            "size": manifest_bytes.len(),
            "annotations": {"io.containerd.image.name": img.reference},
        });
        if let Some((os, arch)) = img.platform {
            desc["platform"] = serde_json::json!({"os": os, "architecture": arch});
        }
        index_manifests.push(desc);
    }

    let index = serde_json::json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.index.v1+json",
        "manifests": index_manifests,
    });
    tar_append(
        &mut builder,
        "index.json",
        &serde_json::to_vec(&index).unwrap(),
    );
    tar_append(
        &mut builder,
        "oci-layout",
        br#"{"imageLayoutVersion": "1.0.0"}"#,
    );

    builder.into_inner().unwrap()
}

fn image(reference: &str, layers: &[&str]) -> FakeImage {
    FakeImage {
        reference: reference.to_string(),
        layer_bodies: layers.iter().map(|l| l.as_bytes().to_vec()).collect(),
        platform: None,
    }
}

fn deployment_with(images: &[(&str, &str)]) -> Deployment {
    let mut d = Deployment::new("demo-stack", Platform::new("linux", "arm64"));
    for (svc, img) in images {
        d.services
            .insert(svc.to_string(), Service::new(*svc, ImageRef::new(*img)));
    }
    d
}

fn writer() -> BundleWriter {
    BundleWriter::new(
        AppName::parse("demo-stack").unwrap(),
        ReleaseId(44),
        Platform::new("linux", "arm64"),
    )
    .unwrap()
}

#[test]
fn bundle_round_trips_with_contents_intact() {
    let archive = make_docker_archive(&[image("app:v44", &["layer-a", "layer-b"])]);
    let mut w = writer()
        .created_at("2026-08-22T13:45:00Z")
        .builder("via/test");
    let added = w
        .add_docker_archive(&archive[..], |name| name.map(str::to_string))
        .unwrap();
    assert_eq!(added, 1);

    let deployment = w.digest_deployment(&deployment_with(&[("app", "app:v44")]));
    assert!(
        deployment.services["app"].image.digest.is_some(),
        "the manifest should identify images by digest, not just by tag"
    );

    let bytes = w
        .finish(
            Vec::new(),
            &deployment,
            Some("services:\n  app:\n    image: app:v44\n"),
        )
        .unwrap();

    let dir = tempfile::TempDir::new().unwrap();
    let unpacked = UnpackedBundle::unpack(&bytes[..], dir.path()).unwrap();

    assert_eq!(unpacked.manifest.app.as_str(), "demo-stack");
    assert_eq!(unpacked.manifest.release, ReleaseId(44));
    assert_eq!(
        unpacked.manifest.created_at.as_deref(),
        Some("2026-08-22T13:45:00Z")
    );
    assert_eq!(unpacked.manifest.images.len(), 1);
    assert_eq!(unpacked.deployment.services.len(), 1);
    assert!(
        unpacked
            .compose_source
            .as_deref()
            .unwrap()
            .contains("app:v44")
    );
    unpacked
        .verify()
        .expect("blobs on disk should still verify");
}

#[test]
fn manifest_can_be_read_without_unpacking_the_blobs() {
    let archive = make_docker_archive(&[image("app:v44", &["a"])]);
    let mut w = writer();
    w.add_docker_archive(&archive[..], |n| n.map(str::to_string))
        .unwrap();
    let bytes = w
        .finish(Vec::new(), &deployment_with(&[("app", "app:v44")]), None)
        .unwrap();

    let manifest = peek_manifest(&bytes[..]).unwrap();
    assert_eq!(manifest.release, ReleaseId(44));
    assert!(manifest.blob_bytes() > 0);
}

#[test]
fn shared_layers_are_stored_once() {
    // Two images on the same base: the base layer must not be paid for twice.
    let archive = make_docker_archive(&[
        image("app:v44", &["shared-base", "app-specific"]),
        image("worker:v44", &["shared-base", "worker-specific"]),
    ]);
    let mut w = writer();
    w.add_docker_archive(&archive[..], |n| n.map(str::to_string))
        .unwrap();
    let deployment = deployment_with(&[("app", "app:v44"), ("worker", "worker:v44")]);
    let bytes = w.finish(Vec::new(), &deployment, None).unwrap();

    let manifest = peek_manifest(&bytes[..]).unwrap();
    let shared = Digest::sha256_of(b"shared-base");
    assert_eq!(
        manifest.blobs.iter().filter(|b| b.digest == shared).count(),
        1,
        "the shared base layer should appear exactly once in the blob list"
    );
    assert_eq!(manifest.images.len(), 2);
}

#[test]
fn missing_blobs_computes_the_transfer_plan() {
    let archive = make_docker_archive(&[image("app:v44", &["a", "b", "c"])]);
    let mut w = writer();
    w.add_docker_archive(&archive[..], |n| n.map(str::to_string))
        .unwrap();
    let bytes = w
        .finish(Vec::new(), &deployment_with(&[("app", "app:v44")]), None)
        .unwrap();
    let manifest = peek_manifest(&bytes[..]).unwrap();

    let all = manifest.blobs.len();
    let have: Vec<Digest> = vec![Digest::sha256_of(b"a"), Digest::sha256_of(b"b")];
    assert_eq!(
        manifest.missing_blobs(&have).len(),
        all - 2,
        "a device that already holds two layers should only be sent the rest"
    );
    assert_eq!(manifest.missing_blobs(&[]).len(), all);
}

#[test]
fn a_deployment_referencing_an_absent_image_is_refused_at_build_time() {
    // Better to fail on the developer's machine than to ship a bundle that
    // would need a registry on a device that has no credentials for one.
    let archive = make_docker_archive(&[image("app:v44", &["a"])]);
    let mut w = writer();
    w.add_docker_archive(&archive[..], |n| n.map(str::to_string))
        .unwrap();

    let deployment = deployment_with(&[("app", "app:v44"), ("db", "postgres:17")]);
    let err = w.finish(Vec::new(), &deployment, None).unwrap_err();
    let msg = err.to_string();
    assert!(matches!(err, Error::MissingImages { .. }), "{msg}");
    assert!(msg.contains("postgres:17"), "{msg}");
    assert!(
        msg.contains("self-contained"),
        "the message should explain why: {msg}"
    );
}

#[test]
fn a_corrupted_blob_is_detected_before_anything_reaches_docker() {
    let archive = make_docker_archive(&[image("app:v44", &["payload-to-corrupt"])]);
    let mut w = writer();
    w.add_docker_archive(&archive[..], |n| n.map(str::to_string))
        .unwrap();
    let bytes = w
        .finish(Vec::new(), &deployment_with(&[("app", "app:v44")]), None)
        .unwrap();

    // Flip a byte inside the compressed stream and reassemble a valid archive
    // with one wrong blob body.
    let dir = tempfile::TempDir::new().unwrap();
    let unpacked = UnpackedBundle::unpack(&bytes[..], dir.path()).unwrap();
    let target = unpacked
        .blob_dir
        .join(Digest::sha256_of(b"payload-to-corrupt").to_path_segment());
    let mut f = std::fs::File::create(&target).unwrap();
    f.write_all(b"tampered").unwrap();
    drop(f);

    let err = unpacked.verify().unwrap_err();
    assert!(matches!(err, Error::BlobDigestMismatch { .. }), "{err}");
}

#[test]
fn a_truncated_bundle_is_rejected_rather_than_partially_applied() {
    let archive = make_docker_archive(&[image("app:v44", &["a", "b", "c"])]);
    let mut w = writer();
    w.add_docker_archive(&archive[..], |n| n.map(str::to_string))
        .unwrap();
    let bytes = w
        .finish(Vec::new(), &deployment_with(&[("app", "app:v44")]), None)
        .unwrap();

    let dir = tempfile::TempDir::new().unwrap();
    let truncated = &bytes[..bytes.len() / 2];
    assert!(
        UnpackedBundle::unpack(truncated, dir.path()).is_err(),
        "half a bundle must never look like a valid release"
    );
}

#[test]
fn only_the_target_platform_is_carried() {
    // Shipping an amd64 image to an arm64 device wastes the transfer and would
    // fail at run time rather than build time.
    let archive = make_docker_archive(&[
        FakeImage {
            reference: "app:v44".into(),
            layer_bodies: vec![b"arm-layer".to_vec()],
            platform: Some(("linux", "arm64")),
        },
        FakeImage {
            reference: "app-amd:v44".into(),
            layer_bodies: vec![b"amd-layer".to_vec()],
            platform: Some(("linux", "amd64")),
        },
    ]);
    let mut w = writer();
    w.add_docker_archive(&archive[..], |n| n.map(str::to_string))
        .unwrap();
    let bytes = w
        .finish(Vec::new(), &deployment_with(&[("app", "app:v44")]), None)
        .unwrap();
    let manifest = peek_manifest(&bytes[..]).unwrap();

    let amd = Digest::sha256_of(b"amd-layer");
    assert!(
        !manifest.blobs.iter().any(|b| b.digest == amd),
        "an amd64 layer must not travel to an arm64 device"
    );
}

#[test]
fn images_can_be_retagged_on_the_way_into_the_bundle() {
    // The tag Docker built under is rarely the tag the deployment refers to.
    let archive = make_docker_archive(&[image("localhost/scratch-build:tmp", &["a"])]);
    let mut w = writer();
    w.add_docker_archive(&archive[..], |_| Some("demo-stack/app:44".to_string()))
        .unwrap();
    let bytes = w
        .finish(
            Vec::new(),
            &deployment_with(&[("app", "demo-stack/app:44")]),
            None,
        )
        .unwrap();
    let manifest = peek_manifest(&bytes[..]).unwrap();
    assert_eq!(manifest.images[0].reference, "demo-stack/app:44");
}

#[test]
fn a_docker_load_archive_can_be_regenerated_from_the_bundle() {
    let archive = make_docker_archive(&[image("app:v44", &["a", "b"])]);
    let mut w = writer();
    w.add_docker_archive(&archive[..], |n| n.map(str::to_string))
        .unwrap();
    let bytes = w
        .finish(Vec::new(), &deployment_with(&[("app", "app:v44")]), None)
        .unwrap();

    let dir = tempfile::TempDir::new().unwrap();
    let unpacked = UnpackedBundle::unpack(&bytes[..], dir.path()).unwrap();
    let load_tar = unpacked.write_docker_load_archive(Vec::new()).unwrap();

    let mut names = Vec::new();
    let mut ar = tar::Archive::new(&load_tar[..]);
    for e in ar.entries().unwrap() {
        names.push(e.unwrap().path().unwrap().to_string_lossy().into_owned());
    }
    // Docker's classic image store needs all three; a pure OCI layout is
    // rejected outright.
    assert!(names.iter().any(|n| n == "oci-layout"), "{names:?}");
    assert!(names.iter().any(|n| n == "index.json"), "{names:?}");
    assert!(names.iter().any(|n| n == "manifest.json"), "{names:?}");
    assert!(
        names.iter().any(|n| n.starts_with("blobs/sha256/")),
        "{names:?}"
    );
}
