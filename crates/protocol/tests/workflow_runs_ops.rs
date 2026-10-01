//! `workflowRun.*` op tests — the M3 re-addition of the two deferred ops
//! (decision N0003), ported from `workflow-runs-delta.ts`.
//!
//! The spine is the donor's byte-convergence contract
//! (`workflow-runs-delta.ts:17-19`): for reducer-produced (prior, next)
//! pairs, `to_string(apply_all(prior, diff(prior, next)))` must equal
//! `to_string(next)` **byte for byte** — canonical key order included.
//! Fixtures therefore run `next` through `canonical_workflow_run` first,
//! which is what the TS reducer does on its own exit.

use okra_protocol as proto;

use std::borrow::Cow;

use serde_json::json;
use proto::{
    apply_delta, apply_workflow_run_removed, apply_workflow_run_updated,
    canonical_workflow_run, coalesce_conversation_deltas_with_bounds, diff_workflow_runs_state,
    entry_key, is_complete_header, json_value_equal, merge_workflow_run_updates,
    update_within_wire_bounds, ConversationDelta, ConversationState, StatePatch,
    WorkflowRunHeaderPatch, WorkflowRunUpdate, WorkflowRunsState, WorkflowRunWireBounds,
};

fn update(v: serde_json::Value) -> WorkflowRunUpdate {
    serde_json::from_value(v).expect("workflowRun.updated payload")
}

fn runs(v: serde_json::Value) -> WorkflowRunsState {
    serde_json::from_value(v).expect("workflowRuns state")
}

fn canon(state: &WorkflowRunsState) -> WorkflowRunsState {
    WorkflowRunsState {
        revision: state.revision,
        runs: state
            .runs
            .iter()
            .map(|r| canonical_workflow_run(r.clone()))
            .collect(),
    }
}

fn wire(state: &WorkflowRunsState) -> String {
    serde_json::to_string(state).expect("serialize state")
}

/// The typed-domain `applyAll`: applies a diff's op mix to a prior state.
fn apply_ops(prior: Option<&WorkflowRunsState>, ops: &[ConversationDelta]) -> WorkflowRunsState {
    let mut state = prior.cloned().unwrap_or_default();
    for op in ops {
        match op {
            ConversationDelta::WorkflowRunUpdated(u) => {
                state = apply_workflow_run_updated(Some(&state), u);
            }
            ConversationDelta::WorkflowRunRemoved { run_id, revision } => {
                state = apply_workflow_run_removed(Some(&state), run_id, *revision);
            }
            ConversationDelta::StateUpdated { patch } => {
                if let Some(v) = patch.keys.get("workflowRuns") {
                    state = serde_json::from_value(v.clone()).expect("resync payload parses");
                }
            }
            _ => panic!("diff_workflow_runs_state produced a non-workflowRun op: {op:?}"),
        }
    }
    state
}

// ---------------------------------------------------------------- birth ----

#[test]
fn unknown_run_with_complete_header_is_born_canonical() {
    // Patch arrives scrambled; the born run must come out in schema
    // declaration order (workflow-runs.ts:361-505), entries at their slots.
    let delta = update(json!({
        "runId": "r1",
        "revision": 7,
        "run": {
            "lastEventSequence": 3,
            "status": "running",
            "runId": "r1",
            "usage": { "spentTokens": 0, "nodesUsed": 2 }
        },
        "actors": [{ "siteId": "a", "ordinal": 0, "status": "running", "name": "reader" }],
        "nodes": [{ "siteId": "n", "ordinal": 0, "phase": "queued", "kind": "ask" }]
    }));
    let state = apply_workflow_run_updated(None, &delta);
    assert_eq!(state.revision, 7);
    assert_eq!(state.runs.len(), 1);
    assert_eq!(state.runs[0].run_id, "r1");
    assert_eq!(
        wire(&state),
        concat!(
            r#"{"revision":7,"runs":[{"runId":"r1","status":"running","#,
            r#""usage":{"nodesUsed":2,"spentTokens":0},"#,
            r#""actors":[{"siteId":"a","ordinal":0,"name":"reader","status":"running"}],"#,
            r#""nodes":[{"siteId":"n","ordinal":0,"kind":"ask","phase":"queued"}],"#,
            r#""lastEventSequence":3}]}"#
        )
    );
}

