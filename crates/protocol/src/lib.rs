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
//! - coalesce rules 1-4 + `conflate_by_key` (`coalesce.ts`; rule 6 deferred,
//!   see decision N0003)
//! - byte-convergence golden tests regenerated from the TS ground truth
//!   (`tests/golden/`; the convergence suite itself was referenced by the
//!   donor at `profiles.ts:5-8` but absent from its public tree — recreated).

mod codec;
mod coalesce;
mod core;
mod delta;
mod frame;
mod rows;

pub use codec::{decode_wire_base64, encode_wire_bytes_base64, crc32_wire_bytes};
pub use coalesce::{apply_delta, coalesce_conversation_deltas, conflate_by_key, ConversationState};
pub use core::{
    delivery_profile, DeliveryProfile, DeliveryProfileName, ProtocolV4Limits, StreamablePath,
    StreamablePathSet, Timestamp, V4_WIRE_PROTOCOL_VERSION,
};
pub use delta::{ConversationDelta, RowIdArg, StatePatch};
pub use frame::{
    assemble_fragments, encode_topic_wire_frames, Crc32Algorithm, EncodeTopicWireFramesOptions,
    TopicFrameDeliveryKind, TopicWireChecksum, TopicWireFrame, WireFrameError, WireVersion,
    MAX_FRAME_BYTES_LIMIT,
};
pub use rows::{ConversationRow, EditDisposition, ResponseState, RowId, RowBase, ToolStatus};
