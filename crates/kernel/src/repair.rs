//! Interrupted-turn repair — port of deepseek
//! `packages/core/session/src/repair.ts:63-177` (openTurnClosers /
//! interruptedTurnClosers).
//!
//! A crash can leave the log ending inside an open turn. Recovery is
//! **logical, not physical**: every fully written event is preserved and
//! nothing is truncated. The repair synthesizes, in order:
//! 1. an error `tool/result` for every `tool/call` without a result —
//!    outcome unknown to the recovered process (code
//!    `TOOL_OUTCOME_UNKNOWN`), carrying model-visible retry guidance;
//! 2. `step/end` for every open step;
//! 3. `turn/end` with reason `{ kind: "interrupted" }`.
//!
//! The synthesized events are APPENDED by the write handle holder before
//! its first turn, closing the seq space — after repair, `check_log` passes
//! and no interrupted tool call can be "re-executed": its result is already
//! logged.

use crate::event::{SessionEvent, CORE_EVENT_TYPES};

/// Model-visible guidance baked into synthesized results (repair.ts:35-44).
pub const TOOL_OUTCOME_UNKNOWN_CODE: &str = "TOOL_OUTCOME_UNKNOWN";
pub const TOOL_OUTCOME_UNKNOWN_TEXT: &str = "[recovered] This tool call was interrupted by a \
    restart before its outcome was recorded. The side effect may or may not have happened. \
    Verify the current state (e.g. read the file) before retrying, and use NEW arguments \
    or a fresh call if you retry.";

/// Compute the closers needed to finish a log that ends mid-turn.
/// Returns them in append order with seqs continuing at `next_seq`.
pub fn interrupted_turn_closers(events: &[SessionEvent]) -> Vec<SessionEvent> {
    let mut next_seq = events.last().map(|e| e.seq + 1).unwrap_or(0);
    let mut closers = Vec::new();
    let now = crate::wall_clock();

    let mut open_calls: Vec<String> = Vec::new();
    let mut open_steps = 0u32;
    let mut open_turn = false;
    for ev in events {
        match ev.event_type.as_str() {
            "tool/call" => {
                if let Some(id) = ev.data.get("callId").and_then(|v| v.as_str()) {
                    open_calls.push(id.to_string());
                }
            }
            "tool/result" => {
                open_calls.pop();
            }
            "step/start" => open_steps += 1,
            "step/end" => open_steps = open_steps.saturating_sub(1),
            "turn/start" => open_turn = true,
            "turn/end" => open_turn = false,
            _ => {}
        }
    }

    let mut mk = |event_type: &str, data: serde_json::Value| SessionEvent {
        event_type: event_type.to_string(),
        seq: {
            let s = next_seq;
            next_seq += 1;
            s
        },
        time: now,
        data,
        ignorable: None,
        // tool/result is a SURFACE type (model-visible): its synthesized
        // closer carries an append op; turn/step closers stay log-only.
        surface_op: if event_type == "tool/result" {
            Some(crate::SurfaceOp::Append)
        } else {
            None
        },
        source_event_seqs: None,
    };

    for call_id in open_calls {
        closers.push(mk(
            "tool/result",
            serde_json::json!({
                "callId": call_id,
                "isError": true,
                "interrupted": true,
                "code": TOOL_OUTCOME_UNKNOWN_CODE,
                "text": TOOL_OUTCOME_UNKNOWN_TEXT,
            }),
        ));
    }
    for _ in 0..open_steps {
        closers.push(mk("step/end", serde_json::json!({ "reason": "interrupted" })));
    }
    if open_turn {
        closers.push(mk("turn/end", serde_json::json!({ "reason": { "kind": "interrupted" } })));
    }
    closers
}

/// True when the log ends inside an open turn (needs repair before use).
pub fn needs_repair(events: &[SessionEvent]) -> bool {
    if interrupted_turn_closers(events).is_empty() {
        // also flag unbalanced steps without a turn
        let mut open_steps = 0u32;
        let mut open_calls = 0u32;
        for ev in events {
            match ev.event_type.as_str() {
                "tool/call" => open_calls += 1,
                "tool/result" => open_calls = open_calls.saturating_sub(1),
                "step/start" => open_steps += 1,
                "step/end" => open_steps = open_steps.saturating_sub(1),
                _ => {}
            }
        }
        return open_steps > 0 || open_calls > 0;
    }
    true
}

/// Validate the closers against the vocabulary (defense against typos).
/// `tool/result` closers are surface events (append op); everything else is
/// log-only.
pub fn validate_closers(closers: &[SessionEvent]) -> Result<(), String> {
    for ev in closers {
        if !CORE_EVENT_TYPES.contains(&ev.event_type.as_str()) {
            return Err(format!("unknown closer type {}", ev.event_type));
        }
        if ev.event_type == "tool/result" {
            if !matches!(ev.surface_op, Some(crate::SurfaceOp::Append)) {
                return Err("tool/result closer must carry a surfaceOp".into());
            }
        } else if ev.surface_op.is_some() {
            return Err("closers other than tool/result must be log-only".into());
        }
        if !matches!(ev.surface_op, Some(crate::SurfaceOp::Append) | None) {
            return Err(format!("bad closer op for {}", ev.event_type));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ev(seq: u64, ty: &str, data: serde_json::Value) -> SessionEvent {
        SessionEvent {
            event_type: ty.to_string(),
            seq,
            time: 1.0,
            data,
            ignorable: None,
            surface_op: None,
            source_event_seqs: None,
        }
    }

    #[test]
    fn interrupted_turn_gets_call_result_and_closers() {
        let log = vec![
            ev(0, "turn/start", json!({})),
            ev(1, "step/start", json!({})),
            ev(2, "tool/call", json!({ "callId": "c1" })),
            ev(3, "tool/result", json!({ "callId": "c1" })),
            ev(4, "tool/call", json!({ "callId": "c2" })), // interrupted here
        ];
        assert!(needs_repair(&log));
        let closers = interrupted_turn_closers(&log);
        assert_eq!(closers.len(), 3, "result for c2 + step/end + turn/end");
        assert_eq!(closers[0].event_type, "tool/result");
        assert_eq!(closers[0].data["callId"], "c2");
        assert_eq!(closers[0].data["code"], TOOL_OUTCOME_UNKNOWN_CODE);
        assert_eq!(closers[1].event_type, "step/end");
        assert_eq!(closers[2].event_type, "turn/end");
        assert_eq!(closers[2].data["reason"]["kind"], "interrupted");
        // seqs continue the log
        assert_eq!(closers[0].seq, 5);
        // repaired log passes the invariant
        let mut repaired = log.clone();
        repaired.extend(closers);
        assert_eq!(crate::check_log(&repaired), Ok(()));
    }

    #[test]
    fn balanced_log_needs_no_repair() {
        let log = vec![
            ev(0, "turn/start", json!({})),
            ev(1, "tool/call", json!({ "callId": "a" })),
            ev(2, "tool/result", json!({ "callId": "a" })),
            ev(3, "turn/end", json!({})),
        ];
        assert!(!needs_repair(&log));
        assert!(interrupted_turn_closers(&log).is_empty());
        validate_closers(&interrupted_turn_closers(&log)).unwrap();
    }
}
