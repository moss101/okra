//! Automation domain (MASTER-PLAN §3 #39/#40 + ZCode's automation
//! service): durable scheduled turns with the self-mutation guard the
//! task runtime already models (`TaskKind::Scheduled`, `TurnDispatch::
//! CronScheduled` — a cron-fired turn may not reschedule itself).
//!
//! Schedule shape (honest subset, no cron-expression dependency):
//! interval (`every_secs`) and daily-at (`at_hhmm`, local time). The
//! store is one atomically-rewritten JSON file (temp + rename — a reader
//! never sees a torn automation table). Firing is the DAEMON's job: the
//! store only computes what is due and records the fire.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// One scheduled automation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AutomationSpec {
    pub id: String,
    pub name: String,
    /// The user text the fired turn runs with (dedicated session).
    pub prompt: String,
    /// The dedicated session this automation fires into.
    pub session_id: String,
    /// Fire every N seconds (minimum 10 — a daemon that hammers itself is
    /// a bug, not a schedule).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub every_secs: Option<u64>,
    /// Fire daily at HH:MM **UTC** (24h) — std carries no tz database,
    /// and an honest UTC schedule beats a wrong local one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at_hhmm: Option<(u8, u8)>,
    pub created_at_epoch_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_fired_epoch_ms: Option<u64>,
    #[serde(default)]
    pub fire_count: u64,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AutomationError {
    #[allow(dead_code)]
    NotFound(String),
    Invalid(String),
}

impl std::fmt::Display for AutomationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AutomationError::NotFound(id) => write!(f, "no such automation: {id}"),
            AutomationError::Invalid(m) => write!(f, "invalid automation: {m}"),
        }
    }
}

/// The durable store: `<dir>/automation.json`, rewritten atomically.
pub struct AutomationStore {
    path: PathBuf,
}

impl AutomationStore {
    pub fn at(dir: impl Into<PathBuf>) -> AutomationStore {
        AutomationStore { path: dir.into().join("automation.json") }
    }

    fn load(&self) -> Vec<AutomationSpec> {
        let Ok(text) = std::fs::read_to_string(&self.path) else {
            return Vec::new();
        };
        serde_json::from_str(&text).unwrap_or_default()
    }

    fn save(&self, specs: &[AutomationSpec]) -> Result<(), AutomationError> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| AutomationError::Invalid(e.to_string()))?;
        }
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(specs).map_err(|e| AutomationError::Invalid(e.to_string()))?)
            .map_err(|e| AutomationError::Invalid(e.to_string()))?;
        std::fs::rename(&tmp, &self.path).map_err(|e| AutomationError::Invalid(e.to_string()))?;
        Ok(())
    }

    pub fn list(&self) -> Vec<AutomationSpec> {
        self.load()
    }

    pub fn get(&self, id: &str) -> Option<AutomationSpec> {
        self.load().into_iter().find(|s| s.id == id)
    }

    /// Create from a validated spec (see [`validate_spec`]).
    pub fn create(&self, mut spec: AutomationSpec) -> Result<AutomationSpec, AutomationError> {
        validate_spec(&spec)?;
        let mut specs = self.load();
        if specs.iter().any(|s| s.id == spec.id) {
            return Err(AutomationError::Invalid(format!("duplicate id {}", spec.id)));
        }
        spec.fire_count = 0;
        spec.last_fired_epoch_ms = None;
        specs.push(spec.clone());
        self.save(&specs)?;
        Ok(spec)
    }

    pub fn update(&self, id: &str, mutate: impl FnOnce(&mut AutomationSpec)) -> Result<AutomationSpec, AutomationError> {
        let mut specs = self.load();
        let spec = specs
            .iter_mut()
            .find(|s| s.id == id)
            .ok_or_else(|| AutomationError::NotFound(id.to_string()))?;
        mutate(spec);
        validate_spec(spec)?;
        let snapshot = spec.clone();
        self.save(&specs)?;
        Ok(snapshot)
    }

    pub fn delete(&self, id: &str) -> Result<bool, AutomationError> {
        let mut specs = self.load();
        let before = specs.len();
        specs.retain(|s| s.id != id);
        let removed = specs.len() != before;
        if removed {
            self.save(&specs)?;
        }
        Ok(removed)
    }

    /// The tick: every due, ENABLED spec gets `last_fired`/`fire_count`
    /// advanced and is returned for firing. A spec fired by an earlier
    /// tick is not re-fired within its interval (idempotent under a slow
    /// caller).
    pub fn tick(&self, now_epoch_ms: u64) -> Vec<AutomationSpec> {
        let mut specs = self.load();
        let mut fired = Vec::new();
        for spec in specs.iter_mut() {
            if !spec.enabled {
                continue;
            }
            let Some(due) = next_due_epoch_ms(spec, spec.last_fired_epoch_ms, now_epoch_ms) else {
                continue;
            };
            if due <= now_epoch_ms {
                spec.last_fired_epoch_ms = Some(now_epoch_ms);
                spec.fire_count += 1;
                fired.push(spec.clone());
            }
        }
        if !fired.is_empty() {
            let _ = self.save(&specs);
        }
        fired
    }
}

