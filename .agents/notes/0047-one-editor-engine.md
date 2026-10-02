# N0047 — one editor engine for every composer (#54), ProseMirror+Yjs still rejected

- **Status:** implemented
- **Decided:** 2026-10-01
- Closes the composer half of MASTER-PLAN §3 #54 ("One editor engine
  across all composers; virtualized transcript w/ layout checkpoints").
  The transcript half has been live since N0011 (custom windowed renderer
  with layout checkpoints).

## Decision

1. **ProseMirror+Yjs stays REJECTED for this implementation** — the
   workbench is dependency-free vanilla JS by decision N0008 (no build
   step, no node_modules); a rich-editor framework contradicts that
   standing decision and buys nothing for a plain-text task composer.
   REVISIT TRIGGER: the TS React UI reuse (same trigger N0008 records).
2. **"One editor engine" is satisfied literally**: `createEditor(surface,
   opts)` in `ui/app.js` wraps EVERY text composer — the task composer
   and the question-card answer input — so all surfaces share ONE
   contract:
   - undo/redo stacks per surface (Ctrl/Cmd+Z, Ctrl/Cmd+Shift+Z /
     Ctrl+Y) that survive PROGRAMMATIC value sets — native textarea undo
     breaks the moment code assigns `.value`; the engine's stacks do not;
   - Enter submits, Shift+Enter newlines (composer), Enter submits
     (question card);
   - autosize for textareas;
   - a single `onChange` seam (send-button state, @-mention refresh).
3. **Mentions keep priority**: the @-mention key handling stays ahead of
   the engine's submit semantics (Enter picks the completion, it does not
   send). The engine handles what mentions do not consume.
4. **Testable seam**: the composer engine instance is exposed as
   `window.__okraComposer` for harness drives.

## Evidence

- `ui/app.js`: `createEditor` + both call sites (task composer wiring in
  `init()`, question card in `renderQuestionCard`); the old per-surface
  keydown/input duplication is gone.
- `node --check ui/app.js` clean; the served workbench embeds the file
  at compile time, so a rebuild ships it.
