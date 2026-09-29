# N0023 — computer control is real: the macOS AX executor behind the M1 contracts

- **Status:** implemented (macOS; hermetic fixtures for CI)
- **Decided:** 2026-09-29
- Blocks: MASTER-PLAN #60 (Claude2 study: AX-first, split consent, batch
  families, pixel guard, no-raise, user_actively_typing) — the M1
  contracts were pinned; this is the executor that makes them real.
- Builds on: okra-computer's M1 contracts; the approval bridge; the
  serve tool registration pattern.

## Decision

1. **`backend.rs` = the computer crate's second sanctioned spawn site**
   (mirrors okra-tools/process.rs; clippy bans stay). Binaries are
   env-overridable (`OKRA_OSASCRIPT`, `OKRA_CLICKER`,
   `OKRA_SCREENCAPTURE`) so acceptance tests run hermetic fixture
   scripts while production uses osascript/cliclick/screencapture.
2. **AX-first observe**: System Events appleScript over an app's windows
   and interactive elements → element ids (`w0/e3`), roles, labels,
   positions, reported actions. Permission denials (Accessibility) are
   TYPED errors quoting the fix — never silent empty trees.
3. **Click path**: AXPress structurally when the element reports the
   action (argv indices, no coordinates); coordinate click at the AX
   tree's center via the clicker otherwise — coordinates come from the
   AX tree, never pixel-guessed (the pixel guard's spirit).
4. **`execute_real`** realizes the M1 batch contract: observe → resolve
   against the fresh tree → act → re-observe, stop-on-first-error,
   user_actively_typing guard on input injection.
5. **Serve tools (approval = consent).** `computer_observe`,
   `computer_act`, `computer_screenshot` — all with read_only=FALSE:
   observing the user's screen is a privacy side-effect, and the
   approval card IS the split consent (app capability per act, screen
   takeover for the screenshot). Screenshots return PNG data URLs the
   workbench renders inside tool cards.
6. **Offline-drivable acceptance**: the demo planner gained `computer
   observe|act|screenshot` branches; fixture binaries run hermetically.

## Bugs the acceptance test caught (each a real lesson)

- **serde enum field renames**: `#[serde(rename_all)]` on an internally
  tagged ENUM renames VARIANTS, not struct-variant FIELDS — the wire key
  is `element_id`, not `elementId`. Symptom: actions deserialized EMPTY
  (`actions=0`) with no error.
- **planner arg shape**: the element was parsed from the verb position
  ("click"), landing "click" as the element id.
- **read_only semantics**: marking observe/screenshot read_only silently
  skipped their consent (the executor's read-only fast path) — privacy
  side-effects must prompt.

## Why

Computer control is the Claude Desktop flagship and the plan's #60
differentiator ("the polish no reference product gets right"). The
contracts existed since M1; without an executor the workbench could not
see or touch the user's machine at all.

## Evidence

- `g4_computer_control_end_to_end` (fixture binaries): observe returns
  the canned AX tree; act's AXPress reaches the fixture osascript (log
  proof); screenshot returns a PNG data URL; all three approval-gated.
- `backend` unit tests: tree parsing, typed permission denial,
  re-observe error, batch consent/stop/typing guard (existing).
- Live workbench drive: approval cards for observe/act/screenshot.
  `scripts/ci.sh` all gates green, twice.
