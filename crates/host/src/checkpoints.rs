//! Rewind checkpoints (MASTER-PLAN §3 #38, from grok
//! `session/checkpoint.rs` + `file_state.rs`): one checkpoint per prompt,
//! bundling the filesystem rewind point (before/after snapshots), an
//! optional hunk delta, and optional git HEAD state. Restoring reverts
//! the enabled domains together and truncates later checkpoints.
//!
//! Donor contracts kept:
//! - **before snapshots are first-wins** per file per prompt (the state
//!   BEFORE any operations); **after snapshots are last-wins** and exist
//!   to detect EXTERNAL modifications — a file whose current bytes differ
//!   from its after-snapshot was changed by something outside the turn;
//! - checkpoints are **last-write-wins** (repeated finalizes are
//!   idempotent);
//! - **optional domains use serde defaults** so an older persisted blob
//!   still deserializes (missing field = domain off for that checkpoint);
//! - the durable mirror is a **JSONL** file: appends on finalize, a
//!   lenient reader skips malformed lines (a transiently unreadable file
//!   must not be treated as empty and drop history), later lines win;
//! - restore to prompt N reverts files to checkpoint N's before
//!   snapshots and truncates checkpoints `>= N`;
//! - the git domain records HEAD and restores with `reset --hard` only
//!   for checkpoints that captured git state.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::git::{GitError, GitRepository};
use crate::plugins::store::sha256_hex;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileSnapshot {
    /// Repo-relative POSIX path.
    pub path: String,
    /// Whether the file existed at snapshot time (absent → deleted on restore).
    pub exists: bool,
    /// sha256 of the contents; `""` when the file did not exist.
    pub sha256: String,
    pub size_bytes: u64,
}

impl FileSnapshot {
    pub fn of_bytes(rel_path: &str, bytes: Option<&[u8]>) -> FileSnapshot {
        match bytes {
            Some(bytes) => FileSnapshot {
                path: rel_path.to_string(),
                exists: true,
                sha256: sha256_hex(bytes),
                size_bytes: bytes.len() as u64,
            },
            None => FileSnapshot {
                path: rel_path.to_string(),
                exists: false,
                sha256: String::new(),
                size_bytes: 0,
            },
        }
    }
}

/// The filesystem rewind point for one prompt.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RewindPoint {
    pub prompt_index: usize,
    pub created_at_epoch_ms: u64,
    /// State BEFORE any operations for this prompt (first-wins per file).
    #[serde(default)]
    pub before: BTreeMap<String, FileSnapshot>,
    /// State AFTER all operations (last-wins; external-modification
    /// detection at restore time compares against these).
    #[serde(default)]
    pub after: BTreeMap<String, FileSnapshot>,
}

impl RewindPoint {
    pub fn new(prompt_index: usize) -> Self {
        RewindPoint {
            prompt_index,
            created_at_epoch_ms: now_ms(),
            before: BTreeMap::new(),
            after: BTreeMap::new(),
        }
    }

    /// First-wins: only the state before the prompt's FIRST operation.
    pub fn record_before(&mut self, snapshot: FileSnapshot) {
        self.before.entry(snapshot.path.clone()).or_insert(snapshot);
    }

    /// Last-wins: the state after the latest operation.
    pub fn record_after(&mut self, snapshot: FileSnapshot) {
        self.after.insert(snapshot.path.clone(), snapshot);
    }
}

/// Optional git domain state for one prompt.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GitState {
    pub branch: String,
    pub head_hash: String,
}

/// One checkpoint: FS rewind point plus optional domains. Optional fields
/// use serde defaults so an older persisted blob still deserializes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RewindCheckpoint {
    pub prompt_index: usize,
    pub fs: RewindPoint,
    /// Opaque per-prompt hunk delta (the hunk tracker owns its schema).
    #[serde(default)]
    pub hunks: Option<Value>,
    #[serde(default)]
    pub git: Option<GitState>,
}