/// Validation: a schedule must exist, be representable, and stay inside
/// the honesty bounds (interval floor; HH:MM range).
pub fn validate_spec(spec: &AutomationSpec) -> Result<(), AutomationError> {
    if spec.id.trim().is_empty() {
        return Err(AutomationError::Invalid("id required".into()));
    }
    if spec.prompt.trim().is_empty() {
        return Err(AutomationError::Invalid("prompt required".into()));
    }
    if spec.session_id.trim().is_empty() {
        return Err(AutomationError::Invalid("session_id required".into()));
    }
    match (spec.every_secs, spec.at_hhmm) {
        (Some(secs), None) => {
            if secs < 10 {
                return Err(AutomationError::Invalid("every_secs below the 10s floor".into()));
            }
        }
        (None, Some((h, m))) => {
            if h > 23 || m > 59 {
                return Err(AutomationError::Invalid("at_hhmm out of range".into()));
            }
        }
        (Some(_), Some(_)) => {
            return Err(AutomationError::Invalid(
                "choose ONE schedule: every_secs or at_hhmm".into(),
            ))
        }
        (None, None) => {
            return Err(AutomationError::Invalid(
                "no schedule: every_secs or at_hhmm required".into(),
            ))
        }
    }
    Ok(())
}

/// The next fire time for a spec, given the last fire and now. `None`
/// means "not schedulable" (invalid/disabled shape).
pub fn next_due_epoch_ms(
    spec: &AutomationSpec,
    last_fired: Option<u64>,
    _now_epoch_ms: u64,
) -> Option<u64> {
    if let Some(secs) = spec.every_secs {
        if secs < 10 {
            return None;
        }
        // first fire: one interval after creation (or after the last fire)
        let anchor = last_fired.or(Some(spec.created_at_epoch_ms));
        Some(anchor? + secs * 1000)
    } else if let Some((h, m)) = spec.at_hhmm {
        if h > 23 || m > 59 {
            return None;
        }
        // the next HH:MM UTC wall clock after `last_fired` (or creation)
        let anchor = last_fired.unwrap_or(spec.created_at_epoch_ms);
        Some(next_daily_hhmm(anchor, h, m))
    } else {
        None
    }
}

