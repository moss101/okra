//! DownloadStore (MASTER-PLAN §3 #52, ChatGPT2 docs/03 §4): state machine
//! + history for everything that lands on disk from the network.
//!
//! Donor contracts kept:
//! - every transition is recorded with a **monotonic lifecycle sequence**
//!   so audit order is total, not arrival order;
//! - downloads surface as **unacknowledged** until the UI consumes them
//!   (acknowledge by ids or all) — drives badge-like hints;
//! - live downloads are **bound to a conversation id** so per-thread
//!   download surfaces can filter;
//! - pause/resume/cancel are guarded transitions (`isPaused`/`canResume`);
//! - terminal entries append to a persisted JSONL history (the donor's
//!   better-sqlite3 store, as okra's JSONL-first projection).

use std::collections::{BTreeSet, HashMap};
use std::io::Write;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DownloadState {
    Active,
    Paused,
    Completed,
    Cancelled,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LifecycleKind {
    Started,
    Progress,
    Paused,
    Resumed,
    Cancelled,
    Completed,
    Failed,
}

/// One audit event: `lifecycleSequence` is global-monotonic across all
/// downloads, so sorting by it reproduces the true order of transitions.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LifecycleEvent {
    pub sequence: u64,
    pub download_id: String,
    pub kind: LifecycleKind,
    pub at_epoch_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DownloadEntry {
    pub id: String,
    /// Per-thread binding: surfaces filter on this.
    pub conversation_id: String,
    pub url: String,
    pub save_path: PathBuf,
    pub state: DownloadState,
    pub bytes_total: Option<u64>,
    pub bytes_received: u64,
    pub created_at_epoch_ms: u64,
    /// Latest sequence that touched this entry (audit pointer).
    pub last_sequence: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum DownloadError {
    #[error("unknown download {0:?}")]
    Unknown(String),
    #[error("invalid transition {from:?} -> {to:?} for download {id:?}")]
    InvalidTransition {
        id: String,
        from: DownloadState,
        to: DownloadState,
    },
    #[error("progress went backwards for {id:?}: {received} < {previous}")]
    NonMonotonicProgress {
        id: String,
        received: u64,
        previous: u64,
    },
    #[error("download io: {0}")]
    Io(#[from] std::io::Error),
    #[error("download history codec: {0}")]
    Codec(#[from] serde_json::Error),
}

/// The store: live map + unacknowledged set + persisted history.
pub struct DownloadStore {
    history_path: PathBuf,
    live: HashMap<String, DownloadEntry>,
    events: Vec<LifecycleEvent>,
    unacknowledged: BTreeSet<String>,
    history: Vec<DownloadEntry>,
    next_sequence: u64,
}

impl DownloadStore {
    /// Open (or create) a store rooted at a directory; loads the persisted
    /// history so `search`/`list_for_conversation` survive restarts.
    pub fn open(root: impl Into<PathBuf>) -> Result<Self, DownloadError> {
        let root = root.into();
        std::fs::create_dir_all(&root)?;
        let history_path = root.join("history.jsonl");
        let mut history = Vec::new();
        let mut max_seq = 0u64;
        if history_path.exists() {
            for line in std::fs::read_to_string(&history_path)?.lines() {
                if line.trim().is_empty() {
                    continue;
                }
                let entry: DownloadEntry = serde_json::from_str(line)?;
                max_seq = max_seq.max(entry.last_sequence);
                history.push(entry);
            }
        }
        Ok(DownloadStore {
            history_path,
            live: HashMap::new(),
            events: Vec::new(),
            unacknowledged: BTreeSet::new(),
            history,
            next_sequence: max_seq + 1,
        })
    }

    fn now_ms() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }

    fn next_id(&self) -> String {
        format!("dl-{}", self.next_sequence)
    }

    fn record(
        &mut self,
        id: &str,
        kind: LifecycleKind,
        detail: Option<String>,
    ) -> LifecycleEvent {
        let event = LifecycleEvent {
            sequence: self.next_sequence,
            download_id: id.to_string(),
            kind,
            at_epoch_ms: Self::now_ms(),
            detail,
        };
        self.next_sequence += 1;
        self.events.push(event.clone());
        if let Some(entry) = self
            .live
            .get_mut(id)
            .or_else(|| self.history.iter_mut().find(|e| e.id == id))
        {
            entry.last_sequence = event.sequence;
        }
        event
    }

    /// `will-download`: create a live entry bound to its conversation,
    /// unacknowledged until the UI consumes it.
    pub fn begin(
        &mut self,
        conversation_id: impl Into<String>,
        url: impl Into<String>,
        save_path: impl Into<PathBuf>,
        bytes_total: Option<u64>,
    ) -> DownloadEntry {
        let id = self.next_id();
        let entry = DownloadEntry {
            id: id.clone(),
            conversation_id: conversation_id.into(),
            url: url.into(),
            save_path: save_path.into(),
            state: DownloadState::Active,
            bytes_total,
            bytes_received: 0,
            created_at_epoch_ms: Self::now_ms(),
            last_sequence: 0,
        };
        self.live.insert(id.clone(), entry.clone());
        self.unacknowledged.insert(id.clone());
        self.record(&id, LifecycleKind::Started, None);
        entry
    }

    fn transition(
        &mut self,
        id: &str,
        from: &[DownloadState],
        to: DownloadState,
        kind: LifecycleKind,
        detail: Option<String>,
    ) -> Result<DownloadEntry, DownloadError> {
        let entry = self.live.get_mut(id).ok_or_else(|| DownloadError::Unknown(id.to_string()))?;
        if !from.contains(&entry.state) {
            return Err(DownloadError::InvalidTransition {
                id: id.to_string(),
                from: entry.state,
                to,
            });
        }
        entry.state = to;
        let entry = entry.clone();
        self.record(id, kind, detail);
        if matches!(to, DownloadState::Completed | DownloadState::Cancelled | DownloadState::Failed) {
            let entry = self.live.remove(id).unwrap_or(entry);
            self.append_history(&entry)?;
            self.history.push(entry.clone());
            return Ok(entry);
        }
        Ok(entry)
    }

    /// Guarded by the donor's `isPaused` / `canResume`: pause only an
    /// active download; resume only if paused with bytes left.
    pub fn pause(&mut self, id: &str) -> Result<DownloadEntry, DownloadError> {
        self.transition(id, &[DownloadState::Active], DownloadState::Paused, LifecycleKind::Paused, None)
    }

    pub fn resume(&mut self, id: &str) -> Result<DownloadEntry, DownloadError> {
        let entry = self.live.get(id).ok_or_else(|| DownloadError::Unknown(id.to_string()))?;
        let can_resume = entry.state == DownloadState::Paused
            && entry.bytes_total.is_none_or(|total| entry.bytes_received < total);
        if !can_resume {
            return Err(DownloadError::InvalidTransition {
                id: id.to_string(),
                from: entry.state,
                to: DownloadState::Active,
            });
        }
        self.transition(id, &[DownloadState::Paused], DownloadState::Active, LifecycleKind::Resumed, None)
    }

    pub fn cancel(&mut self, id: &str) -> Result<DownloadEntry, DownloadError> {
        self.transition(
            id,
            &[DownloadState::Active, DownloadState::Paused],
            DownloadState::Cancelled,
            LifecycleKind::Cancelled,
            None,
        )
    }

    /// Monotonic byte accounting: progress may never go backwards.
    pub fn progress(&mut self, id: &str, bytes_received: u64) -> Result<DownloadEntry, DownloadError> {
        {
            let entry = self
                .live
                .get(id)
                .ok_or_else(|| DownloadError::Unknown(id.to_string()))?;
            if entry.bytes_received > bytes_received {
                return Err(DownloadError::NonMonotonicProgress {
                    id: id.to_string(),
                    received: bytes_received,
                    previous: entry.bytes_received,
                });
            }
        }
        let entry = self
            .live
            .get_mut(id)
            .ok_or_else(|| DownloadError::Unknown(id.to_string()))?;
        entry.bytes_received = bytes_received;
        let entry = entry.clone();
        self.record(id, LifecycleKind::Progress, Some(format!("{bytes_received} bytes")));
        Ok(entry)
    }

    pub fn complete(&mut self, id: &str) -> Result<DownloadEntry, DownloadError> {
        self.transition(id, &[DownloadState::Active, DownloadState::Paused], DownloadState::Completed, LifecycleKind::Completed, None)
    }

    pub fn fail(&mut self, id: &str, reason: impl Into<String>) -> Result<DownloadEntry, DownloadError> {
        let detail = reason.into();
        self.transition(
            id,
            &[DownloadState::Active, DownloadState::Paused],
            DownloadState::Failed,
            LifecycleKind::Failed,
            Some(detail),
        )
    }

    /// Unacknowledged download ids (badge hints).
    pub fn unacknowledged_ids(&self) -> Vec<String> {
        self.unacknowledged.iter().cloned().collect()
    }

    /// Acknowledge by ids; unknown ids are ignored (idempotent consume).
    pub fn acknowledge(&mut self, ids: &[String]) {
        for id in ids {
            self.unacknowledged.remove(id);
        }
    }

    /// Acknowledge everything.
    pub fn acknowledge_all(&mut self) -> usize {
        let n = self.unacknowledged.len();
        self.unacknowledged.clear();
        n
    }

    pub fn get(&self, id: &str) -> Option<&DownloadEntry> {
        self.live.get(id).or_else(|| self.history.iter().find(|e| e.id == id))
    }

    /// Audit trail, total-ordered by the monotonic lifecycle sequence.
    pub fn lifecycle(&self, id: &str) -> Vec<LifecycleEvent> {
        self.events
            .iter()
            .filter(|e| e.download_id == id)
            .cloned()
            .collect()
    }

    /// Per-thread surface: live + history for one conversation.
    pub fn list_for_conversation(&self, conversation_id: &str) -> (Vec<DownloadEntry>, Vec<DownloadEntry>) {
        let live = self
            .live
            .values()
            .filter(|e| e.conversation_id == conversation_id)
            .cloned()
            .collect();
        let history = self
            .history
            .iter()
            .filter(|e| e.conversation_id == conversation_id)
            .cloned()
            .collect();
        (live, history)
    }

    /// `searchHistory` over save path and url substrings.
    pub fn search_history(&self, query: &str) -> Vec<DownloadEntry> {
        let q = query.to_ascii_lowercase();
        self.history
            .iter()
            .filter(|e| {
                e.save_path
                    .to_string_lossy()
                    .to_ascii_lowercase()
                    .contains(&q)
                    || e.url.to_ascii_lowercase().contains(&q)
            })
            .cloned()
            .collect()
    }

    /// `clear-history`: drops persisted + in-memory history only.
    pub fn clear_history(&mut self) -> Result<(), DownloadError> {
        std::fs::write(&self.history_path, b"")?;
        self.history.clear();
        Ok(())
    }

    pub fn history(&self) -> &[DownloadEntry] {
        &self.history
    }

    fn append_history(&self, entry: &DownloadEntry) -> Result<(), DownloadError> {
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.history_path)?;
        serde_json::to_writer(&mut file, entry)?;
        file.write_all(b"\n")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(dir: &std::path::Path) -> DownloadStore {
        DownloadStore::open(dir.join("downloads")).unwrap()
    }

    #[test]
    fn lifecycle_is_total_ordered_by_sequence() {
        let td = tempfile::tempdir().unwrap();
        let mut s = store(td.path());
        let e = s.begin("c1", "https://x/y.tar.gz", td.path().join("y.tar.gz"), Some(100));
        s.progress(&e.id, 40).unwrap();
        s.pause(&e.id).unwrap();
        s.progress(&e.id, 60).unwrap();
        s.resume(&e.id).unwrap();
        s.progress(&e.id, 100).unwrap();
        s.complete(&e.id).unwrap();
        let trail = s.lifecycle(&e.id);
        let kinds: Vec<LifecycleKind> = trail.iter().map(|e| e.kind).collect();
        assert_eq!(
            kinds,
            [LifecycleKind::Started, LifecycleKind::Progress, LifecycleKind::Paused,
             LifecycleKind::Progress, LifecycleKind::Resumed, LifecycleKind::Progress,
             LifecycleKind::Completed]
        );
        let mut seqs: Vec<u64> = trail.iter().map(|e| e.sequence).collect();
        assert!(seqs.windows(2).all(|w| w[0] < w[1]), "strictly monotonic");
        seqs.dedup();
        assert_eq!(seqs.len(), trail.len());
        assert_eq!(s.get(&e.id).unwrap().state, DownloadState::Completed);
    }

    #[test]
    fn guarded_transitions_reject_invalid_paths() {
        let td = tempfile::tempdir().unwrap();
        let mut s = store(td.path());
        let e = s.begin("c1", "u", td.path().join("f"), None);
        assert!(matches!(
            s.resume(&e.id),
            Err(DownloadError::InvalidTransition { .. })
        ));
        s.pause(&e.id).unwrap();
        assert!(matches!(
            s.pause(&e.id),
            Err(DownloadError::InvalidTransition { .. })
        ));
        // total reached: cannot resume, can complete
        let mut s2 = store(td.path());
        let e2 = s2.begin("c1", "u", td.path().join("g"), Some(10));
        s2.progress(&e2.id, 10).unwrap();
        s2.pause(&e2.id).unwrap();
        assert!(matches!(
            s2.resume(&e2.id),
            Err(DownloadError::InvalidTransition { .. })
        ));
        s2.complete(&e2.id).unwrap();
        assert!(matches!(s2.complete(&e2.id), Err(DownloadError::Unknown(_))));
    }

    #[test]
    fn progress_never_goes_backwards() {
        let td = tempfile::tempdir().unwrap();
        let mut s = store(td.path());
        let e = s.begin("c1", "u", td.path().join("f"), None);
        s.progress(&e.id, 50).unwrap();
        assert!(matches!(
            s.progress(&e.id, 49),
            Err(DownloadError::NonMonotonicProgress { .. })
        ));
    }

    #[test]
    fn acknowledge_flow_drives_badges() {
        let td = tempfile::tempdir().unwrap();
        let mut s = store(td.path());
        let a = s.begin("c1", "u", td.path().join("a"), None);
        let b = s.begin("c2", "u", td.path().join("b"), None);
        assert_eq!(s.unacknowledged_ids().len(), 2);
        s.acknowledge(std::slice::from_ref(&a.id));
        assert_eq!(s.unacknowledged_ids(), vec![b.id.clone()]);
        assert_eq!(s.acknowledge_all(), 1);
        assert!(s.unacknowledged_ids().is_empty());
        // idempotent re-ack
        s.acknowledge(std::slice::from_ref(&a.id));
        assert!(s.unacknowledged_ids().is_empty());
    }

    #[test]
    fn conversation_binding_and_history_search() {
        let td = tempfile::tempdir().unwrap();
        let mut s = store(td.path());
        let keep = s.begin("c1", "https://x/keep.zip", td.path().join("keep.zip"), None);
        s.complete(&keep.id).unwrap();
        let gone = s.begin("c2", "https://x/gone.zip", td.path().join("gone.zip"), None);
        s.cancel(&gone.id).unwrap();
        let (live1, hist1) = s.list_for_conversation("c1");
        assert!(live1.is_empty());
        assert_eq!(hist1.len(), 1);
        assert_eq!(hist1[0].id, keep.id);
        assert_eq!(s.search_history("gone").len(), 1);
        assert_eq!(s.search_history("zip").len(), 2);
        s.clear_history().unwrap();
        assert!(s.history().is_empty());
        assert_eq!(s.search_history("zip").len(), 0);
    }

    #[test]
    fn history_survives_reopen() {
        let td = tempfile::tempdir().unwrap();
        let mut s = store(td.path());
        let e = s.begin("c1", "u", td.path().join("f"), None);
        s.complete(&e.id).unwrap();
        let last_seq = s.lifecycle(&e.id).last().unwrap().sequence;
        drop(s);
        let mut s2 = store(td.path());
        assert_eq!(s2.history().len(), 1);
        assert_eq!(s2.history()[0].id, e.id);
        assert_eq!(
            s2.lifecycle(&e.id).len(),
            0,
            "audit trail is in-memory; history persists"
        );
        // new sequences continue after the loaded maximum
        let e2 = s2.begin("c1", "u", td.path().join("g"), None);
        assert!(s2.lifecycle(&e2.id)[0].sequence > last_seq);
    }

    #[test]
    fn failed_downloads_carry_reason() {
        let td = tempfile::tempdir().unwrap();
        let mut s = store(td.path());
        let e = s.begin("c1", "u", td.path().join("f"), None);
        s.fail(&e.id, "connection reset").unwrap();
        let trail = s.lifecycle(&e.id);
        assert_eq!(trail.last().unwrap().kind, LifecycleKind::Failed);
        assert_eq!(trail.last().unwrap().detail.as_deref(), Some("connection reset"));
    }
}
