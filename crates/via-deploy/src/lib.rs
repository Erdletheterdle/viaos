//! Receiving, activating and rolling back releases on a device.
//!
//! This crate owns the part of Via that has to survive power cuts: the deploy
//! state machine (spec section 21), retention of the previous release
//! (section 23), and local rollback (section 25). It sits between the bundle
//! format and the container runtime and depends on both, which is why it is
//! neither of them.

mod pipeline;
mod receive;
mod store;

use std::path::Path;

use thiserror::Error as ThisError;
use via_core::ReleaseId;

pub use pipeline::{DeployOutcome, Deployer, Phase, runtime_context};
pub use receive::{Expectation, INCOMING_DIR, Progress, Reception, digest_file};
pub use store::{
    APPLIED_ENV_FILE, AgentState, AppliedEnv, JOURNAL_FILE, JournalEntry, RELEASES_DIR, STATE_FILE,
    Store,
};

/// Source of timestamps for the journal.
///
/// Injected rather than called directly so tests produce comparable journals,
/// and so a device with no RTC can substitute a monotonic or
/// control-plane-supplied clock instead of writing 1970 into its own history.
pub trait Clock: Send + Sync {
    fn now_rfc3339(&self) -> String;
}

/// Clock backed by the system time.
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_rfc3339(&self) -> String {
        // Formatted by hand to keep a date/time crate out of the agent's
        // dependency tree for one string.
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        format_rfc3339(now)
    }
}

/// A clock frozen at a fixed instant, for tests.
pub struct FixedClock(pub String);

impl Clock for FixedClock {
    fn now_rfc3339(&self) -> String {
        self.0.clone()
    }
}

/// Format a Unix timestamp as RFC 3339 UTC.
fn format_rfc3339(secs: u64) -> String {
    let days = secs / 86_400;
    let rem = secs % 86_400;
    let (h, m, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);

    // Civil-from-days, per Howard Hinnant's algorithm.
    let z = days as i64 + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m_civil = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m_civil <= 2 { y + 1 } else { y };

    format!("{y:04}-{m_civil:02}-{d:02}T{h:02}:{m:02}:{s:02}Z")
}

#[derive(Debug, ThisError)]
pub enum Error {
    #[error("{path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },

    #[error("malformed JSON: {0}")]
    Json(#[source] serde_json::Error),

    #[error("agent state at {path} is corrupt: {source}")]
    CorruptState {
        path: String,
        #[source]
        source: serde_json::Error,
    },

    #[error("agent state schema {found} is not supported by this build (supports {supported})")]
    UnsupportedStateSchema { found: u32, supported: u32 },

    #[error(
        "not enough disk to receive this release: needs about {} MiB, {} MiB available",
        needed / 1_048_576,
        available / 1_048_576
    )]
    InsufficientDisk { needed: u64, available: u64 },

    #[error("applying release {release} failed: {reason}{}", rolled_back(.rolled_back_to))]
    ApplyFailed {
        release: ReleaseId,
        reason: String,
        rolled_back_to: Option<ReleaseId>,
    },

    #[error("release {release} is not stored on this device, so it cannot be rolled back to")]
    RollbackUnavailable { release: ReleaseId },

    #[error(
        "release {release} needs image(s) the runtime no longer has ({}), \
         and its blobs were reclaimed. The bundle must be sent again",
        missing.join(", ")
    )]
    ImagesUnavailable {
        release: ReleaseId,
        missing: Vec<String>,
    },

    #[error("rolling back to release {release} failed: {reason}")]
    RollbackFailed { release: ReleaseId, reason: String },

    #[error("no transfer of release {release} is in progress")]
    NoTransferInProgress { release: ReleaseId },

    #[error(
        "transfer of release {release} is at byte {expected}, but bytes for offset {got} arrived"
    )]
    TransferOffsetMismatch {
        release: ReleaseId,
        expected: u64,
        got: u64,
    },

    #[error(
        "transfer of release {release} would exceed its declared size of {size} bytes (would reach {would_be})"
    )]
    TransferOverrun {
        release: ReleaseId,
        size: u64,
        would_be: u64,
    },

    #[error("transfer of release {release} is incomplete: {received} of {expected} bytes")]
    TransferIncomplete {
        release: ReleaseId,
        received: u64,
        expected: u64,
    },

    #[error(
        "release {release} failed verification: expected {expected}, got {actual}. The transfer has been discarded"
    )]
    TransferDigestMismatch {
        release: ReleaseId,
        expected: via_core::Digest,
        actual: via_core::Digest,
    },

    #[error(transparent)]
    Core(#[from] via_core::Error),

    #[error(transparent)]
    Bundle(#[from] via_bundle::Error),

    #[error(transparent)]
    Runtime(#[from] via_runtime::Error),
}

fn rolled_back(target: &Option<ReleaseId>) -> String {
    match target {
        Some(r) => format!(" (rolled back to release {r})"),
        None => String::new(),
    }
}

impl Error {
    pub(crate) fn io(path: impl AsRef<Path>, source: std::io::Error) -> Self {
        Error::Io {
            path: path.as_ref().display().to_string(),
            source,
        }
    }

    pub(crate) fn json(source: serde_json::Error) -> Self {
        Error::Json(source)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc3339_formatting_matches_known_instants() {
        assert_eq!(format_rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(format_rfc3339(1_000_000_000), "2001-09-09T01:46:40Z");
        // A leap day, which is where hand-rolled date math usually breaks.
        assert_eq!(format_rfc3339(1_709_164_800), "2024-02-29T00:00:00Z");
    }
}
