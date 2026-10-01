# N0034 — the in-turn `subagent` tool (G5 as a tool the model can call)

- **Status:** implemented
- **Decided:** 2026-10-01
- Closes: MASTER-PLAN §3 #43/#44's missing piece — projected context
  (inherit-nothing) and FS isolation (worktree) existed as machinery
  (`host::subagent`, `run-subagent`, `subagent-launch`) but no surface
  let the MODEL delegate mid-turn.

## Decision

1. `subagent(name, task)` registers into every attended serve turn
   (approval-gated like any side-effecting tool — the card IS the
   delegation consent). The provider/model flags pass through so a
   `--provider openai` daemon spawns real-model children; the default
   demo child keeps everything hermetic.
2. Each call runs `run_isolated_subagent`: REAL git worktree on branch
   `<name>-<uuid>` (shared object store) → child = the full okra binary,
   `--sandbox workspace-write` (nono confines writes to the worktree),
   fresh session, empty grants → the child's work is committed on the
   branch (`collect_work`) → the worktree is removed (the branch stays
   for review/merge) → the tool returns the branch, commit, and the
   child's summary (2048-char cap).
3. Inherit-nothing by construction: the child process sees only the
   task text. Grants never cross the process boundary (the parent's
   grant reference lives and dies inside the launcher).
4. Non-repo workspaces get the honest refusal ("subagent requires the
   workspace to be a git repository") — no silent fallback to shared-FS
   delegation (that fallback is exactly the reference-product flaw the
   plan calls out).

## Evidence

- `apps/okra/tests/subagent_tool.rs`: the dispatch path end-to-end on a
  real repo — worktree created, confined child turn streams
  (`text_delta`), work committed on the branch and VISIBLE from the
  parent repo, the child's write never leaks into the parent checkout
  (`notes.txt` absent), worktree removal keeps the branch. Non-repo
  orchestration refuses and names the requirement.
- The G5 kernel-isolation property itself stays proven by the existing
  `g5_subagent` acceptance (EPERM on out-of-grant writes).
