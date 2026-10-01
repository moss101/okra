//! Port of `coalesce.ts` — semantic-preserving merge for flush windows.
//!
//! Contract (`coalesce.ts:2`): `coalesce(deltas)` must be equivalent to
//! delivering each delta in order — after both profiles process the same
//! event sequence, final state must be **byte-identical**. No merge may
//! change post-apply state.
//!
//! Rules (closed set, `coalesce.ts:4-10`):
//!   1. adjacent same-(rowId, path) `row.delta` → append concatenation
//!   2. adjacent `state.updated` → shallow key merge (whole-key replacement
//!      makes this safe)
//!   3. `row.delta` followed by `row.upserted` of the same rowId → the delta
//!      is dropped (whole-row replacement subsumes all appends)
//!   4. `row.removed` is a barrier no rule may cross
//!   5. frame-size splitting is NOT done here (channel layer frames)
//!   6. same-runId `workflowRun.updated` merges into the earliest surviving
//!      slot (payload in `workflow_runs.rs`, rule below)

use serde_json::{Map, Value};

use crate::delta::{ConversationDelta, WorkflowRunUpdate};
use crate::rows::ConversationRow;
use crate::workflow_runs::{
    apply_workflow_run_removed, apply_workflow_run_updated, merge_workflow_run_updates,
    update_within_wire_bounds, WorkflowRunsState, WorkflowRunWireBounds,
};

/// Authoritative reducer state: rows keyed by rowId in authoritative order,
/// plus the state map.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ConversationState {
    pub rows: Vec<ConversationRow>,
    pub state: Map<String, Value>,
}

/// Apply one delta (`applyDeltas` in the golden generator; the canonical
/// reducer both profiles converge on).
pub fn apply_delta(state: &mut ConversationState, delta: &ConversationDelta) {
    match delta {
        ConversationDelta::RowAppended { row } | ConversationDelta::RowUpserted { row } => {
            upsert_row(state, row.clone());
        }
        ConversationDelta::RowRemoved { from_row_id } => {
            state.rows.retain(|r| r.row_id() < *from_row_id);
        }
        ConversationDelta::RowDelta {
            row_id,
            append,
            path: _,
        } => {
            if let Some(row) = state
                .rows
                .iter_mut()
                .find(|r| r.row_id() == *row_id)
                && let ConversationRow::Response { text, .. } = row {
                text.push_str(append);
            }
        }
        ConversationDelta::StateUpdated { patch } => {
            if let Some(rev) = patch.revision {
                state
                    .state
                    .insert("revision".into(), Value::from(rev));
            }
            for (k, v) in &patch.keys {
                state.state.insert(k.clone(), v.clone());
            }
        }
        ConversationDelta::WorkflowRunUpdated(update) => {
            apply_workflow_runs_op(state, |current| {
                apply_workflow_run_updated(current, update)
            });
        }
        ConversationDelta::WorkflowRunRemoved { run_id, revision } => {
            let (run_id, revision) = (run_id.clone(), *revision);
            apply_workflow_runs_op(state, |current| {
                apply_workflow_run_removed(current, &run_id, revision)
            });
        }
    }
}

/// Both `workflowRun.*` ops work the same way `state.updated` does today:
/// against the `workflowRuns` key of the generic state map. A malformed
/// stored value degrades to an absent key — TS rejects such a frame at the
/// zod edge, which okra's generic StatePatch does not have yet. serde_json
/// objects are key-sorted, so this path carries semantics while the typed
/// domain (`workflow_runs.rs`) owns the canonical-order byte contract.
fn apply_workflow_runs_op(
    state: &mut ConversationState,
    op: impl FnOnce(Option<&WorkflowRunsState>) -> WorkflowRunsState,
) {
    let current = state
        .state
        .get("workflowRuns")
        .and_then(|v| serde_json::from_value::<WorkflowRunsState>(v.clone()).ok());
    let next = op(current.as_ref());
    if let Ok(value) = serde_json::to_value(&next) {
        state.state.insert("workflowRuns".into(), value);
    }
}

fn upsert_row(state: &mut ConversationState, row: ConversationRow) {
    match state.rows.iter().position(|r| r.row_id() == row.row_id()) {
        Some(idx) => state.rows[idx] = row,
        None => state.rows.push(row),
    }
}

