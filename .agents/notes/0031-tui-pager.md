# N0031 — the TUI pager (ratatui block scrollback + `--minimal`)

- **Status:** implemented
- **Decided:** 2026-10-01
- Closes: MASTER-PLAN §3 #55 (M4): "TUI: ratatui, block scrollback,
  O(N) streaming markdown, `--minimal` scrollback mode" (grok pager
  analog; contract ported per N0004).

## Decision

1. **The model is pure and tested; the renderer only draws slices.**
   `okra_tui::pager::Scrollback` owns the semantics: block-structured
   lines (divider / user / assistant / tool / note), text deltas
   COALESCE into the last assistant line (O(N) streaming — the last
   line mutates, nothing is rescanned), tool cards render started →
   finished with first-line output.
2. **Scroll-lock:** pinned mode follows the live edge; scrolling up
   FREEZES the viewport's top index so streaming appends never move a
   scrolled view; scrolling back to the bottom (or `G`) re-pins.
3. **`okra tui [--cwd DIR] [--provider openai --model M]`**: ratatui
   over crossterm raw mode + alternate screen (a Drop-guard restores
   the terminal even on panic). The turn engine lives on a dedicated
   worker thread (one kernel session + continuation context chained
   across prompts — n0028 semantics); events cross as serialized
   LoopEvents into the model. Keys: Enter sends, PgUp/PgDn scroll,
   Ctrl-C stops the live turn (the same stop-flag seam as the web
   Stop button) and quits when idle.
4. **`--minimal`** on the one-shot CLI renders the same events as plain
   scrollback lines via `minimal_line` — the pager's no-alt-screen
   sibling (pipe-friendly).

## Honesty notes

- The TUI runs UNATTENDED (ApprovalPolicy::Never, UnattendedAllowed):
   there is no approval surface yet — the composer is the only
   interaction. Writes from TUI turns are not granted, they are
   ceiling-capped.
- Markdown is line-structured, not rendered prose (grok's O(N)
   streaming-markdown is the shape; inline styling lands with it).

## Evidence

- `crates/tui/src/pager.rs` tests: blocks accumulate (deltas coalesce,
  first-line tool output), viewport slices from the live edge with
  clamping, streaming while scrolled up does not jump.
- `okra --cwd ... --minimal "read notes.txt and summarize"` renders the
  scrollback form (divider, tool lines, text).
