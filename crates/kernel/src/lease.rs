//! Single-writer lease — port of deepseek
//! `packages/session/session-persistence-jsonl/src/lease.ts`.
//!
//! Cross-process single-writer via a non-blocking exclusive lock on
//! `session.lock` beside the log — the std file-lock API
//! (`File::try_lock`, stable ≥1.89), which is `flock(2)` on unix and
//! `LockFileEx` on Windows: same advisory semantics on both, no libc.
//! Contention is `AlreadyOwned`. **No expiry** — a crashed holder's
//! kernel releases the lock automatically; a wedged live holder keeps it.
//! The lock file is never removed (stable identity). Readers never touch
//! the lock.
//!
//! In-process single-writer is enforced by the `writers` map on
//! `LogStore` (storage.rs), mirroring the donor's `JsonlBackendTracker`.

use std::fs::{File, OpenOptions};
use std::path::{Path, PathBuf};

pub const LEASE_FILENAME: &str = "session.lock";

#[derive(Debug, thiserror::Error)]
pub enum LeaseError {
    #[error("session already owned by another writer")]
    AlreadyOwned,
    #[error("lease identity changed under the lock (stale path)")]
    InodeChanged,
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// Held for the whole life of a write handle. Drop releases the lock.
#[derive(Debug)]
pub struct SessionWriteLease {
    _file: File,
    path: PathBuf,
    locked_identity: LockIdentity,
}

/// The locked file's identity, for revalidation against the path (the
/// donor's inode check). Unix: (st_dev, st_ino). Windows: (creation
/// time, size) — the lock file is never written after creation, so a
/// recreated file (the stale-path hazard) changes both.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
// the Windows variant is constructed only under cfg(windows); the enum is
// compiled everywhere so the revalidation types match on both platforms
#[allow(dead_code)]
enum LockIdentity {
    Unix(u64, u64),
    Windows(u64, u64),
    Unavailable,
}

fn identity_of(file: &File) -> std::io::Result<LockIdentity> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let meta = file.metadata()?;
        // (st_dev, st_ino) via fstat — no NUL issues, no path races.
        Ok(LockIdentity::Unix(meta.dev(), meta.ino()))
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        let meta = file.metadata()?;
        Ok(LockIdentity::Windows(meta.creation_time(), meta.len()))
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = file;
        Ok(LockIdentity::Unavailable)
    }
}

impl SessionWriteLease {
    /// Atomically claim single-writer ownership; an existing active owner
    /// rejects (`index.ts:163-165`).
    pub fn acquire(session_dir: &Path) -> Result<Self, LeaseError> {
        let path = session_dir.join(LEASE_FILENAME);
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .read(true)
            .open(&path)?;
        match file.try_lock() {
            Ok(()) => {}
            // contention: another live writer holds the lease
            Err(std::fs::TryLockError::WouldBlock) => {
                return Err(LeaseError::AlreadyOwned);
            }
            Err(std::fs::TryLockError::Error(e)) => return Err(LeaseError::Io(e)),
        }
        let locked_identity = identity_of(&file)?;
        let lease = Self { _file: file, path, locked_identity };
        // identity revalidation (`lease.ts:90-115`): verify the locked file
        // is still the one at the lock path; a mismatch is a stale path.
        if lease.revalidate()? {
            Ok(lease)
        } else {
            Err(LeaseError::InodeChanged)
        }
    }

    fn revalidate(&self) -> Result<bool, LeaseError> {
        match File::open(&self.path).and_then(|f| identity_of(&f)) {
            Ok(identity) => Ok(identity == self.locked_identity || identity == LockIdentity::Unavailable),
            // lock file removed under us: treat as stale path
            Err(_) => Ok(false),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

#[cfg(all(test, windows))]
mod windows_probe {
    use super::*;

    /// First-bring-up probe: isolates which step of the lease path fails
    /// on a real Windows runner (open / try_lock / identity / revalidate).
    /// Remove once the lease is green on CI.
    #[test]
    fn lease_probe_windows_step_isolation() {
        let td = tempfile::tempdir().unwrap();
        let dir = td.path().join("sess-probe");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(LEASE_FILENAME);
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .read(true)
            .open(&path)
            .expect("STEP open");
        file.try_lock()
            .map_err(|e| match e {
                std::fs::TryLockError::WouldBlock => "STEP try_lock: WouldBlock".to_string(),
                std::fs::TryLockError::Error(io) => format!("STEP try_lock: {io}"),
            })
            .unwrap();
        let id = identity_of(&file).expect("STEP identity");
        let file2 = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .read(true)
            .open(&path)
            .expect("STEP reopen while held");
        match file2.try_lock() {
            Ok(()) => panic!("STEP second lock unexpectedly succeeded"),
            Err(std::fs::TryLockError::WouldBlock) => {}
            Err(std::fs::TryLockError::Error(io)) => panic!("STEP second lock: {io}"),
        }
        drop(file2);
        drop(id);
        let lease = SessionWriteLease::acquire(&dir).expect("STEP acquire end-to-end");
        assert_eq!(lease.path(), path);
    }
}
