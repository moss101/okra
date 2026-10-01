//! Port of `workflow-runs-delta.ts` — key-level deltas for `workflowRuns`.
//!
//! Why this module exists (`workflow-runs-delta.ts:1-7`): `workflowRuns` is a
//! high-frequency state key while `state.updated` semantics are whole-key
//! replacement — one engine event touches one node yet would resend the whole
//! table, O(N) bytes per event and O(N²) per run. These ops carry "what
//! changed in this step" so wire bytes scale with the change, not the state.
//!
//! Contract (`workflow-runs-delta.ts:17-19`): for any reducer-produced
//! (prior, next) pair, applying `diff(prior, next)` to prior yields a state
//! that serializes **byte-identically** to next — not merely deep-equal. Key
//! ORDER is therefore a first-class citizen here ([`canonical_workflow_run`]).
//!
//! Structural facts the code leans on (`workflow-runs-delta.ts:9-15`): the
//! reducer only ever evicts terminal entries (never reorders), gives only the
//! one changed run a fresh object, and never depends on reference identity
//! for correctness — structural comparison is always the fallback.

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::fmt;

use serde::de;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Map, Value};

use crate::delta::{
    ConversationDelta, StatePatch, WorkflowRunEntryRef, WorkflowRunHeaderPatch, WorkflowRunUpdate,
};

/// `WORKFLOW_RUNS_LIMITS` (`workflow-runs.ts:14-98`) — the four capacity
/// numbers the delta machinery needs. Display-state caps, not engine
/// contracts; the wire bounds reuse `MAX_ACTORS`/`MAX_NODES`
/// (`workflow-runs-delta.ts:420-430`).
pub struct WorkflowRunsLimits;

impl WorkflowRunsLimits {
    /// Recent runs kept; oldest evicted first (`workflow-runs.ts:16`).
    pub const MAX_RUNS: usize = 8;
    /// Shared actor/node capacity so a node is never displayable while its
    /// subagent was already truncated (`workflow-runs.ts:23-24`).
    pub const MAX_ACTORS: usize = 1_024;
    pub const MAX_NODES: usize = 1_024;
    /// Entry budget across the whole state key: sum of every run's
    /// `nodes.length + actors.length` (`workflow-runs.ts:33`).
    pub const MAX_TOTAL_ENTRIES: usize = 6_144;
}

/// Canonical run key order = `workflowRunSchema` declaration order
/// (`workflow-runs-delta.ts:47`, schema at `workflow-runs.ts:361-505`).
/// Hardcoded so producer and consumer cannot drift apart — the byte contract
/// holds only if both sides reorder against the same table.
const WORKFLOW_RUN_KEYS: [&str; 25] = [
    "runId",
    "toolCallId",
    "status",
    "stopReason",
    "resumedFrom",
    "supersededBy",
    "usage",
    "error",
    "resumable",
    "resultPreview",
    "actors",
    "nodes",
    "reports",
    "pendingQuestions",
    "concurrency",
    "concurrencyCeiling",
    "subagentModel",
    "artifacts",
    "phases",
    "currentPhase",
    "phaseNames",
    "phaseAlongside",
    "unlistedByPhase",
    "truncated",
    "lastEventSequence",
];

/// Required header keys: schema fields without `.optional()`
/// (`workflow-runs-delta.ts:59-64`; required at `workflow-runs.ts:362,366,376,504`).
/// Used in exactly one place: judging whether a `workflowRun.updated` header
/// is fit to make an unknown run be **born**.
const WORKFLOW_RUN_REQUIRED_HEADER_KEYS: [&str; 4] = ["runId", "status", "usage", "lastEventSequence"];

/// Entry-table membership: header = the run minus the two tables synced by
/// (siteId, ordinal) deltas (`workflow-runs-delta.ts:50-53`).
fn is_entry_table_key(key: &str) -> bool {
    key == "actors" || key == "nodes"
}

/// Ordered-pair map helpers shared by the run header and the header patch.
/// Both containers keep document-order pairs; `pairs_set` is last-wins
/// keeping the key's first position — the JSON.parse duplicate-key rule,
/// which is what the TS object spread collapses to.
pub(crate) fn pairs_get<'a>(pairs: &'a [(String, Value)], key: &str) -> Option<&'a Value> {
    pairs.iter().find(|(k, _)| k == key).map(|(_, v)| v)
}

pub(crate) fn pairs_set(pairs: &mut Vec<(String, Value)>, key: String, value: Value) {
    if let Some(slot) = pairs.iter_mut().find(|(k, _)| *k == key) {
        slot.1 = value;
    } else {
        pairs.push((key, value));
    }
}

pub(crate) fn pairs_remove(pairs: &mut Vec<(String, Value)>, key: &str) {
    pairs.retain(|(k, _)| k != key);
}

/// `workflowRunEntryKey` (`workflow-runs-delta.ts:66-69`): the dedup key of
/// the two entry tables. `\0` separation so ("a",12) and ("a1",2) never collide.
pub fn entry_key(site_id: &str, ordinal: u64) -> String {
    format!("{site_id}\0{ordinal}")
}

