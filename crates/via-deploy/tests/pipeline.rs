//! Deploy pipeline tests against a scripted runtime.
//!
//! A fake [`Runtime`] is the only way to exercise the paths that matter most
//! and are hardest to produce on demand with a real daemon: a health gate that
//! fails, an apply that errors, and an agent that dies mid-deploy.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Mutex;

use async_trait::async_trait;
use via_core::{Deployment, ImageRef, Platform, Readiness, ReleaseId, Service};
use via_deploy::{AgentState, Deployer, Error, FixedClock, Store};
use via_runtime::{
    ApplyContext, ApplyReport, ContainerStatus, HealthStatus, Runtime, RuntimeInfo, ServiceState,
};

/// How the fake runtime should behave for a given release.
#[derive(Clone, Copy, PartialEq)]
enum Behavior {
    /// Everything comes up and reports ready.
    Healthy,
    /// Containers start but never become ready.
    NeverReady,
    /// The container exits, which the gate treats as terminal.
    Crashes,
    /// `apply` itself fails.
    ApplyErrors,
}

struct FakeRuntime {
    behavior: Mutex<BTreeMap<u64, Behavior>>,
    /// Release last applied, so a test can assert what is running.
    applied: Mutex<Vec<u64>>,
    images: Mutex<Vec<String>>,
    /// Runtime variables each apply was given.
    envs: Mutex<Vec<BTreeMap<String, String>>>,
}

impl FakeRuntime {
    fn new() -> Self {
        Self {
            behavior: Mutex::new(BTreeMap::new()),
            applied: Mutex::new(Vec::new()),
            images: Mutex::new(Vec::new()),
            envs: Mutex::new(Vec::new()),
        }
    }

    fn set(&self, release: u64, behavior: Behavior) {
        self.behavior.lock().unwrap().insert(release, behavior);
    }

    fn behavior_for(&self, release: u64) -> Behavior {
        self.behavior
            .lock()
            .unwrap()
            .get(&release)
            .copied()
            .unwrap_or(Behavior::Healthy)
    }

    fn current(&self) -> Option<u64> {
        self.applied.lock().unwrap().last().copied()
    }

    fn applied_sequence(&self) -> Vec<u64> {
        self.applied.lock().unwrap().clone()
    }

    /// Which release a deployment belongs to, recovered from its image tag.
    fn release_of(deployment: &Deployment) -> u64 {
        deployment
            .services
            .values()
            .next()
            .and_then(|s| s.image.reference.rsplit(':').next())
            .and_then(|t| t.parse().ok())
            .unwrap_or(0)
    }
}

#[async_trait]
impl Runtime for FakeRuntime {
    async fn info(&self) -> Result<RuntimeInfo, via_runtime::Error> {
        Ok(RuntimeInfo {
            engine: "fake".into(),
            version: "1".into(),
            ..Default::default()
        })
    }

    async fn load_images(&self, _archive: &Path) -> Result<Vec<String>, via_runtime::Error> {
        Ok(Vec::new())
    }

    async fn image_present(&self, reference: &str) -> Result<bool, via_runtime::Error> {
        Ok(self.images.lock().unwrap().iter().any(|i| i == reference))
    }

    async fn apply(
        &self,
        deployment: &Deployment,
        ctx: &ApplyContext,
    ) -> Result<ApplyReport, via_runtime::Error> {
        self.envs
            .lock()
            .unwrap()
            .push(ctx.global_environment.clone());
        let release = Self::release_of(deployment);
        if self.behavior_for(release) == Behavior::ApplyErrors {
            return Err(via_runtime::Error::ImageNotLoaded {
                reference: "broken".into(),
            });
        }
        self.applied.lock().unwrap().push(release);
        Ok(ApplyReport {
            created: deployment.services.keys().cloned().collect(),
            ..Default::default()
        })
    }

    async fn service_states(
        &self,
        deployment: &Deployment,
    ) -> Result<Vec<ServiceState>, via_runtime::Error> {
        // Report on whatever was applied last, not on what was asked about:
        // that is what a real engine does, and it is what makes a rollback
        // observable.
        let running = self.current().unwrap_or(0);
        let behavior = self.behavior_for(running);
        Ok(deployment
            .services
            .keys()
            .map(|name| {
                let (status, health) = match behavior {
                    Behavior::Healthy => (ContainerStatus::Running, HealthStatus::Healthy),
                    Behavior::NeverReady => (ContainerStatus::Running, HealthStatus::Starting),
                    Behavior::Crashes => (ContainerStatus::Exited { code: 1 }, HealthStatus::None),
                    Behavior::ApplyErrors => (ContainerStatus::Absent, HealthStatus::None),
                };
                ServiceState {
                    service: name.clone(),
                    status,
                    health,
                    ..ServiceState::absent(name)
                }
            })
            .collect())
    }

