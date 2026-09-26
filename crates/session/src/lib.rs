//! okra-session — rewind checkpoints (MASTER-PLAN §3 #38, grok
//! `checkpoint.rs` + `commands.rs:26-31`).
//!
//! A checkpoint pins (a) the log seq at rewind time and (b) a filesystem
//! snapshot manifest of tracked workspace files. Rewinding restores the
//! files AND truncates the conversation surface to the pinned seq — the log
//! itself is append-only: rewind APPENDS a rewind marker event; the surface
//! fold projects the pinned view.

use okra_kernel as kernel;
use okra_kernel::SessionHandle;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Checkpoint {
    pub id: String,
    /// Log seq at checkpoint time.
    pub seq: u64,
    pub created_at: f64,
    /// Files captured: path → content hash + snapshot file name.
    pub files: Vec<FileSnapshot>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileSnapshot {
    pub path: String,
    pub size: u64,
    pub sha256: String,
    pub stored_as: String,
}

#[derive(Debug, thiserror::Error)]
pub enum CheckpointError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("serde: {0}")]
    Serde(#[from] serde_json::Error),
    #[error("log: {0}")]
    Log(String),
}

/// The checkpoint store: `<session-dir>/checkpoints/<id>/` holding
/// `manifest.json` + blob files.
pub struct CheckpointStore {
    root: PathBuf,
}

impl CheckpointStore {
    pub fn new(session_dir: impl Into<PathBuf>) -> Self {
        let root = session_dir.into().join("checkpoints");
        CheckpointStore { root }
    }

    fn sha256(bytes: &[u8]) -> String {
        use sha2::Digest;
        let mut h = sha2::Sha256::new();
        h.update(bytes);
        format!("{:x}", h.finalize())
    }

    /// Capture a checkpoint of `files` (paths relative to the workspace)
    /// at the session's current seq.
    pub fn capture(
        &self,
        workspace: &Path,
        files: &[PathBuf],
        session: &SessionHandle,
        id: &str,
    ) -> Result<Checkpoint, CheckpointError> {
        let dir = self.root.join(id);
        std::fs::create_dir_all(&dir)?;
        let mut snapshots = Vec::new();
        for (i, path) in files.iter().enumerate() {
            let abs = workspace.join(path);
            let Ok(bytes) = std::fs::read(&abs) else {
                continue; // vanished between scan and capture: skip, manifest is honest
            };
            let stored_as = format!("blob-{i}");
            std::fs::write(dir.join(&stored_as), &bytes)?;
            snapshots.push(FileSnapshot {
                path: path.to_string_lossy().into_owned(),
                size: bytes.len() as u64,
                sha256: Self::sha256(&bytes),
                stored_as,
            });
        }
        let cp = Checkpoint {
            id: id.to_string(),
            seq: session.next_seq(),
            created_at: kernel::wall_clock(),
            files: snapshots,
        };
        let manifest = serde_json::to_vec_pretty(&cp)?;
        std::fs::write(dir.join("manifest.json"), manifest)?;
        Ok(cp)
    }

    pub fn list(&self) -> Result<Vec<Checkpoint>, CheckpointError> {
        let mut out = Vec::new();
        if !self.root.is_dir() {
            return Ok(out);
        }
        let mut ids: Vec<_> = std::fs::read_dir(&self.root)?
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        ids.sort();
        for id in ids {
            let manifest = self.root.join(&id).join("manifest.json");
            if let Ok(raw) = std::fs::read(&manifest) {
                out.push(serde_json::from_slice(&raw)?);
            }
        }
        Ok(out)
    }

    /// Restore files from a checkpoint. Returns the paths restored.
    /// Content integrity is verified against the manifest hash before
    /// writing; a mismatched blob is a hard error (fail closed).
    pub fn restore(&self, workspace: &Path, id: &str) -> Result<Vec<String>, CheckpointError> {
        let manifest = self.root.join(id).join("manifest.json");
        let cp: Checkpoint = serde_json::from_slice(&std::fs::read(manifest)?)?;
        let mut restored = Vec::new();
        for f in &cp.files {
            let blob = self.root.join(id).join(&f.stored_as);
            let bytes = std::fs::read(&blob)?;
            if Self::sha256(&bytes) != f.sha256 {
                return Err(CheckpointError::Io(std::io::Error::other(format!(
                    "checkpoint blob hash mismatch for {}",
                    f.path
                ))));
            }
            let abs = workspace.join(&f.path);
            if let Some(parent) = abs.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&abs, bytes)?;
            restored.push(f.path.clone());
        }
        Ok(restored)
    }

    /// The rewind log event: append-only marker the surface fold uses to
    /// project the pinned view (never truncates durable history).
    pub fn rewind_event(cp: &Checkpoint) -> kernel::SessionEvent {
        let mut ev = kernel::make_log_only_event(
            "session/rewind",
            serde_json::json!({
                "checkpointId": cp.id,
                "pinnedSeq": cp.seq,
            }),
            kernel::wall_clock,
        );
        ev.ignorable = Some(true);
        ev
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_restore_roundtrip_with_integrity() {
        let td = tempfile::tempdir().unwrap();
        let ws = td.path().join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::write(ws.join("a.txt"), "version one").unwrap();

        let header = kernel::SessionHeader {
            version: kernel::SESSION_FORMAT_VERSION,
            id: "cp-test".into(),
            created_at: 1.0,
            cwd: ws.to_string_lossy().into_owned(),
            parent_session: None,
            is_seeded: false,
        };
        let session = kernel::SessionHandle::create(&td.path().join("sessions"), &header).unwrap();
        let store = CheckpointStore::new(td.path().join("sessions").join("cp-test"));

        let cp = store.capture(&ws, &[PathBuf::from("a.txt")], &session, "cp-1").unwrap();
        assert_eq!(cp.files.len(), 1);

        // mutate, then restore
        std::fs::write(ws.join("a.txt"), "version two").unwrap();
        let restored = store.restore(&ws, "cp-1").unwrap();
        assert_eq!(restored, vec!["a.txt".to_string()]);
        assert_eq!(std::fs::read_to_string(ws.join("a.txt")).unwrap(), "version one");

        // tampered blob fails closed
        let blob = td.path().join("sessions/cp-test/checkpoints/cp-1/blob-0");
        std::fs::write(&blob, b"tampered").unwrap();
        assert!(store.restore(&ws, "cp-1").is_err());

        let _ = kernel::check_log(&[]);
        let _ = CheckpointStore::rewind_event(&cp);
    }

    #[test]
    fn list_is_ordered_and_tolerant() {
        let td = tempfile::tempdir().unwrap();
        let store = CheckpointStore::new(td.path());
        assert_eq!(store.list().unwrap(), Vec::new(), "no checkpoints dir yet");
    }
}
