# N0036 — workflow validation passes (taint + causality, pre-run)

- **Status:** implemented
- **Decided:** 2026-10-01
- Closes: MASTER-PLAN §3 #42 (M5): "Workflow validation passes: taint
  analysis, causality graph" (ZCode dynamic-workflow analysis analog,
  over Rhai ASTs instead of the TS IR).

## Decision

`workflow::validate::validate(script) -> ValidationReport` runs BEFORE
a workflow spends anything; `okra workflow validate SCRIPT.rhai` prints
the JSON report and exits non-zero on errors.

1. **Taint:** entry-function parameters are untrusted. A parameter
   referenced inside a `step(...)` argument subtree WITHOUT an
   intervening `gate()` wrapper is an `ungated_input_to_step` warning —
   `gate` is the author's "I validated this text" marker (the engine
   does not define it; the script does). Attribution is by walk path
   (exact for straight-line scripts).
2. **Causality graph:** nodes are named step sites; edge A→B when a
   variable bound in [A, B) is referenced inside B's argument subtree
   (A's output feeds B's input). Cycles (possible through helper-mediated
   recursion, impossible in straight-line code) are errors; the cycle
   checker is rotation-normalized and unit-tested on synthesized graphs.
3. **Structural findings:** `compile_error`, `no_entry_function`
   (errors); `no_steps`, `unnamed_step` (warnings — names key the
   journal and the graph); `helper_fn_opaque` (info — the rhai metadata
   API does not expose fn bodies, so v1 analysis is explicitly NOT
   interprocedural; scripts with helpers say so in their own report).

## Evidence

- `crates/workflow/src/validate.rs` tests: clean script → zero findings
  + the exact graph (fetch→summarize; publish's literal args add no
  edge); ungated param warns, gate() silences; no-entry and compile
  errors; no_steps + unnamed_step; helper opacity declared; cycle
  detection finds back-edges, rotation-normalized, acyclic stays empty.
- Smoke: `okra workflow validate` on a 3-step script with an ungated
  param reports the warning at its line + the fetch→summarize edge.

## Because

Validation that runs before execution is the only kind that saves work:
every finding names a line, and the graph is what a UI will render as
the run timeline later (workflowRun.updated nodes feed it).