/// Entry identity for the generic table helpers (the TS structural bound
/// `{ siteId: string; ordinal: number }`).
trait RunEntry {
    fn site_id(&self) -> &str;
    fn ordinal(&self) -> u64;
}

/// `workflowRunActorSchema` (`workflow-runs.ts:181-196`). `status` is a
/// DERIVED three-state value (no observable actor-level failure exists), so
/// the remaining fields ride as untyped extras: field order inside entries is
/// not part of the canonical contract — identity is (siteId, ordinal).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowRunActor {
    pub site_id: String,
    pub ordinal: u64,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// `workflowRunNodeSchema` (`workflow-runs.ts:222-271`). `phase` is the
/// lifecycle phase the engine actually emitted; `kind` may be absent on
/// resume short-circuits. Same extras rationale as [`WorkflowRunActor`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowRunNode {
    pub site_id: String,
    pub ordinal: u64,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl RunEntry for WorkflowRunActor {
    fn site_id(&self) -> &str {
        &self.site_id
    }
    fn ordinal(&self) -> u64 {
        self.ordinal
    }
}

impl RunEntry for WorkflowRunNode {
    fn site_id(&self) -> &str {
        &self.site_id
    }
    fn ordinal(&self) -> u64 {
        self.ordinal
    }
}

impl RunEntry for WorkflowRunEntryRef {
    fn site_id(&self) -> &str {
        &self.site_id
    }
    fn ordinal(&self) -> u64 {
        self.ordinal
    }
}

/// `workflowRunSchema` (`workflow-runs.ts:361-505`) modeled for the byte
/// contract: `runId`/`status` typed (required strings), the remaining header
/// as ordered `(key, value)` pairs, and the two entry tables typed.
///
/// Invariant: `header` never holds a `runId`/`status`/`actors`/`nodes` key —
/// deserialization routes those to the typed fields and apply/diff filter
/// them out of patches. After [`canonical_workflow_run`], `header` is known
/// keys in schema order followed by unknown keys in arrival order; the
/// serializer derives emission order from [`WORKFLOW_RUN_KEYS`] directly, so
/// the stored order only matters to `PartialEq`/`Debug` observers.
#[derive(Debug, Clone, PartialEq)]
pub struct WorkflowRunState {
    pub run_id: String,
    pub status: String,
    pub header: Vec<(String, Value)>,
    pub actors: Vec<WorkflowRunActor>,
    pub nodes: Vec<WorkflowRunNode>,
}

impl WorkflowRunState {
    /// Header lookup in canonical terms: `runId`/`status` live in the typed
    /// fields (`workflow-runs.ts:362,366`), everything else in the pairs.
    pub fn header_value(&self, key: &str) -> Option<Cow<'_, Value>> {
        match key {
            "runId" => Some(Cow::Owned(Value::String(self.run_id.clone()))),
            "status" => Some(Cow::Owned(Value::String(self.status.clone()))),
            _ => pairs_get(&self.header, key).map(Cow::Borrowed),
        }
    }
}

impl Serialize for WorkflowRunState {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        // Shallow rebuild in canonical order (`workflow-runs-delta.ts:77-90`):
        // schema keys in declaration order (undefined keys absent), then
        // unknown keys in arrival order — this module has no mandate to drop
        // a caller's data. `actors`/`nodes` are required and always emitted.
        let mut map = serializer.serialize_map(Some(self.header.len() + 4))?;
        for key in WORKFLOW_RUN_KEYS {
            match key {
                "runId" => map.serialize_entry("runId", &self.run_id)?,
                "status" => map.serialize_entry("status", &self.status)?,
                "actors" => map.serialize_entry("actors", &self.actors)?,
                "nodes" => map.serialize_entry("nodes", &self.nodes)?,
                _ => {
                    if let Some(value) = pairs_get(&self.header, key) {
                        map.serialize_entry(key, value)?;
                    }
                }
            }
        }
        for (key, value) in &self.header {
            if !WORKFLOW_RUN_KEYS.contains(&key.as_str()) {
                map.serialize_entry(key, value)?;
            }
        }
        map.end()
    }
}

