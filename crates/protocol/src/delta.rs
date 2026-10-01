//! Port of `delta.ts` — the conversation delta ops.
//!
//! `delta.ts:1-8`: seven ops, closed set. There is no `row.inserted` (no
//! mid-insert), no `row.moved`, no field-level JSON patch — anything this
//! model cannot express forces a snapshot resync, deliberately shrinking the
//! client error surface.
//!
//! The five core ops landed in M0 (decision N0003); the two `workflowRun.*`
//! ops are re-added here in M3 together with the workflow-runs domain that
//! owns their merge/bounds logic (`workflow_runs.rs`). `statePatchSchema` is
//! `delta.ts:39-61` — key-level whole replacement (`Object.assign`), the key
//! set is closed, and values are **never** deep-merged within a key.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::core::StreamablePath;
use crate::rows::ConversationRow;
use crate::workflow_runs::{pairs_set, WorkflowRunActor, WorkflowRunNode};

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

/// `conversationDeltaSchema` (`delta.ts:94-144`), seven ops: the M0 core
/// five plus the two `workflowRun.*` key-level ops (N0003 M3).
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
    /// One dwf run's key-level increment (`delta.ts:110-120`). Carried as a
    /// newtype so apply/merge/bounds functions take the payload typed;
    /// `revision` is the ABSOLUTE `workflowRuns.revision` after this change,
    /// and one engine event produces at most one such op (node phase,
    /// derived actor state, usage and watermarks land atomically together).
    #[serde(rename = "workflowRun.updated")]
    WorkflowRunUpdated(WorkflowRunUpdate),
    /// This run was evicted by the producer (`delta.ts:138-143`) — only the
    /// producer evicts, and it must say so; clients never apply caps alone.
    #[serde(rename = "workflowRun.removed")]
    WorkflowRunRemoved { run_id: String, revision: u64 },
}

/// `workflowRunHeaderPatchSchema` (`delta.ts:73-75`): partial header update —
/// present keys whole-key replace, absent keys stay untouched (never a deep
/// merge). Kept as ordered `(key, value)` pairs in document order: merge
/// composes patches with TS object-spread semantics and the wire keeps the
/// producer's key order.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct WorkflowRunHeaderPatch(pub Vec<(String, Value)>);

impl WorkflowRunHeaderPatch {
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.0.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl Serialize for WorkflowRunHeaderPatch {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut map = serializer.serialize_map(Some(self.0.len()))?;
        for (key, value) in &self.0 {
            map.serialize_entry(key, value)?;
        }
        map.end()
    }
}

impl<'de> Deserialize<'de> for WorkflowRunHeaderPatch {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct PatchVisitor;
        impl<'de> serde::de::Visitor<'de> for PatchVisitor {
            type Value = WorkflowRunHeaderPatch;
            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a workflow run header patch object")
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                mut access: A,
            ) -> Result<Self::Value, A::Error> {
                let mut pairs: Vec<(String, Value)> = Vec::new();
                while let Some((key, value)) = access.next_entry::<String, Value>()? {
                    pairs_set(&mut pairs, key, value);
                }
                Ok(WorkflowRunHeaderPatch(pairs))
            }
        }
        deserializer.deserialize_map(PatchVisitor)
    }
}

/// `workflowRunEntryRefSchema` (`delta.ts:82-92`): the identity of an evicted
/// entry, shared by both tables. Deliberately identity-ONLY — all an eviction
/// has to say is "this one left"; carrying the entry itself would read as an
/// upsert.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowRunEntryRef {
    pub site_id: String,
    pub ordinal: u64,
}

/// The `workflowRun.updated` payload (`delta.ts:121-137`). Six payloads with
/// six semantics: `run` whole-key replaces header keys, `cleared` names keys
/// that became absent, `removedActors`/`removedNodes` delete entries by
/// (siteId, ordinal), `actors`/`nodes` whole-entry upsert by the same key.
/// Table caps equal the state key's — an increment must not be able to
/// assemble an illegal state.
///
/// Field declaration order states the application order, header → removals
/// → upserts (`delta.ts:117-120`): a key removed and re-added in one op
/// (a run that overflowed clearing its tables on resume, or an evicted entry
/// coming back) must land at the table TAIL to match two sequentially
/// applied ops.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowRunUpdate {
    pub run_id: String,
    pub revision: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<WorkflowRunHeaderPatch>,
    /// Keys that became ABSENT ("zero entries ⇒ key absent" is the reports /
    /// pendingQuestions convention, so increments must be able to say it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cleared: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub removed_actors: Option<Vec<WorkflowRunEntryRef>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub removed_nodes: Option<Vec<WorkflowRunEntryRef>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actors: Option<Vec<WorkflowRunActor>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nodes: Option<Vec<WorkflowRunNode>>,
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
