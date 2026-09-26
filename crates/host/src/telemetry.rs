//! Conversation telemetry (MASTER-PLAN §3 #48, from ZCode
//! `conversation-telemetry/`): a small, local-only observability plane
//! for the daemon — named event counters, a bounded recent-events tail,
//! and an append-only JSONL audit log so counters survive a restart.
//!
//! Contracts:
//! - **local-only**: counters and the tail live under the workspace's
//!   `.okra` directory; nothing is exported;
//! - **bounded**: the in-memory tail is capped (the donor caps what it
//!   keeps per conversation too) and the JSONL is truncated on load to
//!   `max_persisted` records;
//! - **counter queries by name** with optional since/until epoch-ms
//!   bounds, mirroring the donor's range-filtered aggregation.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TelemetryEvent {
    pub name: String,
    pub recorded_at_epoch_ms: u64,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub attributes: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EventCount {
    pub name: String,
    pub count: u64,
}

/// The telemetry plane over one workspace.
pub struct TelemetryLog {
    jsonl_path: PathBuf,
    events: Vec<TelemetryEvent>,
    max_tail: usize,
    max_persisted: usize,
}

impl TelemetryLog {
    /// Open (or create) telemetry under the workspace's `.okra` dir,
    /// loading the persisted JSONL (bounded by `max_persisted`).
    pub fn open(workspace: &Path) -> Result<Self, std::io::Error> {
        Self::open_with_limits(workspace, 1000, 5000)
    }

    pub fn open_with_limits(
        workspace: &Path,
        max_tail: usize,
        max_persisted: usize,
    ) -> Result<Self, std::io::Error> {
        let okra_dir = workspace.join(".okra");
        std::fs::create_dir_all(&okra_dir)?;
        let jsonl_path = okra_dir.join("telemetry.jsonl");
        let mut log = TelemetryLog {
            jsonl_path: jsonl_path.clone(),
            events: Vec::new(),
            max_tail,
            max_persisted,
        };
        if jsonl_path.exists() {
            let raw = std::fs::read_to_string(&jsonl_path)?;
            for line in raw.lines() {
                if let Ok(ev) = serde_json::from_str::<TelemetryEvent>(line) {
                    log.events.push(ev);
                }
            }
            // keep only the most recent `max_persisted` records
            if log.events.len() > log.max_persisted {
                let keep_from = log.events.len() - log.max_persisted;
                log.events.drain(..keep_from);
                log.rewrite_persisted()?;
            }
        }
        Ok(log)
    }

