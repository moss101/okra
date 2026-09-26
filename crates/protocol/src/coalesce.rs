//! Port of `coalesce.ts` — semantic-preserving merge for flush windows.
//!
//! Contract (`coalesce.ts:2`): `coalesce(deltas)` must be equivalent to
//! delivering each delta in order — after both profiles process the same
//! event sequence, final state must be **byte-identical**. No merge may
//! change post-apply state.
//!
//! Rules (closed set, `coalesce.ts:4-10`; rule 6 deferred — N0003):
//!   1. adjacent same-(rowId, path) `row.delta` → append concatenation
//!   2. adjacent `state.updated` → shallow key merge (whole-key replacement
//!      makes this safe)
//!   3. `row.delta` followed by `row.upserted` of the same rowId → the delta
//!      is dropped (whole-row replacement subsumes all appends)
//!   4. `row.removed` is a barrier no rule may cross
//!   5. frame-size splitting is NOT done here (channel layer frames)
//!   6. workflowRun merge — deferred with the op (N0003)

use serde_json::{Map, Value};

use crate::delta::ConversationDelta;
use crate::rows::ConversationRow;

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
pub fn coalesce_conversation_deltas(deltas: &[ConversationDelta]) -> Vec<ConversationDelta> {
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
