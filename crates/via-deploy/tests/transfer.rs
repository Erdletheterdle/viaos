//! The handoff from a resumed transfer into a staged, verified release.
//!
//! The unit tests cover reception with synthetic bytes. This covers the seam:
//! a real signed bundle, transferred in chunks across a simulated restart,
//! then unpacked and trust-checked as a release.

use std::collections::BTreeMap;

use via_bundle::{BundleWriter, PublicKey, SigningKey, TrustPolicy, UnpackedBundle};
use via_core::{AppName, Deployment, Digest, ImageRef, Platform, ReleaseId, Service};
use via_deploy::{Error, Expectation, Reception, Store, digest_file};

/// Build a real bundle around a synthetic image, optionally signed.
fn build_bundle(release: u64, sign: Option<&SigningKey>) -> Vec<u8> {
    // A `docker save`-shaped archive with one small image.
    let layer = format!("layer bytes for release {release}").into_bytes();
    let layer_digest = Digest::sha256_of(&layer);

    let config = serde_json::json!({
        "architecture": "arm64", "os": "linux",
        "rootfs": {"type": "layers", "diff_ids": []},
    });
    let config_bytes = serde_json::to_vec(&config).unwrap();
    let config_digest = Digest::sha256_of(&config_bytes);

    let manifest = serde_json::json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.manifest.v1+json",
        "config": {
            "mediaType": "application/vnd.oci.image.config.v1+json",
            "digest": config_digest.to_string(), "size": config_bytes.len(),
        },
        "layers": [{
            "mediaType": "application/vnd.oci.image.layer.v1.tar",
            "digest": layer_digest.to_string(), "size": layer.len(),
        }],
    });
    let manifest_bytes = serde_json::to_vec(&manifest).unwrap();
    let manifest_digest = Digest::sha256_of(&manifest_bytes);

    let mut tar = tar::Builder::new(Vec::new());
    let mut put = |name: String, data: &[u8]| {
        let mut h = tar::Header::new_gnu();
        h.set_size(data.len() as u64);
        h.set_mode(0o644);
        h.set_mtime(0);
        h.set_cksum();
        tar.append_data(&mut h, name, data).unwrap();
    };
    for (d, b) in [(&layer_digest, &layer), (&config_digest, &config_bytes), (&manifest_digest, &manifest_bytes)] {
        put(format!("blobs/{}/{}", d.algorithm(), d.hex()), b);
    }
    let index = serde_json::json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.index.v1+json",
        "manifests": [{
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "digest": manifest_digest.to_string(), "size": manifest_bytes.len(),
            "annotations": {"io.containerd.image.name": format!("app:{release}")},
        }],
    });
    put("index.json".into(), &serde_json::to_vec(&index).unwrap());
    let archive = tar.into_inner().unwrap();

    let mut writer = BundleWriter::new(
        AppName::parse("demo-stack").unwrap(),
        ReleaseId(release),
        Platform::new("linux", "arm64"),
    )
    .unwrap();
    if let Some(key) = sign {
        writer = writer.sign_with(SigningKey::from_hex(&key.to_hex()).unwrap());
    }
    writer.add_docker_archive(&archive[..], |n| n.map(str::to_string)).unwrap();

    let mut deployment = Deployment::new("demo-stack", Platform::new("linux", "arm64"));
    deployment
        .services
        .insert("app".into(), Service::new("app", ImageRef::new(format!("app:{release}"))));

    writer.finish(Vec::new(), &deployment, None).unwrap()
}

fn expectation(release: u64, bundle: &[u8]) -> Expectation {
    Expectation {
        release: ReleaseId(release),
        size: bundle.len() as u64,
        digest: Digest::sha256_of(bundle),
    }
}

#[test]
fn a_bundle_transferred_in_chunks_across_a_restart_unpacks_correctly() {
    let dir = tempfile::TempDir::new().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let bundle = build_bundle(44, None);
    let expect = expectation(44, &bundle);

    // First leg: send a third of it, then "lose power".
    let reception = Reception::open(store.root()).unwrap();
    reception.begin(&expect).unwrap();
    let first_leg = bundle.len() / 3;
    reception.append(ReleaseId(44), 0, &bundle[..first_leg]).unwrap();
    drop(reception);

    // Restart: a fresh Reception must find the partial transfer and resume.
    let reception = Reception::open(store.root()).unwrap();
    let resumed = reception.in_progress().unwrap();
    assert_eq!(resumed.len(), 1);
    assert_eq!(resumed[0].received, first_leg as u64);
    assert!(!resumed[0].complete());

    let offset = reception.offset(ReleaseId(44));
    reception.append_stream(ReleaseId(44), &bundle[offset as usize..]).unwrap();

    let path = reception.finish(ReleaseId(44)).unwrap();
    assert_eq!(digest_file(&path).unwrap(), expect.digest);

    // The resumed file must be a working bundle, not merely the right length.
    let unpacked =
        UnpackedBundle::unpack(std::fs::File::open(&path).unwrap(), dir.path().join("staged")).unwrap();
    assert_eq!(unpacked.manifest.release, ReleaseId(44));
    assert_eq!(unpacked.deployment.services.len(), 1);
    unpacked.verify().unwrap();
}