/// `coalesceConversationDeltas` (`coalesce.ts:80-161`).
///
/// Input and output are both in authoritative log order; pure function; does
/// not mutate its input. The merged state must equal per-delta application —
/// enforced by the golden equivalence tests against the TS ground truth.
/// Rule 6's entry-table caps default to the protocol limits
/// (`coalesce.ts:78`); [`coalesce_conversation_deltas_with_bounds`] exposes
/// them.
pub fn coalesce_conversation_deltas(deltas: &[ConversationDelta]) -> Vec<ConversationDelta> {
    coalesce_conversation_deltas_with_bounds(deltas, &WorkflowRunWireBounds::default())
}

pub fn coalesce_conversation_deltas_with_bounds(
    deltas: &[ConversationDelta],
    bounds: &WorkflowRunWireBounds,
) -> Vec<ConversationDelta> {
    let mut result: Vec<ConversationDelta> = Vec::with_capacity(deltas.len());

    for delta in deltas {
        // Rule 3: row.upserted swallows earlier row.delta of the same rowId.
        // Only walk back to the nearest barrier (rule 4), and never past a
        // previous upserted/appended of the same rowId — crossing those would
        // swallow the previous row generation's appends and change final
        // state (`coalesce.ts:88-93`).
        if let ConversationDelta::RowUpserted { row } = delta {
            let mut i = result.len();
            while i > 0 {
                i -= 1;
                let prev = &result[i];
                if prev.is_barrier() {
                    break;
                }
                match prev {
                    ConversationDelta::RowDelta { row_id, .. } if row_id == &row.row_id() => {
                        result.remove(i);
                    }
                    ConversationDelta::RowUpserted { row: prev_row }
                    | ConversationDelta::RowAppended { row: prev_row }
                        if prev_row.row_id() == row.row_id() =>
                    {
                        break;
                    }
                    _ => {}
                }
            }
        }

        // Rule 6, removal half (`coalesce.ts:107-113`): eviction swallows
        // every prior delta of the same run — birth + eviction inside one
        // window means the client never saw the run at all, equivalent to
        // per-delta delivery.
        if let ConversationDelta::WorkflowRunRemoved { run_id, .. } = delta {
            let mut i = result.len();
            while i > 0 {
                i -= 1;
                let Some(prev) = result.get(i) else { break };
                if is_workflow_run_barrier(prev, run_id) {
                    break;
                }
                if let ConversationDelta::WorkflowRunUpdated(update) = prev
                    && update.run_id == *run_id
                {
                    result.remove(i);
                }
            }
        }

        // Rule 6, merge half (`coalesce.ts:116-117`).
        if let ConversationDelta::WorkflowRunUpdated(update) = delta
            && merge_workflow_run_update(&mut result, update, bounds)
        {
            continue;
        }

        let last = result.last();

        // Rule 1: adjacent same-(rowId, path) row.delta concatenation.
        let rule1 = match (delta, last) {
            (
                ConversationDelta::RowDelta { row_id, path, append },
                Some(ConversationDelta::RowDelta {
                    row_id: last_id,
                    path: last_path,
                    append: last_append,
                }),
            ) if last_id == row_id && last_path == path => {
                Some((last_append.clone(), append.clone()))
            }
            _ => None,
        };
        if let Some((last_append, append)) = rule1 {
            if let Some(ConversationDelta::RowDelta { append: a, .. }) = result.last_mut() {
                *a = format!("{last_append}{append}");
            }
            continue;
        }

        // Rule 2: adjacent state.updated shallow merge (later keys overwrite;
        // whole-key replacement makes this safe).
        if let (ConversationDelta::StateUpdated { patch }, Some(ConversationDelta::StateUpdated { patch: last_patch })) =
            (delta, last)
        {
            let mut merged = last_patch.clone();
            if patch.revision.is_some() {
                merged.revision = patch.revision;
            }
            for (k, v) in &patch.keys {
                merged.keys.insert(k.clone(), v.clone());
            }
            result.pop();
            result.push(ConversationDelta::StateUpdated { patch: merged });
            continue;
        }

        // Adjacent row.upserted of the same rowId: keep only the last
        // (transitivity of whole-row replacement, `coalesce.ts:147-155`).
        if let (
            ConversationDelta::RowUpserted { row },
            Some(ConversationDelta::RowUpserted { row: last_row }),
        ) = (delta, last)
            && last_row.row_id() == row.row_id() {
                result.pop();
                result.push(delta.clone());
                continue;
            }

        result.push(delta.clone());
    }

    result
}

