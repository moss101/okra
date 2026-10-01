# N0029 — rewind over the workbench wire (checkpoints captured, restored, refused honestly)

- **Status:** implemented
- **Decided:** 2026-10-01
- Builds on: `host::checkpoints` (RewindPoint first-wins/last-wins,
  content-addressed shadow cache, durable JSONL mirror, git reset at
  restore) — the domain existed and was tested, but NOTHING fed it or
  exposed it: no surface captured writes, no route restored them.

## Decision

1. **Capture:** `build_registry_with_checkpoints` wraps write_file and
   edit_file — read before-bytes, run the real tool, read after-bytes,
   `record_operation` into the CURRENT prompt's checkpoint (workspace-
   relative paths only; outside-workspace refusals are the tool's own).
   `run_turn_streaming` brackets the turn with `begin_prompt` /
   `finalize_prompt` (git HEAD attached at turn end when the cwd is a
   repo — `restore_to` resets to it).
2. **Daemon state:** one `CheckpointManager` per daemon, durable mirror
   at `<cwd>/.okra/checkpoints.jsonl`, loaded at startup (checkpoints
   survive restarts). Per-session turn ordinals (`session_turns`) are the
   prompt indexes; the Agent's turn counter is seeded with the ordinal so
   kernel `turn/start` (and replay row numbering) is monotonic per
   session — previously every daemon turn rebuilt the Agent and logged
   `turn: 1`.
3. **Restore:** `POST /api/rewind {sessionId, promptIndex}` — 409 while
   a turn is in flight, 422 on unknown index, 200 with the honest report
   (restored / recreated / removed / gitResetTo). The session's
   continuation context is DROPPED (model amnesia: v1 resets fully
   rather than slicing mid-context — noted, not hidden).
4. **UI:** completed turn headers grow a hover "↺ rewind here" button
   (confirm → POST → toast + Changes refresh).

## Honesty notes

- The kernel log is append-only and is NOT rewound (it is the durable
  truth); rewind restores WORKSPACE + git + model context. The
  transcript keeps showing post-rewind turns — history, not state.
- stdio/ACP surfaces do not capture checkpoints yet (the seam is the
  registry builder; they pass `None`).

## Evidence

- `apps/okra/tests/g4_rewind.rs` — approved write lands, a manual
  scratch edit over it, `POST /api/rewind {promptIndex: 0}` removes the
  file the turn created (before-state: absent); unknown index refused;
  in-flight rewind refused with 409.
- `crates/host/tests/checkpoints_domain.rs` (pre-existing, still green).
