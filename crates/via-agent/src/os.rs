//! OS updates: fetch a version's files, check them, hand them to
//! systemd-sysupdate, reboot.
//!
//! The agent does the fetching so updates travel the same path as everything
//! else and need no update URL baked into the image. sysupdate does the
//! writing: it fills the idle store slot, installs the UKI with boot
//! counting, and leaves the running slot alone. If the new version does not
//! reach boot-complete.target three times, the boot loader goes back to the
//! old one and the device reports its old version again.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

/// Where sysupdate's transfers look for sources; see nix/sysupdate.nix.
const SOURCE_DIR: &str = "os-update";

#[derive(Debug, Deserialize)]
pub struct Manifest {
    pub version: String,
    pub arch: String,
    pub files: Vec<ManifestFile>,
}

#[derive(Debug, Deserialize)]
pub struct ManifestFile {
    pub name: String,
    pub sha256: String,
    pub size: u64,
}

#[derive(Debug, Clone, Serialize, Default)]
pub struct UpdateStatus {
    pub version: String,
    pub state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub progress_percent: Option<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// The update in flight, if any. Shared by every MCP session on the device.
#[derive(Clone, Default)]
pub struct Updater {
    status: Arc<Mutex<Option<UpdateStatus>>>,
}

impl Updater {
    pub fn status(&self) -> Option<UpdateStatus> {
        self.status.lock().unwrap().clone()
    }

    fn set(&self, version: &str, state: &str, progress: Option<u8>, error: Option<String>) {
        *self.status.lock().unwrap() = Some(UpdateStatus {
            version: version.into(),
            state: state.into(),
            progress_percent: progress,
            error,
        });
    }

    /// Start updating to `version` from `source` (a URL or directory holding
    /// manifest.json and the files it names). Returns at once; progress shows
    /// in [`Updater::status`].
    pub fn start(
        &self,
        data_dir: PathBuf,
        source: String,
        version: String,
        bearer: Option<String>,
        reboot: bool,
    ) -> Result<()> {
        if let Some(s) = self.status() {
            if !matches!(s.state.as_str(), "failed" | "installed") {
                bail!("an update to {} is already {}", s.version, s.state);
            }
        }
        if current_version().as_deref() == Some(version.as_str()) {
            bail!("already running version {version}");
        }
        self.set(&version, "downloading", Some(0), None);
        let this = self.clone();
        tokio::spawn(async move {
            match this.run(&data_dir, &source, &version, bearer.as_deref()).await {
                Ok(()) => {
                    if reboot {
                        this.set(&version, "rebooting", None, None);
                        tracing::info!(%version, "installed; rebooting");
                        // Long enough for the caller to read the status.
                        tokio::time::sleep(Duration::from_secs(5)).await;
                        let _ = tokio::process::Command::new("systemctl").arg("reboot").status().await;
                    } else {
                        this.set(&version, "installed", None, None);
                    }
                }
                Err(e) => {
                    tracing::error!(%version, "update failed: {e:#}");
                    this.set(&version, "failed", None, Some(format!("{e:#}")));
                }
            }
        });
        Ok(())
    }

