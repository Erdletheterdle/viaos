//! Following an OS update from start to finish.
//!
//! A device says how its update is going only until it reboots, and then has
//! nothing to say for a while, so the server keeps the story. What finally
//! settles it is which version the device comes back on: a version that fails
//! its boot check rolls back by itself, about five minutes later.

use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;
use serde_json::Value;

use crate::State;
use crate::db::now;

/// How long a device may stay away before we stop expecting it. Generous: a
/// failed boot takes minutes to roll back, and firmware can be slow.
const LOST_AFTER_SECS: i64 = 20 * 60;
/// How long a device may go without mentioning an update it just accepted.
const NO_NEWS_SECS: i64 = 60;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct OsUpdate {
    #[serde(skip)]
    pub device_id: String,
    pub version: String,
    pub from_version: Option<String>,
    pub started_at: i64,
    pub finished_at: Option<i64>,
    /// `started`, whatever the device reports (`downloading`, `installing`,
    /// `rebooting`...), `waiting` once it's gone quiet, then one of `done`,
    /// `rolled_back`, `failed` or `lost`.
    pub state: String,
    pub progress: Option<u8>,
    pub detail: Option<String>,
    /// When it dropped off, for this update.
    pub offline_since: Option<i64>,
}

/// What a connected device says about itself.
pub struct Report {
    pub version: Option<String>,
    pub state: Option<String>,
    pub progress: Option<u8>,
    pub error: Option<String>,
}

fn parse_report(health: &Value) -> Report {
    let os = &health["os"];
    let update = &os["update"];
    Report {
        version: os["version"].as_str().map(String::from),
        state: update["state"].as_str().map(String::from),
        progress: update["progress_percent"]
            .as_u64()
            .map(|p| p.min(100) as u8),
        error: update["error"].as_str().map(String::from),
    }
}

/// The next chapter, given what can be seen now. `report` is `None` when the
/// device isn't connected or didn't answer.
pub fn step(mut u: OsUpdate, online: bool, report: Option<Report>, at: i64) -> OsUpdate {
    let finish = |u: &mut OsUpdate, state: &str, detail: Option<String>| {
        u.state = state.into();
        u.detail = detail;
        u.progress = None;
        u.finished_at = Some(at);
    };
    if !online {
        let since = *u.offline_since.get_or_insert(at);
        if at - since > LOST_AFTER_SECS {
            finish(
                &mut u,
                "lost",
                Some(format!(
                    "It hasn't come back in {} minutes.",
                    (at - since) / 60
                )),
            );
        } else {
            u.state = "waiting".into();
            u.progress = None;
        }
        return u;
    }
    let Some(r) = report else {
        return u;
    };
    if r.version.as_deref() == Some(u.version.as_str()) {
        finish(&mut u, "done", None);
    } else if let Some(e) = r.error {
        finish(&mut u, "failed", Some(e));
    } else if u.offline_since.is_some() {
        // It went away and came back on something else. Its own status doesn't
        // survive a reboot, so any status it has now is from since then.
        match r.state {
            Some(state) => {
                u.state = state;
                u.progress = r.progress;
            }
            None => {
                let detail = format!(
                    "It came back on version {}: version {} didn't pass its boot check.",
                    r.version.as_deref().unwrap_or("?"),
                    u.version
                );
                finish(&mut u, "rolled_back", Some(detail));
            }
        }
    } else if let Some(state) = r.state {
        u.state = state;
        u.progress = r.progress;
    } else if at - u.started_at > NO_NEWS_SECS {
        finish(
            &mut u,
            "failed",
            Some("The device stopped reporting this update.".into()),
        );
    }
    u
}

