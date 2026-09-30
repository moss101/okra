# N0028 — continuation everywhere: skills + memory + world head on every real surface

- **Status:** implemented
- **Decided:** 2026-10-01
- Closes: the M2 "remaining: skills" line — the activation path existed
  (`run_turn_continuation`) but only the bench harness called it; daemon,
  CLI, and ACP turns used bare `run_turn`.

## Decision

1. **The head is seeded on every turn, not only at install time.**
   `run_turn_continuation` previously relied on `SessionContext::install`
   (compaction crossing) to materialize `prefix_head` as a System message —
   pre-first-compaction turns carried NO world state, skill index, or
   memory recall. The continuation now prepends a synthetic System message
   with the head bytes whenever the seed does not already start with one.
   The synthetic copy is never folded back (`new_messages` starts past
   it); at install time the identical bytes take over, so the prefix stays
   byte-stable across the seam.
2. **Daemon turns are continuations.** `run_turn_streaming` takes an
   optional per-session `SessionContext` (`TcpServeState.contexts`,
   daemon-lifetime like `mcp_sessions`); each turn builds a fresh
   `TieredReader` + project `SkillCatalog` (fail-open: absent dirs are
   empty — a Tools-tab install takes effect on the next send, no daemon
   state) and calls `run_turn_continuation`. Consecutive sends on one
   session now CHAIN (turn N+1 sees turn N's messages until compaction) —
   previously every daemon turn was amnesiac.
3. **The stdio surface** (`serve --stdio`) gets the same per-session
   context map; **the CLI** and **ACP turns** build the trio per
   invocation/session the same way.

## Honesty notes

- The compactor is `ScriptedCompactor` (deterministic); a model-backed
  summarizer for live compaction remains the M2 bench-grade seam it was.
- Contexts live for the daemon's lifetime; a restarted daemon starts a
  fresh context (the kernel log remains the durable truth for UI replay).
  Rebuilding a continuation context from the log is future work.

## Evidence

- `crates/agent-core/tests/continuation_head.rs` — turn 1's first sampled
  message is System carrying `<world_state>` + `# Skills` + `<memory_recall>`
  with zero installs; the synthetic head does not persist into the
  context; turn 2 sees turn 1's user message (chaining) and the skill is
  ACTIVE after `f.txt` was noted.
- serve/stdio/CLI/ACP call sites all route through
  `run_turn_continuation` (grep: no bare `run_turn` remains on live paths
  except quality/bench harnesses which own their contexts).
