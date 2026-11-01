//! Resumable bundle reception.
//!
//! A release is transferred whole and atomically: a device never holds a
//! fragment of a release, only a fragment of a *download*. Those are different
//! things, and this module is about the second one.
//!
//! ```text
//! incoming/44.part     bytes received so far, append-only
//! incoming/44.json     what is expected: size, digest, and where it came from
//! ```
//!
//! The resume offset is the length of the partial file, which is safe because
//! writes are append-only: a crash truncates the tail, it does not corrupt the
//! middle. The whole-file digest is verified before the bundle is handed on,
//! so a torn tail is caught rather than trusted.
//!
//! Nothing here knows about QUIC, HTTP, or a USB stick. A transport calls
//! [`Reception::offset`] to learn where to resume, feeds bytes to
//! [`Reception::append`], and calls [`Reception::finish`].

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use via_core::{Digest, ReleaseId};

use crate::Error;

pub const INCOMING_DIR: &str = "incoming";

/// How often the partial file is flushed to stable storage.
///
/// A crash loses at most this much progress. Syncing every chunk would make a
/// large transfer painfully slow on the SD cards these devices often run on.
const SYNC_INTERVAL_BYTES: u64 = 8 * 1024 * 1024;

/// What the device expects a transfer to deliver.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Expectation {
    pub release: ReleaseId,
    /// Total size of the bundle file.
    pub size: u64,
    /// Digest of the whole bundle file, which is the release artifact's
    /// identity. Verified on completion.
    pub digest: Digest,
}

/// Progress of one transfer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Progress {
    pub release: ReleaseId,
    pub received: u64,
    pub total: u64,
}

impl Progress {
    pub fn complete(&self) -> bool {
        self.received >= self.total
    }

    pub fn percent(&self) -> f64 {
        if self.total == 0 {
            100.0
        } else {
            100.0 * self.received as f64 / self.total as f64
        }
    }

    pub fn remaining(&self) -> u64 {
        self.total.saturating_sub(self.received)
    }
}

impl std::fmt::Display for Progress {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "release {}: {:.1} MiB of {:.1} MiB ({:.0}%)",
            self.release,
            self.received as f64 / 1_048_576.0,
            self.total as f64 / 1_048_576.0,
            self.percent()
        )
    }
}

/// Manages partially received bundles under a store root.
pub struct Reception {
    root: PathBuf,
}

impl Reception {
    pub fn open(store_root: impl AsRef<Path>) -> Result<Self, Error> {
        let root = store_root.as_ref().join(INCOMING_DIR);
        std::fs::create_dir_all(&root).map_err(|e| Error::io(&root, e))?;
        Ok(Self { root })
    }

    fn part_path(&self, release: ReleaseId) -> PathBuf {
        self.root.join(format!("{release}.part"))
    }

    fn meta_path(&self, release: ReleaseId) -> PathBuf {
        self.root.join(format!("{release}.json"))
    }

    /// Declare what is about to be transferred.
    ///
    /// If a partial transfer of the same release exists but expected something
    /// different -- a rebuilt release with the same number, most likely -- the
    /// old bytes are discarded rather than silently mixed with new ones.
    pub fn begin(&self, expect: &Expectation) -> Result<Progress, Error> {
        if let Some(existing) = self.expectation(expect.release)? {
            if &existing != expect {
                self.discard(expect.release)?;
            }
        }
        let meta = self.meta_path(expect.release);
        std::fs::write(
            &meta,
            serde_json::to_vec_pretty(expect).map_err(Error::json)?,
        )
        .map_err(|e| Error::io(&meta, e))?;

        Ok(Progress {
            release: expect.release,
            received: self.offset(expect.release),
            total: expect.size,
        })
    }

    /// What a transfer expected to deliver, if one is in progress.
    pub fn expectation(&self, release: ReleaseId) -> Result<Option<Expectation>, Error> {
        let path = self.meta_path(release);
        match std::fs::read(&path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(Error::io(&path, e)),
            // A corrupt sidecar is not worth an error: discard it and let the
            // transfer start again from zero.
            Ok(bytes) => Ok(serde_json::from_slice(&bytes).ok()),
        }
    }

