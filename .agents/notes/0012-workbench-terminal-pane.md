# N0012 — the workbench has a real terminal: PTY over SSE + keystroke POST

- **Status:** implemented
- **Decided:** 2026-09-28
- Blocks: MASTER-PLAN #48 (terminal/PTY domain → surface), UI-SHELL-PLAN U0
  (panes), #55 analog for the workbench (the TUI stays grok's pager)
- Builds on: the host terminal domain (real PTY, TTY-verified); N0008's
  workbench shell; the SSE pattern from the v4 surface.

## Decision

1. **Reader split.** `TerminalSession::spawn_split` returns the session
   (writer/resize/child) with the PTY reader separated, so an output-pump
   thread owns a blocking read without holding the session lock (writers —
   keystrokes — never stall behind it). `spawn` keeps the original
   contract for `read_to_end` callers.
2. **Serve surface (per-terminal, keyed `t1..tn`):** `POST /api/term/open`
   (interactive `$SHELL` in the workspace by default), `GET /api/term/<id>/sse`
   (incremental base64 chunks off a bounded 256 KB scrollback; reset +
   snapshot when a reader lags past the window; `exit` frame on EOF),
   `POST /api/term/<id>/keys` (bytes written as if typed),
   `POST .../resize`, `POST .../close`, `GET /api/term` (list). Loopback-
   only posture unchanged.
3. **Workbench pane** below the transcript (collapsible): a small stateful
   text processor renders the stream — ANSI/CSI sequences consumed (params
   0x30–0x3F incl. `?` bracketed-paste markers; final byte 0x40–0x7E),
   `\r` rewrites from line start (prompts/progress), `\b` and tabs
   handled. Keystrokes map from real keydowns (Enter → \r, Backspace →
   DEL, arrows → CSI, ^C/^D/^L) and pastes. **Keystroke POSTs are
   serialized client-side** (one in flight): parallel fetches race across
   connections and scramble input — "git status" once arrived as "gits".
4. **Honest scope:** this is the dogfood loop (ls, git status, cat, echo,
   ^C), not a full emulator — curses/alt-screen apps are out of scope
   (grok's ratatui pager remains the full-screen TUI surface). Reload
   reattaches to the same PTY via `GET /api/term`; sessions persist until
   `/close` (exit keeps the entry for final output until closed).

## Why

The terminal was the last surface the dogfood week would miss: without it
there is no way to run build/test commands next to the task, and every
reference product (ZCode included) treats a terminal pane as core
workbench furniture. The daemon already had the whole domain — only the
wire seam and the pane were missing.

## Evidence

- `g4_terminals_run_a_real_pty_over_http`: open → keys to unknown id 404
  → subscribe → `echo okra-term-marker` typed over HTTP → the marker
  streams back through the real PTY → resize accepted → close prunes the
  registry. Full `scripts/ci.sh` all gates green (clippy −D warnings).
- Hands-on browser drive (2026-09-28): pane expands, reattaches/opens
  `t1`, the real zsh prompt streams, `echo okra-term-drive && git status`
  typed through the actual keydown handler echoes cleanly and executes
  (output + restored prompt; clean CSI stripping after fixing the `[`
  introducer and the `?2004h` private-marker leak; input ordering fixed
  by serialized sends). Screenshot in the session record.
