# N0004 — grok vendoring deferred; donor contracts re-implemented with citations

- **Status:** implemented (M0 deviation, revisited at M1 gate)
- **Decided:** 2026-09-26

## Decision

MASTER-PLAN day 3–5 says "vendor grok crates at SOURCE_REV into `vendor/grok`".
M0 of okra instead **re-implements the donor contracts** (cited file:line in
each module's header comment) and defers wholesale vendoring.

## Why

- The local grok-build tree (SOURCE_REV 036a5d8, 84 codegen crates) has a
  workspace `[patch.crates-io]` pointing at a private git fork
  (`our-forks/async-openai`) and inter-crate deps spread across the whole
  tree; making even `xai-grok-sandbox` compile standalone means extracting it
  from its `xai-*` dependency web — substantial surgery for code whose
  *contracts* are what M0 needs.
- The plan's own fork-risk mitigation (§6.2) is "all our changes in wrapper
  crates; `agent-core` wraps rather than edits". Starting from our own
  contract-faithful implementations and vendoring selected leaf crates
  (sandbox, JSONL storage) at M1+ preserves that option without paying the
  extraction tax twice.
- Every re-implementation cites its donor location and is golden-tested
  against donor ground truth where a machine-checkable oracle exists (TS
  protocol functions; documented invariants elsewhere).

## Revisit trigger

At the M1 gate (real multi-file coding tasks), before building the sandbox
and bash-analysis layers: re-evaluate extracting `xai-grok-sandbox` (nono)
and `xai-grok-shell/src/session/storage/jsonl` as vendored leaf crates.
Kernel-enforced sandboxing is exactly the "do not hand-roll" category.