    /// Byte offset to resume from.
    ///
    /// This is the actual length of the partial file, never a separately
    /// recorded counter, so it cannot disagree with what is on disk.
    pub fn offset(&self, release: ReleaseId) -> u64 {
        std::fs::metadata(self.part_path(release))
            .map(|m| m.len())
            .unwrap_or(0)
    }

    /// Progress of an in-flight transfer.
    pub fn progress(&self, release: ReleaseId) -> Result<Option<Progress>, Error> {
        Ok(self.expectation(release)?.map(|e| Progress {
            release,
            received: self.offset(release),
            total: e.size,
        }))
    }

    /// Every transfer currently in progress, so an agent can resume them all
    /// after a restart.
    pub fn in_progress(&self) -> Result<Vec<Progress>, Error> {
        let mut out = Vec::new();
        let entries = match std::fs::read_dir(&self.root) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
            Err(e) => return Err(Error::io(&self.root, e)),
            Ok(e) => e,
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(stem) = name.to_str().and_then(|n| n.strip_suffix(".json")) else {
                continue;
            };
            let Ok(id) = stem.parse::<u64>() else {
                continue;
            };
            if let Some(p) = self.progress(ReleaseId(id))? {
                out.push(p);
            }
        }
        out.sort_by_key(|p| p.release);
        Ok(out)
    }

    /// Append received bytes.
    ///
    /// `at` is the offset the caller believes it is writing to; a mismatch
    /// with the file's actual length is rejected rather than producing a
    /// corrupt file that would only fail its digest check much later.
    pub fn append(&self, release: ReleaseId, at: u64, bytes: &[u8]) -> Result<Progress, Error> {
        let expect = self
            .expectation(release)?
            .ok_or(Error::NoTransferInProgress { release })?;

        let path = self.part_path(release);
        let current = self.offset(release);
        if at != current {
            return Err(Error::TransferOffsetMismatch {
                release,
                expected: current,
                got: at,
            });
        }
        if current + bytes.len() as u64 > expect.size {
            return Err(Error::TransferOverrun {
                release,
                size: expect.size,
                would_be: current + bytes.len() as u64,
            });
        }

        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .map_err(|e| Error::io(&path, e))?;
        file.write_all(bytes).map_err(|e| Error::io(&path, e))?;

        let received = current + bytes.len() as u64;
        // Sync on interval boundaries and at the end, so a crash costs at most
        // one interval of re-download.
        if received >= expect.size
            || received / SYNC_INTERVAL_BYTES != current / SYNC_INTERVAL_BYTES
        {
            file.sync_data().map_err(|e| Error::io(&path, e))?;
        }

        Ok(Progress {
            release,
            received,
            total: expect.size,
        })
    }

    /// Consume a whole stream into the partial file, resuming where it left
    /// off. The reader must already be positioned at [`Self::offset`].
    pub fn append_stream(
        &self,
        release: ReleaseId,
        mut source: impl Read,
    ) -> Result<Progress, Error> {
        let mut buf = vec![0u8; 256 * 1024];
        let mut at = self.offset(release);
        let mut progress = self
            .progress(release)?
            .ok_or(Error::NoTransferInProgress { release })?;

        loop {
            let n = source
                .read(&mut buf)
                .map_err(|e| Error::io(self.part_path(release), e))?;
            if n == 0 {
                return Ok(progress);
            }
            progress = self.append(release, at, &buf[..n])?;
            at = progress.received;
        }
    }

    /// Verify the completed transfer and return the bundle file's path.
    ///
    /// The digest covers the whole file, so this is where a truncated,
    /// reordered or tampered transfer is caught -- before anything is unpacked.
    pub fn finish(&self, release: ReleaseId) -> Result<PathBuf, Error> {
        let expect = self
            .expectation(release)?
            .ok_or(Error::NoTransferInProgress { release })?;

        let received = self.offset(release);
        if received != expect.size {
            return Err(Error::TransferIncomplete {
                release,
                received,
                expected: expect.size,
            });
        }

        let path = self.part_path(release);
        let actual = digest_file(&path)?;
        if actual != expect.digest {
            // Do not keep bytes that failed verification: on the next attempt
            // they would look like resumable progress.
            self.discard(release)?;
            return Err(Error::TransferDigestMismatch {
                release,
                expected: expect.digest,
                actual,
            });
        }

        let done = self.root.join(format!("{release}.bundle.zst"));
        std::fs::rename(&path, &done).map_err(|e| Error::io(&done, e))?;
        let _ = std::fs::remove_file(self.meta_path(release));
        Ok(done)
    }

    /// Throw away a transfer, complete or not.
    pub fn discard(&self, release: ReleaseId) -> Result<(), Error> {
        for path in [
            self.part_path(release),
            self.meta_path(release),
            self.root.join(format!("{release}.bundle.zst")),
        ] {
            match std::fs::remove_file(&path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(Error::io(&path, e)),
            }
        }
        Ok(())
    }
}

