//! `SessionHandle` — port of deepseek
//! `packages/session/session-persistence/src/handle.ts` + in-process writer
//! tracking (`JsonlBackendTracker.writers`).
//!
//! - `SessionAccess::{Read, Write}` (`handle.ts:14`): read never takes
//!   ownership and works while another handle holds write; write is "the
//!   session's single mutator, which also reads its own log".
//! - Append batch must be contiguous; first event's seq MUST equal the
//!   stored next-seq; committed events are never rewritten (`handle.ts:85`).
//! - Persistence is best-effort; `flush()` is the sole durability barrier
//!   (`handle.ts:99-109`).
//! - `open(.., Write)` atomically claims single-writer ownership (in-process
//!   map + flock lease); an existing active owner rejects.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;

use super::event::{
    validate_event, OpenedSession, Seq, SessionEvent, SessionHeader, CORE_EVENT_TYPES,
};
use super::storage::{
    claim_write_lease, scan_log, AppendDurability, JsonlLog, ScanResult, StorageError,
};
use super::lease::SessionWriteLease;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionAccess {
    Read,
    Write,
}

#[derive(Debug, thiserror::Error)]
pub enum HandleError {
    #[error("session is open read-only")]
    ReadOnly,
    #[error("session already owned by another writer")]
    AlreadyOwned,
    #[error("handle closed")]
    Closed,
    #[error("non-contiguous append: expected seq {expected}, got {got}")]
    NonContiguous { expected: Seq, got: Seq },
    #[error("event validation: {0}")]
    InvalidEvent(#[from] super::event::EventError),
    #[error("storage: {0}")]
    Storage(#[from] StorageError),
    #[error("session not found: {0}")]
    NotFound(String),
}

/// In-process single-writer tracker (donor `JsonlBackendTracker.writers`):
/// one write handle per session id per process.
static WRITERS: Mutex<Option<HashMap<String, ()>>> = Mutex::new(None);

fn writers_map() -> std::sync::MutexGuard<'static, Option<HashMap<String, ()>>> {
    WRITERS.lock().unwrap_or_else(|p| p.into_inner())
}

fn claim_in_process(id: &str) -> bool {
    let mut guard = writers_map();
    let map = guard.get_or_insert_with(HashMap::new);
    if map.contains_key(id) {
        false
    } else {
        map.insert(id.to_string(), ());
        true
    }
}

fn release_in_process(id: &str) {
    if let Some(map) = writers_map().as_mut() {
        map.remove(id);
    }
}

/// Opened session handle. `Drop` on a write handle releases ownership.
pub struct SessionHandle {
    id: String,
    access: SessionAccess,
    log: JsonlLog,
    next_seq: Seq,
    /// Torn-tail repair state (deepseek `storage.ts:63-77`): set at open,
    /// executed (truncate + quarantine) before the first append.
    torn_truncate_to: Option<u64>,
    skipped_corrupt: usize,
    /// Held for the life of a write handle; None on read handles.
    _lease: Option<SessionWriteLease>,
}

impl SessionHandle {
    /// `create(header)` (`index.ts:135+`): new session dir + empty log.
    pub fn create(root: &Path, header: &SessionHeader) -> Result<SessionHandle, HandleError> {
        let dir = root.join(&header.id);
        let log = JsonlLog::new(dir);
        log.create(header)?;
        // A new session starts owned by its creator.
        let lease = claim_write_lease(log.dir()).map_err(Self::map_storage_err)?;
        if !claim_in_process(&header.id) {
            return Err(HandleError::AlreadyOwned);
        }
        Ok(SessionHandle {
            id: header.id.clone(),
            access: SessionAccess::Write,
            log,
            next_seq: 0,
            torn_truncate_to: None,
            skipped_corrupt: 0,
            _lease: Some(lease),
        })
    }

    /// Map lease rejection to the handle-level error, not a storage wrapper.
    fn map_storage_err(err: StorageError) -> HandleError {
        match err {
            StorageError::AlreadyOwned => HandleError::AlreadyOwned,
            other => HandleError::Storage(other),
        }
    }