impl<'de> Deserialize<'de> for WorkflowRunState {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct RunVisitor;
        impl<'de> de::Visitor<'de> for RunVisitor {
            type Value = WorkflowRunState;
            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a workflow run object")
            }
            fn visit_map<A: de::MapAccess<'de>>(
                self,
                mut access: A,
            ) -> Result<Self::Value, A::Error> {
                let mut run_id: Option<String> = None;
                let mut status: Option<String> = None;
                let mut header: Vec<(String, Value)> = Vec::new();
                let mut actors: Option<Vec<WorkflowRunActor>> = None;
                let mut nodes: Option<Vec<WorkflowRunNode>> = None;
                while let Some((key, value)) = access.next_entry::<String, Value>()? {
                    match key.as_str() {
                        // Required typed keys fail like zod would (z.string(),
                        // required) — a malformed run rejects, it does not
                        // half-parse.
                        "runId" => {
                            run_id = Some(
                                serde_json::from_value::<String>(value)
                                    .map_err(de::Error::custom)?,
                            );
                        }
                        "status" => {
                            status = Some(
                                serde_json::from_value::<String>(value)
                                    .map_err(de::Error::custom)?,
                            );
                        }
                        "actors" => {
                            actors = Some(
                                serde_json::from_value::<Vec<WorkflowRunActor>>(value)
                                    .map_err(de::Error::custom)?,
                            );
                        }
                        "nodes" => {
                            nodes = Some(
                                serde_json::from_value::<Vec<WorkflowRunNode>>(value)
                                    .map_err(de::Error::custom)?,
                            );
                        }
                        _ => pairs_set(&mut header, key, value),
                    }
                }
                Ok(WorkflowRunState {
                    run_id: run_id.ok_or_else(|| de::Error::missing_field("runId"))?,
                    status: status.ok_or_else(|| de::Error::missing_field("status"))?,
                    header,
                    actors: actors.ok_or_else(|| de::Error::missing_field("actors"))?,
                    nodes: nodes.ok_or_else(|| de::Error::missing_field("nodes"))?,
                })
            }
        }
        deserializer.deserialize_map(RunVisitor)
    }
}

/// `workflowRunsStateSchema` (`workflow-runs.ts:508-512`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct WorkflowRunsState {
    pub revision: u64,
    pub runs: Vec<WorkflowRunState>,
}

/// `canonicalWorkflowRun` (`workflow-runs-delta.ts:77-90`): reorder the header
/// against the schema declaration order, unknown keys appended in arrival
/// order. Byte identity's only leg that is NOT value equality — the reducer
/// appends newly-seen optional keys at the tail while apply has no such
/// history, so both sides realign against this one table (which is why the
/// reducer's exit runs through here too — one place, not two).
pub fn canonical_workflow_run(mut run: WorkflowRunState) -> WorkflowRunState {
    let mut known: Vec<(String, Value)> = Vec::with_capacity(run.header.len());
    let mut unknown: Vec<(String, Value)> = Vec::new();
    for pair in run.header.drain(..) {
        if WORKFLOW_RUN_KEYS.contains(&pair.0.as_str()) {
            known.push(pair);
        } else {
            unknown.push(pair);
        }
    }
    // Ranks are unique, so the sort is deterministic; unknown keys keep
    // arrival order by staying out of it.
    known.sort_by_key(|(key, _)| {
        WORKFLOW_RUN_KEYS
            .iter()
            .position(|k| *k == key.as_str())
            .unwrap_or(usize::MAX)
    });
    known.append(&mut unknown);
    run.header = known;
    run
}

/// `jsonValueEqual` (`workflow-runs-delta.ts:99-118`): structural equality,
/// key order not participating, shared by the reducer idempotence test and
/// diff's per-key/per-entry change detection so the two judges cannot
/// disagree.
///
/// TS also treats `undefined`-valued keys as absent; serde_json has no
/// `undefined` — a key is either present with a real value or not in the
/// `Map` at all, so those two TS guard branches are inexpressible here. JS
/// `===` compares numbers as f64 (`1 === 1.0`); serde_json keeps distinct
/// integer/float representations, so numbers fall through to an f64 compare.
pub fn json_value_equal(a: &Value, b: &Value) -> bool {
    if a == b {
        return true;
    }
    if let (Value::Number(x), Value::Number(y)) = (a, b) {
        return match (x.as_f64(), y.as_f64()) {
            (Some(p), Some(q)) => p == q,
            _ => false,
        };
    }
    match (a, b) {
        (Value::Array(x), Value::Array(y)) => {
            x.len() == y.len()
                && x.iter()
                    .zip(y.iter())
                    .all(|(p, q)| json_value_equal(p, q))
        }
        (Value::Object(x), Value::Object(y)) => {
            x.len() == y.len()
                && x.iter()
                    .all(|(k, v)| y.get(k).is_some_and(|w| json_value_equal(v, w)))
        }
        _ => false,
    }
}

/// `isCompleteWorkflowRunHeader` (`workflow-runs-delta.ts:132-136`): every
/// required header key present (present-at-any-value, exactly the TS
/// `!== undefined` check). Only diff's birth branch ever builds a complete
/// header — an existing run's increments never carry `runId`, so coalesced
/// increments can never assemble one either; that invariant is the pillar
/// merge rule 6 stands on (merging cannot turn two no-ops on an unknown run
/// into a phantom birth).
pub fn is_complete_header(header: &WorkflowRunHeaderPatch) -> bool {
    WORKFLOW_RUN_REQUIRED_HEADER_KEYS
        .iter()
        .all(|key| pairs_get(&header.0, key).is_some())
}