#[test]
fn a_resumed_transfer_is_still_subject_to_signature_checks() {
    let dir = tempfile::TempDir::new().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let key = SigningKey::generate();
    let bundle = build_bundle(45, Some(&key));

    let reception = Reception::open(store.root()).unwrap();
    reception.begin(&expectation(45, &bundle)).unwrap();
    reception.append(ReleaseId(45), 0, &bundle[..500]).unwrap();
    let offset = reception.offset(ReleaseId(45));
    reception.append_stream(ReleaseId(45), &bundle[offset as usize..]).unwrap();
    let path = reception.finish(ReleaseId(45)).unwrap();

    // Trusted key: accepted.
    let trusted = TrustPolicy::Require(vec![PublicKey::from_hex(&key.public().to_hex()).unwrap()]);
    let unpacked = UnpackedBundle::unpack_with(
        std::fs::File::open(&path).unwrap(),
        dir.path().join("ok"),
        &trusted,
    )
    .unwrap();
    assert!(unpacked.trust.is_verified());

    // Someone else's key: refused, even though the transfer itself was clean.
    let other = SigningKey::generate();
    let wrong = TrustPolicy::Require(vec![other.public()]);
    let err = UnpackedBundle::unpack_with(
        std::fs::File::open(&path).unwrap(),
        dir.path().join("bad"),
        &wrong,
    )
    .unwrap_err();
    assert!(matches!(err, via_bundle::Error::UntrustedSigner { .. }), "{err}");
}

#[test]
fn a_transfer_truncated_at_a_chunk_boundary_is_caught_not_deployed() {
    // The dangerous case: a truncation that happens to land on a tar entry
    // boundary could look like a valid short archive.
    let dir = tempfile::TempDir::new().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let bundle = build_bundle(46, None);
    let expect = expectation(46, &bundle);

    let reception = Reception::open(store.root()).unwrap();
    reception.begin(&expect).unwrap();
    reception.append(ReleaseId(46), 0, &bundle[..bundle.len() - 100]).unwrap();

    let err = reception.finish(ReleaseId(46)).unwrap_err();
    assert!(matches!(err, Error::TransferIncomplete { .. }), "{err}");
}

#[test]
fn bytes_that_fail_verification_do_not_survive_as_resumable_progress() {
    let dir = tempfile::TempDir::new().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let mut bundle = build_bundle(47, None);
    let expect = expectation(47, &bundle);

    // Corrupt the payload after the digest was computed, as a faulty link
    // or a hostile middlebox would.
    let mid = bundle.len() / 2;
    bundle[mid] ^= 0xff;

    let reception = Reception::open(store.root()).unwrap();
    reception.begin(&expect).unwrap();
    reception.append(ReleaseId(47), 0, &bundle).unwrap();

    let err = reception.finish(ReleaseId(47)).unwrap_err();
    assert!(matches!(err, Error::TransferDigestMismatch { .. }), "{err}");
    assert_eq!(
        reception.offset(ReleaseId(47)),
        0,
        "a corrupt transfer must restart, not resume on top of bad bytes"
    );
    assert!(reception.in_progress().unwrap().is_empty());
}

#[test]
fn many_small_chunks_reassemble_byte_for_byte() {
    // Models a transport delivering whatever the link gives it.
    let dir = tempfile::TempDir::new().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let bundle = build_bundle(48, None);
    let expect = expectation(48, &bundle);

    let reception = Reception::open(store.root()).unwrap();
    reception.begin(&expect).unwrap();

    let mut at = 0u64;
    let mut sizes = [7usize, 1, 4096, 33, 512].iter().cycle();
    while (at as usize) < bundle.len() {
        let n = (*sizes.next().unwrap()).min(bundle.len() - at as usize);
        let p = reception.append(ReleaseId(48), at, &bundle[at as usize..at as usize + n]).unwrap();
        at = p.received;
    }

    let path = reception.finish(ReleaseId(48)).unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), bundle);
}

#[test]
fn transfers_for_different_releases_do_not_interfere() {
    let dir = tempfile::TempDir::new().unwrap();
    let store = Store::open(dir.path()).unwrap();
    let reception = Reception::open(store.root()).unwrap();

    let mut bundles = BTreeMap::new();
    for release in [44u64, 45] {
        let b = build_bundle(release, None);
        reception.begin(&expectation(release, &b)).unwrap();
        reception.append(ReleaseId(release), 0, &b[..200]).unwrap();
        bundles.insert(release, b);
    }

    for (release, bundle) in &bundles {
        let offset = reception.offset(ReleaseId(*release));
        reception.append_stream(ReleaseId(*release), &bundle[offset as usize..]).unwrap();
        let path = reception.finish(ReleaseId(*release)).unwrap();
        assert_eq!(&std::fs::read(&path).unwrap(), bundle);
    }
}
