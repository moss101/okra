//! Turn governors — grok `acp_session_impl/` (turn.rs:2737+ loop).
//!
//! - **stationarity** (`turn.rs:3873-4012`): identical-tool-call-run
//!   detection over a step signature (every call's `name\u{1f}args-canonical`
//!   sorted + joined + hashed). Nudge at 4 (polling kinds) / 8 (general);
//!   hard stop at 8 / 12 / 4 consecutive `true`-noops.
//! - **length salvage** (`length_salvage.rs:19-86`): budgeted continuations
//!   after a MaxTokens truncation; the reminder injects ONCE per turn;
//!   exhaustion reports rather than looping.
//! - **rate-limit park** (`rate_limit_waits.rs:43-231`): subagent-only
//!   budgeted wait; main sessions never wait.

use serde_json::json;
use sha2::{Digest, Sha256};

// ---- stationarity ----

/// Kinds whose repetition is "problematic" (polling set, turn.rs:3873-3883).
#[derive(Debug, Clone)]
pub struct StationarityConfig {
    pub nudge_problematic: usize,
    pub nudge_general: usize,
    pub hard_stop_problematic: usize,
    pub hard_stop_general: usize,
    pub max_consecutive_true_noops: usize,
}

impl Default for StationarityConfig {
    fn default() -> Self {
        // grok turn.rs:3885-3889 + MAX_CONSECUTIVE_TRUE_NOOPS
        StationarityConfig {
            nudge_problematic: 4,
            nudge_general: 8,
            hard_stop_problematic: 8,
            hard_stop_general: 12,
            max_consecutive_true_noops: 4,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StationarityDecision {
    None,
    Nudge,
    HardStop,
}

pub struct StationarityTracker {
    config: StationarityConfig,
    last_signature: Option<String>,
    run_len: usize,
    nudged_this_run: bool,
    consecutive_true_noops: usize,
}

fn canonical_args(args_json: &str) -> String {
    // args JSON key-canonicalized (turn.rs:3914-3931): parse + sort keys
    match serde_json::from_str::<serde_json::Value>(args_json) {
        Ok(v) => canonical_value(&v),
        Err(_) => args_json.to_string(),
    }
}

fn canonical_value(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let parts: Vec<String> = keys
                .iter()
                .map(|k| format!("{k}:{})", canonical_value(&map[*k])))
                .collect();
            format!("{{{}}}", parts.join(","))
        }
        serde_json::Value::Array(items) => {
            let parts: Vec<String> = items.iter().map(canonical_value).collect();
            format!("[{}]", parts.join(","))
        }
        other => other.to_string(),
    }
}

impl StationarityTracker {
    pub fn new(config: StationarityConfig) -> Self {
        StationarityTracker {
            config,
            last_signature: None,
            run_len: 0,
            nudged_this_run: false,
            consecutive_true_noops: 0,
        }
    }

    /// Observe one step's tool calls. `problematic` = every call's kind is
    /// in the polling set; `noops` counts calls that returned `true` noops.
    pub fn observe_step(
        &mut self,
        calls: &[(/* name */ String, /* args_json */ String)],
        problematic: bool,
        any_true_noop: bool,
    ) -> StationarityDecision {
        // step signature (turn.rs:3935-3947): entries sorted + joined
        let mut entries: Vec<String> = calls
            .iter()
            .map(|(name, args)| format!("{name}\u{1f}{}", canonical_args(args)))
            .collect();
        entries.sort();
        let joined = entries.join("\u{1e}");
        let mut h = Sha256::new();
        h.update(joined.as_bytes());
        let signature = format!("{:x}", h.finalize());

        if self.last_signature.as_deref() == Some(signature.as_str()) && !calls.is_empty() {
            self.run_len += 1;
        } else {
            self.run_len = 1;
            self.nudged_this_run = false;
        }
        self.last_signature = Some(signature);

        // no-op run tracking (4 consecutive true-noops → stop)
        self.consecutive_true_noops = if any_true_noop { self.consecutive_true_noops + 1 } else { 0 };
        if self.consecutive_true_noops >= self.config.max_consecutive_true_noops {
            return StationarityDecision::HardStop;
        }

        let (nudge_at, stop_at) = if problematic {
            (self.config.nudge_problematic, self.config.hard_stop_problematic)
        } else {
            (self.config.nudge_general, self.config.hard_stop_general)
        };

        if self.run_len >= stop_at {
            return StationarityDecision::HardStop;
        }
        if self.run_len >= nudge_at && !self.nudged_this_run {
            self.nudged_this_run = true;
            return StationarityDecision::Nudge;
        }
        StationarityDecision::None
    }