    async fn logs(
        &self,
        _d: &Deployment,
        _o: &via_runtime::LogOptions,
    ) -> Result<via_runtime::Logs, via_runtime::Error> {
        // Nothing in the deploy pipeline reads logs; a fake engine that
        // produced them would be inventing behaviour no test asserts on.
        Ok(via_runtime::Logs {
            services: Vec::new(),
            lines: Box::pin(futures::stream::empty()),
        })
    }

    async fn exec(
        &self,
        _d: &Deployment,
        o: &via_runtime::ExecOptions,
    ) -> Result<via_runtime::Exec, via_runtime::Error> {
        // The deploy pipeline never runs commands in containers, so refusing
        // is more honest than a fake that succeeds: a test that started
        // depending on this would fail rather than pass on invented output.
        Err(via_runtime::Error::NoSuchService {
            service: o.service.clone(),
        })
    }

    async fn stop(&self, _d: &Deployment) -> Result<(), via_runtime::Error> {
        Ok(())
    }

    async fn remove(&self, _d: &Deployment, _v: bool) -> Result<(), via_runtime::Error> {
        Ok(())
    }
}

/// A one-service deployment whose image tag encodes its release.
fn deployment(release: u64) -> Deployment {
    let mut d = Deployment::new("demo-stack", Platform::new("linux", "arm64"));
    let mut svc = Service::new("app", ImageRef::new(format!("app:{release}")));
    svc.readiness = Readiness::Healthy;
    d.services.insert("app".into(), svc);
    // Keep the gate quick; these tests are about the decision, not the wait.
    d.health_gate.timeout_secs = 2;
    d.health_gate.stable_secs = 0;
    d
}

/// Stage a release's deployment directly, bypassing bundle unpacking.
///
/// Also marks its images as already present, which is the state a release is
/// in once its blobs have been loaded and reclaimed. These tests are about the
/// gate and rollback decisions, not about image transfer.
fn stage(store: &Store, runtime: &FakeRuntime, release: u64) {
    let dir = store.release_dir(ReleaseId(release));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("deployment.json"),
        serde_json::to_vec_pretty(&deployment(release)).unwrap(),
    )
    .unwrap();
    runtime
        .images
        .lock()
        .unwrap()
        .push(format!("app:{release}"));
}

struct Harness {
    _dir: tempfile::TempDir,
    store: Store,
    runtime: FakeRuntime,
    clock: FixedClock,
}

impl Harness {
    fn new() -> Self {
        let dir = tempfile::TempDir::new().unwrap();
        let store = Store::open(dir.path()).unwrap();
        Self {
            _dir: dir,
            store,
            runtime: FakeRuntime::new(),
            clock: FixedClock("2026-08-22T13:45:00Z".into()),
        }
    }

    fn deployer(&self) -> Deployer<'_> {
        Deployer::new(&self.store, &self.runtime, &self.clock)
    }
}

#[tokio::test]
async fn a_healthy_release_becomes_current() {
    let h = Harness::new();
    stage(&h.store, &h.runtime, 44);

    let outcome = h
        .deployer()
        .activate(ReleaseId(44), &deployment(44), &ApplyContext::default())
        .await
        .unwrap();
    assert!(outcome.succeeded());

    let state = h.store.load_state().unwrap();
    assert_eq!(state.current, Some(ReleaseId(44)));
    assert_eq!(state.previous, None);
    assert_eq!(
        state.candidate, None,
        "the candidate must be cleared once the release is active"
    );
}

#[tokio::test]
async fn a_second_release_pushes_the_first_to_previous() {
    let h = Harness::new();
    stage(&h.store, &h.runtime, 44);
    stage(&h.store, &h.runtime, 45);
    let d = h.deployer();

    d.activate(ReleaseId(44), &deployment(44), &ApplyContext::default())
        .await
        .unwrap();
    d.activate(ReleaseId(45), &deployment(45), &ApplyContext::default())
        .await
        .unwrap();

    let state = h.store.load_state().unwrap();
    assert_eq!(state.current, Some(ReleaseId(45)));
    assert_eq!(
        state.previous,
        Some(ReleaseId(44)),
        "rollback needs the old release retained"
    );
}

