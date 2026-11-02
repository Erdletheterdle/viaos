//! The deploy state machine (spec section 21), made crash-safe.
//!
//! ```text
//! RECEIVE -> VERIFY -> UNPACK -> LOAD IMAGES -> APPLY -> HEALTH CHECK -> ACTIVE
//!                                                            |
//!                                                            +--> FAILED -> rollback
//! ```
//!
//! Two properties matter more than the diagram:
//!
//! * The running release keeps running until the new one passes its gate. The
//!   candidate is recorded before anything changes, so an agent that dies
//!   mid-deploy knows on restart that it was mid-deploy.
//! * A failed deploy leaves the previous release locally available, so
//!   recovery needs no network (spec sections 22, 25).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use via_bundle::{TrustPolicy, UnpackedBundle, Verified};
use via_core::{Deployment, ReleaseId};
use via_runtime::{ApplyContext, ApplyReport, GateReport, Runtime, wait_for_ready};

use crate::store::{AgentState, AppliedEnv, JournalEntry, Store};
use crate::{Clock, Error};

/// Where a deploy got to. Reported to the operator and written to the journal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Phase {
    Verify,
    Unpack,
    LoadImages,
    Apply,
    HealthCheck,
    Active,
}

impl Phase {
    pub fn as_str(&self) -> &'static str {
        match self {
            Phase::Verify => "verify",
            Phase::Unpack => "unpack",
            Phase::LoadImages => "load_images",
            Phase::Apply => "apply",
            Phase::HealthCheck => "health_check",
            Phase::Active => "active",
        }
    }
}

/// Result of a deploy attempt.
#[derive(Debug)]
pub struct DeployOutcome {
    pub release: ReleaseId,
    pub apply: ApplyReport,
    pub gate: GateReport,
    /// Set when the gate failed and the previous release was restored.
    pub rolled_back_to: Option<ReleaseId>,
    pub pruned: Vec<ReleaseId>,
    /// Blob bytes reclaimed because the images are now held by the runtime.
    pub reclaimed_bytes: u64,
}

impl DeployOutcome {
    pub fn succeeded(&self) -> bool {
        self.gate.passed
    }
}

/// Deploys releases into a [`Store`] using a [`Runtime`].
pub struct Deployer<'a> {
    store: &'a Store,
    runtime: &'a dyn Runtime,
    clock: &'a dyn Clock,
    trust: TrustPolicy,
    env: BTreeMap<String, BTreeMap<String, String>>,
    service_env: BTreeMap<String, BTreeMap<String, BTreeMap<String, String>>>,
}

/// The context that gives a deployment its runtime values.
///
/// A value for one service reaches it only if it declared the name, the same
/// rule as for app-wide values: a service never receives a name it did not
/// list, however the value was aimed.
pub fn runtime_context(
    deployment: &Deployment,
    env: &AppliedEnv,
    release: Option<ReleaseId>,
) -> ApplyContext {
    let mut ctx = ApplyContext {
        release,
        ..Default::default()
    }
    .with_global(env.global.clone());
    for (name, values) in &env.services {
        let Some(service) = deployment.services.get(name) else {
            continue;
        };
        let wanted: BTreeMap<String, String> = values
            .iter()
            .filter(|(k, _)| service.wants_runtime_key(k))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        if !wanted.is_empty() {
            ctx.environment.insert(name.clone(), wanted);
        }
    }
    ctx
}

impl<'a> Deployer<'a> {
    pub fn new(store: &'a Store, runtime: &'a dyn Runtime, clock: &'a dyn Clock) -> Self {
        Self {
            store,
            runtime,
            clock,
            trust: TrustPolicy::Insecure,
            env: BTreeMap::new(),
            service_env: BTreeMap::new(),
        }
    }

    /// Runtime variables and secrets, by project. Every apply uses them,
    /// rollback and recovery included: a release restored without its
    /// secrets would not be the release that worked.
    pub fn with_env(mut self, env: BTreeMap<String, BTreeMap<String, String>>) -> Self {
        self.env = env;
        self
    }

