//! Single-writer lease — port of deepseek
//! `packages/session/session-persistence-jsonl/src/lease.ts`.
//!
//! Cross-process single-writer via POSIX non-blocking `flock(2)` on
//! `session.lock` beside the log. Contention (EAGAIN/EWOULDBLOCK) is
//! `AlreadyOwned`. **No expiry** — a crashed holder's kernel releases the
//! lock automatically; a wedged live holder keeps it. The lock file is never
//! removed (stable inode). Readers never touch the lock.
//!
//! In-process single-writer is enforced by the `writers` map on
//! `LogStore` (storage.rs), mirroring the donor's `JsonlBackendTracker`.

use std::fs::{File, OpenOptions};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};

pub const LEASE_FILENAME: &str = "session.lock";

#[derive(Debug, thiserror::Error)]
pub enum LeaseError {
    #[error("session already owned by another writer")]
    AlreadyOwned,
    #[error("lease inode changed under the lock (stale path)")]
    InodeChanged,
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// Held for the whole life of a write handle. Drop releases the flock.
#[derive(Debug)]
pub struct SessionWriteLease {
    _file: File,
    path: PathBuf,
    locked_ino: (u64, u64),
}

fn inode_of(file: &File) -> std::io::Result<(u64, u64)> {
    // (st_dev, st_ino) via fstat — no NUL issues, no path races.
    use std::os::unix::fs::MetadataExt;
    let meta = file.metadata()?;
    Ok((meta.dev(), meta.ino()))
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
        let fd = file.as_raw_fd();
        let rc = unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) };
        if rc != 0 {
            let err = std::io::Error::last_os_error();
            // contention is EAGAIN (EWOULDBLOCK is its alias on unix); some
            // platforms report EACCES for non-blocking lock contention
            let code = err.raw_os_error().unwrap_or(0);
            return if code == libc::EAGAIN || code == libc::EACCES {
                Err(LeaseError::AlreadyOwned)
            } else {
                Err(LeaseError::Io(err))
            };
        }
        let locked_ino = inode_of(&file)?;
        let lease = Self { _file: file, path, locked_ino };
        // inode revalidation (`lease.ts:90-115`): verify the locked inode is
        // still the one at the lock path; retry once on mismatch.
        if lease.revalidate()? {
            Ok(lease)
        } else {
            Err(LeaseError::InodeChanged)
        }
    }

    fn revalidate(&self) -> Result<bool, LeaseError> {
        match std::fs::metadata(&self.path) {
            Ok(meta) => {
                use std::os::unix::fs::MetadataExt;
                Ok((meta.dev(), meta.ino()) == self.locked_ino)
            }
            // lock file removed under us: treat as stale path
            Err(_) => Ok(false),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}