/// `applyWorkflowRunUpdated` (`workflow-runs-delta.ts:309-348`).
///
/// Container revision takes the **max**: coalesce may merge a later op into
/// an earlier slot (rule 6), so the last op of an output sequence need not
/// carry the highest revision — max keeps final-state byte identity from
/// depending on op position.
pub fn apply_workflow_run_updated(
    state: Option<&WorkflowRunsState>,
    delta: &WorkflowRunUpdate,
) -> WorkflowRunsState {
    let runs: &[WorkflowRunState] = state.map_or(&[], |s| s.runs.as_slice());
    let revision = state.map_or(0, |s| s.revision).max(delta.revision);
    let Some((index, existing)) = runs
        .iter()
        .enumerate()
        .find(|(_, run)| run.run_id == delta.run_id)
    else {
        // Unknown run + complete header = birth; incomplete header = the op
        // speaks of facts this client does not hold — no-op (the same ruling
        // as row.upserted hitting an unloaded row), only the container
        // revision catches up (`workflow-runs-delta.ts:317-320`).
        return match delta.run.as_ref() {
            Some(patch) if is_complete_header(patch) => {
                // Schema-invalid payloads collapse: a non-string patch
                // `runId`/`status` falls back to the op's own identity — zod
                // would have rejected that frame before it got here.
                let run_id = patch
                    .0
                    .iter()
                    .find(|(k, _)| k == "runId")
                    .and_then(|(_, v)| v.as_str())
                    .unwrap_or(&delta.run_id)
                    .to_owned();
                let status = patch
                    .0
                    .iter()
                    .find(|(k, _)| k == "status")
                    .and_then(|(_, v)| v.as_str())
                    .unwrap_or_default()
                    .to_owned();
                let header: Vec<(String, Value)> = patch
                    .0
                    .iter()
                    .filter(|(k, _)| {
                        k != "runId" && k != "status" && !is_entry_table_key(k)
                    })
                    .cloned()
                    .collect();
                let born = canonical_workflow_run(WorkflowRunState {
                    run_id,
                    status,
                    header,
                    actors: delta.actors.clone().unwrap_or_default(),
                    nodes: delta.nodes.clone().unwrap_or_default(),
                });
                let mut next = runs.to_vec();
                next.push(born);
                WorkflowRunsState { revision, runs: next }
            }
            _ => revision_catch_up(state, runs, revision),
        };
    };

    let mut next_run = existing.clone();
    if let Some(patch) = &delta.run {
        for (key, value) in &patch.0 {
            match key.as_str() {
                "runId" => {
                    if let Value::String(s) = value {
                        next_run.run_id = s.clone();
                    }
                }
                "status" => {
                    if let Value::String(s) = value {
                        next_run.status = s.clone();
                    }
                }
                // The entry tables always come from their dedicated payloads:
                // a patch key with those names is schema-invalid and loses,
                // which is the net effect of the TS explicit assignments
                // (`workflow-runs-delta.ts:330-344`).
                "actors" | "nodes" => {}
                _ => pairs_set(&mut next_run.header, key.clone(), value.clone()),
            }
        }
    }
    if let Some(cleared) = &delta.cleared {
        for key in cleared {
            // "zero entries ⇒ key absent" is the protocol convention for
            // reports/pendingQuestions etc.; a schema-valid producer never
            // clears required keys, so the typed pair is protected here.
            if key != "runId" && key != "status" {
                pairs_remove(&mut next_run.header, key);
            }
        }
    }
    // Application order: header → removals → upserts. A key removed and
    // re-added inside one op lands at the table TAIL, matching two
    // sequentially applied ops (`workflow-runs-delta.ts:333-344`).
    let mut actors = remove_workflow_run_entries(
        std::mem::take(&mut next_run.actors),
        delta.removed_actors.as_deref(),
    );
    let mut nodes = remove_workflow_run_entries(
        std::mem::take(&mut next_run.nodes),
        delta.removed_nodes.as_deref(),
    );
    if let Some(incoming) = &delta.actors
        && !incoming.is_empty()
    {
        actors = upsert_workflow_run_entries(actors, incoming);
    }
    if let Some(incoming) = &delta.nodes
        && !incoming.is_empty()
    {
        nodes = upsert_workflow_run_entries(nodes, incoming);
    }
    next_run.actors = actors;
    next_run.nodes = nodes;
    let mut next_runs = runs.to_vec();
    if let Some(slot) = next_runs.get_mut(index) {
        *slot = canonical_workflow_run(next_run);
    }
    WorkflowRunsState {
        revision,
        runs: next_runs,
    }
}

