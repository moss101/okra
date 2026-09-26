//! Session/task SQLite storage domain (MASTER-PLAN §3 #48 — the
//! "session/task SQLite" strangler domain) on top of the kernel's
//! rebuildable `ProjectionDb` (ZCode `taskIndexRepo.ts` +
//! `storageCatalog.ts` semantics).
//!
//! Layering contract: the kernel JSONL log is the ONLY durable truth;
//! SQLite holds derived, rebuildable indexes. This domain adds what the
//! kernel mirror deliberately lacks:
//! - **incremental per-session sync** from live kernel session handles
//!   (indexing session B never wipes session A's rows);
//! - **cross-session queries** (by workspace, title search, task
//!   status/kind filters, status counts);
//! - **lifecycle transitions** (archive/close) that move only the
//!   projection — the log is untouched;
//! - the **storage catalog**: the named locations a workbench install
//!   owns, with existence and size, so surfaces can render storage
//!   health.

use std::path::{Path, PathBuf};

use okra_kernel as kernel;
use okra_kernel::{ProjectionDb, SessionRow, TaskRow};
use okra_kernel::SessionHandle;

#[derive(Debug, thiserror::Error)]
pub enum StorageError {
    #[error("storage projection: {0}")]
    Projection(String),
    #[error("storage io: {0}")]
    Io(#[from] std::io::Error),
    #[error("kernel: {0}")]
    Kernel(String),
}

impl From<kernel::StorageError> for StorageError {
    fn from(e: kernel::StorageError) -> Self {
        StorageError::Projection(e.to_string())
    }
}

/// One entry of the storage catalog (donor `storageCatalog.ts`).
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CatalogEntry {
    pub id: String,
    pub path: PathBuf,
    pub exists: bool,
    pub size_bytes: u64,
}

/// The storage domain over a workspace's sessions dir + projection db.
pub struct StorageService {
    sessions_dir: PathBuf,
    workspace: PathBuf,
    db: ProjectionDb,
}

impl StorageService {
    /// Open (creating) the projection db under the workspace's `.okra`
    /// directory, with the sessions dir it indexes.
    pub fn open(workspace: &Path) -> Result<Self, StorageError> {
        let okra_dir = workspace.join(".okra");
        std::fs::create_dir_all(&okra_dir)?;
        let sessions_dir = okra_dir.join("sessions");
        std::fs::create_dir_all(&sessions_dir)?;
        let db = ProjectionDb::open(&okra_dir.join("index.db"))?;
        Ok(StorageService {
            sessions_dir,
            workspace: workspace.to_path_buf(),
            db,
        })
    }

    /// Index ONE kernel session: open its log read-only and replace its
    /// projection rows. Other sessions' rows are never touched.
    pub fn sync_session(&self, session_id: &str) -> Result<usize, StorageError> {
        let handle = SessionHandle::open(
            &self.sessions_dir,
            session_id,
            kernel::SessionAccess::Read,
        )
        .map_err(|e| StorageError::Kernel(e.to_string()))?;
        let events = handle.read_all().map_err(|e| StorageError::Kernel(e.to_string()))?;
        Ok(self
            .db
            .replace_session(&events, session_id, &self.workspace.to_string_lossy())?)
    }

    /// Index every kernel session under the sessions dir; returns
    /// (session_id, event_count) pairs.
    pub fn sync_all(&self) -> Result<Vec<(String, usize)>, StorageError> {
        let mut indexed = Vec::new();
        for entry in std::fs::read_dir(&self.sessions_dir)? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let session_id = entry.file_name().to_string_lossy().into_owned();
            // kernel sessions are directories containing the log file
            if !entry.path().join(kernel::LOG_FILENAME).exists() {
                continue;
            }
            let count = self.sync_session(&session_id)?;
            indexed.push((session_id, count));
        }
        Ok(indexed)
    }

    pub fn list_sessions(&self) -> Result<Vec<SessionRow>, StorageError> {
        Ok(self.db.list_sessions()?)
    }

    pub fn sessions_in_workspace(&self, workspace: &str) -> Result<Vec<SessionRow>, StorageError> {
        Ok(self.db.sessions_for_workspace(workspace)?)
    }

    pub fn search_sessions(&self, query: &str) -> Result<Vec<SessionRow>, StorageError> {
        Ok(self.db.search_sessions(query)?)
    }

    /// Lifecycle transition on the projection: `active` → `archived` /
    /// `closed` etc. The kernel log is untouched.
    pub fn set_session_status(&self, session_id: &str, status: &str) -> Result<(), StorageError> {
        Ok(self.db.set_session_status(session_id, status)?)
    }

    /// Drop a session's INDEX rows only; the durable log stays.
    pub fn forget_session(&self, session_id: &str) -> Result<(), StorageError> {
        Ok(self.db.delete_session(session_id)?)
    }

    pub fn tasks_for_session(&self, session_id: &str) -> Result<Vec<TaskRow>, StorageError> {
        Ok(self.db.tasks_for_session(session_id)?)
    }

    /// Cross-session task query: open tasks of a kind, all pending, etc.
    pub fn tasks_by_filter(
        &self,
        status: Option<&str>,
        kind: Option<&str>,
    ) -> Result<Vec<TaskRow>, StorageError> {
        Ok(self.db.tasks_by_filter(status, kind)?)
    }

    /// (status → count) for one session's tasks.
    pub fn task_status_counts(&self, session_id: &str) -> Result<Vec<(String, u64)>, StorageError> {
        Ok(self.db.task_status_counts(session_id)?)
    }

    /// The storage catalog: the locations this install owns.
    pub fn catalog(&self) -> Vec<CatalogEntry> {
        let entries = [
            ("projection-db", self.workspace.join(".okra").join("index.db")),
            ("kernel-sessions", self.sessions_dir.clone()),
            ("workspace", PathBuf::from(&self.workspace)),
        ];
        entries
            .into_iter()
            .map(|(id, path)| {
                let meta = std::fs::metadata(&path);
                CatalogEntry {
                    id: id.to_string(),
                    exists: meta.is_ok(),
                    size_bytes: meta.map(|m| m.len()).unwrap_or(0),
                    path,
                }
            })
            .collect()
    }


}
