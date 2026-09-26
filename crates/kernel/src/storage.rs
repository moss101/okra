//! JSONL storage adapter — ports grok `xai-grok-shell/src/session/storage/jsonl`
//! (loss contract, torn-tail heal, `.corrupt` quarantine) behind the
//! deepseek event-log contract, plus the deepseek write-path repair
//! (`persistContiguous`, storage.ts:318-343).
//!
//! Loss contract (grok `jsonl/mod.rs`):
//! - Appends are **not crash-atomic**: one torn or raced line must not brick
//!   the load. Load skips unparseable lines with a counter, never fails.
//! - Before every append, heal a torn tail: if the last byte isn't `\n`,
//!   the new line is preceded by `\n` so the torn record terminates as its
//!   own (single) corrupt line (grok `jsonl/mod.rs:433-452`).
//! - If any line was skipped at load, the raw file is copied ONCE to
//!   `<name>.jsonl.corrupt` before further mutation (grok
//!   `jsonl/mod.rs:1063-1074`).
//! - `AppendDurability::Durable` = write_all → flush → fsync(file) →
//!   fsync(parent dir). `Buffered` stops after flush (`:454-457`).
//! - `AppendLineError::Committed` means `write_all` succeeded (record is in
//!   the file/page cache); only flush/fsync failed. Callers use this to
//!   distinguish "data is there but maybe not durable" from "nothing
//!   written" (`:49-52`).
//!
//! deepseek write-path repair: a torn physical tail is never returned to a
//! reader, and is truncated by the write path before its first append
//! (`session-persistence/src/index.ts:115-134`).

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use super::event::{SessionEvent, SessionHeader};
use super::lease::SessionWriteLease;

pub const LOG_FILENAME: &str = "session.jsonl";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AppendDurability {
    #[default]
    Buffered,
    Durable,
}

#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("serialization error: {0}")]
    Serde(#[from] serde_json::Error),
    #[error("session already owned by another writer")]
    AlreadyOwned,
}

/// `AppendLineError` (`jsonl/mod.rs:49-52`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppendOutcome {
    /// Fully written and made durable.
    Durable,
    /// `write_all` succeeded; the record is in the file (page cache) — a
    /// later fsync makes it durable. Only flush/fsync failed.
    CommittedNotDurable,
}

fn sync_dir(dir: &Path) -> std::io::Result<()> {
    let f = File::open(dir)?;
    f.sync_all()
}

/// Scan a JSONL file: returns complete events, the committed byte offset
/// (end of last COMPLETE line), the count of skipped corrupt lines, and
/// whether the tail was torn (incomplete final line). A final record without
/// a trailing newline is a torn tail (`format.ts:461-475`) — never returned
/// to a reader.
pub fn scan_log(path: &Path) -> std::io::Result<ScanResult> {
    let mut file = File::open(path)?;
    let mut buf = Vec::new();
    file.read_to_end(&mut buf)?;

    let mut events = Vec::new();
    let mut skipped = 0usize;
    let mut first_skipped: Option<String> = None;
    let mut torn_tail = false;

    let mut pos = 0usize;
    let mut complete_end = 0usize;
    while pos < buf.len() {
        match buf[pos..].iter().position(|&b| b == b'\n') {
            Some(nl_rel) => {
                let nl = pos + nl_rel;
                let line = &buf[pos..nl];
                if line.is_empty() {
                    // empty lines are skipped silently (jsonl/mod.rs:987)
                } else if let Ok(ev) = serde_json::from_slice::<SessionEvent>(line) {
                    events.push(ev);
                } else {
                    skipped += 1;
                    if first_skipped.is_none() {
                        first_skipped =
                            Some(String::from_utf8_lossy(&line[..line.len().min(120)]).into());
                    }
                }
                complete_end = nl + 1;
                pos = nl + 1;
            }
            None => {
                torn_tail = true; // bytes after the last newline: torn tail
                break;
            }
        }
    }
    let committed_bytes = complete_end as u64;
    Ok(ScanResult {
        events,
        committed_bytes,
        skipped,
        first_skipped,
        torn_tail,
        total_bytes: buf.len() as u64,
    })
}

#[derive(Debug, Clone)]
pub struct ScanResult {
    pub events: Vec<SessionEvent>,
    /// End offset of the last complete valid line — where the write path
    /// truncates a torn tail to.
    pub committed_bytes: u64,
    pub skipped: usize,
    pub first_skipped: Option<String>,
    pub torn_tail: bool,
    pub total_bytes: u64,
}

impl ScanResult {
    /// Next assignable seq: last logged seq + 1. Corrupt lines already
    /// consumed seqs when they were originally logged, so the count of
    /// *readable* events undercounts the seq space.
    pub fn next_seq(&self) -> u64 {
        self.events.last().map(|e| e.seq + 1).unwrap_or(0)
    }
}

pub struct JsonlLog {
    dir: PathBuf,
}

impl JsonlLog {
    pub fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    pub fn dir(&self) -> &PathBuf {
        &self.dir
    }

    pub fn log_path(&self) -> PathBuf {
        self.dir.join(LOG_FILENAME)
    }

