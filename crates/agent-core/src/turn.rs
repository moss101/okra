//! Turn phase machine — ZCode `turn-state.ts` semantics re-expressed as a
//! Rust enum + `match` (MASTER-PLAN §3 #10): illegal transitions are
//! rejected at runtime with the donor's `InvalidTurnPhase` error class.
//!
//! `TurnPhase` variants (`turn-state.ts:25-37`); `canTransitionTo`
//! (`turn-state.ts:219-226`): completing | error are terminal sinks;
//! transitions are enforced in `turn-machine.ts:92-103`.

use serde::{Deserialize, Serialize};

/// The canonical turn phases (ZCode `turn-state.ts:25-37`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnPhase {
    Idle,
    ProcessingInput,
    AwaitingModelResponse,
    Streaming,
    SchedulingTools,
    ExecutingTools,
    AggregatingResults,
    AwaitingPermission,
    Completing,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("InvalidTurnPhase: cannot transition from {from:?} to {to:?}")]
pub struct InvalidTurnPhase {
    pub from: TurnPhase,
    pub to: TurnPhase,
}

/// Legal transitions, written as ONE total function over (from, to) so the
/// illegal space is explicit and testable.
pub fn can_transition(from: TurnPhase, to: TurnPhase) -> bool {
    use TurnPhase::*;
    match (from, to) {
        // same-phase re-entry is a no-op, not an error
        (a, b) if a == b => true,
        // a finished turn resets the machine for the next turn
        (Error | Completing, Idle) => true,
        // Completing is otherwise a sink within a turn
        (Completing, _) => false,
        // any active phase may fail or complete
        (_, Error) => true,
        (_, Completing) => from != TurnPhase::Idle,
        (Idle, ProcessingInput) => true,
        (ProcessingInput, AwaitingModelResponse) => true,
        (AwaitingModelResponse, Streaming) => true,
        // a response may complete with no tool work (semantic end, refusal,
        // length-salvage exhaustion) or go straight to the next step
        (Streaming, AggregatingResults) => true,
        (Streaming, SchedulingTools) => true,
        (SchedulingTools, ExecutingTools) => true,
        (ExecutingTools, AwaitingPermission) => true,
        (AwaitingPermission, ExecutingTools) => true,
        (ExecutingTools, AggregatingResults) => true,
        (AggregatingResults, AwaitingModelResponse) => true, // next step
        (AggregatingResults, SchedulingTools) => true,       // steering landed
        _ => false,
    }
}

/// The guarded machine. `transition` rejects with the donor's
/// InvalidTurnPhase (`turn-machine.ts:92-103`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TurnMachine {
    phase: TurnPhase,
}

impl TurnMachine {
    pub fn new() -> Self {
        TurnMachine { phase: TurnPhase::Idle }
    }

    pub fn phase(&self) -> TurnPhase {
        self.phase
    }

    pub fn transition(&mut self, to: TurnPhase) -> Result<TurnPhase, InvalidTurnPhase> {
        let from = self.phase;
        if !can_transition(from, to) {
            return Err(InvalidTurnPhase { from, to });
        }
        self.phase = to;
        Ok(to)
    }

    pub fn is_terminal(&self) -> bool {
        matches!(self.phase, TurnPhase::Completing | TurnPhase::Error)
    }
}

impl Default for TurnMachine {
    fn default() -> Self {
        Self::new()
    }
}

// ---- grok's emitted phase vocabulary (session-events types.rs:400-408) ----

/// The phase labels emitted to the event log while a turn runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EmittedPhase {
    WaitingForModel,
    StreamingText,
    StreamingReasoning,
    ToolExecution,
    PermissionPrompt,
}

/// `TurnOutcome` (grok `acp_session_impl/types.rs:93-114`).
#[derive(Debug, Clone, PartialEq)]
pub enum TurnOutcome {
    Completed {
        tools_called: Vec<String>,
        structured_output: Option<okra_providers::StructuredOutput>,
        stop: CompletedStop,
    },
    Cancelled {
        category: Option<CancellationCategory>,
    },
    MaxTurnsReached {
        limit: usize,
    },
    /// Silent EndTurn; kept distinct so recovery/goal paths cannot re-open
    /// the loop (`types.rs:111-113`).
    StationarityEnded,
}

/// `CompletedStop` (`types.rs:84-90`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CompletedStop {
    EndTurn,
    MaxTokens,
    Refusal,
}

/// `CancellationCategory` (`types.rs:437-444`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CancellationCategory {
    HookDenied,
    PermissionRejected,
    PermissionCancelled,
    MidTurnAbort,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn happy_path_walks_the_machine() {
        let mut m = TurnMachine::new();
        for p in [
            TurnPhase::ProcessingInput,
            TurnPhase::AwaitingModelResponse,
            TurnPhase::Streaming,
            TurnPhase::SchedulingTools,
            TurnPhase::ExecutingTools,
            TurnPhase::AggregatingResults,
            TurnPhase::AwaitingModelResponse, // second step
            TurnPhase::Streaming,
            TurnPhase::Completing,
        ] {
            m.transition(p).unwrap();
        }
        assert!(m.is_terminal());
    }

    #[test]
    fn illegal_transitions_are_rejected() {
        let mut m = TurnMachine::new();
        assert!(m.transition(TurnPhase::Streaming).is_err());
        m.transition(TurnPhase::ProcessingInput).unwrap();
        m.transition(TurnPhase::Completing).unwrap();
        assert!(m.transition(TurnPhase::Streaming).is_err());
        assert!(m.transition(TurnPhase::ExecutingTools).is_err());
    }

    #[test]
    fn any_active_phase_may_fail() {
        // walk to each phase legally, then fail from it
        let walks: &[&[TurnPhase]] = &[
            &[TurnPhase::ProcessingInput],
            &[TurnPhase::ProcessingInput, TurnPhase::AwaitingModelResponse],
            &[TurnPhase::ProcessingInput, TurnPhase::AwaitingModelResponse, TurnPhase::Streaming],
            &[
                TurnPhase::ProcessingInput,
                TurnPhase::AwaitingModelResponse,
                TurnPhase::Streaming,
                TurnPhase::SchedulingTools,
                TurnPhase::ExecutingTools,
            ],
        ];
        for walk in walks.iter() {
            let mut m = TurnMachine::new();
            for p in walk.iter() {
                m.transition(*p).unwrap();
            }
            m.transition(TurnPhase::Error).unwrap();
        }
    }

    #[test]
    fn error_recovers_to_idle_for_next_turn() {
        let mut m = TurnMachine::new();
        m.transition(TurnPhase::ProcessingInput).unwrap();
        m.transition(TurnPhase::Error).unwrap();
        m.transition(TurnPhase::Idle).unwrap();
    }
}
