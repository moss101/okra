//! okra-protocol — port of ZCode `packages/shared/src/zcode-protocol-v4/`.
//!
//! Discipline inherited from the donor (`core.ts:2-3`): **schema types + pure
//! functions only** — no runtime, no IO, no transport logic in this crate.
//!
//! Ported blocks (MASTER-PLAN §3 #1-#3):
//! - wire/snapshot/delta model with payload caps and delivery profiles
//!   `continuous` / `replayable` (`core.ts`)
//! - physical wire frames with UTF-8-byte-level fragmentation and crc32
//!   checksums (`wire.ts`, `wire-codec.ts`, `wire-binary.ts`)
//! - coalesce rules 1-6 + `conflate_by_key` (`coalesce.ts`; rule 6 and the
//!   two `workflowRun.*` ops re-added per decision N0003's M3 milestone)
//! - byte-convergence golden tests regenerated from the TS ground truth
//!   (`tests/golden/`; the convergence suite itself was referenced by the
//!   donor at `profiles.ts:5-8` but absent from its public tree — recreated).

mod codec;
mod coalesce;
mod core;
mod delta;
mod frame;
mod rows;
mod workflow_runs;

pub use codec::{decode_wire_base64, encode_wire_bytes_base64, crc32_wire_bytes};
pub use coalesce::{
    apply_delta, coalesce_conversation_deltas, coalesce_conversation_deltas_with_bounds,
    conflate_by_key, ConversationState,
};
pub use core::{
    delivery_profile, DeliveryProfile, DeliveryProfileName, ProtocolV4Limits, StreamablePath,
    StreamablePathSet, Timestamp, V4_WIRE_PROTOCOL_VERSION,
};
pub use delta::{
    ConversationDelta, RowIdArg, StatePatch, WorkflowRunEntryRef, WorkflowRunHeaderPatch,
    WorkflowRunUpdate,
};
pub use frame::{
    assemble_fragments, encode_topic_wire_frames, Crc32Algorithm, EncodeTopicWireFramesOptions,
    TopicFrameDeliveryKind, TopicWireChecksum, TopicWireFrame, WireFrameError, WireVersion,
    MAX_FRAME_BYTES_LIMIT,
};
pub use rows::{ConversationRow, EditDisposition, ResponseState, RowId, RowBase, ToolStatus};
pub use workflow_runs::{
    apply_workflow_run_removed, apply_workflow_run_updated, canonical_workflow_run,
    diff_workflow_runs_state, entry_key, is_complete_header, json_value_equal,
    merge_workflow_run_updates, update_within_wire_bounds, WorkflowRunActor, WorkflowRunNode,
    WorkflowRunsLimits, WorkflowRunsState, WorkflowRunState, WorkflowRunWireBounds,
};