/// Follow every unfinished update, for as long as the server runs. Unfinished
/// ones are in the database, so a restart doesn't lose them.
pub async fn run(state: Arc<State>) {
    let mut tick = tokio::time::interval(Duration::from_secs(3));
    loop {
        tick.tick().await;
        let active = match state.db.active_os_updates() {
            Ok(a) => a,
            Err(e) => {
                tracing::warn!("reading OS updates failed: {e:#}");
                continue;
            }
        };
        for u in active {
            let online = state.hub.is_online(&u.device_id);
            let report = if online {
                match state
                    .hub
                    .call(&u.device_id, "device_health", Default::default())
                    .await
                {
                    Ok(r) => r.structured_content.as_ref().map(parse_report),
                    Err(_) => None,
                }
            } else {
                None
            };
            let next = step(u.clone(), online, report, now());
            if next != u {
                if let Err(e) = state.db.save_os_update(&next) {
                    tracing::warn!("saving an OS update failed: {e:#}");
                }
                if next.finished_at.is_some() {
                    let name = state
                        .db
                        .device(&next.device_id)
                        .ok()
                        .flatten()
                        .map_or(next.device_id.clone(), |d| d.name);
                    state.db.audit(
                        "viaos",
                        "os_update_finished",
                        &serde_json::json!({ "device": name, "version": next.version, "state": next.state, "detail": next.detail }),
                        next.state == "done",
                    );
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn started() -> OsUpdate {
        OsUpdate {
            device_id: "d".into(),
            version: "3".into(),
            from_version: Some("2".into()),
            started_at: 100,
            finished_at: None,
            state: "started".into(),
            progress: None,
            detail: None,
            offline_since: None,
        }
    }

    fn says(version: &str, state: Option<&str>, progress: Option<u8>) -> Option<Report> {
        Some(Report {
            version: Some(version.into()),
            state: state.map(String::from),
            progress,
            error: None,
        })
    }

    #[test]
    fn a_good_update_from_start_to_finish() {
        let u = step(
            started(),
            true,
            says("2", Some("downloading"), Some(40)),
            110,
        );
        assert_eq!((u.state.as_str(), u.progress), ("downloading", Some(40)));
        let u = step(u, true, says("2", Some("rebooting"), None), 150);
        assert_eq!(u.state, "rebooting");
        let u = step(u, false, None, 153);
        assert_eq!((u.state.as_str(), u.offline_since), ("waiting", Some(153)));
        let u = step(u, false, None, 160);
        assert_eq!(u.offline_since, Some(153), "keeps the first time");
        let u = step(u, true, says("3", None, None), 200);
        assert_eq!((u.state.as_str(), u.finished_at), ("done", Some(200)));
    }

    #[test]
    fn coming_back_on_the_old_version_is_a_rollback() {
        let u = step(started(), false, None, 120);
        let u = step(u, true, says("2", None, None), 500);
        assert_eq!(u.state, "rolled_back");
        assert!(u.detail.unwrap().contains("version 3 didn't pass"));
    }

    #[test]
    fn a_failure_on_the_device_says_why() {
        let r = Report {
            version: Some("2".into()),
            state: Some("failed".into()),
            progress: None,
            error: Some("no space".into()),
        };
        let u = step(started(), true, Some(r), 110);
        assert_eq!(
            (u.state.as_str(), u.detail.as_deref()),
            ("failed", Some("no space"))
        );
    }

    #[test]
    fn it_stops_expecting_a_device_eventually() {
        let u = step(started(), false, None, 200);
        let u = step(u, false, None, 200 + LOST_AFTER_SECS);
        assert_eq!(u.state, "waiting");
        let u = step(u, false, None, 201 + LOST_AFTER_SECS);
        assert_eq!(u.state, "lost");
    }

    #[test]
    fn a_device_that_never_mentions_it_doesnt_hang_forever() {
        let u = step(started(), true, says("2", None, None), 130);
        assert_eq!(u.state, "started", "it may not have got going yet");
        let u = step(u, true, says("2", None, None), 100 + NO_NEWS_SECS + 1);
        assert_eq!(u.state, "failed");
    }

    #[test]
    fn a_blip_in_answering_changes_nothing() {
        let u = step(started(), true, says("2", Some("installing"), None), 110);
        let again = step(u.clone(), true, None, 120);
        assert_eq!(u, again);
    }

    #[test]
    fn the_agents_status_is_read_as_it_is_written() {
        let r = parse_report(
            &serde_json::json!({ "os": { "version": "2", "update": { "version": "3", "state": "downloading", "progress_percent": 40 } } }),
        );
        assert_eq!(
            (r.state.as_deref(), r.progress),
            (Some("downloading"), Some(40))
        );
    }
}
