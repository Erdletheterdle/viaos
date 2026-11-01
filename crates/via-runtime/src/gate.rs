//! The deploy health gate (spec sections 21, 22).
//!
//! Engine-agnostic on purpose: it decides readiness from [`ServiceState`], so
//! the same rules apply to any runtime. The spec had a HEALTH CHECK step but
//! never said what passes it; this is that decision, made explicit.

use std::collections::BTreeMap;

use tokio::time::{Duration, Instant, sleep};
use via_core::{Deployment, HealthGate, Readiness};

use crate::types::{ContainerStatus, HealthStatus, ServiceState};
use crate::{Error, Runtime};

const POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Why a service is not ready yet, or that it never will be.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    Ready,
    /// Still converging; worth waiting.
    Pending(String),
    /// Terminally failed. Waiting longer cannot help, so the gate stops now
    /// rather than burning the remaining timeout.
    Failed(String),
    /// Excluded from the gate.
    Ignored,
}

/// Decide one service's contribution to the gate.
pub fn evaluate(readiness: &Readiness, state: &ServiceState) -> Verdict {
    match readiness {
        Readiness::Ignored => Verdict::Ignored,

        Readiness::Healthy => match (&state.status, &state.health) {
            (ContainerStatus::Running, HealthStatus::Healthy) => Verdict::Ready,
            (ContainerStatus::Running, HealthStatus::Unhealthy) => {
                Verdict::Pending("healthcheck failing".into())
            }
            (ContainerStatus::Running, _) => Verdict::Pending("waiting for healthcheck".into()),
            (ContainerStatus::Exited { code }, _) => {
                Verdict::Failed(format!("exited with code {code} before becoming healthy"))
            }
            (ContainerStatus::Dead, _) => Verdict::Failed("container is dead".into()),
            (ContainerStatus::Absent, _) => Verdict::Pending("container not created yet".into()),
            (status, _) => Verdict::Pending(format!("{status:?}")),
        },

        Readiness::Running => match &state.status {
            ContainerStatus::Running => Verdict::Ready,
            // Restarting means it is crash-looping. Not yet terminal, because
            // a slow-starting service may legitimately restart once, but never
            // ready either.
            ContainerStatus::Restarting => Verdict::Pending("restarting".into()),
            ContainerStatus::Exited { code } => Verdict::Failed(format!("exited with code {code}")),
            ContainerStatus::Dead => Verdict::Failed("container is dead".into()),
            ContainerStatus::Absent => Verdict::Pending("container not created yet".into()),
            status => Verdict::Pending(format!("{status:?}")),
        },

        Readiness::Completed => match &state.status {
            ContainerStatus::Exited { code: 0 } => Verdict::Ready,
            ContainerStatus::Exited { code } => {
                Verdict::Failed(format!("exited with code {code}, expected 0"))
            }
            ContainerStatus::Dead => Verdict::Failed("container is dead".into()),
            _ => Verdict::Pending("waiting for completion".into()),
        },
    }
}

/// Outcome of waiting on the gate.
#[derive(Clone, Debug)]
pub struct GateReport {
    pub passed: bool,
    /// Final verdict per service, for reporting to an operator.
    pub verdicts: BTreeMap<String, Verdict>,
    pub elapsed: Duration,
}

impl GateReport {
    /// Services that were not ready, with the reason.
    pub fn blockers(&self) -> Vec<(&str, &str)> {
        self.verdicts
            .iter()
            .filter_map(|(name, v)| match v {
                Verdict::Pending(why) | Verdict::Failed(why) => Some((name.as_str(), why.as_str())),
                _ => None,
            })
            .collect()
    }

    pub fn summary(&self) -> String {
        if self.passed {
            return format!("all services ready in {:.1}s", self.elapsed.as_secs_f64());
        }
        let blockers: Vec<String> = self
            .blockers()
            .iter()
            .map(|(n, why)| format!("{n} ({why})"))
            .collect();
        format!(
            "not ready after {:.1}s: {}",
            self.elapsed.as_secs_f64(),
            blockers.join(", ")
        )
    }
}