#[test]
fn unknown_run_with_incomplete_header_is_a_no_op_with_revision_catch_up() {
    let prior = runs(json!({
        "revision": 4,
        "runs": [{
            "runId": "r1", "status": "running",
            "usage": { "spentTokens": 0, "nodesUsed": 0 },
            "lastEventSequence": 1,
            "actors": [], "nodes": []
        }]
    }));
    // Missing runId/usage/lastEventSequence — not fit to birth a run.
    let delta = update(json!({
        "runId": "ghost", "revision": 9,
        "run": { "status": "running" },
        "actors": [{ "siteId": "g", "ordinal": 0, "status": "waiting" }]
    }));
    let next = apply_workflow_run_updated(Some(&prior), &delta);
    assert_eq!(next.revision, 9, "container revision catches up");
    assert_eq!(next.runs.len(), 1, "no run is born from an incomplete header");
    assert_eq!(next.runs[0].run_id, "r1");
    assert_eq!(next.runs[0].actors.len(), 0, "entry payloads are not applied either");

    // Same-revision no-op returns the state unchanged.
    let same = update(json!({ "runId": "ghost", "revision": 4, "run": { "status": "x" } }));
    let unchanged = apply_workflow_run_updated(Some(&prior), &same);
    assert_eq!(unchanged, prior);
}

// ----------------------------------------------------- update semantics ----

#[test]
fn header_patch_cleared_and_remove_then_upsert_land_in_order() {
    let prior = runs(json!({
        "revision": 5,
        "runs": [{
            "runId": "r1", "status": "running",
            "usage": { "spentTokens": 1, "nodesUsed": 1 },
            "lastEventSequence": 10,
            "reports": [{ "siteId": "rep", "ordinal": 0, "preview": "early" }],
            "actors": [
                { "siteId": "s", "ordinal": 0, "status": "completed" },
                { "siteId": "s", "ordinal": 1, "status": "waiting" },
                { "siteId": "s", "ordinal": 2, "status": "running" }
            ],
            "nodes": [{ "siteId": "n", "ordinal": 0, "phase": "settled", "outcome": "ok" }]
        }]
    }));
    let delta = update(json!({
        "runId": "r1", "revision": 6,
        "run": { "status": "completed", "error": "boom" },
        "cleared": ["reports"],
        "removedActors": [{ "siteId": "s", "ordinal": 1 }],
        "actors": [{ "siteId": "s", "ordinal": 1, "status": "running" }]
    }));
    let next = apply_workflow_run_updated(Some(&prior), &delta);
    assert_eq!(next.revision, 6);
    let run = &next.runs[0];
    assert_eq!(run.status, "completed");
    assert_eq!(run.header_value("error"), Some(Cow::Borrowed(&json!("boom"))));
    assert_eq!(run.header_value("reports"), None, "cleared key is gone");
    // Removed-then-upserted keys land at the TAIL (application order
    // header → removals → upserts), matching two sequential ops.
    let ids: Vec<(String, u64)> = run
        .actors
        .iter()
        .map(|a| (a.site_id.clone(), a.ordinal))
        .collect();
    assert_eq!(
        ids,
        vec![("s".into(), 0), ("s".into(), 2), ("s".into(), 1)]
    );
    assert_eq!(run.actors[2].extra.get("status"), Some(&json!("running")));
}

#[test]
fn out_of_order_revision_still_applies_but_keeps_max_revision() {
    let prior = runs(json!({
        "revision": 10,
        "runs": [{
            "runId": "r1", "status": "running",
            "usage": { "spentTokens": 0, "nodesUsed": 0 },
            "lastEventSequence": 4,
            "actors": [], "nodes": []
        }]
    }));
    let delta = update(json!({ "runId": "r1", "revision": 3, "run": { "status": "stopped", "stopReason": "user" } }));
    let next = apply_workflow_run_updated(Some(&prior), &delta);
    assert_eq!(next.revision, 10, "container revision is a max, not an overwrite");
    assert_eq!(next.runs[0].status, "stopped", "content still applies");
    assert_eq!(next.runs[0].header_value("stopReason"), Some(Cow::Borrowed(&json!("user"))));
}