    /// `open(id, access)` (`index.ts:135-202`).
    pub fn open(root: &Path, id: &str, access: SessionAccess) -> Result<SessionHandle, HandleError> {
        let dir = root.join(id);
        if !dir.is_dir() {
            return Err(HandleError::NotFound(id.to_string()));
        }
        let log = JsonlLog::new(dir);
        let scan: ScanResult = scan_log(&log.log_path()).map_err(StorageError::Io)?;
        match access {
            SessionAccess::Read => Ok(SessionHandle {
                id: id.to_string(),
                access,
                log,
                next_seq: scan.next_seq(),
                torn_truncate_to: None,
                skipped_corrupt: scan.skipped,
                _lease: None,
            }),
            SessionAccess::Write => {
                let lease = claim_write_lease(log.dir()).map_err(Self::map_storage_err)?;
                if !claim_in_process(id) {
                    return Err(HandleError::AlreadyOwned);
                }
                Ok(SessionHandle {
                    id: id.to_string(),
                    access,
                    log,
                    next_seq: scan.next_seq(),
                    // torn physical tail repair, scheduled for first append
                    torn_truncate_to: if scan.torn_tail { Some(scan.committed_bytes) } else { None },
                    skipped_corrupt: scan.skipped,
                    _lease: Some(lease),
                })
            }
        }
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn access(&self) -> SessionAccess {
        self.access
    }

    pub fn header(&self) -> Result<SessionHeader, HandleError> {
        Ok(self.log.read_header()?)
    }

    pub fn opened(&self) -> OpenedSession {
        OpenedSession {
            header: self.header().expect("header readable"),
            event_count: self.next_seq as usize,
        }
    }

    /// Events committed so far in this handle's view.
    pub fn next_seq(&self) -> Seq {
        self.next_seq
    }

    /// Read all complete events currently in the log (read handles may run
    /// concurrently with a writer elsewhere; they never see torn tails).
    pub fn read_all(&self) -> Result<Vec<SessionEvent>, HandleError> {
        let scan = scan_log(&self.log.log_path()).map_err(StorageError::Io)?;
        Ok(scan.events)
    }

    /// `handle.append(events)` (`handle.ts:85-97`): batch must be
    /// contiguous; first event's seq MUST equal the stored next-seq.
    /// Buffered (best-effort) durability; `flush()` is the barrier.
    pub fn append(&mut self, events: Vec<SessionEvent>) -> Result<(), HandleError> {
        self.append_with_durability(events, AppendDurability::Buffered)
    }

    /// Append that fsyncs file + parent dir before returning.
    pub fn append_durable(&mut self, events: Vec<SessionEvent>) -> Result<(), HandleError> {
        self.append_with_durability(events, AppendDurability::Durable)
    }

    fn append_with_durability(
        &mut self,
        mut events: Vec<SessionEvent>,
        durability: AppendDurability,
    ) -> Result<(), HandleError> {
        if self.access != SessionAccess::Write {
            return Err(HandleError::ReadOnly);
        }
        if events.is_empty() {
            return Ok(());
        }
        // The single mutator assigns contiguous seqs. A caller-supplied
        // nonzero seq must equal the expected next seq (donor contract,
        // handle.ts:85-97: "first event's seq MUST equal stored next-seq").
        for (i, ev) in events.iter_mut().enumerate() {
            let expected = self.next_seq + i as Seq;
            if ev.seq != 0 && ev.seq != expected {
                return Err(HandleError::NonContiguous { expected, got: ev.seq });
            }
            ev.seq = expected;
            validate_event(ev, &CORE_EVENT_TYPES)?;
        }

        // Torn-tail repair BEFORE the first mutation (deepseek
        // `persistContiguous`, storage.ts:318-343): truncate torn bytes, and
        // quarantine corrupt lines found at load.
        if let Some(to) = self.torn_truncate_to.take() {
            self.log.truncate_to(to)?;
        }
        if self.skipped_corrupt > 0 {
            self.skipped_corrupt = 0;
            self.log.quarantine_once()?;
        }

        self.log.append_batch(&events, durability)?;
        self.next_seq += events.len() as Seq;
        Ok(())
    }

    /// `flush()` (`handle.ts:99-109`): the sole durability barrier.
    pub fn flush(&self) -> Result<(), HandleError> {
        if self.access != SessionAccess::Write {
            return Err(HandleError::ReadOnly);
        }
        // Every append in this implementation writes through to the file;
        // Durable appends already fsynced. Buffered data is in the page
        // cache; sync it now.
        let f = std::fs::File::open(self.log.log_path()).map_err(StorageError::Io)?;
        f.sync_all().map_err(StorageError::Io)?;
        Ok(())
    }

    /// Count of corrupt (skipped) lines observed at open.
    pub fn corrupt_lines(&self) -> usize {
        self.skipped_corrupt
    }

    /// True while a torn physical tail awaits repair by the first append.
    pub fn torn_tail_pending(&self) -> bool {
        self.torn_truncate_to.is_some()
    }
}

impl Drop for SessionHandle {
    fn drop(&mut self) {
        if self.access == SessionAccess::Write {
            release_in_process(&self.id);
        }
    }
}

impl std::fmt::Debug for SessionHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionHandle")
            .field("id", &self.id)
            .field("access", &self.access)
            .field("next_seq", &self.next_seq)
            .finish()
    }
}
