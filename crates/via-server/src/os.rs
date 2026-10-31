//! OS versions available to devices: `<os_dir>/<arch>/<version>/`, each the
//! output of `nix build .#updates.<arch>`.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize)]
pub struct Release {
    pub arch: String,
    pub version: String,
    #[serde(skip)]
    pub dir: PathBuf,
    pub files: Vec<String>,
    pub size_mb: u64,
}

#[derive(Deserialize)]
struct Manifest {
    version: String,
    arch: String,
    files: Vec<ManifestFile>,
}

#[derive(Deserialize)]
struct ManifestFile {
    name: String,
    size: u64,
}

fn safe(part: &str) -> bool {
    !part.is_empty() && part.chars().all(|c| c.is_ascii_alphanumeric() || "._-".contains(c)) && !part.starts_with('.')
}

pub fn find(os_dir: &Path, arch: &str, version: &str) -> Result<Release> {
    if !safe(arch) || !safe(version) {
        bail!("bad arch or version");
    }
    let dir = os_dir.join(arch).join(version);
    let text = std::fs::read_to_string(dir.join("manifest.json"))
        .with_context(|| format!("no OS version {version} for {arch}; `list_os_versions` shows them"))?;
    let m: Manifest = serde_json::from_str(&text).context("reading manifest.json")?;
    if m.version != version || m.arch != arch {
        bail!("{} holds {} for {}, not {version} for {arch}", dir.display(), m.version, m.arch);
    }
    Ok(Release {
        arch: m.arch,
        version: m.version,
        size_mb: m.files.iter().map(|f| f.size).sum::<u64>() / 1_048_576,
        files: m.files.into_iter().map(|f| f.name).collect(),
        dir,
    })
}

/// Where an uploaded file of a not-yet-published version goes. A version
/// with a manifest is published, and published versions do not change.
pub fn upload_dir(os_dir: &Path, arch: &str, version: &str, file: &str) -> Result<PathBuf> {
    if !safe(arch) || !safe(version) || !safe(file) {
        bail!("bad arch, version or file name");
    }
    let dir = os_dir.join(arch).join(version);
    if dir.join("manifest.json").exists() {
        bail!("OS version {version} for {arch} is already published; versions are immutable");
    }
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Check a staged manifest against the files uploaded beside it.
pub fn check_complete(dir: &Path, manifest: &Path, arch: &str, version: &str) -> Result<()> {
    let m: Manifest = serde_json::from_slice(&std::fs::read(manifest)?).context("manifest.json is not valid")?;
    if m.version != version || m.arch != arch {
        bail!("manifest is for {} on {}, uploaded as {version} on {arch}", m.version, m.arch);
    }
    for f in &m.files {
        if !safe(&f.name) {
            bail!("manifest names a bad file {:?}", f.name);
        }
        let size = std::fs::metadata(dir.join(&f.name)).map(|m| m.len()).ok();
        if size != Some(f.size) {
            bail!("{} is missing or the wrong size; upload it before the manifest", f.name);
        }
    }
    Ok(())
}

pub fn list(os_dir: &Path) -> Vec<Release> {
    let mut out = Vec::new();
    for arch in read_names(os_dir) {
        for version in read_names(&os_dir.join(&arch)) {
            if let Ok(r) = find(os_dir, &arch, &version) {
                out.push(r);
            }
        }
    }
    out
}

fn read_names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().is_dir())
        .filter_map(|e| e.file_name().into_string().ok())
        .collect();
    names.sort();
    names
}