    /// Values for one service, by project then service, which beat
    /// [`Self::with_env`] for that service. Whoever works these out has
    /// already settled which layer wins: this layer is simply on top.
    pub fn with_service_env(
        mut self,
        env: BTreeMap<String, BTreeMap<String, BTreeMap<String, String>>>,
    ) -> Self {
        self.service_env = env;
        self
    }

    fn context(&self, deployment: &Deployment, release: ReleaseId) -> ApplyContext {
        let env = AppliedEnv {
            global: self
                .env
                .get(&deployment.project)
                .cloned()
                .unwrap_or_default(),
            services: self
                .service_env
                .get(&deployment.project)
                .cloned()
                .unwrap_or_default(),
        };
        runtime_context(deployment, &env, Some(release))
    }

    /// The context for going back to a release: the values it last passed its
    /// gate with, so a change that broke it does not come back with it. A
    /// release with no record gets the values on hand.
    fn known_good_context(&self, deployment: &Deployment, release: ReleaseId) -> ApplyContext {
        match self.store.load_applied_env(release) {
            Some(env) => runtime_context(deployment, &env, Some(release)),
            None => self.context(deployment, release),
        }
    }

    /// Require released bundles to be signed by a trusted key.
    ///
    /// Verification happens during staging, before any blob is written, so an
    /// untrusted release never reaches the disk.
    pub fn trusting(mut self, policy: TrustPolicy) -> Self {
        self.trust = policy;
        self
    }

    fn note(&self, event: &str, release: Option<ReleaseId>, detail: Option<String>) {
        // Journalling is diagnostics, not control flow. A failure to write it
        // must never abort a deploy that is otherwise fine.
        let _ = self.store.journal(&JournalEntry {
            at: self.clock.now_rfc3339(),
            event: event.to_string(),
            release,
            detail,
        });
    }

    /// Unpack and verify a bundle into the store without applying it.
    ///
    /// Split out from [`Self::deploy`] because receiving a release and
    /// activating it are separate decisions: a device can hold a staged
    /// release and switch to it later, or on command.
    pub fn stage(&self, bundle: &Path) -> Result<UnpackedBundle, Error> {
        let manifest = {
            let file = std::fs::File::open(bundle).map_err(|e| Error::io(bundle, e))?;
            via_bundle::peek_manifest(file)?
        };
        let release = manifest.release;

        // Check the disk before writing anything. Filling the disk on an
        // unattended device is not recoverable remotely.
        if let Some(available) = self.store.available_bytes() {
            // Blobs land unpacked, plus headroom for the load archive Docker
            // reads and the image store it writes.
            let needed = manifest.blob_bytes().saturating_mul(2);
            if available < needed {
                return Err(Error::InsufficientDisk { needed, available });
            }
        }

        self.note("stage_started", Some(release), None);
        let dir = self.store.release_dir(release);
        if dir.exists() {
            // A previous attempt may have left a partial directory behind.
            std::fs::remove_dir_all(&dir).map_err(|e| Error::io(&dir, e))?;
        }
        std::fs::create_dir_all(&dir).map_err(|e| Error::io(&dir, e))?;

        let file = std::fs::File::open(bundle).map_err(|e| Error::io(bundle, e))?;
        let unpacked = match UnpackedBundle::unpack_with(file, &dir, &self.trust) {
            Ok(u) => u,
            Err(e) => {
                // Never leave an unverified release on disk looking deployable.
                let _ = std::fs::remove_dir_all(&dir);
                self.note("stage_failed", Some(release), Some(e.to_string()));
                return Err(e.into());
            }
        };

        // Persist both halves of the release next to its blobs. The manifest
        // is what lets the load archive be rebuilt later without the original
        // bundle file, which is exactly what a rollback needs.
        let deployment_path = dir.join("deployment.json");
        std::fs::write(
            &deployment_path,
            serde_json::to_vec_pretty(&unpacked.deployment).map_err(Error::json)?,
        )
        .map_err(|e| Error::io(&deployment_path, e))?;

        if let Some(compose) = &unpacked.compose_source {
            let path = dir.join(via_bundle::COMPOSE_PATH);
            std::fs::write(&path, compose).map_err(|e| Error::io(&path, e))?;
        }

        let manifest_path = dir.join(via_bundle::MANIFEST_PATH);
        std::fs::write(
            &manifest_path,
            serde_json::to_vec_pretty(&unpacked.manifest).map_err(Error::json)?,
        )
        .map_err(|e| Error::io(&manifest_path, e))?;

        let trust_path = dir.join("trust.json");
        std::fs::write(
            &trust_path,
            serde_json::to_vec(&TrustRecord::from(&unpacked.trust)).map_err(Error::json)?,
        )
        .map_err(|e| Error::io(&trust_path, e))?;

        self.note(
            "staged",
            Some(release),
            Some(format!(
                "{} blob(s), {}",
                unpacked.manifest.blobs.len(),
                unpacked.trust
            )),
        );
        Ok(unpacked)
    }

