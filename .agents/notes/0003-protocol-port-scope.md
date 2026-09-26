# N0003 — Protocol port scope: core delta ops; `workflowRun.*` deferred to M3

- **Status:** implemented
- **Decided:** 2026-09-26

## Decision

The okra-protocol port of `ConversationDelta` carries the five core ops
(`row.appended`, `row.upserted`, `row.removed`, `row.delta`, `state.updated`)
as a closed serde enum. The two `workflowRun.*` ops are **not** ported in M0;
they are re-added in M3 together with the workflow-runs host domain that owns
their wire-bounds logic (`workflow-runs-delta.ts`).

## Why

The workflowRun increment exists because `workflowRuns` is a high-frequency
state key in ZCode's UI (O(N²) avoidance). okra has no workflow-runs state
yet, so porting the op now would mean porting `mergeWorkflowRunUpdates`,
`workflowRunUpdateWithinWireBounds`, and the header/entry schemas with no
consumer — the exact "porting code before its contract has a user" mistake
the plan warns against (PORT-TO-RUST §1).

## Consequences

- Coalesce rule 6 (workflowRun merge) is correspondingly deferred; rules 1–4
  plus `conflateByKey` are ported and golden-tested.
- The delta enum stays **closed**; adding `workflowRun.*` in M3 is a
  non-breaking addition for readers that reject unknown ops only when
  `ignorable` is unset (kernel event envelope rule, reused here).