#[test]
fn removed_op_on_unknown_run_id_only_catches_revision_up() {
    let prior = runs(json!({
        "revision": 1,
        "runs": [{
            "runId": "r1", "status": "running",
            "usage": { "spentTokens": 0, "nodesUsed": 0 },
            "lastEventSequence": 1,
            "actors": [], "nodes": []
        }]
    }));
    let unknown = apply_workflow_run_removed(Some(&prior), "nope", 42);
    assert_eq!(unknown.revision, 42);
    assert_eq!(unknown.runs.len(), 1);
    let known = apply_workflow_run_removed(Some(&prior), "r1", 42);
    assert_eq!(known.revision, 42);
    assert!(known.runs.is_empty());
}

// ------------------------------------------------------ small primitives ---

#[test]
fn entry_key_separates_site_and_ordinal() {
    // ("a",12) and ("a1",2) must not collide — the \0 is load-bearing
    // (workflow-runs-delta.ts:66-69).
    assert_ne!(entry_key("a", 12), entry_key("a1", 2));
    assert_eq!(entry_key("a", 12), concat!("a", "\0", "12"));
}

#[test]
fn json_value_equal_is_structural_and_order_insensitive() {
    assert!(json_value_equal(&json!({"a":1,"b":2}), &json!({"b":2,"a":1})));
    assert!(!json_value_equal(&json!({"a":1}), &json!({"a":1,"b":2})));
    assert!(!json_value_equal(&json!([1,2]), &json!([2,1])), "array order matters");
    // JS compares numbers as f64: 1 === 1.0.
    assert!(json_value_equal(&json!(1), &json!(1.0)));
    // null is a value, not an absent key (TS undefined ≙ serde_json absent).
    assert!(!json_value_equal(&json!({"a":null}), &json!({})));
}

#[test]
fn complete_header_requires_all_four_required_keys() {
    // Required = schema fields without .optional(): runId, status, usage,
    // lastEventSequence (workflow-runs.ts:362,366,376,504).
    assert!(is_complete_header(&serde_json::from_value::<WorkflowRunHeaderPatch>(json!({
        "runId": "r", "status": "running",
        "usage": { "spentTokens": 0, "nodesUsed": 0 },
        "lastEventSequence": 0
    })).expect("patch")));
    let missing_usage: WorkflowRunHeaderPatch =
        serde_json::from_value(json!({ "runId": "r", "status": "running", "lastEventSequence": 0 }))
            .expect("patch");
    assert!(!is_complete_header(&missing_usage));
    // Present-at-any-value mirrors the TS `!== undefined` check.
    let null_usage: WorkflowRunHeaderPatch = serde_json::from_value(json!({
        "runId": "r", "status": "running", "usage": null, "lastEventSequence": 0
    }))
    .expect("patch");
    assert!(is_complete_header(&null_usage));
}

// --------------------------------------------- diff→apply byte roundtrip ---

fn roundtrip(name: &str, prior: Option<&WorkflowRunsState>, next: &WorkflowRunsState) -> Vec<ConversationDelta> {
    let ops = diff_workflow_runs_state(prior, next);
    let applied = apply_ops(prior, &ops);
    assert_eq!(wire(&applied), wire(next), "case {name}: byte equality broken");
    ops
}

