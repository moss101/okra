//! Backstops + semantic termination (MASTER-PLAN §3 #11, Codex dossier +
//! its own recommended fix): a turn ends because the model said it is done
//! (semantic), never because a numeric cap was hit — while hard backstops
//! (wall-clock, no-progress, spend) exist to stop runaway turns honestly.

use std::time::{Duration, Instant};

use okra_providers::{SampleResponse, StopReason, Usage};

/// Semantic termination: the model's EndTurn with no pending tool calls IS
/// the end of the turn. `StopGateDecision` (grok types.rs:254-258) lets a
/// stop-hook keep the agent working by injecting feedback.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopGateDecision {
    AllowStop,
    KeepWorking { feedback: String },
}

/// Evaluate semantic termination for a response.
pub fn evaluate_stop(
    response: &SampleResponse,
    pending_tool_calls: usize,
    stop_gate: &dyn Fn(&SampleResponse) -> StopGateDecision,
) -> StopGateDecision {
    if response.stop_reason == StopReason::ToolUse || pending_tool_calls > 0 {
        return StopGateDecision::KeepWorking {
            feedback: String::new(), // tool results follow; no injected feedback needed
        };
    }
    stop_gate(response)
}

#[derive(Debug, Clone)]
pub struct BackstopConfig {
    pub max_wall_clock: Duration,
    /// Steps with zero forward progress before intervention.
    pub max_idle_steps: usize,
    /// Cumulative token spend ceiling.
    pub max_total_tokens: u64,
}

impl Default for BackstopConfig {
    fn default() -> Self {
        BackstopConfig {
            max_wall_clock: Duration::from_secs(15 * 60),
            max_idle_steps: 12,
            max_total_tokens: 2_000_000,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackstopTrip {
    None,
    WallClock { elapsed: Duration },
    NoProgress { idle_steps: usize },
    Spend { total_tokens: u64 },
}

/// Tracks the three backstops over one turn.
#[derive(Debug, Clone)]
pub struct Backstops {
    config: BackstopConfig,
    started_at: Instant,
    idle_steps: usize,
    total_tokens: u64,
    last_progress_token: u64,
}

impl Backstops {
    pub fn new(config: BackstopConfig) -> Self {
        Backstops {
            config,
            started_at: Instant::now(),
            idle_steps: 0,
            total_tokens: 0,
            last_progress_token: 0,
        }
    }

    pub fn on_step(&mut self, response: &SampleResponse, progressed: bool) -> BackstopTrip {
        self.total_tokens += response.usage.input_tokens + response.usage.output_tokens;
        if progressed {
            self.idle_steps = 0;
            self.last_progress_token = self.total_tokens;
        } else {
            self.idle_steps += 1;
        }

        let elapsed = self.started_at.elapsed();
        if elapsed >= self.config.max_wall_clock {
            return BackstopTrip::WallClock { elapsed };
        }
        if self.total_tokens >= self.config.max_total_tokens {
            return BackstopTrip::Spend { total_tokens: self.total_tokens };
        }
        if self.idle_steps >= self.config.max_idle_steps {
            return BackstopTrip::NoProgress { idle_steps: self.idle_steps };
        }
        BackstopTrip::None
    }

    pub fn total_tokens(&self) -> u64 {
        self.total_tokens
    }

    pub fn idle_steps(&self) -> usize {
        self.idle_steps
    }
}

/// Honest backstop reporting: a backstop-tripped turn is CANCELLED with a
/// reason, never reported as a completed answer (dossier fix: numeric caps
/// must not masquerade as semantic termination).
pub fn backstop_outcome(trip: BackstopTrip) -> Option<String> {
    match trip {
        BackstopTrip::None => None,
        BackstopTrip::WallClock { elapsed } => Some(format!(
            "turn stopped by the wall-clock backstop after {:.0}s (no semantic end)",
            elapsed.as_secs_f32()
        )),
        BackstopTrip::NoProgress { idle_steps } => Some(format!(
            "turn stopped by the no-progress backstop after {idle_steps} idle steps"
        )),
        BackstopTrip::Spend { total_tokens } => Some(format!(
            "turn stopped by the spend backstop at {total_tokens} tokens"
        )),
    }
}

/// Max-turns guard: a DIFFERENT honest limit than semantic termination —
/// explicitly reported as MaxTurnsReached in the outcome (grok
/// types.rs:104-108).
#[derive(Debug, Clone, Copy)]
pub struct MaxTurnsGuard {
    pub limit: usize,
}

impl MaxTurnsGuard {
    pub fn tripped(&self, steps: usize) -> bool {
        steps >= self.limit
    }
}

/// Usage accumulator for spend reporting.
pub fn accumulate(a: Usage, b: Usage) -> Usage {
    Usage {
        input_tokens: a.input_tokens + b.input_tokens,
        output_tokens: a.output_tokens + b.output_tokens,
    }
}
