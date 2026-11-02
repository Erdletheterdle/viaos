//! App releases: `<data>/releases/<app>/<release>/<arch>.bundle`, uploaded by
//! `via release` and fetched by devices.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Serialize;
use via_core::VariableSpec;

#[derive(Debug, Serialize)]
pub struct Release {
    pub app: String,
    pub release: u64,
    pub artifacts: Vec<Artifact>,
}

#[derive(Debug, Serialize)]
pub struct Artifact {
    pub arch: String,
    pub sha256: String,
    pub size_mb: u64,
}

pub fn valid_app(app: &str) -> bool {
    !app.is_empty()
        && app.len() <= 63
        && app
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

pub fn bundle_path(root: &Path, app: &str, release: u64, arch: &str) -> Result<PathBuf> {
    if !valid_app(app) || !crate::image::ARCHES.contains(&arch) {
        bail!("bad app or arch");
    }
    Ok(root
        .join(app)
        .join(release.to_string())
        .join(format!("{arch}.bundle")))
}

pub fn artifact(root: &Path, app: &str, release: u64, arch: &str) -> Result<Artifact> {
    let path = bundle_path(root, app, release, arch)?;
    let sha256 = std::fs::read_to_string(path.with_extension("sha256")).map_err(|_| {
        anyhow::anyhow!("no {arch} build of {app} release {release}; `list_releases` shows them")
    })?;
    Ok(Artifact {
        arch: arch.into(),
        sha256: sha256.trim().into(),
        size_mb: std::fs::metadata(&path)?.len() / 1_048_576,
    })
}

fn schema_path(root: &Path, app: &str, release: u64) -> PathBuf {
    root.join(app)
        .join(release.to_string())
        .join("variables.json")
}

/// Read a release's variables out of an uploaded bundle and keep them beside
/// it. This reads the normalized `deployment.json` the build wrote and nothing
/// else: the server neither parses compose nor runs anything from a bundle.
pub fn record_schema(
    root: &Path,
    app: &str,
    release: u64,
    bundle: &Path,
) -> Result<Vec<VariableSpec>> {
    let file = std::fs::File::open(bundle).with_context(|| format!("{}", bundle.display()))?;
    let schema = via_bundle::peek_deployment(file)?.variable_schema();
    let path = schema_path(root, app, release);
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, serde_json::to_vec(&schema)?)?;
    std::fs::rename(tmp, path)?;
    Ok(schema)
}

/// What a release says about its variables. One uploaded before this was kept
/// is read from its bundle the first time it is asked about.
pub fn schema(root: &Path, app: &str, release: u64) -> Vec<VariableSpec> {
    if let Some(schema) = std::fs::read(schema_path(root, app, release))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
    {
        return schema;
    }
    for arch in crate::image::ARCHES {
        let Ok(bundle) = bundle_path(root, app, release, arch) else {
            continue;
        };
        if !bundle.is_file() {
            continue;
        }
        match record_schema(root, app, release, &bundle) {
            Ok(schema) => return schema,
            Err(e) => tracing::warn!(app, release, arch, "reading its variables failed: {e:#}"),
        }
    }
    Vec::new()
}

/// Each app's newest release and what it says about its variables.
pub fn newest_schemas(root: &Path, app: Option<&str>) -> Vec<(String, u64, Vec<VariableSpec>)> {
    let mut seen = std::collections::BTreeSet::new();
    list(root, app)
        .into_iter()
        .filter(|r| seen.insert(r.app.clone()))
        .map(|r| {
            let schema = schema(root, &r.app, r.release);
            (r.app, r.release, schema)
        })
        .collect()
}