    /// Read a staged release's deployment manifest.
    pub fn load_deployment(&self, release: ReleaseId) -> Result<Deployment, Error> {
        let path = self.store.release_dir(release).join("deployment.json");
        let bytes = std::fs::read(&path).map_err(|e| Error::io(&path, e))?;
        serde_json::from_slice(&bytes).map_err(Error::json)
    }

    /// Run the full pipeline for a bundle file.
    pub async fn deploy(&self, bundle: &Path, ctx: &ApplyContext) -> Result<DeployOutcome, Error> {
        let unpacked = self.stage(bundle)?;
        self.activate(unpacked.manifest.release, &unpacked.deployment, ctx)
            .await
    }

    /// Apply an already-staged release and hold it to its health gate.
    pub async fn activate(
        &self,
        release: ReleaseId,
        deployment: &Deployment,
        ctx: &ApplyContext,
    ) -> Result<DeployOutcome, Error> {
        let mut state = self.store.load_state()?;

        // Record the candidate before touching anything. This is the single
        // fact that makes a mid-deploy power cut recoverable.
        state.candidate = Some(release);
        self.store.save_state(&state)?;
        self.note("deploy_started", Some(release), None);

        let load_result = self.load_images(release, deployment).await;
        if let Err(e) = load_result {
            self.abandon_candidate(&mut state, release, &e.to_string())?;
            return Err(e);
        }

        let mut ctx = ctx.clone();
        ctx.release = Some(release);
        let runtime = self.context(deployment, release);
        ctx.global_environment.extend(runtime.global_environment);
        for (service, values) in runtime.environment {
            ctx.environment.entry(service).or_default().extend(values);
        }

        let apply = match self.apply(release, deployment, ctx.clone()).await {
            Ok(r) => r,
            Err(e) => {
                self.note("apply_failed", Some(release), Some(e.to_string()));
                let rolled_back_to = self.try_rollback(&mut state, deployment).await?;
                self.abandon_candidate(&mut state, release, &e.to_string())?;
                return Err(Error::ApplyFailed {
                    release,
                    reason: e.to_string(),
                    rolled_back_to,
                });
            }
        };
        self.note("applied", Some(release), Some(apply.summary()));

        let gate = wait_for_ready(self.runtime, deployment, &deployment.health_gate).await?;

        if gate.passed {
            let good = AppliedEnv {
                global: ctx.global_environment.clone(),
                services: ctx.environment.clone(),
            };
            // Without this a later failure goes back to whatever values are on
            // hand, which is worse but no reason to fail a healthy deploy.
            if let Err(e) = self.store.save_applied_env(release, &good) {
                self.note("applied_env_not_saved", Some(release), Some(e.to_string()));
            }

            // Promote only now. Until this point `current` still names the
            // release that was actually working.
            if state.current != Some(release) {
                state.previous = state.current;
            }
            state.current = Some(release);
            state.candidate = None;
            self.store.save_state(&state)?;
            self.note("active", Some(release), Some(gate.summary()));

            let pruned = self.store.prune(&state)?;
            if !pruned.is_empty() {
                self.note(
                    "pruned",
                    None,
                    Some(
                        pruned
                            .iter()
                            .map(|r| r.to_string())
                            .collect::<Vec<_>>()
                            .join(", "),
                    ),
                );
            }
            let reclaimed = self.reclaim_blobs(&state).await?;
            return Ok(DeployOutcome {
                release,
                apply,
                gate,
                rolled_back_to: None,
                pruned,
                reclaimed_bytes: reclaimed,
            });
        }

        self.note("health_gate_failed", Some(release), Some(gate.summary()));
        let rolled_back_to = if deployment.health_gate.rollback_on_failure {
            self.try_rollback(&mut state, deployment).await?
        } else {
            None
        };
        self.abandon_candidate(&mut state, release, &gate.summary())?;

        // A failed release is not current, previous or candidate, so it is
        // dead weight. Leaving it would let a device that keeps retrying a bad
        // release fill its own disk, which is the one failure an unattended
        // device cannot be talked through over the phone.
        let pruned = self.store.prune(&state)?;
        if !pruned.is_empty() {
            self.note(
                "pruned_after_failure",
                None,
                Some(
                    pruned
                        .iter()
                        .map(|r| r.to_string())
                        .collect::<Vec<_>>()
                        .join(", "),
                ),
            );
        }

        Ok(DeployOutcome {
            release,
            apply,
            gate,
            rolled_back_to,
            pruned,
            reclaimed_bytes: 0,
        })
    }