/// `applyWorkflowRunRemoved` (`workflow-runs-delta.ts:351-362`): drop the
/// run. An unknown runId only catches the revision up.
pub fn apply_workflow_run_removed(
    state: Option<&WorkflowRunsState>,
    run_id: &str,
    revision: u64,
) -> WorkflowRunsState {
    let runs: &[WorkflowRunState] = state.map_or(&[], |s| s.runs.as_slice());
    let revision = state.map_or(0, |s| s.revision).max(revision);
    let remaining: Vec<WorkflowRunState> = runs
        .iter()
        .filter(|run| run.run_id != run_id)
        .cloned()
        .collect();
    if remaining.len() == runs.len() {
        return revision_catch_up(state, runs, revision);
    }
    WorkflowRunsState {
        revision,
        runs: remaining,
    }
}

/// The no-op arm's return (`workflow-runs-delta.ts:320,359`): the TS hands
/// back the same state object when the revision did not move — Rust's
/// clone-on-unchanged is that identity note.
fn revision_catch_up(
    state: Option<&WorkflowRunsState>,
    runs: &[WorkflowRunState],
    revision: u64,
) -> WorkflowRunsState {
    match state {
        Some(s) if s.revision == revision => s.clone(),
        _ => WorkflowRunsState {
            revision,
            runs: runs.to_vec(),
        },
    }
}

/// `removeWorkflowRunEntries` (`workflow-runs-delta.ts:369-377`): drop by
/// (siteId, ordinal). Missing keys are a well-defined no-op — merges union
/// two ops' removals, some of which never reached this client. TS returns
/// the SAME array when nothing matched (untouched tables keep their
/// reference for memo); the Rust move-in/move-out is that identity.
fn remove_workflow_run_entries<T: RunEntry>(
    current: Vec<T>,
    removed: Option<&[WorkflowRunEntryRef]>,
) -> Vec<T> {
    let Some(removed) = removed else { return current };
    if removed.is_empty() {
        return current;
    }
    let dropped: HashSet<String> = removed
        .iter()
        .map(|r| entry_key(&r.site_id, r.ordinal))
        .collect();
    current
        .into_iter()
        .filter(|entry| !dropped.contains(&entry_key(entry.site_id(), entry.ordinal())))
        .collect()
}

/// `upsertWorkflowRunEntries` (`workflow-runs-delta.ts:386-404`): upsert by
/// (siteId, ordinal) — existing keys replaced in place, new keys appended at
/// the tail. The client NEVER applies caps (maxNodes/maxActors/maxRuns):
/// only the producer may evict, and eviction must be spoken aloud
/// (`workflowRun.removed` / `removedActors` / `removedNodes`); client-side
/// trimming only makes the two sides silently diverge.
fn upsert_workflow_run_entries<T: RunEntry + Clone>(current: Vec<T>, incoming: &[T]) -> Vec<T> {
    let mut next = current;
    let mut index_by_key: HashMap<String, usize> = HashMap::with_capacity(next.len());
    for (index, entry) in next.iter().enumerate() {
        index_by_key.insert(entry_key(entry.site_id(), entry.ordinal()), index);
    }
    for entry in incoming {
        let key = entry_key(entry.site_id(), entry.ordinal());
        match index_by_key.get(&key) {
            Some(&index) => {
                if let Some(slot) = next.get_mut(index) {
                    *slot = entry.clone();
                }
            }
            None => {
                index_by_key.insert(key, next.len());
                next.push(entry.clone());
            }
        }
    }
    next
}

/// `WorkflowRunWireBounds` (`workflow-runs-delta.ts:436-439`): entry-table
/// capacity caps; delta application and merging must use consistent caps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorkflowRunWireBounds {
    pub max_actors: usize,
    pub max_nodes: usize,
}

impl Default for WorkflowRunWireBounds {
    fn default() -> Self {
        Self {
            max_actors: WorkflowRunsLimits::MAX_ACTORS,
            max_nodes: WorkflowRunsLimits::MAX_NODES,
        }
    }
}

/// `workflowRunUpdateWithinWireBounds` (`workflow-runs-delta.ts:420-430`):
/// are an op's four entry tables within the wire caps? What actually
/// overflows is the two REMOVAL tables (eviction frees a slot, so a wide
/// flush window can drop more distinct keys than the table cap); the merged
/// upsert tables are self-bounded by the merge rule. All four are checked
/// because the gate is cheap and should not lean on that argument holding.
/// Oversized payloads fail the whole patch parse and drop the frame, so
/// refusing the merge is always safe: delivered one-by-one, the two ops'
/// final state is identical to the byte.
pub fn update_within_wire_bounds(delta: &WorkflowRunUpdate, bounds: &WorkflowRunWireBounds) -> bool {
    delta.actors.as_ref().is_none_or(|v| v.len() <= bounds.max_actors)
        && delta
            .removed_actors
            .as_ref()
            .is_none_or(|v| v.len() <= bounds.max_actors)
        && delta.nodes.as_ref().is_none_or(|v| v.len() <= bounds.max_nodes)
        && delta
            .removed_nodes
            .as_ref()
            .is_none_or(|v| v.len() <= bounds.max_nodes)
}

