# N0032 — the `workflowRun.*` ops land (N0003's M3 revisit)

- **Status:** implemented
- **Decided:** 2026-10-01
- Closes: N0003's deferral — "the two `workflowRun.*` ops are re-added
  in M3 together with the workflow-runs host domain that owns their
  wire-bounds logic."

## Decision

Ported `workflow-runs-delta.ts` (ZCode ground truth) into
`crates/protocol/src/workflow_runs.rs`: key-level diff for the
high-frequency `workflowRuns` state key — birth-on-complete-header,
canonical key order (schema declaration order, extras in arrival
order), entry identity `(siteId \0 ordinal)`, remove-then-upsert apply
semantics, max-revision on out-of-order ops, wire bounds
(maxActors/maxNodes 1024), coalesce rule 6 at full fidelity
(walk-back merge into the last same-runId slot crossing other ops,
removal-half swallow, `state.updated`-with-workflowRuns as the barrier,
refused-merge-never-retries). `delta.rs` gains `workflowRun.updated` /
`workflowRun.removed` with the TS serde shapes; `coalesce.rs` wires
rule 6 and the reducer.

## Evidence

- Because: the TS module is the contract (N0003) — convergence is
  byte-level, not structural.
- 17 Rust tests in `crates/protocol/tests/workflow_runs_ops.rs`
  (byte roundtrips: `apply(diff(prior, next))` === `next` across 8
  prior/next pairs, merge ≡ sequential apply, bounds refusal, barrier
  semantics, wire-shape roundtrip); DIFFERENTIAL against the live donor
  — the actual TS modules (tsx + zod from the ZCode workspace) executed
  16 shared fixtures (diff→apply, merge, coalesce) and the Rust output
  was byte-identical on all 16; the 12 golden convergence tests are
  untouched and green.

## TS-contract adaptations (documented in-code)

- `WorkflowRunUpdated` is a newtype (one typed payload) — wire shape
  identical, verified by roundtrip + differential.
- No `preserve_order` feature: the header is ordered `(String, Value)`
  pairs with custom Serde (document order in, canonical order out);
  generic `state.updated` payloads carry semantics, not canonical
  order — noted at both sites.
- serde_json has no undefined: key-present-vs-absent is the whole
  story; numbers compare via an f64 fallback (JS `===` is double).
- zod-refused garbage (cleared `runId`/`status`, non-string birth
  runId) collapses instead of carrying through.
