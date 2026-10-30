//! On-device release storage and durable agent state.
//!
//! The spec describes a straight-line deploy pipeline but never says what
//! survives a power cut halfway through it. This module is that answer.
//!
//! ```text
//! /var/lib/via/
//! ├── state.json           current / previous / candidate  (atomically replaced)
//! ├── journal.jsonl        append-only record of every transition
//! └── releases/
//!     ├── 44/              manifest, deployment.json, blobs/
//!     └── 45/
//! ```
//!
//! State is written by writing a temporary file, fsyncing it, and renaming it
//! over the old one. `rename` is atomic on POSIX, so a device losing power
//! mid-write comes back to either the old state or the new one, never to a
//! half-written file. There is deliberately no database here: the state is a
//! handful of fields and is re-derivable from the control plane, so a file a
//! field technician can read with `cat` is worth more than a query engine.

use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use via_core::ReleaseId;

use crate::Error;

pub const STATE_FILE: &str = "state.json";
pub const JOURNAL_FILE: &str = "journal.jsonl";
pub const RELEASES_DIR: &str = "releases";

/// Bumped if the on-disk state shape changes incompatibly.
pub const STATE_SCHEMA_VERSION: u32 = 1;

/// What the device believes about itself. Small, and rebuildable from the
/// control plane if it is ever lost.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AgentState {
    pub schema_version: u32,

    /// The release currently running, as far as the agent knows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current: Option<ReleaseId>,

    /// The last release known to have been healthy. Retained so rollback needs
    /// no network (spec sections 23, 25).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous: Option<ReleaseId>,

    /// A release being deployed right now.
    ///
    /// Its presence at startup means the agent died mid-deploy, which is
    /// exactly the case a straight-line pipeline cannot recover from. Recorded
    /// before anything is changed so recovery is possible at all.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub candidate: Option<ReleaseId>,

    /// What the control plane last told the device to run. Kept separately
    /// from `current` so the agent can tell "not converged yet" from
    /// "converged".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub desired: Option<ReleaseId>,
}

impl Default for AgentState {
    fn default() -> Self {
        Self {
            schema_version: STATE_SCHEMA_VERSION,
            current: None,
            previous: None,
            candidate: None,
            desired: None,
        }
    }
}

impl AgentState {
    /// Whether the device is running what it was told to run.
    ///
    /// A recorded candidate always means not converged, even when no desired
    /// release has been set. The candidate is the marker of a deploy that
    /// started and did not finish, and that is unfinished work regardless of
    /// what the control plane has asked for.
    pub fn converged(&self) -> bool {
        if self.candidate.is_some() {
            return false;
        }
        match (self.desired, self.current) {
            (None, _) => true,
            (Some(d), Some(c)) => d == c,
            (Some(_), None) => false,
        }
    }

    /// Releases that must not be deleted (spec section 26).
    pub fn pinned(&self) -> Vec<ReleaseId> {
        [self.current, self.previous, self.candidate].into_iter().flatten().collect()
    }
}

/// One entry in the append-only journal.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct JournalEntry {
    /// Caller-supplied timestamp. The agent passes a real clock reading; tests
    /// pass a fixed value so journals are comparable.
    pub at: String,
    pub event: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release: Option<ReleaseId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// The agent's data directory.
pub struct Store {
    root: PathBuf,
}

impl Store {
    /// Open or create a store rooted at `root`.
    pub fn open(root: impl Into<PathBuf>) -> Result<Self, Error> {
        let root = root.into();
        std::fs::create_dir_all(root.join(RELEASES_DIR)).map_err(|e| Error::io(&root, e))?;
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn release_dir(&self, release: ReleaseId) -> PathBuf {
        self.root.join(RELEASES_DIR).join(release.to_string())
    }

    /// Read the current state, or a default if the device has never deployed.
    ///
    /// A corrupt state file is an error rather than a silent reset: silently
    /// forgetting `previous` would silently remove the ability to roll back.
    pub fn load_state(&self) -> Result<AgentState, Error> {
        let path = self.root.join(STATE_FILE);
        match std::fs::read(&path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(AgentState::default()),
            Err(e) => Err(Error::io(&path, e)),
            Ok(bytes) => {
                let state: AgentState = serde_json::from_slice(&bytes)
                    .map_err(|e| Error::CorruptState { path: path.display().to_string(), source: e })?;
                if state.schema_version != STATE_SCHEMA_VERSION {
                    return Err(Error::UnsupportedStateSchema {
                        found: state.schema_version,
                        supported: STATE_SCHEMA_VERSION,
                    });
                }
                Ok(state)
            }
        }
    }

    /// Replace the state atomically.
    pub fn save_state(&self, state: &AgentState) -> Result<(), Error> {
        let path = self.root.join(STATE_FILE);
        let tmp = self.root.join(format!("{STATE_FILE}.tmp"));
        let bytes = serde_json::to_vec_pretty(state).map_err(Error::json)?;

        let mut file = std::fs::File::create(&tmp).map_err(|e| Error::io(&tmp, e))?;
        file.write_all(&bytes).map_err(|e| Error::io(&tmp, e))?;
        // Without this the rename can land before the contents do, and a power
        // cut leaves a correctly named, empty state file.
        file.sync_all().map_err(|e| Error::io(&tmp, e))?;
        drop(file);

        std::fs::rename(&tmp, &path).map_err(|e| Error::io(&path, e))?;

        // Also fsync the directory, or the rename itself may not survive.
        if let Ok(dir) = std::fs::File::open(&self.root) {
            let _ = dir.sync_all();
        }
        Ok(())
    }

    /// Append to the journal. Best-effort ordering, durable per line.
    pub fn journal(&self, entry: &JournalEntry) -> Result<(), Error> {
        let path = self.root.join(JOURNAL_FILE);
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|e| Error::io(&path, e))?;
        let mut line = serde_json::to_vec(entry).map_err(Error::json)?;
        line.push(b'\n');
        file.write_all(&line).map_err(|e| Error::io(&path, e))?;
        file.sync_data().map_err(|e| Error::io(&path, e))?;
        Ok(())
    }

