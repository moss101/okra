# N0030 — the Rhai workflow engine (run-to-run durable, budget-honest)

- **Status:** implemented
- **Decided:** 2026-10-01
- Closes: MASTER-PLAN §3 #41's engine half ("Rhai workflow engine +
  budgets + run journal" — the journal existed; the engine did not).
  The N0004 vendoring verdict applies: grok's `xai-workflow` extraction
  was rejected for the same dependency-web reasons as the sandbox; the
  CONTRACT is ported instead.

## Decision

1. **The only host effect is `step(name, input)`.** A script is Rhai
   `fn run()` (or `main`); every step crosses the [`WorkflowHost`] seam,
   which owns execution and its own policy. The engine never touches the
   world.
2. **Budgets are cancellations, not failures** (the backstop honesty
   rule): steps / wall-clock / stop-flag abort as `RunStatus::Cancelled`
   with the breach named. Checks run per step AND per Rhai statement
   (`on_progress`) — a tight pure-Rhai loop cannot outlive the wall
   clock. A script's `try/catch` may swallow the step-level error, but
   the abort flag wins at the top level (and the next statement re-flags).
3. **Journal first**: `run/started`, `step/started`, `step/finished`
   (before and after the effect), `run/completed|failed|cancelled` —
   same durable-truth-first rule as the kernel log. Display caps match
   the workflow-runs wire budget (2048).
4. **`okra workflow run SCRIPT.rhai [--provider openai --model M]
   [--max-steps N]`**: steps execute as FULL okra turns in child
   processes (`okra --cwd --json`, provider passthrough) — own kernel
   session, own crash domain; the workflow survives a step that dies
   (the step fails, the run fails honestly). The app spawn site is
   sanctioned (same pattern as host git.rs).

## Honesty notes

- The sequential engine keeps one step in flight: fan-out is enforced
  at 1 (the `check_budgets` seam is live; a parallel engine raises it
  without contract changes).
- The engine's own operations cap is disabled (set_max_operations(0)) —
  our wall clock owns termination via on_progress, rather than two
  caps racing to different verdicts.

## Evidence

- `crates/workflow/src/engine.rs` unit tests: entry dispatch + per-step
  journaling; failing step fails the run; step budget cancels; wall
  clock cancels inside pure loops (no steps at all); stop flag cancels;
  a script catching its own step error completes.
- `apps/okra/tests/workflow_cli.rs` (real binary): a two-step script
  runs REAL child turns (demo planner), `WORKFLOW` summary reports
  completed, the journal carries both steps; `--max-steps 2` on a
  three-step script exits 130 with a cancelled status naming the breach.
