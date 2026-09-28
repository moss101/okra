# N0011 — the transcript is a windowed renderer with layout checkpoints

- **Status:** implemented
- **Decided:** 2026-09-28
- Blocks: MASTER-PLAN #54 (transcript with layout checkpoints), UI-SHELL-PLAN
  U1 ("a 2k-message streaming task stays at 60 fps")
- Builds on: N0008's transcript; the daemon's full-snapshot projection
  frames.

## Decision

`renderRows` used to destroy and rebuild every row on every SSE frame —
and frames arrive several times per second during a turn. O(transcript)
per frame, markdown re-rendered for every assistant row every time, and
tool-card expansion silently reset by each frame. Replaced with the
ChatGPT2 docs/07 architecture, dependency-free:

1. **Keyed reconciliation.** Rows are cached by stable identity (`rowId`;
   pending approvals by `approvalId`) with a per-kind mutation signature
   (state + text length for streaming assistant rows; status/output/path
   for tool rows; state for turn/approval rows). Unchanged rows reuse
   their DOM node — markdown is rendered exactly once per row. Tool-card
   expansion state lives in a `callId`-keyed set, so cards survive both
   frame churn and window rebuilds.
2. **Windowed rendering with layout checkpoints.** Only the viewport
   ± 900 px overscan exists in the DOM; top/bottom spacers position it in
   the virtual coordinate space. Positions come from a checkpoint map of
   REAL measured heights (rAF-batched, remembered per row); rows never
   rendered use per-kind estimates (assistant rows scale with text
   length). When checkpoints refine above the viewport, scroll position
   is compensated by the spacer delta — content never jumps.
3. **Triggers.** Frames reconcile; scrolling re-windows (rAF-throttled);
   the pinned-to-bottom follow still wins while streaming. The cache is
   bounded (≤1200 entries, far-from-window evicted — they rebuild if
   ever scrolled back).

Deferred: true measured-height virtualization without estimates relies on
the checkpoint pass converging (it does after first paint of a row); a
`ResizeObserver`-driven variant can replace the rAF pass if fonts/zoom
drift becomes visible.

## Why

Long dogfood tasks (100+ turns, file bodies in tool cards) made the old
path re-render megabytes of markdown per frame — the single biggest UI
correctness-and-performance debt left from N0008, and the plan's own U1
acceptance line.

## Evidence

- Hands-on drive (2026-09-28): a 127-row live transcript keeps a 24-node
  DOM window (spacers carry the rest); scrolling to the top re-windows to
  early rows (33 nodes); a tool card opened and scrolled far away and
  back stays open; reload → 125 replayed rows render as 26 nodes; a new
  turn at the bottom keeps the pinned follow. `g4_workbench_assets` now
  regression-guards the virtualizer markers (keyed reconciliation +
  layout checkpoints) and the script parses under `node --check`.
- `scripts/ci.sh` all gates green.