#[derive(Debug, thiserror::Error)]
pub enum CheckpointError {
    #[error("no checkpoint for prompt {0}")]
    Unknown(usize),
    #[error("no snapshot contents cached for hash {0}")]
    MissingContents(String),
    #[error("checkpoint codec: {0}")]
    Codec(#[from] serde_json::Error),
    #[error("checkpoint io: {0}")]
    Io(#[from] std::io::Error),
    #[error("git restore failed: {0}")]
    Git(#[from] GitError),
}

/// What a restore did.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct RestoreReport {
    /// Files whose contents were reverted.
    pub restored: Vec<String>,
    /// Files that had vanished from disk and were recreated.
    pub recreated: Vec<String>,
    /// Files removed because the before-snapshot said they did not exist.
    pub removed: Vec<String>,
    /// Files whose current bytes differed from the checkpoint's
    /// after-snapshot — changed by something outside the turn.
    pub external_modifications: Vec<String>,
    /// HEAD hash the git domain was reset to (when captured).
    pub git_reset_to: Option<String>,
    /// DIAGNOSTIC (windows removal triage): every composed-before path
    /// with its recorded exists flag — shows whether the before record
    /// was even captured for the removed-file candidates.
    pub before_seen: Vec<(String, bool)>,
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// The checkpoint manager over one workspace. Snapshot CONTENTS are kept
/// in a content-addressed shadow cache (`.okra/checkpoint-contents/<sha>`)
/// so restore never depends on the live working tree.
pub struct CheckpointManager {
    workspace: PathBuf,
    contents_cache: PathBuf,
    durable_path: Option<PathBuf>,
    checkpoints: BTreeMap<usize, RewindCheckpoint>,
}

impl CheckpointManager {
    pub fn new(workspace: impl Into<PathBuf>) -> Self {
        let workspace = workspace.into();
        let contents_cache = workspace.join(".okra").join("checkpoint-contents");
        CheckpointManager {
            workspace,
            contents_cache,
            durable_path: None,
            checkpoints: BTreeMap::new(),
        }
    }

    /// Enable the durable JSONL mirror (the donor's durable mode is
    /// off-by-default; without it the manager stays in-memory only).
    pub fn with_durable_mirror(mut self, path: impl Into<PathBuf>) -> Self {
        self.durable_path = Some(path.into());
        self
    }

    /// Load a durable mirror (lenient: malformed lines skipped; missing
    /// file = empty). Later lines win, preserving last-write-wins.
    pub fn load_durable_mirror(&mut self, path: impl Into<PathBuf>) -> Result<(), CheckpointError> {
        let path = path.into();
        let content = match std::fs::read_to_string(&path) {
            Ok(content) => content,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e.into()),
        };
        for line in content.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            // lenient: malformed lines are skipped, not fatal
            if let Ok(checkpoint) = serde_json::from_str::<RewindCheckpoint>(trimmed) {
                self.checkpoints.insert(checkpoint.prompt_index, checkpoint);
            }
        }
        self.durable_path = Some(path);
        Ok(())
    }

    fn cache_contents(&self, bytes: &[u8]) -> Result<String, CheckpointError> {
        let hash = sha256_hex(bytes);
        let path = self.contents_cache.join(&hash);
        if !path.exists() {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(path, bytes)?;
        }
        Ok(hash)
    }

    fn read_cached(&self, hash: &str) -> Result<Vec<u8>, CheckpointError> {
        if hash.is_empty() {
            return Err(CheckpointError::MissingContents(String::new()));
        }
        std::fs::read(self.contents_cache.join(hash))
            .map_err(|_| CheckpointError::MissingContents(hash.to_string()))
    }

    /// Begin (or re-open) the checkpoint for a prompt. Idempotent.
    pub fn begin_prompt(&mut self, prompt_index: usize) {
        self.checkpoints
            .entry(prompt_index)
            .or_insert_with(|| RewindCheckpoint {
                fs: RewindPoint::new(prompt_index),
                prompt_index,
                hunks: None,
                git: None,
            });
    }

    /// Record one file operation for the prompt with pre-captured
    /// before/after bytes (None = file absent). Before is first-wins,
    /// after is last-wins; both contents are cached.
    pub fn record_operation(
        &mut self,
        prompt_index: usize,
        rel_path: &str,
        before: Option<&[u8]>,
        after: Option<&[u8]>,
    ) -> Result<(), CheckpointError> {
        let before_hash = match before {
            Some(bytes) => self.cache_contents(bytes)?,
            None => String::new(),
        };
        let after_hash = match after {
            Some(bytes) => self.cache_contents(bytes)?,
            None => String::new(),
        };
        let before_snap = FileSnapshot::of_bytes(rel_path, before);
        let after_snap = FileSnapshot::of_bytes(rel_path, after);
        let checkpoint = self
            .checkpoints
            .entry(prompt_index)
            .or_insert_with(|| RewindCheckpoint {
                fs: RewindPoint::new(prompt_index),
                prompt_index,
                hunks: None,
                git: None,
            });
        checkpoint.fs.record_before(before_snap);
        checkpoint.fs.record_after(after_snap);
        let _ = (before_hash, after_hash);
        Ok(())
    }

    /// Finalize the prompt: attach the hunk delta and git state
    /// (last-write-wins — repeated finalizes are idempotent), then mirror
    /// to disk.
    pub fn finalize_prompt(
        &mut self,
        prompt_index: usize,
        hunks: Option<Value>,
        repo: Option<&GitRepository>,
    ) -> Result<(), CheckpointError> {
        let Some(checkpoint) = self.checkpoints.get_mut(&prompt_index) else {
            return Ok(());
        };
        if let Some(delta) = hunks {
            checkpoint.hunks = Some(delta);
        }
        if let Some(repo) = repo {
            let head = repo.head()?;
            checkpoint.git = Some(GitState {
                branch: head.branch,
                head_hash: head.hash,
            });
        }
        let checkpoint = checkpoint.clone();
        self.append_durable(&checkpoint)?;
        Ok(())
    }

    fn append_durable(&self, checkpoint: &RewindCheckpoint) -> Result<(), CheckpointError> {
        let Some(path) = &self.durable_path else {
            return Ok(());
        };
        use std::io::Write as _;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut file = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
        serde_json::to_writer(&mut file, checkpoint)?;
        file.write_all(b"\n")?;
        Ok(())
    }

    pub fn get_checkpoint(&self, prompt_index: usize) -> Option<&RewindCheckpoint> {
        self.checkpoints.get(&prompt_index)
    }

    pub fn checkpoint_count(&self) -> usize {
        self.checkpoints.len()
    }

    /// Drop checkpoints `>= target_prompt_index` from memory and rewrite
    /// the disk mirror to match.
    pub fn truncate(&mut self, target_prompt_index: usize) -> Result<(), CheckpointError> {
        self.checkpoints.retain(|&idx, _| idx < target_prompt_index);
        self.rewrite_durable()?;
        Ok(())
    }

    fn rewrite_durable(&self) -> Result<(), CheckpointError> {
        let Some(path) = &self.durable_path else {
            return Ok(());
        };
        let mut text = String::new();
        for checkpoint in self.checkpoints.values() {
            text.push_str(&serde_json::to_string(checkpoint)?);
            text.push('\n');
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, text)?;
        Ok(())
    }

    /// Restore the workspace to the start of `target_prompt_index`: files
    /// revert to the checkpoint's before-snapshots (contents from the
    /// shadow cache), external modifications (current ≠ after-snapshot)
    /// are reported, the git domain resets to the captured HEAD when
    /// captured, and checkpoints `>= target` are truncated.
    pub fn restore_to(
        &mut self,
        repo: Option<&GitRepository>,
        target_prompt_index: usize,
    ) -> Result<RestoreReport, CheckpointError> {
        if !self.checkpoints.contains_key(&target_prompt_index) {
            return Err(CheckpointError::Unknown(target_prompt_index));
        }

        // Compose the rewound range: every checkpoint >= target
        // contributes; the EARLIEST before-snapshot per path wins (the
        // state before the earliest touched prompt), the LATEST
        // after-snapshot per path wins (for external-modification
        // detection).
        let mut composed_before: BTreeMap<String, FileSnapshot> = BTreeMap::new();
        let mut composed_after: BTreeMap<String, FileSnapshot> = BTreeMap::new();
        let mut git_state: Option<GitState> = None;
        for (&idx, checkpoint) in &self.checkpoints {
            if idx < target_prompt_index {
                continue;
            }
            for (path, snapshot) in &checkpoint.fs.before {
                composed_before.entry(path.clone()).or_insert(snapshot.clone());
            }
            for (path, snapshot) in &checkpoint.fs.after {
                composed_after.insert(path.clone(), snapshot.clone());
            }
            if checkpoint.git.is_some() {
                git_state = checkpoint.git.clone();
            }
        }

        let before_seen: Vec<(String, bool)> = composed_before
            .iter()
            .map(|(p, snap)| (p.clone(), snap.exists))
            .collect();
        let mut report = RestoreReport { before_seen, ..Default::default() };
        for (rel_path, snapshot) in &composed_before {
            let disk_path = self.workspace.join(rel_path);
            let on_disk = std::fs::metadata(&disk_path).is_ok();
            // external modification detection via the latest after-snapshot
            if let Some(after) = composed_after.get(rel_path) {
                if on_disk {
                    let current = std::fs::read(&disk_path)?;
                    let externally_modified =
                        !after.exists || after.sha256 != sha256_hex(&current);
                    if externally_modified {
                        report.external_modifications.push(rel_path.clone());
                    }
                } else if after.exists {
                    report.external_modifications.push(rel_path.clone());
                }
            }
            if snapshot.exists {
                let contents = self.read_cached(&snapshot.sha256)?;
                if let Some(parent) = disk_path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::write(&disk_path, &contents)?;
                report.restored.push(rel_path.clone());
                if !on_disk {
                    report.recreated.push(rel_path.clone());
                }
            } else if on_disk {
                std::fs::remove_file(&disk_path)?;
                report.removed.push(rel_path.clone());
            }
        }

        // git domain: reset to the captured HEAD when captured
        if let Some(git_state) = &git_state {
            let Some(repo) = repo else {
                return Err(CheckpointError::Git(GitError::Git(
                    "checkpoint captured git state but no repository was provided".into(),
                )));
            };
            repo.reset_hard(&git_state.head_hash)?;
            report.git_reset_to = Some(git_state.head_hash.clone());
        }

        self.truncate(target_prompt_index)?;
        Ok(report)
    }
}
