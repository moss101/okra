//! Port of `delta.ts` — the conversation delta ops.
//!
//! `delta.ts:1-8`: seven ops, closed set. There is no `row.inserted` (no
//! mid-insert), no `row.moved`, no field-level JSON patch — anything this
//! model cannot express forces a snapshot resync, deliberately shrinking the
//! client error surface.
//!
//! Scope: the five core ops (decision N0003). `statePatchSchema` is
//! `delta.ts:39-61` — key-level whole replacement (`Object.assign`), the key
//! set is closed, and values are **never** deep-merged within a key.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::core::StreamablePath;
use crate::rows::ConversationRow;

/// `StatePatch` (`delta.ts:39`). M0 carries the generic state-key map with the
/// donor's replacement (never merge) semantics enforced by `apply_delta`;
/// typed state keys (control/usage/queue/…) land with the snapshot crate in
/// M3 alongside the UI state catalog.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct StatePatch {
    #[serde(rename = "revision", skip_serializing_if = "Option::is_none")]
    pub revision: Option<u64>,
    /// Extra closed keys are represented as a JSON object so unknown-state
    /// resilience matches the donor's container rule (`delta.ts:54-56`): an
    /// old client **strips the key** and keeps every other key of the patch.
    #[serde(flatten)]
    pub keys: std::collections::BTreeMap<String, Value>,
}

/// `conversationDeltaSchema` (`delta.ts:94-144`), core five ops.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum ConversationDelta {
    /// Append to the tail (the 99% case).
    #[serde(rename = "row.appended")]
    RowAppended { row: ConversationRow },
    /// Whole-row replacement by rowId (state-machine transition).
    #[serde(rename = "row.upserted")]
    RowUpserted { row: ConversationRow },
    /// Delete this row and everything after it (edit/retry branch); applies
    /// to every loaded rowId >= fromRowId (`delta.ts:99-100`).
    #[serde(rename = "row.removed")]
    RowRemoved { from_row_id: RowIdArg },
    /// Streaming text append. Only ever applied to a row in streaming state —
    /// the server guarantees it, the client may assert it (`delta.ts:101`).
    #[serde(rename = "row.delta")]
    RowDelta {
        row_id: RowIdArg,
        path: StreamablePath,
        append: String,
    },
    /// Key-level state replacement (`delta.ts:108`).
    #[serde(rename = "state.updated")]
    StateUpdated { patch: StatePatch },
}

/// Wire spelling for row ids (donor uses plain numbers).
pub type RowIdArg = u64;

impl ConversationDelta {
    /// `isBarrier` (`coalesce.ts:18`): rule 4 — nothing may merge across a
    /// removal.
    pub fn is_barrier(&self) -> bool {
        matches!(self, ConversationDelta::RowRemoved { .. })
    }
}