#[tokio::test]
async fn a_release_that_never_becomes_ready_is_rolled_back() {
    let h = Harness::new();
    stage(&h.store, &h.runtime, 44);
    stage(&h.store, &h.runtime, 45);
    h.runtime.set(45, Behavior::NeverReady);
    let d = h.deployer();

    d.activate(ReleaseId(44), &deployment(44), &ApplyContext::default())
        .await
        .unwrap();
    let outcome = d
        .activate(ReleaseId(45), &deployment(45), &ApplyContext::default())
        .await
        .unwrap();

    assert!(!outcome.succeeded());
    assert_eq!(outcome.rolled_back_to, Some(ReleaseId(44)));
    assert_eq!(
        h.runtime.current(),
        Some(44),
        "the device must end up running the good release"
    );

    let state = h.store.load_state().unwrap();
    assert_eq!(
        state.current,
        Some(ReleaseId(44)),
        "a failed release must never become current"
    );
    assert_eq!(state.candidate, None);
}

#[tokio::test]
async fn a_crashing_release_fails_fast_rather_than_burning_the_timeout() {
    let h = Harness::new();
    stage(&h.store, &h.runtime, 44);
    stage(&h.store, &h.runtime, 45);
    h.runtime.set(45, Behavior::Crashes);
    let d = h.deployer();

    d.activate(ReleaseId(44), &deployment(44), &ApplyContext::default())
        .await
        .unwrap();

    let started = std::time::Instant::now();
    let outcome = d
        .activate(ReleaseId(45), &deployment(45), &ApplyContext::default())
        .await
        .unwrap();
    assert!(!outcome.succeeded());
    assert!(
        started.elapsed() < std::time::Duration::from_secs(2),
        "an exited container is terminal; waiting out the gate helps nobody"
    );
    assert_eq!(outcome.rolled_back_to, Some(ReleaseId(44)));
}

#[tokio::test]
async fn a_rollback_keeps_the_devices_secrets() {
    let h = Harness::new();
    stage(&h.store, &h.runtime, 44);
    stage(&h.store, &h.runtime, 45);
    h.runtime.set(45, Behavior::NeverReady);
    let project = deployment(44).project;
    let vars = BTreeMap::from([("TOKEN".to_string(), "s3cret".to_string())]);
    let d = h
        .deployer()
        .with_env(BTreeMap::from([(project, vars.clone())]));

    d.activate(ReleaseId(44), &deployment(44), &ApplyContext::default())
        .await
        .unwrap();
    let outcome = d
        .activate(ReleaseId(45), &deployment(45), &ApplyContext::default())
        .await
        .unwrap();

    assert_eq!(outcome.rolled_back_to, Some(ReleaseId(44)));
    let envs = h.runtime.envs.lock().unwrap();
    assert_eq!(envs.len(), 3, "44, 45, then 44 again");
    assert!(envs.iter().all(|e| *e == vars), "{envs:?}");
}

#[tokio::test]
async fn a_failing_apply_rolls_back_and_reports_why() {
    let h = Harness::new();
    stage(&h.store, &h.runtime, 44);
    stage(&h.store, &h.runtime, 45);
    h.runtime.set(45, Behavior::ApplyErrors);
    let d = h.deployer();

    d.activate(ReleaseId(44), &deployment(44), &ApplyContext::default())
        .await
        .unwrap();
    let err = d
        .activate(ReleaseId(45), &deployment(45), &ApplyContext::default())
        .await
        .unwrap_err();

    match err {
        Error::ApplyFailed {
            release,
            rolled_back_to,
            ..
        } => {
            assert_eq!(release, ReleaseId(45));
            assert_eq!(rolled_back_to, Some(ReleaseId(44)));
        }
        other => panic!("expected ApplyFailed, got {other}"),
    }
    let state = h.store.load_state().unwrap();
    assert_eq!(state.current, Some(ReleaseId(44)));
    assert_eq!(
        state.candidate, None,
        "a failed attempt must not leave a candidate behind"
    );
}

