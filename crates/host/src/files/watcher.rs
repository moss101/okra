//! Per-conversation open-file watches (MASTER-PLAN §3 #52, ChatGPT2
//! docs/03 §6): each conversation can watch the files it has open; a poll
//! diff produces `open-file-changed`-style change records the surface
//! layer fans out. Watches are removed on host retirement and on
//! conversation discard (`removeOpenFileWatches`).
//!
//! The donor leans on `@parcel/watcher`; okra polls mtime/size snapshots,
//! which is dependency-free, cross-platform, and deterministic to test.
//! Poll cadence belongs to the caller (the serve loop ticks it).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeKind {
    /// Content changed (mtime or size moved) — or a missing file appeared.
    Modified,
    /// A previously-present file disappeared.
    Removed,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileChange {
    pub watch_id: u64,
    pub conversation_id: String,
    pub path: PathBuf,
    pub kind: ChangeKind,
}

#[derive(Debug, Default)]
pub struct FileWatchService {
    next_id: u64,
    watches: HashMap<u64, OpenFileWatch>,
}

#[derive(Debug)]
struct OpenFileWatch {
    conversation_id: String,
    snapshot: HashMap<PathBuf, Option<(SystemTime, u64)>>,
}

impl FileWatchService {
    pub fn new() -> Self {
        Self::default()
    }

    /// Watch the given paths for one conversation; the baseline snapshot
    /// is taken immediately, so only later changes are reported.
    pub fn watch(
        &mut self,
        conversation_id: impl Into<String>,
        paths: Vec<PathBuf>,
    ) -> u64 {
        self.next_id += 1;
        let id = self.next_id;
        let snapshot = paths
            .into_iter()
            .map(|p| {
                let s = snapshot_of(&p);
                (p, s)
            })
            .collect();
        self.watches.insert(
            id,
            OpenFileWatch {
                conversation_id: conversation_id.into(),
                snapshot,
            },
        );
        id
    }

    /// Diff every watch against its baseline, updating in place. A file
    /// that changes is reported once per poll (not once per change).
    pub fn poll(&mut self) -> Vec<FileChange> {
        let mut changes = Vec::new();
        for (id, watch) in self.watches.iter_mut() {
            let paths: Vec<PathBuf> = watch.snapshot.keys().cloned().collect();
            for path in paths {
                let now = snapshot_of(&path);
                let was = watch.snapshot.get(&path).cloned().flatten();
                let kind = match (was, now) {
                    (Some(old), Some(new)) if old != new => Some(ChangeKind::Modified),
                    (Some(_), None) => Some(ChangeKind::Removed),
                    (None, Some(_)) => Some(ChangeKind::Modified),
                    _ => None,
                };
                if let Some(kind) = kind {
                    changes.push(FileChange {
                        watch_id: *id,
                        conversation_id: watch.conversation_id.clone(),
                        path: path.clone(),
                        kind,
                    });
                }
                let updated = snapshot_of(&path);
                watch.snapshot.insert(path, updated);
            }
        }
        changes
    }

    /// Watch removed (file closed / host retirement).
    pub fn remove(&mut self, watch_id: u64) -> bool {
        self.watches.remove(&watch_id).is_some()
    }

    /// Conversation discarded: drop all its watches, returning how many.
    pub fn remove_for_conversation(&mut self, conversation_id: &str) -> usize {
        let before = self.watches.len();
        self.watches
            .retain(|_, w| w.conversation_id != conversation_id);
        before - self.watches.len()
    }

    pub fn watch_count(&self) -> usize {
        self.watches.len()
    }
}

fn snapshot_of(path: &Path) -> Option<(SystemTime, u64)> {
    let meta = std::fs::metadata(path).ok()?;
    Some((meta.modified().ok()?, meta.len()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_modify_and_remove() {
        let td = tempfile::tempdir().unwrap();
        let f = td.path().join("open.rs");
        std::fs::write(&f, "v1").unwrap();
        let mut svc = FileWatchService::new();
        let id = svc.watch("c1", vec![f.clone()]);
        assert_eq!(svc.poll(), Vec::new(), "baseline poll is quiet");

        // modify: mtime may not move fast enough on every fs; also bump size
        std::fs::write(&f, "v2 with more bytes").unwrap();
        let changes = svc.poll();
        assert_eq!(changes.len(), 1, "{changes:?}");
        assert_eq!(changes[0].kind, ChangeKind::Modified);
        assert_eq!(changes[0].watch_id, id);
        assert_eq!(changes[0].conversation_id, "c1");
        // quiescent again after the report
        assert_eq!(svc.poll(), Vec::new());

        std::fs::remove_file(&f).unwrap();
        let changes = svc.poll();
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].kind, ChangeKind::Removed);
    }

    #[test]
    fn missing_then_appearing_files_report_modified() {
        let td = tempfile::tempdir().unwrap();
        let f = td.path().join("later.log");
        let mut svc = FileWatchService::new();
        svc.watch("c1", vec![f.clone()]);
        std::fs::write(&f, "appears").unwrap();
        let changes = svc.poll();
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].kind, ChangeKind::Modified);
    }

    #[test]
    fn watches_are_per_conversation_and_removable() {
        let td = tempfile::tempdir().unwrap();
        let mut svc = FileWatchService::new();
        let w1 = svc.watch("c1", vec![td.path().join("a")]);
        let w2 = svc.watch("c2", vec![td.path().join("b")]);
        assert_eq!(svc.watch_count(), 2);
        assert!(svc.remove(w1));
        assert!(!svc.remove(w1), "idempotent remove");
        assert_eq!(svc.remove_for_conversation("c2"), 1);
        assert_eq!(svc.watch_count(), 0);
        assert_ne!(w1, w2);
    }
}