    async fn run(&self, data_dir: &Path, source: &str, version: &str, bearer: Option<&str>) -> Result<()> {
        let dir = data_dir.join(SOURCE_DIR);
        // Only this version's files: sysupdate installs the newest it sees.
        let _ = tokio::fs::remove_dir_all(&dir).await;
        tokio::fs::create_dir_all(&dir).await?;

        let fetch = Fetch::new(source, bearer)?;
        let manifest: Manifest = serde_json::from_slice(&fetch.bytes("manifest.json").await?)
            .context("reading manifest.json")?;
        if manifest.version != version {
            bail!("source holds version {}, not {version}", manifest.version);
        }
        if manifest.arch != std::env::consts::ARCH {
            bail!("update is for {}, this device is {}", manifest.arch, std::env::consts::ARCH);
        }

        let total: u64 = manifest.files.iter().map(|f| f.size).sum::<u64>().max(1);
        let mut done = 0u64;
        for file in &manifest.files {
            if file.name.contains('/') || file.name.starts_with('.') {
                bail!("refusing file name {:?}", file.name);
            }
            let path = dir.join(&file.name);
            let digest = fetch
                .to_file(&file.name, &path, |n| {
                    let pct = ((done + n) * 100 / total).min(99) as u8;
                    self.set(version, "downloading", Some(pct), None);
                })
                .await
                .with_context(|| format!("fetching {}", file.name))?;
            if digest != file.sha256 {
                bail!("{} is corrupt: sha256 {digest}, expected {}", file.name, file.sha256);
            }
            done += file.size;
        }

        self.set(version, "installing", None, None);
        // Not on PATH on NixOS; the unit says where it is.
        let sysupdate = std::env::var("VIA_SYSUPDATE").unwrap_or_else(|_| "systemd-sysupdate".into());
        let out = tokio::process::Command::new(sysupdate)
            .args(["update", version])
            .output()
            .await
            .context("running systemd-sysupdate")?;
        if !out.status.success() {
            bail!("systemd-sysupdate failed: {}", String::from_utf8_lossy(&out.stderr).trim());
        }
        let _ = tokio::fs::remove_dir_all(&dir).await;
        Ok(())
    }
}

/// The running OS version, from os-release.
pub fn current_version() -> Option<String> {
    let text = std::fs::read_to_string("/etc/os-release").ok()?;
    text.lines()
        .find_map(|l| l.strip_prefix("IMAGE_VERSION="))
        .map(|v| v.trim_matches('"').to_string())
}

/// Reads an update source: an http(s) URL or a local directory.
enum Fetch {
    Http { base: String, client: reqwest::Client, bearer: Option<String> },
    Dir(PathBuf),
}

impl Fetch {
    fn new(source: &str, bearer: Option<&str>) -> Result<Self> {
        if source.starts_with("http://") || source.starts_with("https://") {
            Ok(Self::Http {
                base: source.trim_end_matches('/').into(),
                client: reqwest::Client::new(),
                bearer: bearer.map(Into::into),
            })
        } else {
            Ok(Self::Dir(PathBuf::from(source)))
        }
    }

    async fn get(&self, name: &str) -> Result<reqwest::Response> {
        let Self::Http { base, client, bearer } = self else { unreachable!() };
        let mut req = client.get(format!("{base}/{name}"));
        if let Some(b) = bearer {
            req = req.bearer_auth(b);
        }
        let resp = req.send().await?;
        if !resp.status().is_success() {
            bail!("{name}: {}", resp.status());
        }
        Ok(resp)
    }

    async fn bytes(&self, name: &str) -> Result<Vec<u8>> {
        match self {
            Self::Http { .. } => Ok(self.get(name).await?.bytes().await?.to_vec()),
            Self::Dir(dir) => Ok(tokio::fs::read(dir.join(name)).await?),
        }
    }

    /// Copy `name` to `path`, returning its sha256 in hex.
    async fn to_file(&self, name: &str, path: &Path, mut progress: impl FnMut(u64)) -> Result<String> {
        let mut hash = Sha256::new();
        let mut out = tokio::fs::File::create(path).await?;
        let mut written = 0u64;
        match self {
            Self::Http { .. } => {
                let mut resp = self.get(name).await?;
                while let Some(chunk) = resp.chunk().await? {
                    hash.update(&chunk);
                    out.write_all(&chunk).await?;
                    written += chunk.len() as u64;
                    progress(written);
                }
            }
            Self::Dir(dir) => {
                use tokio::io::AsyncReadExt;
                let mut src = tokio::fs::File::open(dir.join(name)).await?;
                let mut buf = vec![0u8; 1 << 20];
                loop {
                    let n = src.read(&mut buf).await?;
                    if n == 0 {
                        break;
                    }
                    hash.update(&buf[..n]);
                    out.write_all(&buf[..n]).await?;
                    written += n as u64;
                    progress(written);
                }
            }
        }
        out.sync_all().await?;
        Ok(hex::encode(hash.finalize()))
    }
}