/// What a release is: its builds and, from its normalized `deployment.json`,
/// the services it runs. Inline environment values aren't included: a compose
/// file may have baked in something that shouldn't be on a page.
pub fn details(root: &Path, app: &str, release: u64) -> Result<serde_json::Value> {
    use serde_json::json;
    let artifacts: Vec<Artifact> = crate::image::ARCHES
        .iter()
        .filter_map(|a| artifact(root, app, release, a).ok())
        .collect();
    let first = artifacts
        .first()
        .with_context(|| format!("no release {release} of {app}; `list_releases` shows them"))?;
    let bundle = bundle_path(root, app, release, &first.arch)?;
    let uploaded_at = std::fs::metadata(&bundle)?
        .modified()?
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs();
    let d = via_bundle::peek_deployment(std::fs::File::open(&bundle)?)?;
    let variables = schema(root, app, release);
    let services: Vec<serde_json::Value> = d
        .services
        .values()
        .map(|s| {
            json!({
                "name": s.name,
                "image": s.image,
                "ports": s.ports,
                "depends_on": s.depends_on.iter().map(|x| &x.service).collect::<Vec<_>>(),
                "healthcheck": s.healthcheck.is_some(),
                "restart": s.restart,
                "mounts": s.mounts,
                "privileged": s.privileged,
                "network_mode": s.network_mode,
                // By the release's own variables, which cover names with defaults too.
                "takes": variables
                    .iter()
                    .filter(|v| v.services.contains(&s.name.to_string()))
                    .map(|v| &v.key)
                    .collect::<Vec<_>>(),
            })
        })
        .collect();
    Ok(json!({
        "app": app,
        "release": release,
        "uploaded_at": uploaded_at,
        "artifacts": artifacts,
        "services": services,
        "volumes": d.volumes.keys().collect::<Vec<_>>(),
        "networks": d.networks.keys().collect::<Vec<_>>(),
        "health_gate": d.health_gate,
        "variables": variables,
    }))
}

/// Everything in one build of a release but its layer bytes: the compose file
/// and normalized manifest as written, and each image's config. Reading it
/// means decompressing the bundle, so the answer is kept beside it. Bundles are
/// immutable, and anything that can fetch a release can already read all this.
pub fn contents(
    root: &Path,
    app: &str,
    release: u64,
    arch: Option<&str>,
) -> Result<serde_json::Value> {
    use serde_json::json;
    let builds: Vec<&str> = crate::image::ARCHES
        .iter()
        .copied()
        .filter(|a| bundle_path(root, app, release, a).is_ok_and(|p| p.is_file()))
        .collect();
    let arch = arch
        .or(builds.first().copied())
        .with_context(|| format!("no release {release} of {app}; `list_releases` shows them"))?;
    anyhow::ensure!(
        builds.contains(&arch),
        "no {arch} build of {app} release {release}"
    );
    let bundle = bundle_path(root, app, release, arch)?;
    let kept = bundle.with_extension("contents.json");
    let c: via_bundle::BundleContents = match std::fs::read(&kept)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
    {
        Some(c) => c,
        None => {
            let c = via_bundle::read_contents(std::fs::File::open(&bundle)?)?;
            let tmp = kept.with_extension("tmp");
            std::fs::write(&tmp, serde_json::to_vec(&c)?)?;
            std::fs::rename(tmp, &kept)?;
            c
        }
    };
    let pretty = |text: &str| {
        serde_json::from_str::<serde_json::Value>(text)
            .ok()
            .and_then(|v| serde_json::to_string_pretty(&v).ok())
            .unwrap_or_else(|| text.to_string())
    };
    let size = |d: &via_core::Digest| c.manifest.blob(d).map_or(0, |b| b.size);
    let images: Vec<serde_json::Value> = c
        .manifest
        .images
        .iter()
        .map(|i| {
            let config = c.image_blobs.get(&i.config_digest.to_string());
            let parsed: serde_json::Value = config
                .and_then(|t| serde_json::from_str(t).ok())
                .unwrap_or_default();
            json!({
                "reference": i.reference,
                "digest": i.manifest_digest,
                "media_type": i.media_type,
                "size": i.blob_digests().iter().map(size).sum::<u64>(),
                "layers": i.layers.iter().map(|d| json!({ "digest": d, "size": size(d) })).collect::<Vec<_>>(),
                "created": parsed["created"],
                "architecture": parsed["architecture"],
                "os": parsed["os"],
                "config": parsed["config"],
                "config_text": config.map(|t| pretty(t)),
                "manifest_text": c.image_blobs.get(&i.manifest_digest.to_string()).map(|t| pretty(t)),
            })
        })
        .collect();
    // What each archive entry is, for a reader who sees only digests.
    let role = |path: &str| -> String {
        if path == via_bundle::MANIFEST_PATH {
            return "bundle manifest".into();
        }
        if path == via_bundle::DEPLOYMENT_PATH {
            return "normalized deployment".into();
        }
        if path == via_bundle::COMPOSE_PATH {
            return "compose file".into();
        }
        if path == via_bundle::SIGNATURE_PATH {
            return "signature".into();
        }
        let digest = path
            .strip_prefix(via_bundle::BLOB_PREFIX)
            .map(|b| b.replacen('-', ":", 1))
            .unwrap_or_default();
        let mut roles: Vec<String> = c
            .manifest
            .images
            .iter()
            .filter_map(|i| {
                if i.manifest_digest.to_string() == digest {
                    Some(format!("{} image manifest", i.reference))
                } else if i.config_digest.to_string() == digest {
                    Some(format!("{} config", i.reference))
                } else if i.layers.iter().any(|l| l.to_string() == digest) {
                    Some(format!("{} layer", i.reference))
                } else {
                    None
                }
            })
            .collect();
        roles.dedup();
        roles.join(", ")
    };
    let files: Vec<serde_json::Value> = c
        .files
        .iter()
        .map(|f| json!({ "path": f.path, "size": f.size, "role": role(&f.path) }))
        .collect();
    Ok(json!({
        "app": app,
        "release": release,
        "arch": arch,
        "builds": builds,
        "created_at": c.manifest.created_at,
        "builder": c.manifest.builder,
        "compose": c.compose,
        "deployment": pretty(&c.deployment),
        "bundle_manifest": serde_json::to_string_pretty(&c.manifest)?,
        "images": images,
        "files": files,
    }))
}

