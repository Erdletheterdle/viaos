//! App releases: `<data>/releases/<app>/<release>/<arch>.bundle`, uploaded by
//! `via release` and fetched by devices.

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use serde::Serialize;

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