    /// Record one named event with optional attributes.
    pub fn record(
        &mut self,
        name: impl Into<String>,
        attributes: Value,
        recorded_at_epoch_ms: u64,
    ) -> Result<(), std::io::Error> {
        let event = TelemetryEvent {
            name: name.into(),
            recorded_at_epoch_ms,
            attributes,
        };
        let mut line = serde_json::to_vec(&event)?;
        line.push(b'\n');
        {
            use std::io::Write as _;
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.jsonl_path)?;
            file.write_all(&line)?;
        }
        self.events.push(event);
        if self.events.len() > self.max_tail {
            let overflow = self.events.len() - self.max_tail;
            self.events.drain(..overflow);
        }
        Ok(())
    }

    fn rewrite_persisted(&self) -> Result<(), std::io::Error> {
        let mut out = Vec::new();
        for event in &self.events {
            let mut line = serde_json::to_vec(event)?;
            line.push(b'\n');
            out.extend_from_slice(&line);
        }
        std::fs::write(&self.jsonl_path, &out)
    }

    /// Counts per event name over an optional inclusive epoch-ms range.
    pub fn counts(
        &self,
        range: Option<(u64, u64)>,
    ) -> BTreeMap<String, u64> {
        let mut counts: BTreeMap<String, u64> = BTreeMap::new();
        for event in &self.events {
            if let Some((since, until)) = range
                && (event.recorded_at_epoch_ms < since
                    || event.recorded_at_epoch_ms > until)
            {
                continue;
            }
            *counts.entry(event.name.clone()).or_insert(0) += 1;
        }
        counts
    }

    /// The most recent `limit` events (oldest first).
    pub fn recent(&self, limit: usize) -> Vec<TelemetryEvent> {
        let start = self.events.len().saturating_sub(limit);
        self.events[start..].to_vec()
    }

    pub fn tail_len(&self) -> usize {
        self.events.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const T0: u64 = 1_700_000_000_000;

    #[test]
    fn records_count_and_persist_across_reopen() {
        let td = tempfile::tempdir().unwrap();
        {
            let mut log = TelemetryLog::open(td.path()).unwrap();
            log.record("turn_started", json!({}), T0).unwrap();
            log.record("tool_call", json!({"name": "read_file"}), T0 + 1).unwrap();
            log.record("turn_started", json!({}), T0 + 2).unwrap();
        }
        // reopen: counters rebuilt from the persisted JSONL
        let log = TelemetryLog::open(td.path()).unwrap();
        let counts = log.counts(None);
        assert_eq!(counts.get("turn_started"), Some(&2));
        assert_eq!(counts.get("tool_call"), Some(&1));
    }

    #[test]
    fn range_filter_bounds_counts() {
        let td = tempfile::tempdir().unwrap();
        let mut log = TelemetryLog::open(td.path()).unwrap();
        log.record("a", json!({}), T0).unwrap();
        log.record("a", json!({}), T0 + 10).unwrap();
        log.record("b", json!({}), T0 + 20).unwrap();
        let counts = log.counts(Some((T0 + 15, T0 + 25)));
        assert_eq!(counts.get("a"), None, "outside the range");
        assert_eq!(counts.get("b"), Some(&1));
    }

    #[test]
    fn tail_is_bounded_and_recent_returns_newest() {
        let workspace = tempfile::tempdir().unwrap();
        let mut log = TelemetryLog::open_with_limits(workspace.path(), 5, 100).unwrap();
        for i in 0..20u64 {
            log.record(format!("e{i}"), json!({}), T0 + i).unwrap();
        }
        assert_eq!(log.tail_len(), 5, "tail cap holds");
        let recent = log.recent(3);
        let names: Vec<&str> = recent.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["e17", "e18", "e19"], "oldest-first, newest kept");
    }

    #[test]
    fn persisted_file_is_truncated_to_max_on_load() {
        let workspace = tempfile::tempdir().unwrap();
        {
            let mut log = TelemetryLog::open_with_limits(workspace.path(), 1000, 10).unwrap();
            for i in 0..25u64 {
                log.record(format!("e{i}"), json!({}), T0 + i).unwrap();
            }
        }
        // loading with a smaller persisted cap truncates the JSONL
        let log = TelemetryLog::open_with_limits(workspace.path(), 1000, 10).unwrap();
        assert_eq!(log.tail_len(), 10);
        let oldest = log.recent(10).first().unwrap().name.clone();
        assert_eq!(oldest, "e15", "the newest 10 survive");
    }

    #[test]
    fn malformed_persisted_lines_are_skipped() {
        let td = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(td.path().join(".okra")).unwrap();
        std::fs::write(
            td.path().join(".okra/telemetry.jsonl"),
            format!(
                "{}\nGARBAGE\n{}\n",
                serde_json::to_string(&TelemetryEvent {
                    name: "good".into(),
                    recorded_at_epoch_ms: T0,
                    attributes: json!({}),
                })
                .unwrap(),
                serde_json::to_string(&TelemetryEvent {
                    name: "good2".into(),
                    recorded_at_epoch_ms: T0 + 1,
                    attributes: json!({}),
                })
                .unwrap(),
            ),
        )
        .unwrap();
        let log = TelemetryLog::open(td.path()).unwrap();
        assert_eq!(log.tail_len(), 2, "malformed line skipped, good ones kept");
    }
}