    /// Restore whatever release was running before this attempt.
    ///
    /// Uses only what is already on disk: no registry, no network, no rebuild
    /// (spec section 25).
    async fn try_rollback(
        &self,
        state: &mut AgentState,
        failed: &Deployment,
    ) -> Result<Option<ReleaseId>, Error> {
        let Some(target) = state.current.or(state.previous) else {
            // Nothing was ever healthy here, so there is nothing to go back
            // to. Tear the failed attempt down rather than leaving a broken
            // stack half-running.
            let _ = self.runtime.remove(failed, false).await;
            self.note(
                "rollback_skipped",
                None,
                Some("no previous release on this device".into()),
            );
            return Ok(None);
        };

        if !self.store.has_release(target) {
            self.note(
                "rollback_unavailable",
                Some(target),
                Some("release is no longer stored on this device".into()),
            );
            return Err(Error::RollbackUnavailable { release: target });
        }

        self.note("rollback_started", Some(target), None);
        let deployment = self.load_deployment(target)?;
        let ctx = self.known_good_context(&deployment, target);
        self.apply(target, &deployment, ctx)
            .await
            .map_err(|e| Error::RollbackFailed {
                release: target,
                reason: e.to_string(),
            })?;
        self.note("rolled_back", Some(target), None);
        Ok(Some(target))
    }

    fn abandon_candidate(
        &self,
        state: &mut AgentState,
        release: ReleaseId,
        reason: &str,
    ) -> Result<(), Error> {
        state.candidate = None;
        self.store.save_state(state)?;
        self.note("deploy_failed", Some(release), Some(reason.to_string()));
        Ok(())
    }

    /// Hand the release's images to the runtime.
    ///
    /// Skipped entirely when every image is already present, which is the
    /// common case for a rollback and for a redeploy of an unchanged stack.
    async fn load_images(&self, release: ReleaseId, deployment: &Deployment) -> Result<(), Error> {
        let mut missing = Vec::new();
        for image in deployment.required_images() {
            if !self.runtime.image_present(&image.reference).await? {
                missing.push(image.reference.clone());
            }
        }
        if missing.is_empty() {
            self.note("images_already_present", Some(release), None);
            return Ok(());
        }

        if !self.store.has_blobs(release) {
            // The images were reclaimed on the assumption the runtime held
            // them, and now it does not. Say so precisely: this needs the
            // bundle again, and no amount of retrying locally will help.
            return Err(Error::ImagesUnavailable { release, missing });
        }

        let dir = self.store.release_dir(release);
        let unpacked = self.reopen(release)?;
        let archive_path = dir.join("load.tar");
        let file = std::fs::File::create(&archive_path).map_err(|e| Error::io(&archive_path, e))?;
        unpacked.write_docker_load_archive(file)?;

        let loaded = self.runtime.load_images(&archive_path).await;
        // The load archive is a rebuildable staging artifact, not part of the
        // release. Keeping it around would double every release's disk cost.
        let _ = std::fs::remove_file(&archive_path);
        let loaded = loaded?;

        self.note(
            "images_loaded",
            Some(release),
            Some(format!("{} image(s)", loaded.len())),
        );
        Ok(())
    }