#[test]
fn diff_apply_roundtrip_is_byte_identical() {
    // (a) two births from nothing.
    let next = canon(&runs(json!({
        "revision": 2,
        "runs": [
            {
                "runId": "r1", "status": "running",
                "usage": { "spentTokens": 5, "nodesUsed": 3 },
                "lastEventSequence": 9,
                "actors": [{ "siteId": "a", "ordinal": 0, "status": "waiting" }],
                "nodes": [{ "siteId": "n", "ordinal": 0, "phase": "executing" }]
            },
            {
                "runId": "r2", "toolCallId": "tc-7", "status": "pending",
                "usage": { "spentTokens": 0, "nodesUsed": 0 },
                "lastEventSequence": 0,
                "actors": [], "nodes": []
            }
        ]
    })));
    let ops = roundtrip("births", None, &next);
    assert_eq!(ops.len(), 2, "one birth op per run");

    // (b) header change + cleared key + entry eviction/update + birth.
    let prior = runs(json!({
        "revision": 5,
        "runs": [{
            "runId": "r1", "status": "running",
            "usage": { "spentTokens": 5, "nodesUsed": 3 },
            "lastEventSequence": 9,
            "reports": [{ "siteId": "rep", "ordinal": 0, "preview": "early" }],
            "actors": [
                { "siteId": "a", "ordinal": 0, "status": "waiting" },
                { "siteId": "a", "ordinal": 1, "status": "waiting" },
                { "siteId": "a", "ordinal": 2, "status": "waiting" }
            ],
            "nodes": [{ "siteId": "n", "ordinal": 0, "phase": "executing" }]
        }]
    }));
    let next = canon(&runs(json!({
        "revision": 6,
        "runs": [
            {
                "runId": "r1", "status": "completed", "error": "done-for",
                "usage": { "spentTokens": 5, "nodesUsed": 4 },
                "lastEventSequence": 12,
                "actors": [
                    { "siteId": "a", "ordinal": 0, "status": "waiting" },
                    { "siteId": "a", "ordinal": 2, "status": "completed" }
                ],
                "nodes": [
                    { "siteId": "n", "ordinal": 0, "phase": "settled", "outcome": "ok" },
                    { "siteId": "n", "ordinal": 1, "phase": "queued", "kind": "ask" }
                ]
            },
            {
                "runId": "r3", "status": "running",
                "usage": { "spentTokens": 0, "nodesUsed": 0 },
                "lastEventSequence": 1,
                "actors": [], "nodes": []
            }
        ]
    })));
    let ops = roundtrip("mixed", Some(&prior), &next);
    assert_eq!(ops.len(), 2, "one update for r1, one birth for r3");
    if let ConversationDelta::WorkflowRunUpdated(u) = &ops[0] {
        assert_eq!(u.run_id, "r1");
        let cleared = u.cleared.as_ref().expect("reports were dropped");
        assert_eq!(cleared, &vec!["reports".to_string()]);
        assert_eq!(u.removed_actors.as_ref().map(|v| v.len()), Some(1));
        assert_eq!(u.actors.as_ref().map(|v| v.len()), Some(1), "only a2 changed");
        assert_eq!(
            u.nodes.as_ref().map(|v| v.len()),
            Some(2),
            "n0 settled (phase+outcome changed) and n1 is new"
        );
    } else {
        panic!("expected workflowRun.updated for r1, got {ops:?}");
    }

    // (c) whole-run eviction: removal first, survivor untouched.
    let prior = runs(json!({
        "revision": 10,
        "runs": [
            { "runId": "old", "status": "completed",
              "usage": { "spentTokens": 0, "nodesUsed": 0 }, "lastEventSequence": 3,
              "actors": [], "nodes": [] },
            { "runId": "live", "status": "running",
              "usage": { "spentTokens": 0, "nodesUsed": 0 }, "lastEventSequence": 4,
              "actors": [], "nodes": [] }
        ]
    }));
    let next = canon(&runs(json!({
        "revision": 11,
        "runs": [
            { "runId": "live", "status": "running",
              "usage": { "spentTokens": 0, "nodesUsed": 0 }, "lastEventSequence": 4,
              "actors": [], "nodes": [] }
        ]
    })));
    let ops = roundtrip("eviction", Some(&prior), &next);
    assert_eq!(ops.len(), 1);
    assert!(matches!(
        &ops[0],
        ConversationDelta::WorkflowRunRemoved { run_id, revision: 11 } if run_id == "old"
    ));

    // (d) revision moved with zero ops: whole-key resync via state.updated.
    let prior = runs(json!({
        "revision": 3,
        "runs": [{ "runId": "r1", "status": "running",
            "usage": { "spentTokens": 0, "nodesUsed": 0 }, "lastEventSequence": 2,
            "actors": [], "nodes": [] }]
    }));
    let mut next = prior.clone();
    next.revision = 4;
    let next = canon(&next);
    let ops = roundtrip("revision-only", Some(&prior), &next);
    assert_eq!(ops.len(), 1, "container version has no carrier of its own");
    assert!(matches!(&ops[0], ConversationDelta::StateUpdated { patch }
        if patch.keys.contains_key("workflowRuns")));

    // (e) identical states, identical revision: no ops at all.
    let next = canon(&prior);
    let ops = roundtrip("identical", Some(&prior), &next);
    assert!(ops.is_empty());

    // (f) entry reorder is inexpressible: whole-key resync, still byte equal.
    let prior = runs(json!({
        "revision": 8,
        "runs": [{ "runId": "r1", "status": "running",
            "usage": { "spentTokens": 0, "nodesUsed": 0 }, "lastEventSequence": 2,
            "actors": [
                { "siteId": "a", "ordinal": 0, "status": "waiting" },
                { "siteId": "a", "ordinal": 1, "status": "waiting" },
                { "siteId": "a", "ordinal": 2, "status": "waiting" }
            ],
            "nodes": [] }]
    }));
    let next = canon(&runs(json!({
        "revision": 9,
        "runs": [{ "runId": "r1", "status": "running",
            "usage": { "spentTokens": 0, "nodesUsed": 0 }, "lastEventSequence": 2,
            "actors": [
                { "siteId": "a", "ordinal": 2, "status": "waiting" },
                { "siteId": "a", "ordinal": 1, "status": "waiting" },
                { "siteId": "a", "ordinal": 0, "status": "waiting" }
            ],
            "nodes": [] }]
    })));
    let ops = roundtrip("reorder", Some(&prior), &next);
    assert_eq!(ops.len(), 1);
    assert!(matches!(&ops[0], ConversationDelta::StateUpdated { .. }));

    // (g) schema-unknown keys survive an update untouched (diff walks schema
    // keys only; the contract covers reducer-produced states, which never
    // change unknown keys).
    let prior = runs(json!({
        "revision": 1,
        "runs": [{ "runId": "r1", "status": "running", "customFlag": true,
            "usage": { "spentTokens": 0, "nodesUsed": 0 }, "lastEventSequence": 2,
            "actors": [], "nodes": [] }]
    }));
    let next = canon(&runs(json!({
        "revision": 2,
        "runs": [{ "runId": "r1", "status": "completed", "customFlag": true,
            "usage": { "spentTokens": 0, "nodesUsed": 0 }, "lastEventSequence": 3,
            "actors": [], "nodes": [] }]
    })));
    roundtrip("unknown-key-stable", Some(&prior), &next);
}

