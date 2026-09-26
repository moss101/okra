//! SQLite projections — MASTER-PLAN §3 #7 (ZCode `taskIndexRepo.ts` +
//! `storageCatalog.ts` semantics): the JSONL event log is the only durable
//! truth; SQLite holds derived, REBUILDABLE indexes for the host domains
//! (M3 strangler, first domain: session/task persistence).
//!
//! Tables:
//! - `sessions(id PRIMARY KEY, workspace, title, status, created_at, updated_at, event_count)`
//! - `tasks(id PRIMARY KEY, session_id, kind, title, status, payload, updated_at)`
//!
//! Consistency contract: every row is derivable from the kernel log; the
//! whole DB can be dropped and rebuilt with `rebuild_from_log` — the log
//! stays authoritative.

use rusqlite::Connection;
use serde_json::Value;

use crate::event::SessionEvent;

pub struct ProjectionDb {
    conn: Connection,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SessionRow {
    pub id: String,
    pub workspace: String,
    pub title: String,
    pub status: String,
    pub event_count: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TaskRow {
    pub id: String,
    pub session_id: String,
    pub kind: String,
    pub title: String,
    pub status: String,
}

/// Input for a task-index upsert.
#[derive(Debug, Clone)]
pub struct TaskRowInput {
    pub id: String,
    pub session_id: String,
    pub kind: String,
    pub title: String,
    pub status: String,
    pub payload: String,
    pub updated_at: f64,
}

impl ProjectionDb {
    pub fn open(path: &std::path::Path) -> Result<Self, crate::storage::StorageError> {
        let conn = Connection::open(path)?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS sessions (
                id TEXT PRIMARY KEY,
                workspace TEXT NOT NULL,
                title TEXT NOT NULL DEFAULT '',
                status TEXT NOT NULL DEFAULT 'active',
                created_at REAL NOT NULL,
                updated_at REAL NOT NULL,
                event_count INTEGER NOT NULL DEFAULT 0
            );
            CREATE TABLE IF NOT EXISTS tasks (
                id TEXT PRIMARY KEY,
                session_id TEXT NOT NULL,
                kind TEXT NOT NULL,
                title TEXT NOT NULL,
                status TEXT NOT NULL DEFAULT 'pending',
                payload TEXT NOT NULL DEFAULT '{}',
                updated_at REAL NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_tasks_session ON tasks(session_id);",
        )?;
        Ok(ProjectionDb { conn })
    }

    /// Upsert the session index row from the header + current log length.
    pub fn upsert_session(
        &self,
        id: &str,
        workspace: &str,
        title: &str,
        status: &str,
        created_at: f64,
        event_count: u64,
    ) -> Result<(), crate::storage::StorageError> {
        self.conn
            .execute(
                "INSERT INTO sessions (id, workspace, title, status, created_at, updated_at, event_count)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?5, ?6)
                 ON CONFLICT(id) DO UPDATE SET
                    workspace=?2, title=?3, status=?4, updated_at=?5, event_count=?6",
                rusqlite::params![id, workspace, title, status, created_at, event_count],
            )?;
        Ok(())
    }

    /// Upsert a task row (task index — ZCode taskIndexRepo analog).
    pub fn upsert_task(&self, task: &crate::TaskRowInput) -> Result<(), crate::storage::StorageError> {
        self.conn.execute(
            "INSERT INTO tasks (id, session_id, kind, title, status, payload, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(id) DO UPDATE SET
                kind=?3, title=?4, status=?5, payload=?6, updated_at=?7",
            rusqlite::params![
                task.id,
                task.session_id,
                task.kind,
                task.title,
                task.status,
                task.payload,
                task.updated_at
            ],
        )?;
        Ok(())
    }

    pub fn list_sessions(&self) -> Result<Vec<SessionRow>, crate::storage::StorageError> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, workspace, title, status, event_count FROM sessions ORDER BY updated_at DESC")?;
        let rows = stmt
            .query_map([], |r| {
                Ok(SessionRow {
                    id: r.get(0)?,
                    workspace: r.get(1)?,
                    title: r.get(2)?,
                    status: r.get(3)?,
                    event_count: r.get::<_, i64>(4)? as u64,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn tasks_for_session(&self, session_id: &str) -> Result<Vec<TaskRow>, crate::storage::StorageError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, session_id, kind, title, status FROM tasks WHERE session_id = ?1 ORDER BY updated_at",
        )?;
        let rows = stmt
            .query_map([session_id], |r| {
                Ok(TaskRow {
                    id: r.get(0)?,
                    session_id: r.get(1)?,
                    kind: r.get(2)?,
                    title: r.get(3)?,
                    status: r.get(4)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Rebuild both tables from a kernel log: the projection is a pure
    /// function of the durable truth.
    pub fn rebuild_from_log(&self, events: &[SessionEvent], session_id: &str, workspace: &str) -> Result<usize, crate::storage::StorageError> {
        self.conn.execute("DELETE FROM tasks", [])?;
        self.conn.execute("DELETE FROM sessions", [])?;
        let created = events.first().map(|e| e.time).unwrap_or(0.0);
        self.upsert_session(session_id, workspace, "", "active", created, events.len() as u64)?;
        let mut tasks: Vec<Value> = Vec::new();
        for ev in events {
            if ev.event_type == "task/upserted"
                && let Some(t) = ev.data.get("task") {
                    tasks.push(t.clone());
                }
        }
        let now = crate::wall_clock();
        for t in tasks {
            let input = crate::TaskRowInput {
                id: t.get("id").and_then(|v| v.as_str()).unwrap_or("task").to_string(),
                session_id: session_id.to_string(),
                kind: t.get("kind").and_then(|v| v.as_str()).unwrap_or("todo").to_string(),
                title: t.get("title").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                status: t.get("status").and_then(|v| v.as_str()).unwrap_or("pending").to_string(),
                payload: t.get("payload").cloned().unwrap_or(serde_json::Value::Null).to_string(),
                updated_at: now,
            };
            self.upsert_task(&input)?;
        }
        Ok(events.len())
    }
}
