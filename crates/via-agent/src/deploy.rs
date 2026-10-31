//! App releases: fetch a bundle, check it, and hand it to Via's deployer.
//!
//! The deployer stages the bundle, loads its images into Docker, applies the
//! deployment, and holds it to its health gate. A release that fails the gate
//! is rolled back to the previous one on the device itself, with no network.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, bail};
use serde::Serialize;
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;
use via_bundle::TrustPolicy;
use via_deploy::{Deployer, Store, SystemClock};
use via_runtime::{ApplyContext, DockerRuntime};

#[derive(Debug, Clone, Serialize)]
pub struct DeployStatus {
    pub app: String,
    pub release: u64,
    pub state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rolled_back_to: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// One release to deploy, and how to get and check it.
pub struct Job {
    pub app: String,
    pub release: u64,
    /// URL or file.
    pub source: String,
    pub sha256: String,
    pub bearer: Option<String>,
    pub trust: TrustPolicy,
}

#[derive(Clone, Default)]
pub struct Deploys {
    status: Arc<Mutex<Option<DeployStatus>>>,
}

impl Deploys {
    pub fn status(&self) -> Option<DeployStatus> {
        self.status.lock().unwrap().clone()
    }

    fn set(&self, app: &str, release: u64, state: &str) {
        *self.status.lock().unwrap() = Some(DeployStatus {
            app: app.into(),
            release,
            state: state.into(),
            rolled_back_to: None,
            error: None,
        });
    }

    fn finish(&self, f: impl FnOnce(&mut DeployStatus)) {
        if let Some(s) = self.status.lock().unwrap().as_mut() {
            f(s);
        }
    }

    /// Start deploying `job`. Returns at once.
    pub fn start(&self, data_dir: PathBuf, job: Job) -> Result<()> {
        if let Some(s) = self.status() {
            if matches!(s.state.as_str(), "downloading" | "deploying") {
                bail!("{} release {} is already {}", s.app, s.release, s.state);
            }
        }
        self.set(&job.app, job.release, "downloading");
        let this = self.clone();
        tokio::spawn(async move {
            let (app, release) = (job.app.clone(), job.release);
            let result = this.run(&data_dir, job).await;
            match result {
                Ok(rolled_back_to) => this.finish(|s| match rolled_back_to {
                    Some(prev) => {
                        s.state = "rolled_back".into();
                        s.rolled_back_to = Some(prev);
                        s.error = Some("the release failed its health gate".into());
                    }
                    None => s.state = "running".into(),
                }),
                Err(e) => {
                    tracing::error!(%app, release, "deploy failed: {e:#}");
                    this.finish(|s| {
                        s.state = "failed".into();
                        s.error = Some(format!("{e:#}"));
                    });
                }
            }
        });
        Ok(())
    }

    /// Returns the release rolled back to, if the new one failed its gate.
    async fn run(&self, data_dir: &Path, job: Job) -> Result<Option<String>> {
        let Job { app, release, source, sha256, bearer, trust } = job;
        let incoming = data_dir.join("incoming");
        tokio::fs::create_dir_all(&incoming).await?;
        let bundle = incoming.join(format!("{app}-{release}.bundle"));
        let digest = fetch(&source, &bundle, bearer.as_deref()).await.context("fetching the bundle")?;
        if digest != sha256.trim_start_matches("sha256:") {
            let _ = tokio::fs::remove_file(&bundle).await;
            bail!("bundle is corrupt: sha256 {digest}, expected {sha256}");
        }

        self.set(&app, release, "deploying");
        let store = Store::open(data_dir)?;
        let runtime = DockerRuntime::connect()?;
        let outcome = Deployer::new(&store, &runtime, &SystemClock)
            .trusting(trust)
            .deploy(&bundle, &ApplyContext::default())
            .await;
        let _ = tokio::fs::remove_file(&bundle).await;
        let outcome = outcome?;
        Ok(outcome.rolled_back_to.map(|r| r.to_string()))
    }
}

/// Finish whatever a power cut interrupted: a release that was mid-deploy is
/// either completed or rolled back before anything else runs.
pub async fn recover(data_dir: &Path) {
    let result = async {
        let store = Store::open(data_dir)?;
        let runtime = DockerRuntime::connect()?;
        Ok::<_, anyhow::Error>(Deployer::new(&store, &runtime, &SystemClock).recover().await?)
    }
    .await;
    match result {
        Ok(Some(r)) => tracing::warn!(release = %r, "recovered an interrupted deploy"),
        Ok(None) => {}
        Err(e) => tracing::error!("deploy recovery failed: {e:#}"),
    }
}

/// Copy `source` to `path`, returning its sha256 in hex.
async fn fetch(source: &str, path: &Path, bearer: Option<&str>) -> Result<String> {
    let mut hash = Sha256::new();
    let mut out = tokio::fs::File::create(path).await?;
    if source.starts_with("http://") || source.starts_with("https://") {
        let mut req = reqwest::Client::new().get(source);
        if let Some(b) = bearer {
            req = req.bearer_auth(b);
        }
        let mut resp = req.send().await?.error_for_status()?;
        while let Some(chunk) = resp.chunk().await? {
            hash.update(&chunk);
            out.write_all(&chunk).await?;
        }
    } else {
        let bytes = tokio::fs::read(source).await?;
        hash.update(&bytes);
        out.write_all(&bytes).await?;
    }
    out.sync_all().await?;
    Ok(hex::encode(hash.finalize()))
}