/// `mergeWorkflowRunUpdates` (`workflow-runs-delta.ts:442-478`) — the payload
/// half of coalesce rule 6 (the rule itself lives in `coalesce.rs`).
pub fn merge_workflow_run_updates(
    earlier: &WorkflowRunUpdate,
    later: &WorkflowRunUpdate,
) -> WorkflowRunUpdate {
    let mut run = WorkflowRunHeaderPatch::default();
    if let Some(patch) = &earlier.run {
        for (key, value) in &patch.0 {
            pairs_set(&mut run.0, key.clone(), value.clone());
        }
    }
    if let Some(patch) = &later.run {
        for (key, value) in &patch.0 {
            pairs_set(&mut run.0, key.clone(), value.clone());
        }
    }
    // Header keys and `cleared` are each other's negation and must annihilate
    // on merge: a key later set is no longer cleared, a key later cleared no
    // longer has a value. Keeping one half would make apply write-then-delete
    // (or delete-then-write) with an outcome that depends on key order.
    if let Some(later_cleared) = &later.cleared {
        for key in later_cleared {
            pairs_remove(&mut run.0, key);
        }
    }
    let mut cleared: Vec<String> = Vec::new();
    if let Some(earlier_cleared) = &earlier.cleared {
        for key in earlier_cleared {
            if pairs_get(&run.0, key).is_none() && !cleared.contains(key) {
                cleared.push(key.clone());
            }
        }
    }
    if let Some(later_cleared) = &later.cleared {
        for key in later_cleared {
            if !cleared.contains(key) {
                cleared.push(key.clone());
            }
        }
    }
    let removed_actors = merge_removed_refs(
        earlier.removed_actors.as_deref(),
        later.removed_actors.as_deref(),
    );
    let removed_nodes = merge_removed_refs(
        earlier.removed_nodes.as_deref(),
        later.removed_nodes.as_deref(),
    );
    let actors = merge_entry_lists(&earlier.actors, &later.actors, later.removed_actors.as_deref());
    let nodes = merge_entry_lists(&earlier.nodes, &later.nodes, later.removed_nodes.as_deref());
    WorkflowRunUpdate {
        run_id: later.run_id.clone(),
        // Highest watermark: the merged op sits in the earlier slot; apply's
        // max still lands the container version on the highest revision.
        revision: earlier.revision.max(later.revision),
        run: (!run.0.is_empty()).then_some(run),
        cleared: (!cleared.is_empty()).then_some(cleared),
        removed_actors,
        removed_nodes,
        actors,
        nodes,
    }
}

/// `mergeRemovedRefs` (`workflow-runs-delta.ts:481-495`): union of both
/// sides' removals, deduped by first occurrence (removal filters by key —
/// order only affects bytes). None when no side carried any.
fn merge_removed_refs(
    earlier: Option<&[WorkflowRunEntryRef]>,
    later: Option<&[WorkflowRunEntryRef]>,
) -> Option<Vec<WorkflowRunEntryRef>> {
    if earlier.is_none() && later.is_none() {
        return None;
    }
    let mut merged: Vec<WorkflowRunEntryRef> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for refs in [earlier.unwrap_or(&[]), later.unwrap_or(&[])] {
        for r in refs {
            let key = entry_key(&r.site_id, r.ordinal);
            if seen.contains(&key) {
                continue;
            }
            seen.insert(key);
            merged.push(r.clone());
        }
    }
    (!merged.is_empty()).then_some(merged)
}

/// `mergeEntryLists` (`workflow-runs-delta.ts:504-517`): later-wins by key,
/// first-occurrence order preserved; None when neither side had entries.
///
/// Earlier upserts whose key the later op REMOVED are dropped outright —
/// applied sequentially they enter the table and are then plucked out, so
/// the merged op must not carry them. If later re-adds the same key, it
/// appears once, in the later list — the tail — exactly where sequential
/// application lands it.
fn merge_entry_lists<T: RunEntry + Clone>(
    earlier: &Option<Vec<T>>,
    later: &Option<Vec<T>>,
    later_removed: Option<&[WorkflowRunEntryRef]>,
) -> Option<Vec<T>> {
    if earlier.is_none() && later.is_none() {
        return None;
    }
    let dropped: HashSet<String> = later_removed
        .unwrap_or(&[])
        .iter()
        .map(|r| entry_key(&r.site_id, r.ordinal))
        .collect();
    let kept: Vec<T> = earlier
        .as_deref()
        .unwrap_or(&[])
        .iter()
        .filter(|entry| !dropped.contains(&entry_key(entry.site_id(), entry.ordinal())))
        .cloned()
        .collect();
    let merged = upsert_workflow_run_entries(kept, later.as_deref().unwrap_or(&[]));
    (!merged.is_empty()).then_some(merged)
}