    pub fn read_journal(&self) -> Result<Vec<JournalEntry>, Error> {
        let path = self.root.join(JOURNAL_FILE);
        match std::fs::read_to_string(&path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(Error::io(&path, e)),
            Ok(body) => Ok(body
                .lines()
                .filter(|l| !l.trim().is_empty())
                .filter_map(|l| serde_json::from_str(l).ok())
                .collect()),
        }
    }

    /// Releases present on disk, ascending.
    pub fn stored_releases(&self) -> Result<Vec<ReleaseId>, Error> {
        let dir = self.root.join(RELEASES_DIR);
        let mut out = Vec::new();
        let entries = match std::fs::read_dir(&dir) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
            Err(e) => return Err(Error::io(&dir, e)),
            Ok(e) => e,
        };
        for entry in entries.flatten() {
            if let Some(id) = entry.file_name().to_str().and_then(|n| n.parse::<u64>().ok()) {
                out.push(ReleaseId(id));
            }
        }
        out.sort();
        Ok(out)
    }

    pub fn has_release(&self, release: ReleaseId) -> bool {
        self.release_dir(release).join("deployment.json").is_file()
    }

    /// Delete releases that are neither current, previous, nor candidate.
    ///
    /// Returns what was removed. Retention is deliberately dull: the two
    /// releases that matter are kept and everything else goes, which is enough
    /// to guarantee a local rollback without unbounded disk growth
    /// (spec sections 23, 26).
    pub fn prune(&self, state: &AgentState) -> Result<Vec<ReleaseId>, Error> {
        let keep = state.pinned();
        let mut removed = Vec::new();
        for release in self.stored_releases()? {
            if keep.contains(&release) {
                continue;
            }
            let dir = self.release_dir(release);
            std::fs::remove_dir_all(&dir).map_err(|e| Error::io(&dir, e))?;
            removed.push(release);
        }
        Ok(removed)
    }

    /// Delete a release's blobs, keeping its manifest and deployment manifest.
    ///
    /// Once images are loaded, Docker's own content store holds them, and the
    /// bundle's copy is a second copy of the same bytes. On a device with a
    /// small disk that is the difference between keeping two releases and
    /// keeping none. The manifest stays, so the release is still identifiable
    /// and its deployment still applicable -- which is all a rollback needs
    /// while the images remain loaded.
    pub fn drop_blobs(&self, release: ReleaseId) -> Result<u64, Error> {
        let dir = self.release_dir(release).join("blobs");
        if !dir.is_dir() {
            return Ok(0);
        }
        let freed = dir_size(&dir);
        std::fs::remove_dir_all(&dir).map_err(|e| Error::io(&dir, e))?;
        Ok(freed)
    }

    /// Whether a release still has its blobs, and could therefore reload its
    /// images without a network.
    pub fn has_blobs(&self, release: ReleaseId) -> bool {
        let dir = self.release_dir(release).join("blobs");
        std::fs::read_dir(&dir).map(|mut d| d.next().is_some()).unwrap_or(false)
    }

    /// Free bytes on the filesystem holding the store.
    ///
    /// Checked before accepting a bundle: filling the disk on an unattended
    /// device is unrecoverable without a site visit (spec section 26).
    pub fn available_bytes(&self) -> Option<u64> {
        available_bytes(&self.root)
    }
}