/// Newest release first.
pub fn list(root: &Path, app: Option<&str>) -> Vec<Release> {
    let mut out = Vec::new();
    for app_dir in std::fs::read_dir(root).into_iter().flatten().flatten() {
        let Ok(name) = app_dir.file_name().into_string() else {
            continue;
        };
        if app.is_some_and(|a| a != name) {
            continue;
        }
        for rel in std::fs::read_dir(app_dir.path())
            .into_iter()
            .flatten()
            .flatten()
        {
            let Some(n) = rel.file_name().to_str().and_then(|s| s.parse::<u64>().ok()) else {
                continue;
            };
            let artifacts: Vec<Artifact> = crate::image::ARCHES
                .iter()
                .filter_map(|a| artifact(root, &name, n, a).ok())
                .collect();
            if !artifacts.is_empty() {
                out.push(Release {
                    app: name.clone(),
                    release: n,
                    artifacts,
                });
            }
        }
    }
    out.sort_by(|a, b| a.app.cmp(&b.app).then(b.release.cmp(&a.release)));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use via_bundle::{BUNDLE_SCHEMA_VERSION, BundleManifest, DEPLOYMENT_PATH, MANIFEST_PATH};
    use via_core::{AppName, Deployment, Digest, ImageRef, Platform, ReleaseId, Service};

    fn entry(tar: &mut tar::Builder<impl std::io::Write>, name: &str, bytes: &[u8]) {
        let mut h = tar::Header::new_gnu();
        h.set_size(bytes.len() as u64);
        h.set_mode(0o644);
        h.set_cksum();
        tar.append_data(&mut h, name, bytes).unwrap();
    }

    /// Put a bundle where an upload would, its front being all this reads:
    /// `web` declares a bare `- TOKEN`.
    fn upload(root: &Path, release: u64, edit: impl FnOnce(&mut Deployment)) -> PathBuf {
        let mut d = Deployment::new("hello", Platform::new("linux", "amd64"));
        let mut web = Service::new("web", ImageRef::new("web:1"));
        web.runtime_env = vec!["TOKEN".into()];
        d.services.insert("web".into(), web);
        edit(&mut d);
        let deployment = serde_json::to_vec(&d).unwrap();
        let manifest = BundleManifest {
            schema_version: BUNDLE_SCHEMA_VERSION,
            app: AppName::parse("hello").unwrap(),
            release: ReleaseId(release),
            platform: Platform::new("linux", "amd64"),
            created_at: None,
            builder: None,
            images: vec![],
            blobs: vec![],
            deployment_digest: Digest::sha256_of(&deployment),
            compose_digest: None,
            project_files: vec![],
        };
        let path = bundle_path(root, "hello", release, "x86_64").unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let encoder = zstd::Encoder::new(std::fs::File::create(&path).unwrap(), 1).unwrap();
        let mut tar = tar::Builder::new(encoder);
        entry(
            &mut tar,
            MANIFEST_PATH,
            &serde_json::to_vec(&manifest).unwrap(),
        );
        entry(&mut tar, DEPLOYMENT_PATH, &deployment);
        tar.into_inner().unwrap().finish().unwrap();
        std::fs::write(path.with_extension("sha256"), "abc").unwrap();
        path
    }

    #[test]
    fn contents_show_what_a_release_is_and_are_kept_beside_it() {
        let dir = tempfile::tempdir().unwrap();
        let bundle = upload(dir.path(), 1, |_| {});
        let v = contents(dir.path(), "hello", 1, None).unwrap();
        assert_eq!(v["arch"], "x86_64");
        assert!(v["deployment"].as_str().unwrap().contains("\"web\""));
        assert_eq!(v["files"][0]["role"], "bundle manifest");
        assert_eq!(v["files"][1]["role"], "normalized deployment");
        assert!(v["compose"].is_null());
        // Kept: asking again doesn't need the bundle.
        std::fs::write(&bundle, b"gone").unwrap();
        assert_eq!(
            contents(dir.path(), "hello", 1, None).unwrap()["arch"],
            "x86_64"
        );
        assert!(contents(dir.path(), "hello", 1, Some("aarch64")).is_err());
        assert!(contents(dir.path(), "hello", 2, None).is_err());
    }

    #[test]
    fn a_bundle_with_no_description_still_says_what_it_takes() {
        let dir = tempfile::tempdir().unwrap();
        upload(dir.path(), 1, |_| {});
        let schema = schema(dir.path(), "hello", 1);
        assert_eq!(schema.len(), 1);
        assert_eq!(schema[0].key, "TOKEN");
        assert_eq!(schema[0].services, ["web"]);
        assert!(schema[0].required);
        assert_eq!(schema[0].default, None);
    }

    #[test]
    fn only_each_apps_newest_release_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        upload(dir.path(), 1, |_| {});
        let bundle = upload(dir.path(), 2, |d| {
            d.variables.push(VariableSpec {
                key: "LOG_LEVEL".into(),
                services: vec!["web".into()],
                description: Some("How much to say".into()),
                default: Some("info".into()),
                secret: false,
                required: false,
            })
        });
        let newest = newest_schemas(dir.path(), None);
        assert_eq!(newest.len(), 1);
        let (app, release, schema) = &newest[0];
        assert_eq!((app.as_str(), *release), ("hello", 2));
        let keys: Vec<&str> = schema.iter().map(|v| v.key.as_str()).collect();
        assert_eq!(keys, ["LOG_LEVEL", "TOKEN"]);

        // Kept beside the bundle, so later reads need not open it.
        std::fs::remove_file(bundle).unwrap();
        assert_eq!(schema, &self::schema(dir.path(), "hello", 2));
    }

    #[test]
    fn the_value_of_a_secret_is_not_kept_whatever_the_bundle_says() {
        let dir = tempfile::tempdir().unwrap();
        upload(dir.path(), 1, |d| {
            d.variables.push(VariableSpec {
                key: "TOKEN".into(),
                services: vec!["web".into()],
                description: None,
                default: Some("hand-made".into()),
                secret: true,
                required: true,
            })
        });
        let schema = schema(dir.path(), "hello", 1);
        assert_eq!(schema[0].default, None);
        let kept = std::fs::read_to_string(schema_path(dir.path(), "hello", 1)).unwrap();
        assert!(!kept.contains("hand-made"));
    }

    #[test]
    fn details_describe_the_services_without_their_inline_values() {
        let dir = tempfile::tempdir().unwrap();
        upload(dir.path(), 3, |d| {
            let web = d.services.get_mut("web").unwrap();
            web.environment.insert("BAKED".into(), "hunter2".into());
        });
        let v = details(dir.path(), "hello", 3).unwrap();
        assert_eq!(v["services"][0]["name"], "web");
        assert_eq!(v["services"][0]["takes"][0], "TOKEN");
        assert_eq!(v["artifacts"][0]["arch"], "x86_64");
        assert!(!v.to_string().contains("hunter2"));
        assert!(details(dir.path(), "hello", 9).is_err());
    }

    #[test]
    fn a_bundle_that_cannot_be_read_has_no_schema() {
        let dir = tempfile::tempdir().unwrap();
        let path = bundle_path(dir.path(), "hello", 1, "x86_64").unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, b"not a bundle").unwrap();
        assert!(schema(dir.path(), "hello", 1).is_empty());
    }
}