    pub fn run_len(&self) -> usize {
        self.run_len
    }

    /// The nudge system-reminder (turn.rs:3894-3901 template idea).
    pub fn nudge_text(&self) -> serde_json::Value {
        json!({
            "reminder": "You are repeating the same tool calls with identical arguments. \
                         Try a different approach, or finish the turn with your best answer."
        })
    }
}

// ---- length salvage (length_salvage.rs:19-86) ----

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SalvageStep {
    /// Continue the truncated response; inject the reminder if armed.
    Continue { inject_reminder: bool },
    /// Budget exhausted: report exhaustion.
    Exhaust,
    /// No truncation happened; nothing to do.
    None,
}

pub struct LengthSalvage {
    budget: u32,
    continues: u32,
    reminder_armed: bool,
    truncated: bool,
}

impl LengthSalvage {
    /// Budget resolution precedence (`length_salvage.rs:19-45`): explicit
    /// kill switch (0) → off; cursor 5; env opt-in 2; else remote/default.
    pub fn with_budget(budget: u32) -> Self {
        LengthSalvage { budget, continues: 0, reminder_armed: true, truncated: false }
    }

    pub fn on_response(&mut self, stop: okra_providers::StopReason) -> SalvageStep {
        if stop != okra_providers::StopReason::MaxTokens {
            self.truncated = false;
            return SalvageStep::None;
        }
        self.truncated = true;
        if self.continues >= self.budget {
            return SalvageStep::Exhaust;
        }
        self.continues += 1;
        let inject = self.reminder_armed;
        self.reminder_armed = false; // once per turn (length_salvage.rs:12-14)
        SalvageStep::Continue { inject_reminder: inject }
    }

    /// A truncated tail forces MaxTokens on the outcome and disengages any
    /// todo gate (`length_salvage.rs` invariants).
    pub fn truncated(&self) -> bool {
        self.truncated
    }

    pub const REMINDER: &'static str = "Your previous response exceeded the output token limit and was cut off. \
        Continue from exactly where it stopped — or if a newer user message follows this note, answer that instead.";
}

// ---- rate-limit park (rate_limit_waits.rs) ----

#[derive(Debug, Clone, Copy)]
pub struct RateLimitWaitConfig {
    pub max_attempts: u32,
    pub max_total_wait_secs: u64,
}

impl Default for RateLimitWaitConfig {
    fn default() -> Self {
        // rate_limit_waits.rs:11-41 defaults
        RateLimitWaitConfig { max_attempts: 8, max_total_wait_secs: 150 }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RateLimitDecision {
    Wait { attempt: u32, backoff_secs: u64 },
    /// Main sessions never wait (`for_main_session`, :94-96).
    Disabled,
    NotRateLimited,
    BudgetSpent,
}

pub struct RateLimitWaitBudget {
    config: Option<RateLimitWaitConfig>,
    attempts: u32,
    total_wait_secs: u64,
}

impl RateLimitWaitBudget {
    pub fn for_main_session() -> Self {
        RateLimitWaitBudget { config: None, attempts: 0, total_wait_secs: 0 }
    }

    pub fn for_subagent(config: RateLimitWaitConfig) -> Self {
        RateLimitWaitBudget { config: Some(config), attempts: 0, total_wait_secs: 0 }
    }

    pub fn decide(&mut self, retry_after_secs: Option<u64>) -> RateLimitDecision {
        let Some(config) = self.config else {
            return RateLimitDecision::Disabled;
        };
        if self.attempts >= config.max_attempts || self.total_wait_secs >= config.max_total_wait_secs {
            return RateLimitDecision::BudgetSpent;
        }
        self.attempts += 1;
        // escalating backoff capped at 60s, honoring retry_after when present
        let backoff = retry_after_secs
            .unwrap_or(2u64 << self.attempts.min(5))
            .min(60);
        self.total_wait_secs += backoff;
        // an over-budget wait stops rather than truncating (:204-211)
        if self.total_wait_secs > config.max_total_wait_secs {
            return RateLimitDecision::BudgetSpent;
        }
        RateLimitDecision::Wait { attempt: self.attempts, backoff_secs: backoff }
    }

    pub fn attempts(&self) -> u32 {
        self.attempts
    }
}
