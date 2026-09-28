# N0008 — the workbench web shell lives in the daemon, driven by the real v4 seam

- **Status:** implemented
- **Decided:** 2026-09-28
- Blocks: MASTER-PLAN #49 (UI package), #53 (approval/stop UX slice), UI-SHELL-PLAN U0/U3 (browser surface)
- Supersedes: the G4 "browser demo page" (a monospace test harness) and the
  demo-only `serve` turn path.

## Decision

`okra serve --tcp` is now a product surface, not a test harness:

1. **The workbench UI** (`ui/index.html|app.css|app.js`, embedded at compile
   time via `include_str!`, no build step, no node_modules) replaces the demo
   page at `GET /`. Design authority: ChatGPT2 docs/07 token architecture —
   semantic tokens re-declared per `[data-theme]` (one stylesheet, paired
   light/dark), `--corner-radius-scale` multiplier, the 0.4/0/0.2/1 easing.
   Layout: tasks sidebar (ZCode Task terminology per UI-SHELL-PLAN §3.3 —
   no "thread/chat/conversation" in copy) · transcript (markdown assistant
   rows, collapsible tool cards with status/duration, turn dividers, steered
   chips) · composer (send/stop dual-mode, steering hint while running).
   In-app toasts implement the turn-complete notification class.
2. **Serve drives the real pipeline**: `--provider openai --model NAME`
   (fail-fast on missing key) builds per-turn samplers behind a
   `SamplerFactory`; the tool plane is the full CLI registry
   (read/list/write/edit) under `ApprovalPolicy::Ask` +
   `UnattendedAllowed` — the served page is no longer read-only.
3. **Sessions survive reloads/restarts**: serve turns fold into the SQLite
   index with `replace_session` (per-session; the old `rebuild_from_log`
   call wiped every other web session's rows — the multi-session bug) plus
   a title upsert (first user text). `GET /api/sessions` lists the index;
   `GET /api/sessions/<id>/rows` replays projection rows from the kernel
   log (404 unknown / 422 non-ignorable-unknown-event — honest refusal per
   the vocabulary-growth rule). The UI replays on task click, then live
   full-snapshot SSE frames take over wholesale.
4. **Stop is a first-class seam**: `CancellationCategory::UserRequested`
   extends the donor enum; `Agent::stop_flag/set_stop_flag/request_stop`
   lets a surface flip the flag while the turn owns the agent on another
   thread; the loop checks it at every step boundary. The `stop` v4 command
   flips it; an interrupted turn is honest (`completedInterrupted`, never
   `error`) and recovers through the standard repair path. Tool outputs are
   not yet replayed (the kernel `tool/result` event logs callId+error only
   — content stays in the model-visible log contract; deferred until the
   event carries content).

## Why

The user's parity complaint ("not at parity with ZCode — UI/UX is missing")
was structurally correct: the plan's own UI phase (UI-SHELL-PLAN U0) had
never landed, so the only surface was a test-grade page driving a demo
planner with read-only tools. A daemon without a product surface cannot
dogfood (G3) or prove the multi-surface claim (G4) — the UI layer is the
acceptance vehicle for every later phase, and the kernel + protocol work
already supports it (full-snapshot projections, durable log, SQLite index).

## Evidence

- `g4_workbench_assets_are_served`, `g4_sessions_index_and_replay`,
  `g4_stop_command_cancels_a_live_turn` (HTTP surface), the two agent-core
  stop-seam tests, and the two kernel `replace_session` tests; workspace
  `cargo test` green, clippy clean, `scripts/ci.sh` boundaries ok.
- Hands-on browser drive (2026-09-28): real turn with streaming + tool
  card, mid-turn steering (STEERED chip), stop → interrupted chip,
  reload → task list restored + full replay from the log, dark/light
  tokens. Screenshots in the session record.