    /// Drop the stored blobs of any retained release whose images the runtime
    /// already holds.
    ///
    /// After a successful load the engine's content store has the same bytes,
    /// so keeping the bundle's copy doubles the disk cost of every release for
    /// no added ability. Blobs are only dropped once the images are confirmed
    /// present, so this never removes the last copy of anything.
    async fn reclaim_blobs(&self, state: &AgentState) -> Result<u64, Error> {
        let mut freed = 0;
        for release in state.pinned() {
            if !self.store.has_blobs(release) {
                continue;
            }
            let Ok(deployment) = self.load_deployment(release) else {
                continue;
            };

            let mut all_present = true;
            for image in deployment.required_images() {
                if !self.runtime.image_present(&image.reference).await? {
                    all_present = false;
                    break;
                }
            }
            if all_present {
                freed += self.store.drop_blobs(release)?;
            }
        }
        if freed > 0 {
            self.note(
                "blobs_reclaimed",
                None,
                Some(format!("{} MiB", freed / 1_048_576)),
            );
        }
        Ok(freed)
    }

    /// Reconstruct an `UnpackedBundle` view over an already-staged release.
    fn reopen(&self, release: ReleaseId) -> Result<UnpackedBundle, Error> {
        let dir = self.store.release_dir(release);
        let manifest_path = dir.join(via_bundle::MANIFEST_PATH);
        let bytes = std::fs::read(&manifest_path).map_err(|e| Error::io(&manifest_path, e))?;
        let manifest = serde_json::from_slice(&bytes).map_err(Error::json)?;
        let deployment = self.load_deployment(release)?;
        Ok(UnpackedBundle {
            manifest,
            deployment,
            compose_source: None,
            blob_dir: dir.join("blobs"),
            trust: self.recorded_trust(release),
        })
    }

    /// How a stored release was authenticated when it was staged.
    pub fn recorded_trust(&self, release: ReleaseId) -> Verified {
        let path = self.store.release_dir(release).join("trust.json");
        std::fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice::<TrustRecord>(&b).ok())
            .map(Into::into)
            .unwrap_or(Verified::Unsigned)
    }

    /// Recover after an agent restart.
    ///
    /// If a candidate is recorded, the agent died mid-deploy. The safe move is
    /// to re-apply whatever release is `current`, since that is the last one
    /// known to have passed a health gate. Re-applying is cheap because
    /// `apply` is idempotent: unchanged services are left running.
    pub async fn recover(&self) -> Result<Option<ReleaseId>, Error> {
        let mut state = self.store.load_state()?;
        let Some(candidate) = state.candidate else {
            return Ok(None);
        };

        self.note("interrupted_deploy_detected", Some(candidate), None);
        state.candidate = None;
        self.store.save_state(&state)?;

        let Some(current) = state.current else {
            self.note(
                "recovery_skipped",
                None,
                Some("no known-good release to restore".into()),
            );
            return Ok(None);
        };
        if !self.store.has_release(current) {
            return Err(Error::RollbackUnavailable { release: current });
        }

        let deployment = self.load_deployment(current)?;
        let ctx = self.known_good_context(&deployment, current);
        self.apply(current, &deployment, ctx)
            .await
            .map_err(|e| Error::ApplyFailed {
                release: current,
                reason: e.to_string(),
                rolled_back_to: None,
            })?;
        self.note("recovered", Some(current), None);
        Ok(Some(current))
    }

    /// Bring the running release back up as it last worked, after a boot. A
    /// container that crashed, was removed, or has no `restart:` comes back;
    /// one that is fine is left alone. None if there is nothing to do, or the
    /// release predates carrying a compose file.
    pub async fn reconcile(&self) -> Result<Option<ReleaseId>, Error> {
        let state = self.store.load_state()?;
        let Some(current) = state.current.filter(|_| state.candidate.is_none()) else {
            return Ok(None);
        };
        if !self
            .store
            .release_dir(current)
            .join(via_bundle::COMPOSE_PATH)
            .is_file()
        {
            return Ok(None);
        }
        let deployment = self.load_deployment(current)?;
        let ctx = self.known_good_context(&deployment, current);
        self.apply(current, &deployment, ctx)
            .await
            .map_err(|e| Error::ApplyFailed {
                release: current,
                reason: e.to_string(),
                rolled_back_to: None,
            })?;
        Ok(Some(current))
    }

    /// Lay out a release's files where Compose reads them, then apply it.
    async fn apply(
        &self,
        release: ReleaseId,
        deployment: &Deployment,
        mut ctx: ApplyContext,
    ) -> Result<ApplyReport, via_runtime::Error> {
        ctx.project_dir =
            self.lay_out(release, deployment)
                .map_err(|e| via_runtime::Error::Invalid {
                    reason: e.to_string(),
                })?;
        self.runtime.apply(deployment, &ctx).await
    }

    /// Copy a release's compose file and project files into the app's project
    /// directory, over what is there: anything else in it was written by a
    /// container, into a bind mount, and is that container's to keep. None if
    /// the release has no compose file.
    fn lay_out(
        &self,
        release: ReleaseId,
        deployment: &Deployment,
    ) -> Result<Option<PathBuf>, Error> {
        let source = self.store.release_dir(release);
        let compose = source.join(via_bundle::COMPOSE_PATH);
        if !compose.is_file() {
            return Ok(None);
        }
        let dir = self.store.project_dir(&deployment.project);
        std::fs::create_dir_all(&dir).map_err(|e| Error::io(&dir, e))?;
        copy_tree(&source.join("project"), &dir)?;
        let target = dir.join(via_bundle::COMPOSE_PATH);
        std::fs::copy(&compose, &target).map_err(|e| Error::io(&target, e))?;
        Ok(Some(dir))
    }

    /// Roll back to a specific stored release on request (spec section 25).
    pub async fn rollback_to(&self, target: ReleaseId) -> Result<DeployOutcome, Error> {
        if !self.store.has_release(target) {
            return Err(Error::RollbackUnavailable { release: target });
        }
        let deployment = self.load_deployment(target)?;
        let ctx = self.context(&deployment, target);
        self.activate(target, &deployment, &ctx).await
    }
}

