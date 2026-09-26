//! Coding-plan quota windows (MASTER-PLAN §3 #48, from ZCode
//! `shared/coding-plan-reset.ts` + `shared/usage-quota.ts` +
//! `ui/lib/codingPlanQuotaResetUi.ts`): the reset-opportunity state
//! machine that turns server quota snapshots into window state, and the
//! optimistic quota-limit overrides a completed reset implies.
//!
//! Contracts ported from the TS sources:
//! - **reset cycles**: five-hour quota resets on a 5h cycle, week quota on
//!   a 7d cycle; `next_reset_at = completed_at + cycle`;
//! - **shared unread cursor**: `has_unread_history` is ONE flag for both
//!   reset types — it belongs to the type whose `used_at` is the latest
//!   (ties: the type currently being applied). Without this, an old
//!   five-hour history would masquerade as a just-completed week reset;
//! - **opportunity lifecycle**: expired opportunities (expire_at <= now)
//!   never count; the earliest expiry of the remaining ones drives the
//!   countdown; a NEW opportunity after a completion starts a new cycle
//!   (completed state must not swallow it);
//! - **manual reconciliation**: `start_manual_use` keeps the opportunity
//!   snapshot and idempotency key so a failure can restore losslessly and
//!   retry with the SAME key; while processing, a stale status snapshot
//!   never regresses the entry — only a server `used_at` confirms;
//! - **sticky completion**: re-polling the same `used_at` must not renew
//!   `observed_at` (no looping done-hints), and a repeated reconciliation
//!   must keep the original manual/automatic classification;
//! - **optimistic quota overrides**: a completed reset with a pending
//!   entitlement refresh displays 0% used / next window from the cycle;
//!   once refreshed, the real limit wins — except a missing
//!   `nextResetTime` is still backfilled from the cycle (the fresh pool
//!   has no active window until the next prompt);
//! - **automatic vs manual**: only automatic completions (no
//!   `started_at`) surface the short "quota reset" hints.

use serde::{Deserialize, Serialize};

/// Reset cycle identity (`CodingPlanResetType`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ResetType {
    FiveHour,
    Week,
}

impl ResetType {
    /// Cycle length the NEXT window runs for (`resolveCodingPlanQuotaResetDurationMs`).
    pub fn cycle_ms(self) -> u64 {
        const FIVE_HOURS_MS: u64 = 5 * 60 * 60 * 1_000;
        const WEEK_MS: u64 = 7 * 24 * 60 * 60 * 1_000;
        match self {
            ResetType::FiveHour => FIVE_HOURS_MS,
            ResetType::Week => WEEK_MS,
        }
    }
}

/// `CodingPlanResetStatusSnapshot` — the server's quota-reset view.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResetStatusSnapshot {
    pub available_five_hour_resets: Vec<ResetOpportunity>,
    pub available_week_resets: Vec<ResetOpportunity>,
    pub latest_five_hour_used_at: Option<u64>,
    pub latest_week_used_at: Option<u64>,
    pub has_unread_history: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResetOpportunity {
    pub expire_at: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ResetEntryStatus {
    Available,
    Processing,
    Completed,
}

/// Window-shared reset state for ONE reset type (`CodingPlanQuotaResetUiEntry`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct QuotaResetEntry {
    pub status: ResetEntryStatus,
    /// Remaining reset opportunities; 0 while processing/completed.
    pub opportunity_count: usize,
    /// Earliest opportunity expiry; only meaningful while available.
    pub opportunity_expires_at: Option<u64>,
    /// Local manual-use start; automatic/operations resets are None.
    pub started_at: Option<u64>,
    /// Server `latest_{five_hour,week}_reset_history.used_at`.
    pub completed_at: Option<u64>,
    /// When THIS process first observed the current completed_at.
    pub observed_at: Option<u64>,
    /// Until the entitlement refresh lands, quota may optimistically
    /// display 100% remaining.
    pub quota_override_pending: bool,
    pub next_reset_at: Option<u64>,
    /// One manual use = one idempotency key, reused across retries.
    pub idempotency_key: Option<String>,
    pub error: Option<String>,
}