// ----------------------------------------------------------- coalesce 6 ----

fn one_run(revision: u64, status: &str) -> WorkflowRunsState {
    runs(json!({
        "revision": revision,
        "runs": [{
            "runId": "r1", "status": status,
            "usage": { "spentTokens": 0, "nodesUsed": 0 },
            "lastEventSequence": 1,
            "actors": [{ "siteId": "s", "ordinal": 0, "status": "waiting" }],
            "nodes": []
        }]
    }))
}

#[test]
fn merged_update_applies_like_two_sequential_updates() {
    let prior = one_run(4, "running");
    let u1 = update(json!({
        "runId": "r1", "revision": 5,
        "run": { "error": "halfway", "resultPreview": "…" },
        "actors": [{ "siteId": "s", "ordinal": 0, "status": "running" }]
    }));
    let u2 = update(json!({
        "runId": "r1", "revision": 6,
        "cleared": ["error"],
        "removedActors": [{ "siteId": "s", "ordinal": 0 }]
    }));
    let merged = merge_workflow_run_updates(&u1, &u2);
    // cleared/run annihilation: error was set earlier and cleared later —
    // the merged op must not carry both halves.
    let merged_error = merged.run.as_ref().and_then(|p| p.get("error"));
    assert_eq!(merged_error, None);
    assert_eq!(merged.cleared, Some(vec!["error".to_string()]));
    assert_eq!(merged.revision, 6, "merged revision is the max");
    // Earlier upsert dropped by the later removal ⇒ the actors key is absent.
    assert!(merged.actors.is_none());

    let sequential = {
        let s = apply_workflow_run_updated(Some(&prior), &u1);
        apply_workflow_run_updated(Some(&s), &u2)
    };
    let one_shot = apply_workflow_run_updated(Some(&prior), &merged);
    assert_eq!(wire(&sequential), wire(&one_shot), "merge must not change final state");
    assert!(one_shot.runs[0].actors.is_empty());
    assert_eq!(one_shot.runs[0].header_value("error"), None);
    assert_eq!(one_shot.runs[0].header_value("resultPreview"), Some(Cow::Borrowed(&json!("…"))));
}

