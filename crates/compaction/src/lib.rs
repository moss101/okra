//! okra-compaction — context plane (MASTER-PLAN §3 #27-#32).
//!
//! Landed:
//! - origin-tagged context messages, 13-member closed union (§3 #31,
//!   kimi contextMemory/types.ts:114-126 + okra steering)
//! - validated compaction summary schema — reject, never install (§3 #30,
//!   clean-room)
//! - byte-stable world_state projection for prefix-cache hits (§3 #32,
//!   clean-room)
//!
//! M2 adds: grok prefire two-pass (§3 #27), qwen microcompaction (§3 #28),
//! ZCode post-compaction file-state hydration (§3 #29).

pub mod origin;
pub mod summary;
pub mod world_state;

pub use origin::{Origin, OriginTaggedMessage};
pub use summary::{check, validate_summary, CompactionSummary, FileStateNote, SummaryValidationError};
pub use world_state::{Entry, Section, WorldState, SECTION_ORDER};

/// Wire-name alias used in tool/result payloads.
pub use origin::Origin as OriginTag;