/// Apply a server status snapshot for ONE reset type
/// (`applyCodingPlanQuotaResetStatus`). `manual_started_at` is the local
/// processing start (manual use), `None` for automatic/operations flows.
pub fn apply_quota_reset_status(
    previous: Option<&QuotaResetEntry>,
    status: &ResetStatusSnapshot,
    now: u64,
    manual_started_at: Option<u64>,
    reset_type: ResetType,
) -> Option<QuotaResetEntry> {
    let (available, latest_used_at, other_used_at) = match reset_type {
        ResetType::FiveHour => (
            &status.available_five_hour_resets,
            status.latest_five_hour_used_at,
            status.latest_week_used_at,
        ),
        ResetType::Week => (
            &status.available_week_resets,
            status.latest_week_used_at,
            status.latest_five_hour_used_at,
        ),
    };
    // The shared unread flag belongs to whichever type completed last;
    // ties go to the type being applied so at least one type can complete.
    let owns_unread = status.has_unread_history
        && latest_used_at.is_some()
        && (other_used_at.is_none() || latest_used_at >= other_used_at);
    let mut valid: Vec<&ResetOpportunity> = available
        .iter()
        .filter(|o| o.expire_at > now)
        .collect();
    valid.sort_by_key(|o| o.expire_at);
    let has_valid_opportunity = !valid.is_empty();

    let sticky_completed = matches!(
        previous,
        Some(QuotaResetEntry {
            status: ResetEntryStatus::Completed,
            completed_at: Some(prev_used),
            ..
        }) if Some(*prev_used) == latest_used_at
    );
    let should_complete = latest_used_at.is_some()
        && (owns_unread
            || previous.map(|p| p.status) == Some(ResetEntryStatus::Processing)
            || ((manual_started_at.is_some() || sticky_completed) && !has_valid_opportunity));
    if let (true, Some(used_at)) = (should_complete, latest_used_at) {
        return Some(completed_entry(previous, used_at, now, manual_started_at, reset_type));
    }

    // While a manual use is in flight, stale pre-use snapshots must not
    // resurrect a clickable opportunity — only the server used_at settles.
    if previous.map(|p| p.status) == Some(ResetEntryStatus::Processing) {
        return previous.cloned();
    }

    valid.first().map(|earliest| QuotaResetEntry {
        status: ResetEntryStatus::Available,
        opportunity_count: valid.len(),
        opportunity_expires_at: Some(earliest.expire_at),
        started_at: None,
        completed_at: None,
        observed_at: None,
        quota_override_pending: false,
        next_reset_at: None,
        // reuse the key while still available; a fresh cycle starts keyless
        idempotency_key: previous
            .filter(|p| p.status == ResetEntryStatus::Available)
            .and_then(|p| p.idempotency_key.clone()),
        error: None,
    })
}

/// Build the completed entry (`createCompletedEntry`): same completion
/// re-polls keep the original classification and first-observed time.
fn completed_entry(
    previous: Option<&QuotaResetEntry>,
    completed_at: u64,
    observed_at: u64,
    manual_started_at: Option<u64>,
    reset_type: ResetType,
) -> QuotaResetEntry {
    let same_completion = matches!(previous, Some(p) if p.status == ResetEntryStatus::Completed && p.completed_at == Some(completed_at));
    let started_at = if same_completion {
        previous.and_then(|p| p.started_at)
    } else if previous.map(|p| p.status) == Some(ResetEntryStatus::Processing) {
        previous.and_then(|p| p.started_at)
    } else {
        manual_started_at
    };
    let observed_at = if same_completion {
        previous.and_then(|p| p.observed_at).or(Some(observed_at))
    } else {
        Some(observed_at)
    };
    QuotaResetEntry {
        status: ResetEntryStatus::Completed,
        opportunity_count: 0,
        opportunity_expires_at: None,
        started_at,
        completed_at: Some(completed_at),
        observed_at,
        quota_override_pending: !same_completion
            || previous.map(|p| p.quota_override_pending).unwrap_or(false),
        next_reset_at: Some(completed_at + reset_type.cycle_ms()),
        idempotency_key: None,
        error: None,
    }
}

/// Begin a manual reset use (`startCodingPlanQuotaResetManualUse`): only
/// an available entry can start; the opportunity snapshot and any
/// existing idempotency key are preserved for lossless failure recovery.
pub fn start_manual_use(
    entry: Option<QuotaResetEntry>,
    idempotency_key: &str,
    now: u64,
) -> Option<QuotaResetEntry> {
    let mut entry = entry?;
    if entry.status != ResetEntryStatus::Available {
        return Some(entry);
    }
    entry.status = ResetEntryStatus::Processing;
    entry.started_at = Some(now);
    entry.completed_at = None;
    entry.observed_at = None;
    entry.quota_override_pending = false;
    entry.next_reset_at = None;
    entry.idempotency_key =
        Some(entry.idempotency_key.unwrap_or_else(|| idempotency_key.to_string()));
    entry.error = None;
    Some(entry)
}