#[test]
fn later_re_add_of_a_removed_key_lands_at_the_tail() {
    let prior = one_run(4, "running");
    let u1 = update(json!({
        "runId": "r1", "revision": 5,
        "actors": [{ "siteId": "s", "ordinal": 0, "status": "running" }]
    }));
    let u2 = update(json!({
        "runId": "r1", "revision": 6,
        "removedActors": [{ "siteId": "s", "ordinal": 0 }],
        "actors": [{ "siteId": "s", "ordinal": 0, "status": "completed" }]
    }));
    let merged = merge_workflow_run_updates(&u1, &u2);
    let actors = merged.actors.as_ref().expect("re-add keeps the key");
    assert_eq!(actors.len(), 1, "the key appears once, from the later list");

    let sequential = {
        let s = apply_workflow_run_updated(Some(&prior), &u1);
        apply_workflow_run_updated(Some(&s), &u2)
    };
    let one_shot = apply_workflow_run_updated(Some(&prior), &merged);
    assert_eq!(wire(&sequential), wire(&one_shot));
    assert_eq!(one_shot.runs[0].actors[0].extra.get("status"), Some(&json!("completed")));
}

fn wr_update(u: WorkflowRunUpdate) -> ConversationDelta {
    ConversationDelta::WorkflowRunUpdated(u)
}

#[test]
fn out_of_bounds_merge_is_refused_and_stays_equivalent() {
    let actors: Vec<_> = (0..5)
        .map(|i| json!({ "siteId": "s", "ordinal": i, "status": "waiting" }))
        .collect();
    let refs: Vec<_> = (0..5)
        .map(|i| json!({ "siteId": "s", "ordinal": i }))
        .collect();
    let u1 = wr_update(update(json!({
        "runId": "r1", "revision": 5, "actors": actors
    })));
    let u2 = wr_update(update(json!({
        "runId": "r1", "revision": 6, "removedActors": refs
    })));
    // The merged removal list (5) exceeds max_actors=4: refusal is the only
    // safe outcome — per-delta delivery is byte-identical.
    let tight = WorkflowRunWireBounds { max_actors: 4, max_nodes: 4 };
    let out = coalesce_conversation_deltas_with_bounds(&[u1.clone(), u2.clone()], &tight);
    assert_eq!(out.len(), 2, "out-of-bounds merge must be refused");
    // Within bounds it merges to one op.
    let merged_out = coalesce_conversation_deltas_with_bounds(
        &[u1.clone(), u2.clone()],
        &WorkflowRunWireBounds::default(),
    );
    assert_eq!(merged_out.len(), 1);
    // And the direct bound check agrees with the coalesce gate.
    let merged = merge_workflow_run_updates(
        match &u1 { ConversationDelta::WorkflowRunUpdated(u) => u, _ => unreachable!() },
        match &u2 { ConversationDelta::WorkflowRunUpdated(u) => u, _ => unreachable!() },
    );
    assert!(!update_within_wire_bounds(&merged, &tight));
    assert!(update_within_wire_bounds(&merged, &WorkflowRunWireBounds::default()));

    // Equivalence either way.
    let prior = one_run(4, "running");
    let applied_two = {
        let s = apply_workflow_run_updated(Some(&prior),
            match &u1 { ConversationDelta::WorkflowRunUpdated(u) => u, _ => unreachable!() });
        apply_workflow_run_updated(Some(&s),
            match &u2 { ConversationDelta::WorkflowRunUpdated(u) => u, _ => unreachable!() })
    };
    let applied_one = apply_workflow_run_updated(Some(&prior),
        match &merged_out[0] { ConversationDelta::WorkflowRunUpdated(u) => u, _ => unreachable!() });
    assert_eq!(wire(&applied_two), wire(&applied_one));
}

