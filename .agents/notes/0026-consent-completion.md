# N0026 — the consent model completed: session lock, clipboard grants, batches

- **Status:** implemented
- **Decided:** 2026-09-30
- Blocks: MASTER-PLAN #60 (Claude2 docs 04–05 deferred items from N0025).
- Builds on: N0025's consent ledger + families.

## Decision

1. **Session driving-lock** (Claude's rule verbatim): only ONE session
   drives the computer at a time. Every computer tool acquires the lock
   (error: "Another okra session is currently using the computer. Press
   stop in that session…"); `run_turn_streaming`'s finalize releases it —
   the computer frees when the driving turn ends, including via approval
   pauses. `GET /api/computer/consent` now reports `driving`.
2. **Clipboard grants**: `computer_request_access` gained the
   `clipboardRead`/`clipboardWrite` checkboxes (separate consent kinds —
   the split-consent rule); `computer_read_clipboard` /
   `computer_write_clipboard` tools are grant-gated with honest
   request-more-grants errors.
3. **Batch families** (Claude's round-trip rationale): `computer_batch`
   (display-scope actions: clicks/type/key/scroll/mouse_move/drag/shot —
   sequential, stop-on-first-error, takeover-gated) and
   `computer_app_batch` (app actions on ONE granted app: click/type/
   focus/key/ax_find, one observe per action keeping element ids fresh).

## Test lessons (recorded)

- The daemon broadcasts frames to ALL subscribers; only the browser
  filters by sessionId — test collectors MUST scope predicates or turns
  from other sessions satisfy the waits (cost a full debug cycle).
- The 8 s MCP probe bound flakes under parallel test load (shell
  fixtures + many daemons); raised to 20 s — bounded still, generous
  under load.

## Why

N0025's deferred list named these: the lock is safety-critical (two
agents driving one screen is chaos), clipboard needs its own consent
kind (Claude ships separate checkboxes for a reason), and batch is the
round-trip economics lesson every reference product converged on.

## Evidence

- `g4_consent_completion_lock_clipboard_batches`: grant → A's mixed turn
  (executed app tool acquires, then a consent card pauses it mid-flight)
  → B's app tool hits the lock error → A resolves + completes → B works
  again; the fullclick batch reaches the cliclick fixture.
- Live workbench drive: grant card → fullclick (takeover card) →
  `cliclick c:30,40` in the fixture log; consent shows granted +
  fullControl with driving freed. `scripts/ci.sh` all gates green twice.