fn dir_size(dir: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else { return 0 };
    entries
        .flatten()
        .filter_map(|e| e.metadata().ok())
        .map(|m| if m.is_dir() { 0 } else { m.len() })
        .sum()
}

#[cfg(unix)]
fn available_bytes(path: &Path) -> Option<u64> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let c_path = CString::new(path.as_os_str().as_bytes()).ok()?;
    // SAFETY: `c_path` is a valid NUL-terminated string that outlives the
    // call, and `stat` is only read after a success return.
    unsafe {
        let mut stat: libc::statvfs = std::mem::zeroed();
        if libc::statvfs(c_path.as_ptr(), &mut stat) != 0 {
            return None;
        }
        Some(stat.f_bavail as u64 * stat.f_frsize as u64)
    }
}

#[cfg(not(unix))]
fn available_bytes(_path: &Path) -> Option<u64> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::TempDir::new().unwrap();
        let store = Store::open(dir.path()).unwrap();
        (dir, store)
    }

    #[test]
    fn a_fresh_device_has_empty_state_rather_than_an_error() {
        let (_d, s) = store();
        let state = s.load_state().unwrap();
        assert!(state.current.is_none());
        assert!(state.converged(), "nothing desired yet means nothing to converge to");
    }

    #[test]
    fn state_round_trips() {
        let (_d, s) = store();
        let state = AgentState {
            current: Some(ReleaseId(45)),
            previous: Some(ReleaseId(44)),
            desired: Some(ReleaseId(45)),
            ..Default::default()
        };
        s.save_state(&state).unwrap();
        let back = s.load_state().unwrap();
        assert_eq!(back.current, Some(ReleaseId(45)));
        assert_eq!(back.previous, Some(ReleaseId(44)));
        assert!(back.converged());
    }

    #[test]
    fn a_candidate_means_not_converged() {
        // This is the state a device wakes up in after dying mid-deploy.
        let state = AgentState {
            current: Some(ReleaseId(44)),
            desired: Some(ReleaseId(44)),
            candidate: Some(ReleaseId(45)),
            ..Default::default()
        };
        assert!(!state.converged());
    }

    #[test]
    fn a_corrupt_state_file_is_an_error_not_a_silent_reset() {
        // Silently defaulting would silently discard the rollback target.
        let (dir, s) = store();
        std::fs::write(dir.path().join(STATE_FILE), b"{ not json").unwrap();
        assert!(matches!(s.load_state(), Err(Error::CorruptState { .. })));
    }

    #[test]
    fn prune_keeps_current_previous_and_candidate() {
        let (_d, s) = store();
        for id in [41u64, 42, 43, 44, 45] {
            std::fs::create_dir_all(s.release_dir(ReleaseId(id))).unwrap();
            std::fs::write(s.release_dir(ReleaseId(id)).join("deployment.json"), b"{}").unwrap();
        }
        let state = AgentState {
            current: Some(ReleaseId(45)),
            previous: Some(ReleaseId(44)),
            ..Default::default()
        };
        let removed = s.prune(&state).unwrap();
        assert_eq!(removed, vec![ReleaseId(41), ReleaseId(42), ReleaseId(43)]);
        assert!(s.has_release(ReleaseId(44)), "the rollback target must survive pruning");
        assert!(s.has_release(ReleaseId(45)));
    }

    #[test]
    fn journal_appends_and_reads_back_in_order() {
        let (_d, s) = store();
        for (i, event) in ["deploy_started", "images_loaded", "active"].iter().enumerate() {
            s.journal(&JournalEntry {
                at: format!("2026-08-22T13:0{i}:00Z"),
                event: event.to_string(),
                release: Some(ReleaseId(45)),
                detail: None,
            })
            .unwrap();
        }
        let entries = s.read_journal().unwrap();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].event, "deploy_started");
        assert_eq!(entries[2].event, "active");
    }

    #[test]
    fn dropping_blobs_keeps_the_release_identifiable() {
        let (_d, s) = store();
        let r = ReleaseId(44);
        std::fs::create_dir_all(s.release_dir(r).join("blobs")).unwrap();
        std::fs::write(s.release_dir(r).join("deployment.json"), b"{}").unwrap();
        std::fs::write(s.release_dir(r).join("blobs").join("sha256-aa"), vec![0u8; 4096]).unwrap();

        assert!(s.has_blobs(r));
        let freed = s.drop_blobs(r).unwrap();
        assert_eq!(freed, 4096);
        assert!(!s.has_blobs(r));
        assert!(s.has_release(r), "the release must still be known after its blobs go");
    }

    #[test]
    fn available_bytes_reports_something_on_this_host() {
        let (_d, s) = store();
        assert!(s.available_bytes().unwrap_or(0) > 0);
    }
}