#[test]
fn rule6_merges_walk_back_past_other_ops_into_the_last_same_run_slot() {
    let u1 = wr_update(update(json!({ "runId": "r1", "revision": 5, "run": { "error": "x" } })));
    let other = wr_update(update(json!({ "runId": "r2", "revision": 6, "run": { "status": "running" } })));
    let u2 = wr_update(update(json!({ "runId": "r1", "revision": 7, "cleared": ["error"] })));
    let out = coalesce_conversation_deltas_with_bounds(
        &[u1.clone(), other, u2],
        &WorkflowRunWireBounds::default(),
    );
    assert_eq!(out.len(), 2, "r1's two increments merge across r2's");
    match &out[0] {
        ConversationDelta::WorkflowRunUpdated(u) => {
            assert_eq!(u.run_id, "r1");
            assert_eq!(u.revision, 7);
            assert_eq!(u.cleared, Some(vec!["error".to_string()]));
            assert!(u.run.as_ref().is_none_or(|p| p.get("error").is_none()));
        }
        other => panic!("merged op must keep the earlier slot, got {other:?}"),
    }
    assert!(matches!(&out[1], ConversationDelta::WorkflowRunUpdated(u) if u.run_id == "r2"));
}

#[test]
fn rule6_removal_half_swallows_the_birth_and_resync_is_a_barrier() {
    let birth = wr_update(update(json!({
        "runId": "r1", "revision": 5,
        "run": { "runId": "r1", "status": "running",
                 "usage": { "spentTokens": 0, "nodesUsed": 0 }, "lastEventSequence": 0 },
        "actors": [{ "siteId": "s", "ordinal": 0, "status": "waiting" }]
    })));
    let gone = ConversationDelta::WorkflowRunRemoved { run_id: "r1".into(), revision: 6 };
    // Birth + eviction in one window: the client never saw the run.
    let out = coalesce_conversation_deltas_with_bounds(&[birth, gone], &WorkflowRunWireBounds::default());
    assert_eq!(out.len(), 1);
    assert!(matches!(&out[0], ConversationDelta::WorkflowRunRemoved { run_id, revision: 6 } if run_id == "r1"));

    // A state.updated carrying workflowRuns is a barrier: no merge across it.
    let u1 = wr_update(update(json!({ "runId": "r1", "revision": 5, "run": { "error": "x" } })));
    let resync = ConversationDelta::StateUpdated {
        patch: StatePatch {
            revision: None,
            keys: [("workflowRuns".to_string(), json!({ "revision": 6, "runs": [] }))]
                .into_iter()
                .collect(),
        },
    };
    let u2 = wr_update(update(json!({ "runId": "r1", "revision": 7, "run": { "status": "stopped" } })));
    let out = coalesce_conversation_deltas_with_bounds(&[u1, resync, u2], &WorkflowRunWireBounds::default());
    assert_eq!(out.len(), 3, "the resync replaced the whole key — merging across it would reorder");

    // row.removed is NOT a rule-6 barrier: row ops and workflowRun ops touch
    // disjoint state and commute (coalesce.ts:23-25).
    let u1 = wr_update(update(json!({ "runId": "r1", "revision": 5, "run": { "error": "x" } })));
    let row = ConversationDelta::RowRemoved { from_row_id: 3 };
    let u2 = wr_update(update(json!({ "runId": "r1", "revision": 6, "cleared": ["error"] })));
    let out = coalesce_conversation_deltas_with_bounds(&[u1, row, u2], &WorkflowRunWireBounds::default());
    assert_eq!(out.len(), 2, "rule 6 walks across row barriers");
    assert!(matches!(&out[0], ConversationDelta::WorkflowRunUpdated(u) if u.revision == 6));
}

