//! okra-kernel — the event-sourced session log, the only durable truth
//! (MASTER-PLAN §3 #4-#6, M1).
//!
//! Contracts ported:
//! - **deepseek** `packages/core/session`: event envelope, surface ops,
//!   "model-visible means logged", `SessionHandle` single-writer, torn-tail
//!   repair-on-first-append.
//! - **grok** `xai-grok-shell/src/session/storage/jsonl`: JSONL adapter,
//!   documented loss contract, torn-tail heal, `.corrupt` quarantine,
//!   buffered/durable append split.
//!
//! SQLite projections + the task index are M3 (MASTER-PLAN §3 #7) and will
//! sit behind the same trait seam.

mod event;
mod handle;
mod invariant;
mod lease;
mod storage;
mod surface;

pub use invariant::{check_log, InvariantError};
pub use event::{
    is_surface_event_type, validate_event, EventError, OpenedSession, Seq, SessionEvent,
    SessionHeader, SurfaceOp, CORE_EVENT_TYPES, SESSION_FORMAT_VERSION, SURFACE_EVENT_TYPES,
};
pub use handle::{HandleError, SessionAccess, SessionHandle};
pub use lease::{LeaseError, SessionWriteLease, LEASE_FILENAME};
pub use storage::{
    claim_write_lease, scan_log, AppendDurability, AppendOutcome, JsonlLog, ScanResult, StorageError, LOG_FILENAME,
};
pub use surface::{fold_surface, SurfaceError, SurfaceFoldResult, SurfaceNode};

use okra_protocol::Timestamp;

/// CLI-clock rule (`zcode-protocol-v4/core.ts:18`): timestamps are Unix ms
/// from the daemon's clock; clients never diff local clocks against them.
pub type Clock = fn() -> Timestamp;

/// Default wall clock (Unix ms).
pub fn wall_clock() -> Timestamp {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as Timestamp)
        .unwrap_or(0.0)
}

/// Create a surface event with the given clock; seq is assigned by the
/// single writer at append time.
pub fn make_event(event_type: &str, data: serde_json::Value, clock: Clock) -> SessionEvent {
    SessionEvent {
        event_type: event_type.to_string(),
        seq: 0, // assigned by SessionHandle::append
        time: clock(),
        data,
        ignorable: None,
        surface_op: Some(SurfaceOp::Append),
        source_event_seqs: None,
    }
}

/// Log-only event (turn/step/approval machinery).
pub fn make_log_only_event(event_type: &str, data: serde_json::Value, clock: Clock) -> SessionEvent {
    SessionEvent {
        surface_op: None,
        ..make_event(event_type, data, clock)
    }
}

/// A surface event replacing an inclusive seq range (used by compaction).
pub fn make_replace_event(
    event_type: &str,
    data: serde_json::Value,
    source_event_seqs: Vec<Seq>,
    start: Seq,
    end: Seq,
    clock: Clock,
) -> SessionEvent {
    SessionEvent {
        surface_op: Some(SurfaceOp::Replace { start_seq: start, end_seq: end }),
        source_event_seqs: Some(source_event_seqs),
        ..make_event(event_type, data, clock)
    }
}