/// A manual use failed (`failCodingPlanQuotaResetManualUse`): restore the
/// pre-click opportunities and KEEP the idempotency key so the retry
/// reuses it.
pub fn fail_manual_use(mut entry: Option<QuotaResetEntry>, error: &str) -> Option<QuotaResetEntry> {
    let e = entry.as_mut()?;
    if e.status != ResetEntryStatus::Processing {
        return entry;
    }
    e.status = ResetEntryStatus::Available;
    e.started_at = None;
    e.completed_at = None;
    e.observed_at = None;
    e.quota_override_pending = false;
    e.next_reset_at = None;
    e.error = Some(error.to_string());
    entry
}

/// Merge the five-hour and week opportunity badges
/// (`mergeCodingPlanQuotaResetOpportunityBadges`): counts sum, countdown
/// is the earliest expiry among visible entries.
pub fn merge_opportunity_badges(items: &[(bool, usize, Option<u64>)]) -> (usize, Option<u64>) {
    let visible = items.iter().filter(|(visible, count, _)| *visible && *count > 0);
    let count = visible.clone().map(|(_, count, _)| count).sum();
    let expires_at = visible
        .filter_map(|(_, _, exp)| *exp)
        .min();
    (count, expires_at)
}

// ---------------------------------------------------------------------------
// optimistic quota-limit overrides (usage-quota.ts shapes)
// ---------------------------------------------------------------------------

/// `UsageQuotaLimit` — the server quota bucket relevant to coding plans.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageQuotaLimit {
    #[serde(rename = "type")]
    pub limit_type: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub period: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remaining: Option<f64>,
    /// Used-percentage (server semantics: HIGHER = more consumed).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub percentage: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_reset_time: Option<u64>,
}

/// Overlay a completed reset onto the displayed quota limit
/// (`resolveCodingPlanQuotaResetLimit`):
/// - pending entitlement refresh → optimistic 0% used + cycle-based next
///   window;
/// - refreshed → the real limit wins, but a missing `next_reset_time` is
///   still backfilled (the fresh pool has no active window until the next
///   prompt starts one).
pub fn resolve_quota_limit(limit: &UsageQuotaLimit, entry: Option<&QuotaResetEntry>) -> UsageQuotaLimit {
    let Some(entry) = entry else {
        return limit.clone();
    };
    if entry.status != ResetEntryStatus::Completed {
        return limit.clone();
    }
    if !entry.quota_override_pending {
        if limit.next_reset_time.is_none() && entry.next_reset_at.is_some() {
            return UsageQuotaLimit {
                next_reset_time: entry.next_reset_at,
                ..limit.clone()
            };
        }
        return limit.clone();
    }
    UsageQuotaLimit {
        percentage: Some(0.0),
        next_reset_time: entry.next_reset_at.or(limit.next_reset_time),
        ..limit.clone()
    }
}

/// The entitlement refresh landed for THIS completion
/// (`completeCodingPlanQuotaResetEntitlementRefresh`) — clear the
/// optimistic override; a different completion is ignored.
pub fn complete_entitlement_refresh(
    entry: Option<QuotaResetEntry>,
    completed_at: u64,
) -> Option<QuotaResetEntry> {
    let mut entry = entry?;
    if entry.status != ResetEntryStatus::Completed
        || entry.completed_at != Some(completed_at)
        || !entry.quota_override_pending
    {
        return Some(entry);
    }
    entry.quota_override_pending = false;
    Some(entry)
}

/// Whether the automatic done-hint is still visible
/// (`resolveCodingPlanQuotaResetStatusVisible`): automatic completions
/// only (no started_at), within `done_display_ms` of first observation.
pub fn reset_status_visible(entry: Option<&QuotaResetEntry>, now: u64, done_display_ms: u64) -> bool {
    match entry {
        Some(QuotaResetEntry {
            status: ResetEntryStatus::Completed,
            started_at: None,
            observed_at: Some(observed_at),
            ..
        }) => now.saturating_sub(*observed_at) < done_display_ms,
        _ => false,
    }
}

