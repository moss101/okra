# N0041 — the workflow→wire link (live workflowRun deltas) + the TypeSafe step gate

- **Status:** implemented
- **Decided:** 2026-10-01
- Closes: the last open seam of the M3 workflow bundle — N0003's "the
  two `workflowRun.*` ops are re-added in M3 together with the
  workflow-runs host domain that owns their wire-bounds logic" had the
  OPS (n0032) and the ENGINE (n0030) but no producer connected them.

## Decision

1. **A journal sink + tee** (`RunJournalSink` / `TeeJournal`): the engine
   journals to durable truth as before; a tee fans every entry out to a
   live observer while it lands on disk. The engine signature takes the
   sink by `Rc<dyn RunJournalSink>` (callers unchanged in behavior).
2. **The projector** (`apps/okra/src/workflow_serve.rs`): folds journal
   events into the ported `WorkflowRunsState` — run birth on
   `run/started`, a node per step (`running` → `completed|failed`),
   status transitions on the terminal events, `lastEventSequence` +
   `usage.nodesUsed` maintained, revision monotonic. Every fold emits
   CHANGE-ONLY deltas via `diff_workflow_runs_state` — the exact
   producer the TS contract was ported for.
3. **`POST /api/workflow/run`**: validate first (422 + findings on
   errors — n0036 in the loop), then engine + projection on a worker
   thread; deltas broadcast to EVERY attached surface (NDJSON + SSE) as
   `v4/workflowRuns` frames carrying serialized `ConversationDelta`s.
   `GET /api/workflow/status?runId=` reads the durable journal (poll
   fallback). The workbench UI ignores unknown methods today — wire
   first, rendering later.
4. **The TypeSafe step gate** (`workflow_gate.rs`, the skill's
   verify-and-escalate pattern): before a step's task text becomes an
   AUTONOMOUS child turn, one Jev noul judges whether it tries to escape
   or subvert the workflow's controls (unrelated files/apps, disabling
   safety, hiding actions, exfiltration). Thresholds: >0.80 refuse
   (honest step error — the engine journals it), >0.50 proceed with a
   surfaced warning. INERT without BOTH `OKRA_WORKFLOW_GATE=on` and
   `TYPESAFE_API_KEY` (the N0021 governor's inertness contract); any API
   error fails OPEN (a semantic signal never blocks a run). Wired into
   `TurnStepHost`, so both `okra workflow run` and the serve endpoint
   get it.

## Evidence

- `apps/okra/tests/g4_workflow_wire.rs` (real daemon + real SSE
  surface): invalid script → 422 with the finding; a two-step run →
  birth + node + terminal deltas arrive over SSE; applying them through
  the protocol's OWN reducer (`apply_delta`) reconstructs the run —
  runId, `status: completed`, both step nodes `completed` — the TS
  byte-contract round-tripping over a live wire; the durable journal
  agrees via the status endpoint.
- `workflow_gate.rs` unit tests: threshold decisions (0.50/0.80
  boundaries exact), inertness without the env pair; live smoke follows
  the jev.rs skip-without-key pattern.
- `g4_rewind_resets_git_to_the_captured_head` (new): an approved write,
  the turn's work committed after the turn, rewind to prompt 0 → the
  file is gone AND `git rev-parse HEAD` equals the captured base HEAD,
  with the report naming it.

## Because

Deltas without a producer are a spec; a producer without deltas is a
journal file. The link is the deliverable — and a workflow that fans
out autonomous child turns without any semantic containment on the step
text is the exact hole the Jev gate fills (lexical filters cannot read
"rename every file in ~/Documents" as an escape attempt).