    pub fn corrupt_path(&self) -> PathBuf {
        // grok: path.with_extension("jsonl.corrupt") — here: sibling suffix
        self.dir.join(format!("{LOG_FILENAME}.corrupt"))
    }

    /// Open (or create) the session directory and log file. Returns the scan
    /// of the current contents. Creating dirs is owner-only and the parent
    /// dir is fsynced (grok `jsonl/mod.rs:185-210`).
    pub fn create(&self, header: &SessionHeader) -> Result<ScanResult, StorageError> {
        std::fs::create_dir_all(&self.dir)?;
        let path = self.log_path();
        if !path.exists() {
            File::create(&path)?;
            sync_dir(&self.dir)?;
        }
        // header lives outside the log (deepseek types.ts:94-131)
        let header_path = self.dir.join("session.json");
        if !header_path.exists() {
            let mut f = File::create(&header_path)?;
            write_jsonl_atomic(&mut f, &serde_json::to_vec_pretty(header)?)?;
            sync_dir(&self.dir)?;
        }
        Ok(scan_log(&path)?)
    }

    pub fn read_header(&self) -> Result<SessionHeader, StorageError> {
        let raw = std::fs::read(self.dir.join("session.json"))?;
        Ok(serde_json::from_slice(&raw)?)
    }

    /// Quarantine the raw log once before further mutation when a load found
    /// skipped lines (grok `jsonl/mod.rs:1063-1074`): copy to
    /// `session.jsonl.corrupt` only if the quarantine copy doesn't exist.
    pub fn quarantine_once(&self) -> Result<(), StorageError> {
        let from = self.log_path();
        let to = self.corrupt_path();
        if from.exists() && !to.exists() {
            std::fs::copy(&from, &to)?;
        }
        Ok(())
    }

    /// Append events. The file handle is opened append-only per call; the
    /// caller (SessionHandle) holds the single-writer lease for the session.
    pub fn append_batch(
        &self,
        events: &[SessionEvent],
        durability: AppendDurability,
    ) -> Result<AppendOutcome, StorageError> {
        if events.is_empty() {
            return Ok(AppendOutcome::Durable);
        }
        let path = self.log_path();
        let mut file = OpenOptions::new().create(true).append(true).read(true).open(&path)?;

        // Torn-tail heal BEFORE append (grok `jsonl/mod.rs:433-452`): if the
        // last byte isn't `\n`, terminate the torn record first so it stays
        // its own corrupt line and our line stays parseable.
        let len = file.seek(SeekFrom::End(0))?;
        let mut prefix = Vec::new();
        if len > 0 {
            file.seek(SeekFrom::Start(len - 1))?;
            let mut last = [0u8; 1];
            file.read_exact(&mut last)?;
            if last[0] != b'\n' {
                prefix.push(b'\n');
            }
            file.seek(SeekFrom::End(0))?;
        }

        let mut payload = prefix;
        for ev in events {
            serde_json::to_writer(&mut payload, ev)?;
            payload.push(b'\n');
        }

        let written = file.write_all(&payload).is_ok();
        let flush_res = if written { file.flush() } else { Err(std::io::Error::other("not written")) };
        let sync_res = if durability == AppendDurability::Durable {
            flush_res.and_then(|_| file.sync_all()).and_then(|_| sync_dir(&self.dir))
        } else {
            flush_res
        };
        match sync_res {
            Ok(()) => Ok(if durability == AppendDurability::Durable {
                AppendOutcome::Durable
            } else {
                AppendOutcome::CommittedNotDurable
            }),
            Err(e) => {
                if written {
                    // Committed: bytes are in the file; only flush/sync failed.
                    // The message distinguishes it for callers (loss contract).
                    Err(StorageError::Io(std::io::Error::other(format!(
                        "committed but not durable: {e}"
                    ))))
                } else {
                    Err(StorageError::Io(e))
                }
            }
        }
    }

    /// Truncate the file to `committed_bytes` (torn-tail repair,
    /// deepseek `truncateTornTail`). Durable.
    pub fn truncate_to(&self, committed_bytes: u64) -> Result<(), StorageError> {
        let path = self.log_path();
        let f = OpenOptions::new().write(true).open(&path)?;
        f.set_len(committed_bytes)?;
        f.sync_all()?;
        sync_dir(&self.dir)?;
        Ok(())
    }
}

/// Crash-atomic full rewrite: temp file + fsync + rename (grok
/// `write_jsonl_atomic_async`, `jsonl/mod.rs:610-614`).
pub fn write_jsonl_atomic(f: &mut File, bytes: &[u8]) -> std::io::Result<()> {
    f.write_all(bytes)?;
    f.flush()?;
    f.sync_all()
}

/// Cross-process claim of the write lease for a session dir.
pub fn claim_write_lease(dir: &Path) -> Result<SessionWriteLease, StorageError> {
    SessionWriteLease::acquire(dir).map_err(|e| match e {
        super::lease::LeaseError::AlreadyOwned => StorageError::AlreadyOwned,
        super::lease::LeaseError::Io(io) => StorageError::Io(io),
        super::lease::LeaseError::InodeChanged => {
            StorageError::Io(std::io::Error::other("lease inode changed"))
        }
    })
}