/// `diffWorkflowRunsState` (`workflow-runs-delta.ts:145-171`): the key-level
/// delta between two workflowRuns states.
///
/// One escape only: an unrecognizable structural change (entries deleted or
/// reordered — see the module header) or "no ops yet the revision moved"
/// resends the whole key as one `state.updated`. Legal on the wire (whole-key
/// replacement), a barrier in coalesce, at the cost of pre-rewrite bytes.
pub fn diff_workflow_runs_state(
    prior: Option<&WorkflowRunsState>,
    next: &WorkflowRunsState,
) -> Vec<ConversationDelta> {
    let prior_runs: &[WorkflowRunState] = prior.map_or(&[], |s| s.runs.as_slice());
    let next_ids: HashSet<&str> = next.runs.iter().map(|run| run.run_id.as_str()).collect();
    let mut ops: Vec<ConversationDelta> = Vec::new();
    // Evictions first, in prior order: the client plucks the departed runs
    // out so every remaining delta lands on the living table.
    for run in prior_runs {
        if next_ids.contains(run.run_id.as_str()) {
            continue;
        }
        ops.push(ConversationDelta::WorkflowRunRemoved {
            run_id: run.run_id.clone(),
            revision: next.revision,
        });
    }
    let mut resync = false;
    for run in &next.runs {
        let before = prior_runs.iter().find(|r| r.run_id == run.run_id);
        // Reference compare is only ever a fast path in the TS; structural
        // equality plays it here.
        if let Some(before_run) = before
            && before_run == run
        {
            continue;
        }
        match diff_workflow_run(before, run, next.revision) {
            RunDiff::Resync => {
                resync = true;
                break;
            }
            RunDiff::Update(op) => ops.push(op),
            RunDiff::Unchanged => {}
        }
    }
    if !resync {
        // No op at all yet the revision moved: the container version has no
        // carrier of its own, only a whole-key resend. The reducer never
        // produces this input (it returns null on no change). TS's sentinel
        // is a -1 revision (`workflow-runs-delta.ts:167`); prior-absence
        // plays that role here.
        let revision_moved = prior.is_none_or(|p| p.revision != next.revision);
        if !ops.is_empty() || !revision_moved {
            return ops;
        }
    }
    vec![workflow_runs_resync(next)]
}

fn workflow_runs_resync(next: &WorkflowRunsState) -> ConversationDelta {
    let mut keys = std::collections::BTreeMap::new();
    // serde_json::Value objects are key-sorted, so the embedded resync copy
    // carries semantics, not canonical run key order; the generic state
    // container cannot express that order. to_value of these types cannot
    // fail (no non-string keys, no non-finite floats) — the fallback is dead.
    keys.insert(
        "workflowRuns".to_string(),
        serde_json::to_value(next).unwrap_or(Value::Null),
    );
    ConversationDelta::StateUpdated {
        patch: StatePatch {
            revision: None,
            keys,
        },
    }
}

/// Single-run diff outcome (`workflow-runs-delta.ts:173-174`): the TS
/// undefined / op / null triad as one enum.
enum RunDiff {
    Unchanged,
    Resync,
    Update(ConversationDelta),
}

fn diff_workflow_run(
    before: Option<&WorkflowRunState>,
    run: &WorkflowRunState,
    revision: u64,
) -> RunDiff {
    let Some(before) = before else {
        // Birth: the whole header (runId included — the client judges the op
        // fit to create the table by it) plus every entry
        // (`workflow-runs-delta.ts:183-197`).
        let patch: Vec<(String, Value)> = WORKFLOW_RUN_KEYS
            .iter()
            .filter(|key| !is_entry_table_key(key))
            .filter_map(|key| {
                run.header_value(key)
                    .map(|value| (key.to_string(), value.into_owned()))
            })
            .collect();
        return RunDiff::Update(ConversationDelta::WorkflowRunUpdated(
            WorkflowRunUpdate {
                run_id: run.run_id.clone(),
                revision,
                run: Some(WorkflowRunHeaderPatch(patch)),
                cleared: None,
                removed_actors: None,
                removed_nodes: None,
                actors: (!run.actors.is_empty()).then(|| run.actors.clone()),
                nodes: (!run.nodes.is_empty()).then(|| run.nodes.clone()),
            },
        ));
    };
    let mut patch: Vec<(String, Value)> = Vec::new();
    let mut cleared: Vec<String> = Vec::new();
    for key in WORKFLOW_RUN_KEYS.iter().filter(|k| !is_entry_table_key(k)) {
        let left = before.header_value(key);
        let right = run.header_value(key);
        match (left, right) {
            (None, None) => {}
            (Some(l), Some(r)) => {
                if l != r && !json_value_equal(&l, &r) {
                    patch.push((key.to_string(), r.into_owned()));
                }
            }
            (None, Some(r)) => patch.push((key.to_string(), r.into_owned())),
            (Some(_), None) => cleared.push(key.to_string()),
        }
    }
    let actors = diff_workflow_run_entries(&before.actors, &run.actors);
    let nodes = diff_workflow_run_entries(&before.nodes, &run.nodes);
    let (Some(actors), Some(nodes)) = (actors, nodes) else {
        return RunDiff::Resync;
    };
    let changed_header = !patch.is_empty() || !cleared.is_empty();
    if !changed_header
        && matches!(actors, EntriesDiff::Unchanged)
        && matches!(nodes, EntriesDiff::Unchanged)
    {
        return RunDiff::Unchanged;
    }
    let (removed_actors, changed_actors) = split_entries_diff(actors);
    let (removed_nodes, changed_nodes) = split_entries_diff(nodes);
    // Field presence = application order (header → removals → upserts): a
    // key removed and re-added in one op lands at the table tail, matching
    // two sequentially applied ops (`workflow-runs-delta.ts:216-227`).
    RunDiff::Update(ConversationDelta::WorkflowRunUpdated(
        WorkflowRunUpdate {
            run_id: run.run_id.clone(),
            revision,
            run: (!patch.is_empty()).then_some(WorkflowRunHeaderPatch(patch)),
            cleared: (!cleared.is_empty()).then_some(cleared),
            removed_actors,
            removed_nodes,
            actors: changed_actors,
            nodes: changed_nodes,
        },
    ))
}