/// Rule 6's barrier (`coalesce.ts:27-30`): wherever `workflowRuns` is
/// wholesale-replaced (a `state.updated` carrying the key), and a
/// `workflowRun.removed` of the SAME run. Every other op touches state
/// disjoint from this run's increment — row ops never touch state keys,
/// other runs' increments never touch this run — and therefore commutes.
/// Note this is NOT `row.removed`: that barrier guards row-ordering rules,
/// and row order is irrelevant to these ops.
fn is_workflow_run_barrier(delta: &ConversationDelta, run_id: &str) -> bool {
    match delta {
        ConversationDelta::StateUpdated { patch } => patch.keys.contains_key("workflowRuns"),
        ConversationDelta::WorkflowRunRemoved { run_id: id, .. } => id == run_id,
        _ => false,
    }
}

/// `mergeWorkflowRunUpdate` (`coalesce.ts:53-73`): merge one
/// `workflowRun.updated` into the LAST same-runId increment inside the
/// window.
///
/// Walk back rather than look at adjacency only: a wide fan-out run emits one
/// increment per engine event and other runs/row ops separate them inside
/// the window — adjacent-only merging would merge nothing at all. The target
/// is the LAST increment, not an earlier one: while merging keeps succeeding
/// at most one increment of this run survives past the barrier, so "earliest"
/// and "last" coincide and the birth order in `runs[]` is preserved; once a
/// merge is refused there are two in the window, and merging into the
/// EARLIER one would let a later upsert jump ahead of the middle op's
/// removals — a remove-then-add key would vanish.
///
/// The merged op sits early yet carries the later revision; apply's
/// max-revision absorbs that. A merge cannot assemble a complete header, so
/// it cannot turn two no-ops on an unknown run into a phantom birth.
///
/// Returns true when the delta was merged (the caller skips pushing it).
fn merge_workflow_run_update(
    result: &mut [ConversationDelta],
    delta: &WorkflowRunUpdate,
    bounds: &WorkflowRunWireBounds,
) -> bool {
    let mut target: Option<usize> = None;
    let mut i = result.len();
    while i > 0 {
        i -= 1;
        let Some(prev) = result.get(i) else { break };
        if is_workflow_run_barrier(prev, &delta.run_id) {
            break;
        }
        if let ConversationDelta::WorkflowRunUpdated(update) = prev
            && update.run_id == delta.run_id
        {
            target = Some(i);
            break;
        }
    }
    let Some(target) = target else { return false };
    let Some(ConversationDelta::WorkflowRunUpdated(earlier)) = result.get(target) else {
        return false;
    };
    let merged = merge_workflow_run_updates(earlier, delta);
    // Refused merges do NOT retry an earlier target: that is exactly the
    // jump-across-the-middle-removal walk described above. Refusal is always
    // safe — delivered one-by-one the two ops' final state is identical.
    if !update_within_wire_bounds(&merged, bounds) {
        return false;
    }
    if let Some(slot) = result.get_mut(target) {
        *slot = ConversationDelta::WorkflowRunUpdated(merged);
    }
    true
}

/// `conflateByKey` (`coalesce.ts:167-173`): keep only each key's last update,
/// preserving order of last occurrence. For latest-state topics
/// (sessions-index etc.).
pub fn conflate_by_key<T, F: Fn(&T) -> &str>(items: &[T], key_of: F) -> Vec<T>
where
    T: Clone,
{
    let mut last_index: std::collections::BTreeMap<String, usize> = Default::default();
    for (i, item) in items.iter().enumerate() {
        last_index.insert(key_of(item).to_string(), i);
    }
    items
        .iter()
        .enumerate()
        .filter(|(i, item)| last_index.get(key_of(item)) == Some(i))
        .map(|(_, item)| item.clone())
        .collect()
}
