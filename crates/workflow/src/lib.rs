//! okra-workflow — run journal + budgets (MASTER-PLAN §3 #41). The Rhai
//! engine itself is grok's `xai-workflow` crate, vendored at M3 (decision
//! N0004); M1 lands the durable run journal so workflow runs are
//! reconstructable like every other durable truth.

pub mod engine;
pub mod validate;

use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::PathBuf;

pub fn now_ms() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as f64)
        .unwrap_or(0.0)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Pending,
    Running,
    Completed,
    Failed,
    Cancelled,
}

/// Budgets enforced per run (grok xai-workflow budgets analog).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunBudgets {
    pub max_wall_clock_ms: u64,
    pub max_steps: u32,
    pub max_fan_out: u32,
}

impl Default for RunBudgets {
    fn default() -> Self {
        RunBudgets { max_wall_clock_ms: 120_000, max_steps: 256, max_fan_out: 16 }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowRun {
    pub run_id: String,
    pub status: RunStatus,
    pub budgets: RunBudgets,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JournalEntry {
    pub seq: u64,
    pub run_id: String,
    pub event: String,
    pub at_ms: f64,
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub data: serde_json::Value,
}

/// Append-only NDJSON run journal (ZCode dynamic-workflow run journal
/// analog). One file per run; a torn final line is skipped on read, never
/// fatal (kernel loss contract reused).
#[derive(Clone)]
pub struct RunJournal {
    dir: PathBuf,
}

impl RunJournal {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        RunJournal { dir: dir.into() }
    }

    fn path_for(&self, run_id: &str) -> PathBuf {
        self.dir.join(format!("run-{run_id}.ndjson"))
    }

    pub fn append(&self, entry: &JournalEntry) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.path_for(&entry.run_id))?;
        let mut line = serde_json::to_vec(entry)?;
        line.push(b'\n');
        f.write_all(&line)
    }

    pub fn read(&self, run_id: &str) -> std::io::Result<Vec<JournalEntry>> {
        let raw = std::fs::read(self.path_for(run_id))?;
        let mut out = Vec::new();
        for line in raw.split(|&b| b == b'\n') {
            if line.is_empty() {
                continue;
            }
            if let Ok(e) = serde_json::from_slice(line) {
                out.push(e);
            }
        }
        Ok(out)
    }
}

/// Budget check helper: returns the exceeded budget, if any.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BudgetBreach {
    Steps { used: u32, max: u32 },
    FanOut { requested: u32, max: u32 },
    WallClock { elapsed_ms: u64, max_ms: u64 },
}

pub fn check_budgets(
    budgets: &RunBudgets,
    steps_used: u32,
    fan_out_requested: u32,
    elapsed_ms: u64,
) -> Option<BudgetBreach> {
    if fan_out_requested > budgets.max_fan_out {
        return Some(BudgetBreach::FanOut { requested: fan_out_requested, max: budgets.max_fan_out });
    }
    if steps_used > budgets.max_steps {
        return Some(BudgetBreach::Steps { used: steps_used, max: budgets.max_steps });
    }
    if elapsed_ms > budgets.max_wall_clock_ms {
        return Some(BudgetBreach::WallClock { elapsed_ms, max_ms: budgets.max_wall_clock_ms });
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn journal_roundtrip_tolerates_torn_tail() {
        let td = tempfile::tempdir().unwrap();
        let journal = RunJournal::new(td.path().join("j"));
        for i in 0..3 {
            journal
                .append(&JournalEntry {
                    seq: i,
                    run_id: "r1".into(),
                    event: format!("step-{i}"),
                    at_ms: i as f64 * 100.0,
                    data: serde_json::json!({}),
                })
                .unwrap();
        }
        // tear the tail
        let path = td.path().join("j/run-r1.ndjson");
        {
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
            write!(f, "{{\"seq\":3,\"run").unwrap();
        }
        let entries = journal.read("r1").unwrap();
        assert_eq!(entries.len(), 3, "torn tail skipped, complete entries kept");
    }

    #[test]
    fn budget_breaches_report_which_limit() {
        let budgets = RunBudgets::default();
        assert_eq!(check_budgets(&budgets, 10, 4, 1_000), None);
        assert_eq!(
            check_budgets(&budgets, 10, 32, 1_000),
            Some(BudgetBreach::FanOut { requested: 32, max: 16 })
        );
        assert_eq!(
            check_budgets(&budgets, 300, 1, 1_000),
            Some(BudgetBreach::Steps { used: 300, max: 256 })
        );
        assert!(matches!(
            check_budgets(&budgets, 1, 1, 999_999),
            Some(BudgetBreach::WallClock { .. })
        ));
    }
}