/// SHA-256 of a file, read in chunks so a multi-gigabyte bundle never lands in
/// memory on a small device.
pub fn digest_file(path: impl AsRef<Path>) -> Result<Digest, Error> {
    use sha2::{Digest as _, Sha256};

    let path = path.as_ref();
    let mut file = std::fs::File::open(path).map_err(|e| Error::io(path, e))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        let n = file.read(&mut buf).map_err(|e| Error::io(path, e))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Digest::parse(format!("sha256:{}", hex::encode(hasher.finalize()))).map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> (tempfile::TempDir, Reception) {
        let dir = tempfile::TempDir::new().unwrap();
        let r = Reception::open(dir.path()).unwrap();
        (dir, r)
    }

    fn expectation(payload: &[u8]) -> Expectation {
        Expectation {
            release: ReleaseId(44),
            size: payload.len() as u64,
            digest: Digest::sha256_of(payload),
        }
    }

    #[test]
    fn a_transfer_completes_and_verifies() {
        let (_d, r) = setup();
        let payload = b"a complete bundle".repeat(100);
        let expect = expectation(&payload);

        r.begin(&expect).unwrap();
        let progress = r.append(ReleaseId(44), 0, &payload).unwrap();
        assert!(progress.complete());

        let path = r.finish(ReleaseId(44)).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), payload);
    }

    #[test]
    fn an_interrupted_transfer_resumes_where_it_stopped() {
        // The whole point: a 90% download that dies must not start over.
        let (_d, r) = setup();
        let payload: Vec<u8> = (0..50_000u32).map(|i| (i % 251) as u8).collect();
        let expect = expectation(&payload);
        r.begin(&expect).unwrap();

        let cut = 42_000;
        r.append(ReleaseId(44), 0, &payload[..cut]).unwrap();

        // Simulate the agent restarting: reopen and ask where to resume.
        let r2 = Reception::open(_d.path()).unwrap();
        let offset = r2.offset(ReleaseId(44));
        assert_eq!(offset, cut as u64);
        assert_eq!(
            r2.progress(ReleaseId(44)).unwrap().unwrap().remaining(),
            8_000
        );

        r2.append(ReleaseId(44), offset, &payload[cut..]).unwrap();
        let path = r2.finish(ReleaseId(44)).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), payload);
    }

    #[test]
    fn a_wrong_offset_is_refused_rather_than_corrupting_the_file() {
        let (_d, r) = setup();
        let payload = vec![7u8; 1000];
        r.begin(&expectation(&payload)).unwrap();
        r.append(ReleaseId(44), 0, &payload[..500]).unwrap();

        let err = r.append(ReleaseId(44), 0, &payload[..10]).unwrap_err();
        assert!(matches!(err, Error::TransferOffsetMismatch { .. }), "{err}");
    }

    #[test]
    fn a_transfer_that_would_overrun_its_declared_size_is_refused() {
        let (_d, r) = setup();
        let payload = vec![1u8; 100];
        r.begin(&expectation(&payload)).unwrap();
        let err = r.append(ReleaseId(44), 0, &[1u8; 200]).unwrap_err();
        assert!(matches!(err, Error::TransferOverrun { .. }), "{err}");
    }

    #[test]
    fn finishing_early_is_refused() {
        let (_d, r) = setup();
        let payload = vec![3u8; 1000];
        r.begin(&expectation(&payload)).unwrap();
        r.append(ReleaseId(44), 0, &payload[..400]).unwrap();

        let err = r.finish(ReleaseId(44)).unwrap_err();
        assert!(matches!(err, Error::TransferIncomplete { .. }), "{err}");
    }

    #[test]
    fn corrupted_bytes_fail_the_digest_and_are_not_kept_as_progress() {
        // Otherwise the next attempt would resume on top of bad bytes forever.
        let (_d, r) = setup();
        let payload = vec![9u8; 2000];
        let mut expect = expectation(&payload);
        expect.digest = Digest::sha256_of(b"something else entirely");
        r.begin(&expect).unwrap();
        r.append(ReleaseId(44), 0, &payload).unwrap();

        let err = r.finish(ReleaseId(44)).unwrap_err();
        assert!(matches!(err, Error::TransferDigestMismatch { .. }), "{err}");
        assert_eq!(
            r.offset(ReleaseId(44)),
            0,
            "failed bytes must not look like progress"
        );
    }

    #[test]
    fn a_rebuilt_release_discards_the_old_partial_transfer() {
        let (_d, r) = setup();
        let first = vec![1u8; 1000];
        r.begin(&expectation(&first)).unwrap();
        r.append(ReleaseId(44), 0, &first[..600]).unwrap();
        assert_eq!(r.offset(ReleaseId(44)), 600);

        // Same release number, different artifact.
        let second = vec![2u8; 900];
        let progress = r.begin(&expectation(&second)).unwrap();
        assert_eq!(
            progress.received, 0,
            "bytes from a different artifact must not be reused"
        );
        assert_eq!(progress.total, 900);
    }

    #[test]
    fn resuming_the_same_expectation_keeps_progress() {
        let (_d, r) = setup();
        let payload = vec![5u8; 1000];
        let expect = expectation(&payload);
        r.begin(&expect).unwrap();
        r.append(ReleaseId(44), 0, &payload[..700]).unwrap();

        let progress = r.begin(&expect).unwrap();
        assert_eq!(
            progress.received, 700,
            "re-declaring the same transfer must not restart it"
        );
    }

    #[test]
    fn append_stream_resumes_from_the_current_offset() {
        let (_d, r) = setup();
        let payload: Vec<u8> = (0..300_000u32).map(|i| (i % 253) as u8).collect();
        r.begin(&expectation(&payload)).unwrap();
        r.append(ReleaseId(44), 0, &payload[..100_000]).unwrap();

        // A transport would seek its source to `offset` before handing it over.
        let offset = r.offset(ReleaseId(44)) as usize;
        let progress = r.append_stream(ReleaseId(44), &payload[offset..]).unwrap();
        assert!(progress.complete());
        let path = r.finish(ReleaseId(44)).unwrap();
        assert_eq!(digest_file(&path).unwrap(), Digest::sha256_of(&payload));
    }

    #[test]
    fn in_progress_lists_transfers_to_resume_after_a_restart() {
        let (_d, r) = setup();
        for id in [44u64, 45] {
            let payload = vec![id as u8; 1000];
            r.begin(&Expectation {
                release: ReleaseId(id),
                size: 1000,
                digest: Digest::sha256_of(&payload),
            })
            .unwrap();
            r.append(ReleaseId(id), 0, &payload[..100]).unwrap();
        }
        let list = Reception::open(_d.path()).unwrap().in_progress().unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].release, ReleaseId(44));
        assert_eq!(list[0].received, 100);
    }

    #[test]
    fn appending_without_beginning_is_an_error() {
        let (_d, r) = setup();
        let err = r.append(ReleaseId(44), 0, b"stray bytes").unwrap_err();
        assert!(matches!(err, Error::NoTransferInProgress { .. }), "{err}");
    }
}