#[tokio::test]
async fn the_first_release_on_a_device_has_nothing_to_roll_back_to() {
    let h = Harness::new();
    stage(&h.store, &h.runtime, 44);
    h.runtime.set(44, Behavior::NeverReady);

    let outcome = h
        .deployer()
        .activate(ReleaseId(44), &deployment(44), &ApplyContext::default())
        .await
        .unwrap();
    assert!(!outcome.succeeded());
    assert_eq!(outcome.rolled_back_to, None);
    assert_eq!(h.store.load_state().unwrap().current, None);
}

#[tokio::test]
async fn an_interrupted_deploy_is_detected_and_the_good_release_restored() {
    // The case a straight-line pipeline cannot handle: power lost between
    // recording the candidate and finishing the deploy.
    let h = Harness::new();
    stage(&h.store, &h.runtime, 44);
    stage(&h.store, &h.runtime, 45);

    h.deployer()
        .activate(ReleaseId(44), &deployment(44), &ApplyContext::default())
        .await
        .unwrap();

    // Simulate the crash: a candidate is recorded, nothing else happened.
    let mut state = h.store.load_state().unwrap();
    state.candidate = Some(ReleaseId(45));
    h.store.save_state(&state).unwrap();
    assert!(!state.converged());

    let restored = h.deployer().recover().await.unwrap();
    assert_eq!(restored, Some(ReleaseId(44)));

    let state = h.store.load_state().unwrap();
    assert_eq!(state.candidate, None);
    assert_eq!(state.current, Some(ReleaseId(44)));
    assert_eq!(h.runtime.applied_sequence().last(), Some(&44));
}

#[tokio::test]
async fn recovery_is_a_no_op_when_no_deploy_was_interrupted() {
    let h = Harness::new();
    stage(&h.store, &h.runtime, 44);
    h.deployer()
        .activate(ReleaseId(44), &deployment(44), &ApplyContext::default())
        .await
        .unwrap();
    assert_eq!(h.deployer().recover().await.unwrap(), None);
}

#[tokio::test]
async fn rolling_back_to_a_release_this_device_never_had_is_refused() {
    let h = Harness::new();
    stage(&h.store, &h.runtime, 44);
    h.deployer()
        .activate(ReleaseId(44), &deployment(44), &ApplyContext::default())
        .await
        .unwrap();

    let err = h.deployer().rollback_to(ReleaseId(99)).await.unwrap_err();
    assert!(matches!(err, Error::RollbackUnavailable { .. }), "{err}");
}

#[tokio::test]
async fn a_failed_release_does_not_stay_on_disk() {
    // Otherwise a device retrying a bad release fills its own disk.
    let h = Harness::new();
    stage(&h.store, &h.runtime, 44);
    stage(&h.store, &h.runtime, 45);
    h.runtime.set(45, Behavior::NeverReady);
    let d = h.deployer();

    d.activate(ReleaseId(44), &deployment(44), &ApplyContext::default())
        .await
        .unwrap();
    d.activate(ReleaseId(45), &deployment(45), &ApplyContext::default())
        .await
        .unwrap();

    assert!(
        !h.store.has_release(ReleaseId(45)),
        "the failed release should have been pruned"
    );
    assert!(h.store.has_release(ReleaseId(44)));
}

#[tokio::test]
async fn the_journal_records_the_whole_story() {
    let h = Harness::new();
    stage(&h.store, &h.runtime, 44);
    stage(&h.store, &h.runtime, 45);
    h.runtime.set(45, Behavior::NeverReady);
    let d = h.deployer();

    d.activate(ReleaseId(44), &deployment(44), &ApplyContext::default())
        .await
        .unwrap();
    d.activate(ReleaseId(45), &deployment(45), &ApplyContext::default())
        .await
        .unwrap();

    let events: Vec<String> = h
        .store
        .read_journal()
        .unwrap()
        .into_iter()
        .map(|e| e.event)
        .collect();
    for expected in [
        "deploy_started",
        "active",
        "health_gate_failed",
        "rolled_back",
        "deploy_failed",
    ] {
        assert!(
            events.iter().any(|e| e == expected),
            "missing {expected:?} in {events:?}"
        );
    }
}

#[test]
fn converged_reflects_desired_versus_current() {
    let mut state = AgentState::default();
    assert!(state.converged(), "a device told nothing has nothing to do");

    state.desired = Some(ReleaseId(45));
    assert!(!state.converged());

    state.current = Some(ReleaseId(45));
    assert!(state.converged());

    state.candidate = Some(ReleaseId(46));
    assert!(!state.converged(), "a deploy in flight means not converged");
}
