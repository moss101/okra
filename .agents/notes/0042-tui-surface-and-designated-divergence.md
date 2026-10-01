# N0042 — M4 end to end: the TUI as a daemon surface + designated mediation that actually diverges

- **Status:** implemented
- **Decided:** 2026-10-01
- Closes: M4's G4 line for the TUI leg ("one live session visible and
  steerable from browser + TUI simultaneously") and n0033's own caveat
  ("one attached client makes every policy coincide today").

## Decision

1. **The TUI attaches** (`okra tui --attach ADDR [--session ID]`): the
   client speaks the daemon's NDJSON v4 protocol over TCP — hello,
   `v4/conversation/subscribe` (params.envelope command nesting),
   `v4/command`. Projection rows render into the pager's scrollback:
   completed rows append once (rowId-deduped), streaming assistant rows
   OWN the live line (`set_last_line` replaces, never re-appends).
   The composer sends when idle and STEERS when running (the daemon's
   running-turn gate routes it); `y`/`n` resolve the pending approval
   when the composer is empty; Ctrl-C stops the live turn, quits when
   idle; scrolling keeps the n0031 scroll-lock. The standalone mode
   (`okra tui` without --attach) is unchanged.
2. **`--smoke`** drives the same client headless (send → poll → resolve
   approvals → terminal phase → print the scrollback) so CI proves the
   TUI surface against a real daemon without a tty.
3. **Designated mediation diverges for real.** `Mediator` gains an
   attached-probe (`Send + Sync`): when the DESIGNATED client is not
   live at ask time, the ask answers NOTHING (waterfall → unavailable →
   the side-effecting tool is denied) — the bridge is never consulted,
   so no approval card registers. `SurfaceRegistry::has_kind` backs the
   probe ("workbench" = a live Browser surface). The same gate sits on
   `resolveApproval`: under designated with the browser absent, a
   resolution from ANY surface is rejected
   (`okra.mediation.designatedAbsent`) — an NDJSON client cannot
   click-approve around a browser designation. first-responder/
   consensus/local-only semantics are unchanged (still one bridge
   channel; consensus's real multi-client waits on per-surface
   resolution channels — noted, not hidden).

## Evidence

- `apps/okra/tests/g4_tui_surface.rs` (real daemon): smoke ALLOW — the
  projection renders (turn/tool rows), the approval resolves, the write
  lands; smoke DENY — the write never lands, the turn still ends
  honestly; attach-to-EXISTING-session — turn 1's rows replay to the
  late TUI subscriber and turn 2 renders in the same scrollback.
- `apps/okra/tests/g4_mediation.rs` (real daemon, `--mediation
  designated --designated workbench`): browser ABSENT — no ask ever
  registers, `resolveApproval` rejected with
  `okra.mediation.designatedAbsent`, the turn completes WITHOUT the
  write; browser ATTACHED (SSE held live) — the same resolution is
  accepted and the write lands. The probe logs its verdict per ask.
- `crates/policy/src/mediation.rs` probe unit tests (absent → no
  verdict even with the bridge attached; live → bridge answers; no
  probe → static back-compat). `crates/tui/src/pager.rs`
  `set_last_line` test (streaming replaces, kind transitions push).
- Also: pre-existing `[comp-debug]` eprintln residue removed (it fired
  on every turn registration).

## TypeSafe note

The typesafe-ai skill was consulted for this milestone: the TUI,
mediation, and sessions-page legs are deterministic surface mechanics —
code owns them entirely, and no judgment improves them (the skill's
own "ignore uncertainty on unused branches" applies). The Jev seam
already lives where semantics matter in this codebase (N0021 wander
governor, n0041 step gate).