/// The automatic hint's two-phase display
/// (`resolveCodingPlanQuotaResetAutomaticPhase`): a synthetic
/// "processing" beat before "completed" (the server has no processing
/// signal; this restores the perception of one).
pub fn automatic_phase(
    entry: Option<&QuotaResetEntry>,
    now: u64,
    dismissed: bool,
    processing_ms: u64,
) -> Option<AutomaticPhase> {
    if dismissed || !reset_status_visible(entry, now, u64::MAX) {
        return None;
    }
    let observed = entry.and_then(|e| e.observed_at)?;
    if now.saturating_sub(observed) < processing_ms {
        Some(AutomaticPhase::Processing)
    } else {
        Some(AutomaticPhase::Completed)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum AutomaticPhase {
    Processing,
    Completed,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(
        five_hour: &[u64],
        week: &[u64],
        five_used: Option<u64>,
        week_used: Option<u64>,
        unread: bool,
    ) -> ResetStatusSnapshot {
        ResetStatusSnapshot {
            available_five_hour_resets: five_hour.iter().map(|&e| ResetOpportunity { expire_at: e }).collect(),
            available_week_resets: week.iter().map(|&e| ResetOpportunity { expire_at: e }).collect(),
            latest_five_hour_used_at: five_used,
            latest_week_used_at: week_used,
            has_unread_history: unread,
        }
    }

    #[test]
    fn expired_opportunities_are_filtered_and_earliest_wins() {
        let st = status(&[1_000, 500, 2_000], &[], None, None, false);
        let entry = apply_quota_reset_status(None, &st, 600, None, ResetType::FiveHour).unwrap();
        assert_eq!(entry.status, ResetEntryStatus::Available);
        assert_eq!(entry.opportunity_count, 2); // 1_000 and 2_000 survive
        assert_eq!(entry.opportunity_expires_at, Some(1_000));
        assert_eq!(entry.idempotency_key, None);
    }

    #[test]
    fn fresh_unread_history_completes_immediately() {
        let st = status(&[9_999], &[], Some(42), None, true);
        let entry = apply_quota_reset_status(None, &st, 100, None, ResetType::FiveHour).unwrap();
        assert_eq!(entry.status, ResetEntryStatus::Completed);
        assert_eq!(entry.completed_at, Some(42));
        assert_eq!(entry.observed_at, Some(100));
        assert_eq!(entry.next_reset_at, Some(42 + 5 * 60 * 60 * 1_000));
        assert!(entry.quota_override_pending);
        assert_eq!(entry.started_at, None); // automatic
    }

    #[test]
    fn shared_unread_flag_belongs_to_the_latest_completion_only() {
        // a week reset just completed; the five-hour history is OLD. The
        // five-hour application must NOT read as a fresh completion.
        let st = status(&[], &[], Some(1_000), Some(2_000), true);
        let five = apply_quota_reset_status(None, &st, 2_500, None, ResetType::FiveHour);
        assert!(five.is_none(), "old five-hour history must not complete");
        let week = apply_quota_reset_status(None, &st, 2_500, None, ResetType::Week).unwrap();
        assert_eq!(week.completed_at, Some(2_000));
        // equal used_at: both types may claim (at least one completes)
        let tie = status(&[], &[], Some(7), Some(7), true);
        assert!(apply_quota_reset_status(None, &tie, 8, None, ResetType::FiveHour).is_some());
    }

    #[test]
    fn stale_history_without_unread_and_without_manual_never_completes() {
        let st = status(&[], &[], Some(10), None, false);
        assert!(apply_quota_reset_status(None, &st, 100_000, None, ResetType::FiveHour).is_none());
    }

    #[test]
    fn processing_sticks_through_stale_snapshots() {
        let st_available = status(&[9_999], &[], None, None, false);
        let entry = apply_quota_reset_status(None, &st_available, 1, None, ResetType::FiveHour).unwrap();
        let processing = start_manual_use(Some(entry), "idem-1", 5).unwrap();
        assert_eq!(processing.idempotency_key.as_deref(), Some("idem-1"));

        // the poll still sees the pre-use snapshot: stay processing, do not
        // regress to a clickable opportunity
        let again = apply_quota_reset_status(
            Some(&processing),
            &st_available,
            6,
            Some(5),
            ResetType::FiveHour,
        )
        .unwrap();
        assert_eq!(again.status, ResetEntryStatus::Processing);
        assert_eq!(again.started_at, Some(5));
    }

    #[test]
    fn manual_completion_preserves_started_at() {
        let st_available = status(&[9_999], &[], None, None, false);
        let entry = apply_quota_reset_status(None, &st_available, 1, None, ResetType::FiveHour).unwrap();
        let processing = start_manual_use(Some(entry), "idem-1", 5).unwrap();
        let done_status = status(&[], &[], Some(9), None, false);
        let done = apply_quota_reset_status(
            Some(&processing),
            &done_status,
            12,
            Some(5),
            ResetType::FiveHour,
        )
        .unwrap();
        assert_eq!(done.status, ResetEntryStatus::Completed);
        assert_eq!(done.started_at, Some(5)); // manual classification kept
        assert_eq!(done.completed_at, Some(9));
        assert_eq!(done.idempotency_key, None);
        // automatic completion at the same tick would be unclassified
    }

    #[test]
    fn repeated_poll_of_same_used_at_is_sticky() {
        let st = status(&[], &[], Some(9), None, true);
        let first = apply_quota_reset_status(None, &st, 12, None, ResetType::FiveHour).unwrap();
        let again = apply_quota_reset_status(Some(&first), &st, 500, None, ResetType::FiveHour).unwrap();
        assert_eq!(again.observed_at, Some(12)); // NOT renewed to 500
        assert_eq!(again.started_at, None);
        assert!(again.quota_override_pending); // still pending after re-poll
    }

    #[test]
    fn new_opportunity_after_completion_starts_a_new_cycle() {
        let done = QuotaResetEntry {
            status: ResetEntryStatus::Completed,
            opportunity_count: 0,
            opportunity_expires_at: None,
            started_at: None,
            completed_at: Some(9),
            observed_at: Some(12),
            quota_override_pending: true,
            next_reset_at: Some(9 + 5 * 60 * 60 * 1_000),
            idempotency_key: None,
            error: None,
        };
        // same used_at, but the server granted a fresh opportunity within
        // the same cycle — the entry must return to available, not stay stuck
        let st = status(&[99_999], &[], Some(9), None, false);
        let next = apply_quota_reset_status(Some(&done), &st, 20, None, ResetType::FiveHour).unwrap();
        assert_eq!(next.status, ResetEntryStatus::Available);
        assert_eq!(next.opportunity_count, 1);
    }

    #[test]
    fn manual_use_failure_restores_opportunities_and_reuses_key() {
        let st = status(&[9_999], &[], None, None, false);
        let entry = apply_quota_reset_status(None, &st, 1, None, ResetType::FiveHour).unwrap();
        let processing = start_manual_use(Some(entry), "key-a", 5).unwrap();
        // server assigned a different key? keep the first one for retries
        let failed = fail_manual_use(Some(processing), "boom").unwrap();
        assert_eq!(failed.status, ResetEntryStatus::Available);
        assert_eq!(failed.opportunity_count, 1);
        assert_eq!(failed.opportunity_expires_at, Some(9_999));
        assert_eq!(failed.idempotency_key.as_deref(), Some("key-a"));
        assert_eq!(failed.error.as_deref(), Some("boom"));
        // a second start reuses the SAME key
        let retry = start_manual_use(Some(failed), "key-b", 7).unwrap();
        assert_eq!(retry.idempotency_key.as_deref(), Some("key-a"));
        // failure on a non-processing entry is a no-op
        let st2 = status(&[9_999], &[], None, None, false);
        let available = apply_quota_reset_status(None, &st2, 1, None, ResetType::FiveHour).unwrap();
        let untouched = fail_manual_use(Some(available.clone()), "nope").unwrap();
        assert_eq!(untouched.error, None);
    }

    #[test]
    fn badges_sum_counts_and_take_earliest_expiry() {
        let merged = merge_opportunity_badges(&[
            (true, 2, Some(3_000)),
            (true, 1, Some(1_000)),
            (false, 5, Some(100)), // hidden
            (true, 0, None),        // empty
        ]);
        assert_eq!(merged, (3, Some(1_000)));
    }

    #[test]
    fn completed_reset_overrides_quota_then_yields_to_refresh() {
        let limit = UsageQuotaLimit {
            limit_type: "TOKENS_LIMIT".into(),
            period: Some("daily".into()),
            usage: Some(120.0),
            remaining: Some(0.0),
            percentage: Some(100.0),
            next_reset_time: None,
        };
        let done = QuotaResetEntry {
            status: ResetEntryStatus::Completed,
            opportunity_count: 0,
            opportunity_expires_at: None,
            started_at: None,
            completed_at: Some(9),
            observed_at: Some(12),
            quota_override_pending: true,
            next_reset_at: Some(9 + 5 * 60 * 60 * 1_000),
            idempotency_key: None,
            error: None,
        };
        // pending: 0% used + optimistic window
        let shown = resolve_quota_limit(&limit, Some(&done));
        assert_eq!(shown.percentage, Some(0.0));
        assert_eq!(shown.next_reset_time, done.next_reset_at);
        // entitlement refresh lands for this completion → real values win,
        // but the missing next window is backfilled from the cycle
        let refreshed = complete_entitlement_refresh(Some(done.clone()), 9).unwrap();
        assert!(!refreshed.quota_override_pending);
        let shown = resolve_quota_limit(&limit, Some(&refreshed));
        assert_eq!(shown.percentage, Some(100.0));
        assert_eq!(shown.next_reset_time, done.next_reset_at);
        // a real server window replaces the backfill
        let with_window = UsageQuotaLimit { next_reset_time: Some(777), ..limit.clone() };
        assert_eq!(resolve_quota_limit(&with_window, Some(&refreshed)).next_reset_time, Some(777));
        // refresh for a DIFFERENT completion is ignored
        let still_pending = complete_entitlement_refresh(Some(done.clone()), 999).unwrap();
        assert!(still_pending.quota_override_pending);
        // non-completed entries never touch the limit
        let st = status(&[9_999], &[], None, None, false);
        let available = apply_quota_reset_status(None, &st, 1, None, ResetType::FiveHour).unwrap();
        assert_eq!(resolve_quota_limit(&limit, Some(&available)), limit);
    }

    #[test]
    fn automatic_hints_show_only_for_automatic_completions_within_windows() {
        let done = QuotaResetEntry {
            status: ResetEntryStatus::Completed,
            opportunity_count: 0,
            opportunity_expires_at: None,
            started_at: None, // automatic
            completed_at: Some(9),
            observed_at: Some(100),
            quota_override_pending: true,
            next_reset_at: Some(9 + 5 * 60 * 60 * 1_000),
            idempotency_key: None,
            error: None,
        };
        assert_eq!(automatic_phase(Some(&done), 1_050, false, 1_000), Some(AutomaticPhase::Processing));
        assert_eq!(automatic_phase(Some(&done), 1_200, false, 1_000), Some(AutomaticPhase::Completed));
        assert_eq!(automatic_phase(Some(&done), 1_200, true, 1_000), None); // dismissed
        assert!(!reset_status_visible(Some(&done), 2_700, 2_600)); // 2600 < 2600 is false
        assert!(reset_status_visible(Some(&done), 2_699, 2_600)); // 2599 < 2600
        // manual completions (started_at set) never show the automatic hint
        let manual = QuotaResetEntry { started_at: Some(50), ..done.clone() };
        assert_eq!(automatic_phase(Some(&manual), 1_050, false, 1_000), None);
    }

    #[test]
    fn wire_shapes_serialize_with_zcode_fields() {
        let st = status(&[100], &[200], Some(9), None, true);
        let json = serde_json::to_value(&st).unwrap();
        assert_eq!(json["availableFiveHourResets"][0]["expireAt"], 100);
        assert_eq!(json["latestFiveHourUsedAt"], 9);
        assert_eq!(json["hasUnreadHistory"], true);

        let entry = QuotaResetEntry {
            status: ResetEntryStatus::Completed,
            opportunity_count: 0,
            opportunity_expires_at: None,
            started_at: None,
            completed_at: Some(9),
            observed_at: Some(12),
            quota_override_pending: true,
            next_reset_at: Some(15),
            idempotency_key: Some("k".into()),
            error: None,
        };
        let json = serde_json::to_value(&entry).unwrap();
        assert_eq!(json["status"], "completed");
        assert_eq!(json["quotaOverridePending"], true);
        assert_eq!(json["nextResetAt"], 15);

        let limit = UsageQuotaLimit { percentage: Some(50.0), next_reset_time: Some(3), ..Default::default() };
        let json = serde_json::to_value(&limit).unwrap();
        assert_eq!(json["type"], "");
        assert_eq!(json["percentage"], 50.0);
        assert_eq!(json["nextResetTime"], 3);

        let rt: ResetType = serde_json::from_str("\"FIVE_HOUR\"").unwrap();
        assert_eq!(rt, ResetType::FiveHour);
        assert_eq!(serde_json::to_value(ResetType::Week).unwrap(), "WEEK");
    }
}
