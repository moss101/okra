# N0027 — per-app consent choice: one card per app (exceeding the reference)

- **Status:** implemented
- **Decided:** 2026-09-30
- Blocks: the day-4 dogfood critique — "grants are bundled and sticky: a
  single dialog approves a whole batch of apps at once with no per-app
  choice… 'no' on one prompt is not 'no' overall."
- Builds on: N0025/N0026 consent model; the approval bridge's heartbeat
  (day-4 fix).

## Decision

1. **`request_access` now raises ONE CARD PER APP.** The tool became
   read_only (the executor-level card is skipped) and drives its own
   consent: `SurfaceApprovalChannel` gained a `register`/`wait_for`
   split — ALL cards register first (the user sees the whole request at
   once), then each waits. Allow/deny is decided PER APP; the ledger
   gains only allowed apps; the tool result reports the split
   (`granted: [A] denied: [B]`).
2. **Clipboard grants** attach only when at least one app was allowed.
3. **Grant cards read as consent dialogs**: "Approve control of Finder"
   with the app + reason in the args.
4. This EXCEEDS the Claude reference (its dialog is whole-set
   allow/deny) — driven by our own dogfood signal.

## Why

The day-4 real-model session surfaced the risk in its own words; the
fix is the strictest reading of the split-consent rule: consent is
per-subject, so the dialog must be too.

## Evidence

- `g4_per_app_consent_partial_grant`: a two-app request surfaces BOTH
  cards (`apr-app-Finder`, `apr-app-TextEdit`) registered together;
  deny/allow split → the result reports `granted: [TextEdit] denied:
  [Finder]`; the ledger holds ONLY TextEdit.
- Live workbench drive (real model): two "Approve control of …" cards;
  Finder denied, TextEdit allowed → ledger `[TextEdit]`, result text
  reports the split with the model's own reason line.
  `scripts/ci.sh` all gates green twice.