fn copy_tree(from: &Path, to: &Path) -> Result<(), Error> {
    let entries = match std::fs::read_dir(from) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(Error::io(from, e)),
    };
    for entry in entries {
        let entry = entry.map_err(|e| Error::io(from, e))?;
        let target = to.join(entry.file_name());
        if entry.path().is_dir() {
            std::fs::create_dir_all(&target).map_err(|e| Error::io(&target, e))?;
            copy_tree(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), &target).map_err(|e| Error::io(&target, e))?;
        }
    }
    Ok(())
}

/// Serializable form of [`Verified`], so how a release was authenticated
/// survives a restart alongside the release itself.
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(tag = "trust", rename_all = "snake_case")]
enum TrustRecord {
    Signed { key_id: String },
    UncheckedSignature { key_id: String },
    Unsigned,
}

impl From<&Verified> for TrustRecord {
    fn from(v: &Verified) -> Self {
        match v {
            Verified::Signed { key_id } => TrustRecord::Signed {
                key_id: key_id.to_string(),
            },
            Verified::UncheckedSignature { key_id } => TrustRecord::UncheckedSignature {
                key_id: key_id.to_string(),
            },
            Verified::Unsigned => TrustRecord::Unsigned,
        }
    }
}

impl From<TrustRecord> for Verified {
    fn from(r: TrustRecord) -> Self {
        // A key id that will not parse degrades to unsigned rather than
        // failing a rollback: this record is a note about how a release
        // arrived, not the check itself.
        let id = |s: String| via_bundle::KeyId::parse(s).ok();
        match r {
            TrustRecord::Signed { key_id } => match id(key_id) {
                Some(key_id) => Verified::Signed { key_id },
                None => Verified::Unsigned,
            },
            TrustRecord::UncheckedSignature { key_id } => match id(key_id) {
                Some(key_id) => Verified::UncheckedSignature { key_id },
                None => Verified::Unsigned,
            },
            TrustRecord::Unsigned => Verified::Unsigned,
        }
    }
}