/// Wait until every gated service is ready, one fails terminally, or the gate
/// times out.
///
/// `stable_secs` requires a `Running` service to stay up for that long, which
/// is what distinguishes a started service from a crash-looping one.
pub async fn wait_for_ready(
    runtime: &dyn Runtime,
    deployment: &Deployment,
    gate: &HealthGate,
) -> Result<GateReport, Error> {
    let started = Instant::now();
    let deadline = started + Duration::from_secs(gate.timeout_secs);
    let stable = Duration::from_secs(gate.stable_secs);
    // When each service first looked ready, so `stable_secs` can be measured
    // without trusting the engine's clock or parsing its timestamps.
    let mut ready_since: BTreeMap<String, Instant> = BTreeMap::new();

    loop {
        let states = runtime.service_states(deployment).await?;
        let mut verdicts = BTreeMap::new();
        let mut all_ready = true;
        let mut terminal_failure = false;

        for state in &states {
            let Some(service) = deployment.services.get(&state.service) else {
                continue;
            };
            let verdict = evaluate(&service.readiness, state);

            match &verdict {
                Verdict::Ready => {
                    let since = *ready_since
                        .entry(state.service.clone())
                        .or_insert_with(Instant::now);
                    // A completed job has nothing left to destabilize, so the
                    // stability window applies only to long-running services.
                    let needs_stability = service.readiness != Readiness::Completed;
                    if needs_stability && since.elapsed() < stable {
                        all_ready = false;
                        verdicts.insert(
                            state.service.clone(),
                            Verdict::Pending(format!(
                                "up {:.0}s of {}s required",
                                since.elapsed().as_secs_f64(),
                                gate.stable_secs
                            )),
                        );
                        continue;
                    }
                }
                Verdict::Ignored => {}
                Verdict::Pending(_) => {
                    // Lost readiness: restart the stability clock.
                    ready_since.remove(&state.service);
                    all_ready = false;
                }
                Verdict::Failed(_) => {
                    ready_since.remove(&state.service);
                    all_ready = false;
                    terminal_failure = true;
                }
            }
            verdicts.insert(state.service.clone(), verdict);
        }

        if all_ready {
            return Ok(GateReport {
                passed: true,
                verdicts,
                elapsed: started.elapsed(),
            });
        }
        if terminal_failure || Instant::now() >= deadline {
            return Ok(GateReport {
                passed: false,
                verdicts,
                elapsed: started.elapsed(),
            });
        }
        sleep(POLL_INTERVAL).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(status: ContainerStatus, health: HealthStatus) -> ServiceState {
        ServiceState {
            status,
            health,
            ..ServiceState::absent("app")
        }
    }

    #[test]
    fn healthy_readiness_needs_the_healthcheck_to_pass() {
        let r = Readiness::Healthy;
        assert_eq!(
            evaluate(&r, &state(ContainerStatus::Running, HealthStatus::Healthy)),
            Verdict::Ready
        );
        assert!(matches!(
            evaluate(&r, &state(ContainerStatus::Running, HealthStatus::Starting)),
            Verdict::Pending(_)
        ));
        // A failing healthcheck is pending, not failed: Docker retries it, and
        // a service may legitimately go healthy on the third attempt.
        assert!(matches!(
            evaluate(
                &r,
                &state(ContainerStatus::Running, HealthStatus::Unhealthy)
            ),
            Verdict::Pending(_)
        ));
    }

    #[test]
    fn an_exit_while_waiting_for_health_is_terminal() {
        // Waiting out a five minute timeout on a container that has already
        // died helps nobody.
        let v = evaluate(
            &Readiness::Healthy,
            &state(ContainerStatus::Exited { code: 1 }, HealthStatus::None),
        );
        assert!(matches!(v, Verdict::Failed(_)), "{v:?}");
    }

    #[test]
    fn running_readiness_accepts_a_bare_running_container() {
        assert_eq!(
            evaluate(
                &Readiness::Running,
                &state(ContainerStatus::Running, HealthStatus::None)
            ),
            Verdict::Ready
        );
    }

    #[test]
    fn a_clean_exit_still_fails_a_long_running_service() {
        // `Running` means it should stay up. Exiting 0 is a crash for a server.
        let v = evaluate(
            &Readiness::Running,
            &state(ContainerStatus::Exited { code: 0 }, HealthStatus::None),
        );
        assert!(matches!(v, Verdict::Failed(_)), "{v:?}");
    }

    #[test]
    fn completed_readiness_wants_exit_zero_and_nothing_else() {
        let r = Readiness::Completed;
        assert_eq!(
            evaluate(
                &r,
                &state(ContainerStatus::Exited { code: 0 }, HealthStatus::None)
            ),
            Verdict::Ready
        );
        assert!(matches!(
            evaluate(
                &r,
                &state(ContainerStatus::Exited { code: 2 }, HealthStatus::None)
            ),
            Verdict::Failed(_)
        ));
        assert!(matches!(
            evaluate(&r, &state(ContainerStatus::Running, HealthStatus::None)),
            Verdict::Pending(_)
        ));
    }

    #[test]
    fn ignored_services_never_block_a_deploy() {
        let v = evaluate(
            &Readiness::Ignored,
            &state(ContainerStatus::Exited { code: 9 }, HealthStatus::Unhealthy),
        );
        assert_eq!(v, Verdict::Ignored);
    }
}
