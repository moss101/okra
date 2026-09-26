//! Usage telemetry (MASTER-PLAN §3 #48, from ZCode `usage-stats/` — the
//! portable "App Usage" slice): a LOCAL ledger of per-turn sampler usage
//! (tokens per provider/model/session) with snapshot queries by range,
//! model, session, and UTC day.
//!
//! Privacy posture: the ledger is local-only SQLite; the donor's cloud
//! quota monitors (Coding Plan/BigModel endpoints and their reset
//! flows) are deliberately NOT ported — okra records what its own
//! sampler consumed and nothing leaves the machine.
//!
//! Schema (derived + rebuildable from kernel logs in principle, but kept
//! additive here since usage rows arrive per turn):
//! `usage(id, session_id, provider, model, input_tokens, output_tokens, recorded_at)`.

use std::path::Path;

use rusqlite::Connection;

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageRecord {
    pub session_id: String,
    pub provider: String,
    pub model: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub recorded_at_epoch_ms: u64,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageTotals {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub turns: u64,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GroupedUsage {
    pub key: String,
    pub totals: UsageTotals,
}

/// A usage snapshot over an optional time range (epoch ms, inclusive).
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageSnapshot {
    pub range: Option<(u64, u64)>,
    pub totals: UsageTotals,
    pub by_model: Vec<GroupedUsage>,
    pub by_session: Vec<GroupedUsage>,
    /// UTC-day buckets (`recorded_at / 86_400_000`).
    pub by_day: Vec<GroupedUsage>,
}

#[derive(Debug, thiserror::Error)]
pub enum UsageError {
    #[error("usage ledger: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("usage ledger io: {0}")]
    Io(#[from] std::io::Error),
}

/// Milliseconds per UTC day — the snapshot day-bucket width.
pub const MS_PER_DAY: u64 = 86_400_000;

pub struct UsageLedger {
    conn: Connection,
}

impl UsageLedger {
    pub fn open(path: &Path) -> Result<Self, UsageError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS usage (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                session_id TEXT NOT NULL,
                provider TEXT NOT NULL,
                model TEXT NOT NULL,
                input_tokens INTEGER NOT NULL,
                output_tokens INTEGER NOT NULL,
                recorded_at INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_usage_session ON usage(session_id);
            CREATE INDEX IF NOT EXISTS idx_usage_recorded ON usage(recorded_at);",
        )?;
        Ok(UsageLedger { conn })
    }

    pub fn record(&self, entry: &UsageRecord) -> Result<(), UsageError> {
        self.conn.execute(
            "INSERT INTO usage (session_id, provider, model, input_tokens, output_tokens, recorded_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![
                entry.session_id,
                entry.provider,
                entry.model,
                entry.input_tokens,
                entry.output_tokens,
                entry.recorded_at_epoch_ms
            ],
        )?;
        Ok(())
    }

    /// Snapshot over an optional inclusive epoch-ms range.
    pub fn snapshot(&self, range: Option<(u64, u64)>) -> Result<UsageSnapshot, UsageError> {
        let totals = self.totals(range)?;
        let by_model = self.grouped("model", range)?;
        let by_session = self.grouped("session_id", range)?;
        let by_day = self.grouped("CAST(recorded_at / 86400000 AS TEXT)", range)?;
        Ok(UsageSnapshot {
            range,
            totals,
            by_model,
            by_session,
            by_day,
        })
    }

    fn totals(&self, range: Option<(u64, u64)>) -> Result<UsageTotals, UsageError> {
        let mut stmt = self.conn.prepare(
            "SELECT COALESCE(SUM(input_tokens),0), COALESCE(SUM(output_tokens),0), COUNT(*)
             FROM usage
             WHERE (?1 IS NULL OR recorded_at >= ?1) AND (?2 IS NULL OR recorded_at <= ?2)",
        )?;
        let (since, until) = range_params(range);
        let totals = stmt
            .query_row(rusqlite::params![since, until], |r| {
                Ok(UsageTotals {
                    input_tokens: r.get::<_, i64>(0)? as u64,
                    output_tokens: r.get::<_, i64>(1)? as u64,
                    turns: r.get::<_, i64>(2)? as u64,
                })
            })?;
        Ok(totals)
    }

    fn grouped(&self, key_expr: &str, range: Option<(u64, u64)>) -> Result<Vec<GroupedUsage>, UsageError> {
        let sql = format!(
            "SELECT {key_expr} AS k, COALESCE(SUM(input_tokens),0), COALESCE(SUM(output_tokens),0), COUNT(*)
             FROM usage
             WHERE (?1 IS NULL OR recorded_at >= ?1) AND (?2 IS NULL OR recorded_at <= ?2)
             GROUP BY k ORDER BY k"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let (since, until) = range_params(range);
        let rows = stmt
            .query_map(rusqlite::params![since, until], |r| {
                Ok(GroupedUsage {
                    key: r.get::<_, String>(0)?,
                    totals: UsageTotals {
                        input_tokens: r.get::<_, i64>(1)? as u64,
                        output_tokens: r.get::<_, i64>(2)? as u64,
                        turns: r.get::<_, i64>(3)? as u64,
                    },
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// All usage rows for one session (oldest first).
    pub fn usage_for_session(&self, session_id: &str) -> Result<Vec<UsageRecord>, UsageError> {
        let mut stmt = self.conn.prepare(
            "SELECT session_id, provider, model, input_tokens, output_tokens, recorded_at
             FROM usage WHERE session_id = ?1 ORDER BY recorded_at",
        )?;
        let rows = stmt
            .query_map([session_id], |r| {
                Ok(UsageRecord {
                    session_id: r.get(0)?,
                    provider: r.get(1)?,
                    model: r.get(2)?,
                    input_tokens: r.get::<_, i64>(3)? as u64,
                    output_tokens: r.get::<_, i64>(4)? as u64,
                    recorded_at_epoch_ms: r.get::<_, i64>(5)? as u64,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Drop usage rows recorded before `epoch_ms` (retention policy).
    pub fn purge_before(&self, epoch_ms: u64) -> Result<usize, UsageError> {
        Ok(self
            .conn
            .execute("DELETE FROM usage WHERE recorded_at < ?1", [epoch_ms as i64])?)
    }
}

fn range_params(range: Option<(u64, u64)>) -> (Option<i64>, Option<i64>) {
    match range {
        None => (None, None),
        Some((since, until)) => (Some(since as i64), Some(until as i64)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(session: &str, model: &str, at: u64, input: u64, output: u64) -> UsageRecord {
        UsageRecord {
            session_id: session.to_string(),
            provider: "openai".into(),
            model: model.to_string(),
            input_tokens: input,
            output_tokens: output,
            recorded_at_epoch_ms: at,
        }
    }

    const DAY0: u64 = 1_700_000_000_000;
    const DAY1: u64 = DAY0 + MS_PER_DAY;

    #[test]
    fn records_and_totals_by_model_session_day() {
        let td = tempfile::tempdir().unwrap();
        let ledger = UsageLedger::open(&td.path().join("usage.db")).unwrap();
        ledger.record(&entry("s1", "glm-4", DAY0 + 100, 100, 20)).unwrap();
        ledger.record(&entry("s1", "glm-4", DAY0 + 200, 50, 10)).unwrap();
        ledger.record(&entry("s2", "glm-5", DAY1 + 100, 7, 3)).unwrap();

        let snap = ledger.snapshot(None).unwrap();
        assert_eq!(snap.totals.input_tokens, 157);
        assert_eq!(snap.totals.output_tokens, 33);
        assert_eq!(snap.totals.turns, 3);

        let by_model: Vec<&GroupedUsage> = snap.by_model.iter().collect();
        assert_eq!(by_model.len(), 2);
        let glm4 = by_model.iter().find(|g| g.key == "glm-4").unwrap();
        assert_eq!(glm4.totals.input_tokens, 150);

        let by_session: Vec<&GroupedUsage> = snap.by_session.iter().collect();
        let s1 = by_session.iter().find(|g| g.key == "s1").unwrap();
        assert_eq!(s1.totals.turns, 2);

        // two UTC-day buckets
        assert_eq!(snap.by_day.len(), 2);
        let day0 = snap.by_day.iter().find(|g| g.key == "19675").unwrap();
        assert_eq!(day0.totals.input_tokens, 150);
    }

    #[test]
    fn range_filter_bounds_the_snapshot() {
        let td = tempfile::tempdir().unwrap();
        let ledger = UsageLedger::open(&td.path().join("usage.db")).unwrap();
        ledger.record(&entry("s1", "m", DAY0 + 100, 10, 1)).unwrap();
        ledger.record(&entry("s1", "m", DAY1 + 100, 20, 2)).unwrap();

        let snap = ledger.snapshot(Some((DAY1, DAY1 + MS_PER_DAY))).unwrap();
        assert_eq!(snap.totals.input_tokens, 20);
        assert_eq!(snap.by_day.len(), 1);
    }

    #[test]
    fn per_session_query_and_retention_purge() {
        let td = tempfile::tempdir().unwrap();
        let ledger = UsageLedger::open(&td.path().join("usage.db")).unwrap();
        ledger.record(&entry("s1", "m", DAY0, 5, 1)).unwrap();
        ledger.record(&entry("s2", "m", DAY1, 6, 1)).unwrap();

        assert_eq!(ledger.usage_for_session("s1").unwrap().len(), 1);
        assert_eq!(ledger.usage_for_session("s2").unwrap().len(), 1);

        // purge everything from before day 1
        let purged = ledger.purge_before(DAY1).unwrap();
        assert_eq!(purged, 1);
        let snap = ledger.snapshot(None).unwrap();
        assert_eq!(snap.totals.turns, 1);
        assert_eq!(snap.totals.input_tokens, 6);
    }
}