/// The next UTC HH:MM strictly after `after_epoch_ms` (ms).
fn next_daily_hhmm(after_epoch_ms: u64, h: u8, m: u8) -> u64 {
    const DAY_MS: u64 = 86_400_000;
    let target_secs = h as u64 * 3600 + m as u64 * 60;
    let today_target = after_epoch_ms - (after_epoch_ms % DAY_MS) + target_secs * 1000;
    if today_target > after_epoch_ms {
        today_target
    } else {
        today_target + DAY_MS
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(every_secs: u64) -> AutomationSpec {
        AutomationSpec {
            id: "a1".into(),
            name: "nightly bench".into(),
            prompt: "run the benchmark".into(),
            session_id: "auto-a1".into(),
            every_secs: Some(every_secs),
            at_hhmm: None,
            created_at_epoch_ms: 1_000_000,
            last_fired_epoch_ms: None,
            fire_count: 0,
            enabled: true,
        }
    }

    #[test]
    fn interval_spec_first_fires_one_interval_after_creation() {
        let s = spec(60);
        assert_eq!(next_due_epoch_ms(&s, None, 1_000_000), Some(1_060_000));
        // after a fire: one interval after THAT
        assert_eq!(next_due_epoch_ms(&s, Some(1_060_000), 1_100_000), Some(1_120_000));
    }

    #[test]
    fn tick_fires_due_specs_and_records_the_fire() {
        let td = tempfile::tempdir().unwrap();
        let store = AutomationStore::at(td.path());
        store.create(spec(60)).unwrap();
        // before the interval: nothing due
        assert!(store.tick(1_000_000 + 30_000).is_empty());
        // after: fires, records, does not re-fire in the same interval
        let fired = store.tick(1_000_000 + 61_000);
        assert_eq!(fired.len(), 1);
        assert_eq!(fired[0].fire_count, 1);
        assert!(store.tick(1_000_000 + 62_000).is_empty(), "already fired this interval");
        let stored = store.get("a1").unwrap();
        assert_eq!(stored.fire_count, 1);
        assert_eq!(stored.last_fired_epoch_ms, Some(1_061_000));
    }

    #[test]
    fn disabled_and_deleted_specs_never_fire() {
        let td = tempfile::tempdir().unwrap();
        let store = AutomationStore::at(td.path());
        store.create(spec(60)).unwrap();
        store.update("a1", |s| s.enabled = false).unwrap();
        assert!(store.tick(2_000_000).is_empty());
        assert!(store.delete("a1").unwrap());
        assert!(store.tick(3_000_000).is_empty());
        assert!(!store.delete("a1").unwrap(), "second delete is honest");
    }

    #[test]
    fn validation_refuses_impossible_schedules() {
        let td = tempfile::tempdir().unwrap();
        let store = AutomationStore::at(td.path());
        let mut s = spec(5);
        assert!(store.create(s.clone()).is_err(), "below the interval floor");
        s.every_secs = None;
        s.at_hhmm = Some((24, 0));
        assert!(store.create(s.clone()).is_err(), "hour out of range");
        s.at_hhmm = Some((7, 30));
        s.every_secs = Some(60);
        assert!(store.create(s.clone()).is_err(), "two schedules at once");
        s.every_secs = None;
        assert!(store.create(s.clone()).is_ok());
    }

    #[test]
    fn daily_hhmm_fires_at_the_next_occurrence() {
        let mut s = spec(60);
        s.every_secs = None;
        s.at_hhmm = Some((7, 30));
        s.created_at_epoch_ms = 1_700_000_000_000; // the anchor is creation
        // created 06:00 local on some day → fires 07:30 same day
        let six_am = 1_700_000_000_000u64; // arbitrary ms epoch
        let due = next_due_epoch_ms(&s, None, six_am).unwrap();
        let target = six_am - (six_am % 86_400_000) + (7 * 3600 + 30 * 60) * 1000;
        if target > six_am {
            assert_eq!(due, target, "same-day occurrence when still ahead");
        } else {
            assert_eq!(due, target + 86_400_000, "next-day occurrence");
        }
        // after firing, the next due is strictly later
        let due2 = next_due_epoch_ms(&s, Some(due), due).unwrap();
        assert!(due2 > due);
    }

    #[test]
    fn store_survives_reopen() {
        let td = tempfile::tempdir().unwrap();
        {
            let store = AutomationStore::at(td.path());
            store.create(spec(120)).unwrap();
        }
        let reopened = AutomationStore::at(td.path());
        let s = reopened.get("a1").unwrap();
        assert_eq!(s.every_secs, Some(120));
        assert_eq!(s.prompt, "run the benchmark");
        assert_eq!(s.session_id, "auto-a1");
    }
}