// ------------------------------------------------------- generic reducer ---

#[test]
fn apply_delta_routes_workflow_run_ops_through_the_state_map() {
    let birth = wr_update(update(json!({
        "runId": "r1", "revision": 5,
        "run": { "runId": "r1", "status": "running",
                 "usage": { "spentTokens": 0, "nodesUsed": 0 }, "lastEventSequence": 0 },
        "actors": [{ "siteId": "s", "ordinal": 0, "status": "waiting" }]
    })));
    let mut state = ConversationState::default();
    apply_delta(&mut state, &birth);
    assert_eq!(state.state["workflowRuns"]["revision"], json!(5));
    assert_eq!(state.state["workflowRuns"]["runs"].as_array().map(Vec::len), Some(1));

    apply_delta(&mut state, &wr_update(update(json!({
        "runId": "r1", "revision": 6, "run": { "status": "completed" }
    }))));
    assert_eq!(state.state["workflowRuns"]["revision"], json!(6));
    assert_eq!(state.state["workflowRuns"]["runs"][0]["status"], json!("completed"));

    apply_delta(&mut state, &ConversationDelta::WorkflowRunRemoved {
        run_id: "r1".into(),
        revision: 7,
    });
    assert_eq!(state.state["workflowRuns"]["revision"], json!(7));
    assert_eq!(state.state["workflowRuns"]["runs"].as_array().map(Vec::len), Some(0));
}

// ----------------------------------------------------------- wire shape ----

#[test]
fn workflow_run_updated_wire_shape_roundtrips_byte_identically() {
    // Field names and conditional presence straight from delta.ts:121-137;
    // declaration order encodes the application order.
    let raw = concat!(
        r#"{"op":"workflowRun.updated","runId":"run-1","revision":12,"#,
        r#""run":{"status":"completed","resultPreview":"ok"},"#,
        r#""cleared":["reports"],"#,
        r#""removedActors":[{"siteId":"s1","ordinal":3}],"#,
        r#""removedNodes":[{"siteId":"s0","ordinal":9},{"siteId":"s0","ordinal":2}],"#,
        r#""actors":[{"siteId":"s1","ordinal":4,"status":"waiting"}],"#,
        r#""nodes":[{"siteId":"s0","ordinal":10,"phase":"executing"}]}"#
    );
    let delta: ConversationDelta = serde_json::from_str(raw).expect("decode workflowRun.updated");
    assert_eq!(serde_json::to_string(&delta).expect("encode"), raw);

    // Absent optionals stay absent (TS conditional spread).
    let bare = r#"{"op":"workflowRun.updated","runId":"r","revision":1}"#;
    let delta: ConversationDelta = serde_json::from_str(bare).expect("decode bare op");
    assert_eq!(serde_json::to_string(&delta).expect("encode"), bare);

    let gone = r#"{"op":"workflowRun.removed","runId":"run-1","revision":13}"#;
    let delta: ConversationDelta = serde_json::from_str(gone).expect("decode workflowRun.removed");
    assert_eq!(serde_json::to_string(&delta).expect("encode"), gone);
}

#[test]
fn state_updated_patch_carries_a_parseable_workflow_runs_state() {
    // The resync path embeds the whole key; the payload must round-trip
    // through the typed state on the consumer side. (A prior-absent diff with
    // runs present produces birth ops instead — covered by the roundtrip
    // suite — so exercise the resync arm through the revision-moved case.)
    let next = canon(&one_run(9, "running"));
    let prior = next.clone();
    let mut moved = next;
    moved.revision = 10;
    let ops = diff_workflow_runs_state(Some(&prior), &moved);
    match &ops[0] {
        ConversationDelta::StateUpdated { patch } => {
            let parsed: WorkflowRunsState = serde_json::from_value(
                patch.keys["workflowRuns"].clone(),
            )
            .expect("embedded workflowRuns parses back");
            assert_eq!(wire(&parsed), wire(&moved));
        }
        other => panic!("expected resync, got {other:?}"),
    }
}
