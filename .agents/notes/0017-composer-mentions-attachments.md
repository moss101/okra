# N0017 — the composer mentions and attaches: @files and content folding

- **Status:** implemented
- **Decided:** 2026-09-28
- Blocks: MASTER-PLAN #52 (conversation-scoped attachments analog),
  UI-SHELL-PLAN U1 (composer: @-mention lexer + attachment previews)
- Builds on: GET /api/files confinement rules, the safe-read pattern,
  N0008's composer, N0011's keyed transcript (chips render on cached rows).

## Decision

1. **@-mentions.** An `@token` at the caret queries
   `GET /api/files/search?q=` — a daemon-side recursive walk with the same
   confinement and dot-entry rules as the tree (symlinks never followed,
   50-result cap). Arrow/Enter/click completes the path into the composer
   AND adds it as an attachment (a mentioned file rides the turn).
2. **Attachments fold before anything surfaces.** `run_turn_streaming`
   folds each confined file's content into the user message as a fenced
   block (16 KB/file, 48 KB turn budget; missing/unreadable ones are
   reported in-band as `[attachments not loaded: …]`). Model-visible
   means logged: the fold happens BEFORE the user row and kernel event,
   so the log stays the transcript of record — replay derives the
   attachment list from the logged `[Attached file: X]` markers, and
   live rows carry the explicit list for the chips.
3. **Refusals are pre-surface.** Traversal (`..`) and option-looking
   pathspecs in the payload are rejected at the command gate
   (`okra.attachment.pathRefused`) — nothing leaks into a turn.
4. **Known limitation (recorded):** a command landing on a LIVE turn is
   steering, and the steering queue carries TEXT only — attachments
   belong to idle-turn sends. Widening the queue to (text, attachments)
   tuples is the follow-up if dogfood needs it.

## Follow-up — implemented same day

The limitation turned out to be worse than recorded: mid-turn steered
text was COSMETIC — the serve-level queue rendered `[steered]` rows but
never fed the model (the loop's own steering inbox was never fed; the
event sink is buffered until turn end, so draining it in the sink was
too late). Fixed properly:

- The steering queue carries `SteeredInput { text, attachments }`.
- A forwarder thread (independent of the event sink) moves each entry
  into the AGENT's steering inbox with attachments folded into the text —
  the loop then injects it at the next step boundary, logged as
  `user/message origin=steering` (its own governors + log machinery).
- `SteeringInjected` events render the receipt row (with the attachment
  chips); the turn thread's worklist runs each queued entry as its OWN
  turn so per-entry attachments hold.
- `g4_steered_sends_carry_attachments`: a steered send with an attachment
  produces the receipt row with the attachment list mid-turn.

## Why

References were the last composer gap: getting a file's content into a
turn required hoping the model would find and read it. Attachments make
the intent explicit, bounded, and logged; mentions make them one
keystroke away.

## Evidence

- `g4_attachments_fold_into_the_turn`: traversal refused with
  `okra.attachment.pathRefused`; a real attachment folds its content into
  the model-visible turn (marker string asserted in the replayed log);
  the row carries `"attachments":["notes.md"]`; a missing attachment is
  reported in-band, not fatal.
- Hands-on browser drive (2026-09-28): typing `@note` opened the popup
  with `notes.md`, accepting it inserted the path and raised the chip;
  sending folded the content into the user bubble (visible with its
  notes.md chip) and the assistant answered from it. Screenshot in the
  session record. Full `scripts/ci.sh` all gates green — twice
  consecutively (an initial flake in the new test's turn-gate race was
  fixed by waiting for turn 1, and the limitation recorded above).
