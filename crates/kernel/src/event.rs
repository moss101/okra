//! Event envelope — port of deepseek `packages/core/session/src/types.ts`.
//!
//! The rule (donor `docs/architecture.md:125`): **model-visible means
//! logged.** Anything that reaches a model request must be reconstructable
//! from the log; a new model-visible input requires a session event.
//!
//! Surface ops vs log-only (`types.ts:433-478`): exactly five event types
//! produce LLM-visible surface nodes and are REQUIRED to carry a
//! `surfaceOp`; every other type is log-only and FORBIDDEN to carry one.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use okra_protocol::Timestamp;

/// Storage format version (donor `SESSION_FORMAT_VERSION = 4`).
pub const SESSION_FORMAT_VERSION: u64 = 4;

/// Monotonic sequence number, contiguous from 0.
pub type Seq = u64;

/// `SurfaceEventType` (`types.ts:433-440`) — the only LLM-message-producing
/// types.
pub const SURFACE_EVENT_TYPES: [&str; 5] = [
    "system/message",
    "developer/message",
    "user/message",
    "assistant/message",
    "tool/result",
];

pub fn is_surface_event_type(ty: &str) -> bool {
    SURFACE_EVENT_TYPES.contains(&ty)
}

/// `SurfaceOp` (`types.ts:442-448`): append, or replace an inclusive seq
/// range with this node.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SurfaceOp {
    Append,
    Replace { start_seq: Seq, end_seq: Seq },
}

/// `SessionEvent` (`types.ts:493-516`). Discriminated over `type`.
/// `ignorable: true` marks types a reader may skip when unrecognized; a
/// reader hitting an unrecognized type WITHOUT the marker must refuse
/// reconstruction instead of silently dropping (`types.ts:502-511`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionEvent {
    #[serde(rename = "type")]
    pub event_type: String,
    pub seq: Seq,
    pub time: Timestamp,
    pub data: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ignorable: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub surface_op: Option<SurfaceOp>,
    /// Complete non-empty set of cited sources for replacements on non-
    /// assistant surface events (`types.ts:466-476`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_event_seqs: Option<Vec<Seq>>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EventError {
    #[error("surface event `{0}` requires a surfaceOp")]
    SurfaceOpRequired(String),
    #[error("log-only event `{0}` must not carry a surfaceOp")]
    SurfaceOpForbidden(String),
    #[error("replace on `{0}` must cite sourceEventSeqs covering the replaced nodes")]
    SourceSeqsRequired(String),
    #[error("replace range [{start},{end}] must reference earlier events (event seq {seq})")]
    ReplaceRangeInvalid {
        start: Seq,
        end: Seq,
        seq: Seq,
    },
    #[error("empty replace range [{start},{end}]")]
    ReplaceRangeEmpty { start: Seq, end: Seq },
    #[error("unrecognized event type `{0}` without ignorable marker")]
    UnignorableUnknownType(String),
}

/// Compile-time-equivalent enforcement of the surface rules
/// (`Session.append` in the donor validates exactly these).
pub fn validate_event(event: &SessionEvent, known_types: &[&str]) -> Result<(), EventError> {
    let surface = is_surface_event_type(&event.event_type);
    match (surface, &event.surface_op) {
        (true, None) => return Err(EventError::SurfaceOpRequired(event.event_type.clone())),
        (false, Some(_)) => return Err(EventError::SurfaceOpForbidden(event.event_type.clone())),
        _ => {}
    }
    if let Some(SurfaceOp::Replace { start_seq, end_seq }) = &event.surface_op {
        if start_seq > end_seq {
            return Err(EventError::ReplaceRangeEmpty {
                start: *start_seq,
                end: *end_seq,
            });
        }
        if *end_seq >= event.seq {
            return Err(EventError::ReplaceRangeInvalid {
                start: *start_seq,
                end: *end_seq,
                seq: event.seq,
            });
        }
        // assistant/message embeds its stream instead of citing sources
        // (`types.ts:468-471`); every other replaced surface event must cite.
        if event.event_type != "assistant/message"
            && event
                .source_event_seqs
                .as_ref()
                .is_none_or(|s| s.is_empty())
        {
            return Err(EventError::SourceSeqsRequired(event.event_type.clone()));
        }
    }
    if !surface
        && !known_types.contains(&event.event_type.as_str())
        && event.ignorable != Some(true)
    {
        // vocabulary growth without version bumps: unknown types need the
        // marker or reconstruction must refuse (types.ts:502-511)
        return Err(EventError::UnignorableUnknownType(event.event_type.clone()));
    }
    Ok(())
}

/// Standard M0 vocabulary (donor `SessionEventMap`, `types.ts:281-428`).
pub const CORE_EVENT_TYPES: [&str; 16] = [
    "turn/start",
    "turn/end",
    "step/start",
    "step/end",
    "user/message",
    "developer/message",
    "system/message",
    "assistant/message",
    "assistant/attempt",
    "tool/call",
    "tool/result",
    "request/header",
    "request/context",
    "session/end-seed",
    "approval/asked",
    "approval/decided",
];

/// `SessionHeader` (`types.ts:94-131`) — immutable storage metadata kept
/// OUTSIDE the event log.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionHeader {
    pub version: u64,
    pub id: String,
    pub created_at: Timestamp,
    pub cwd: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session: Option<String>,
    #[serde(default)]
    pub is_seeded: bool,
}

/// Immutable header + derived state returned by open.
#[derive(Debug, Clone)]
pub struct OpenedSession {
    pub header: SessionHeader,
    /// Number of committed events at open time.
    pub event_count: usize,
}