/// `diffWorkflowRunEntries` (`workflow-runs-delta.ts:246-268`) — the two
/// table states between steps. `Some(Unchanged)` is the TS undefined,
/// `None` the TS null (unrecognizable structural change).
///
/// Fast path compares index-aligned: the reducer's steady state is append
/// and in-place update, so untouched entries cost one identity check. The
/// first misalignment (different identity at an index, or prior longer than
/// next) means entries left — eviction — and falls to the keyed path. The
/// keyed path still requires survivors to sit index-aligned on next's
/// prefix: order changes beyond deletion have no syntax in this protocol,
/// only a whole-key resend.
fn diff_workflow_run_entries<T: RunEntry + Clone + PartialEq>(
    before: &[T],
    after: &[T],
) -> Option<EntriesDiff<T>> {
    if before.len() <= after.len() {
        let mut changed: Vec<T> = Vec::new();
        let mut aligned = true;
        for (index, entry) in after.iter().enumerate() {
            let Some(previous) = before.get(index) else {
                changed.push(entry.clone());
                continue;
            };
            if previous.site_id() != entry.site_id() || previous.ordinal() != entry.ordinal() {
                aligned = false;
                break;
            }
            // PartialEq on the typed entries is structural and key-order
            // insensitive for the flattened extras — jsonValueEqual's judge.
            if previous != entry {
                changed.push(entry.clone());
            }
        }
        if aligned {
            return if changed.is_empty() {
                Some(EntriesDiff::Unchanged)
            } else {
                Some(EntriesDiff::Changed {
                    removed: Vec::new(),
                    changed,
                })
            };
        }
    }
    diff_workflow_run_entries_by_key(before, after)
}

fn diff_workflow_run_entries_by_key<T: RunEntry + Clone + PartialEq>(
    before: &[T],
    after: &[T],
) -> Option<EntriesDiff<T>> {
    let survives: HashSet<String> = after
        .iter()
        .map(|e| entry_key(e.site_id(), e.ordinal()))
        .collect();
    let mut removed: Vec<WorkflowRunEntryRef> = Vec::new();
    let mut survivors: Vec<&T> = Vec::new();
    for entry in before {
        if survives.contains(&entry_key(entry.site_id(), entry.ordinal())) {
            survivors.push(entry);
        } else {
            removed.push(WorkflowRunEntryRef {
                site_id: entry.site_id().to_owned(),
                ordinal: entry.ordinal(),
            });
        }
    }
    if survivors.len() > after.len() {
        return None;
    }
    let mut changed: Vec<T> = Vec::new();
    for (index, entry) in after.iter().enumerate() {
        let Some(previous) = survivors.get(index) else {
            changed.push(entry.clone());
            continue;
        };
        // Survivors must still sit on next's prefix in original order —
        // anything else is a reorder, i.e. whole-key resend.
        if previous.site_id() != entry.site_id() || previous.ordinal() != entry.ordinal() {
            return None;
        }
        if **previous == *entry {
            continue;
        }
        changed.push(entry.clone());
    }
    if removed.is_empty() && changed.is_empty() {
        return Some(EntriesDiff::Unchanged);
    }
    Some(EntriesDiff::Changed { removed, changed })
}

/// `EntryDiff` (`workflow-runs-delta.ts:230-234`): departed entries
/// (identity only) + changed entries (whole, in next order).
enum EntriesDiff<T> {
    Unchanged,
    Changed {
        removed: Vec<WorkflowRunEntryRef>,
        changed: Vec<T>,
    },
}

fn split_entries_diff<T>(
    diff: EntriesDiff<T>,
) -> (Option<Vec<WorkflowRunEntryRef>>, Option<Vec<T>>) {
    match diff {
        EntriesDiff::Unchanged => (None, None),
        EntriesDiff::Changed { removed, changed } => (
            (!removed.is_empty()).then_some(removed),
            (!changed.is_empty()).then_some(changed),
        ),
    }
}
