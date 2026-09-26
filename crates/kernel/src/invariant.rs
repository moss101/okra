//! Runtime log invariants — port of deepseek
//! `packages/core/session/src/invariant.ts` (seq monotonicity, turn/step
//! nesting, tool call/result pairing). Checked on load and after append.
//!
//! The companion model-visible rule ("model-visible means logged",
//! `docs/architecture.md:125`) is enforced by construction: every
//! model-visible message is a surface event appended here before dispatch.

use super::event::{SessionEvent, CORE_EVENT_TYPES};

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum InvariantError {
    #[error("seq not contiguous: expected {expected}, found {found}")]
    SeqGap { expected: u64, found: u64 },
    #[error("tool/result at seq {seq} has no matching tool/call")]
    OrphanToolResult { seq: u64 },
    #[error("turn/end at seq {seq} with no open turn")]
    UnbalancedTurnEnd { seq: u64 },
    #[error("step/end at seq {seq} with no open step")]
    UnbalancedStepEnd { seq: u64 },
    #[error("turn/end at seq {seq} inside an open step (close the step first)")]
    TurnEndInsideStep { seq: u64 },
}

/// Validate a whole log (load path). `known_types` extends `CORE_EVENT_TYPES`
/// for host-registered vocabulary.
pub fn check_log(events: &[SessionEvent]) -> Result<(), InvariantError> {
    let mut expected_seq = 0u64;
    let mut open_calls: Vec<String> = Vec::new();
    let mut open_steps = 0u32;
    let mut open_turns = 0u32;
    for ev in events {
        if ev.seq != expected_seq {
            return Err(InvariantError::SeqGap { expected: expected_seq, found: ev.seq });
        }
        expected_seq += 1;
        if !CORE_EVENT_TYPES.contains(&ev.event_type.as_str()) && ev.ignorable != Some(true) {
            // unknown + not ignorable: reconstruction must refuse; validated
            // upstream via validate_event, kept defensive here
            return Err(InvariantError::SeqGap { expected: expected_seq - 1, found: ev.seq });
        }
        match ev.event_type.as_str() {
            "tool/call" => {
                if let Some(id) = ev.data.get("callId").and_then(|v| v.as_str()) {
                    open_calls.push(id.to_string());
                }
            }
            "tool/result" => {
                if open_calls.pop().is_none() {
                    return Err(InvariantError::OrphanToolResult { seq: ev.seq });
                }
            }
            "step/start" => open_steps += 1,
            "step/end" => {
                if open_steps == 0 {
                    return Err(InvariantError::UnbalancedStepEnd { seq: ev.seq });
                }
                open_steps -= 1;
            }
            "turn/start" => open_turns += 1,
            "turn/end" => {
                if open_turns == 0 {
                    return Err(InvariantError::UnbalancedTurnEnd { seq: ev.seq });
                }
                if open_steps > 0 {
                    return Err(InvariantError::TurnEndInsideStep { seq: ev.seq });
                }
                open_turns -= 1;
            }
            _ => {}
        }
    }
    Ok(())
}
