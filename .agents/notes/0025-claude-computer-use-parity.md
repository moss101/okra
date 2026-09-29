# N0025 — Claude Desktop computer-use parity: the consent model + tool families

- **Status:** implemented (macOS; hermetic fixtures)
- **Decided:** 2026-09-30
- Blocks: MASTER-PLAN #60 (Claude2 docs 02–05); closes the parity gap
  named by the user ("okra is lacking parity with Claude Desktop").
- Builds on: N0023's AX executor; the M1 ConsentLedger contracts; the
  approval bridge (a card IS a dialog).

## Decision

1. **The consent model is now Claude's.** `computer_request_access
   {apps[], reason}` — ONE approval card listing the app set — grants
   those apps for the session; `computer_request_full_control` (one card)
   holds screen takeover; `computer_release_access` /
   `computer_release_full_control` drop them. Grants live in a
   daemon-level `ComputerConsent` (per-app set + takeover flag) shared by
   every turn, surfaced at `GET /api/computer/consent` and in a Tools-tab
   Computer section. The M1 `ConsentLedger` split (app capability ≠
   screen takeover) is realized: one never implies the other.
2. **Display-scope family** (takeover-gated, coordinates in the last full
   screenshot's frame): `computer_shot` (+save_to_disk),
   `computer_zoom` (region re-capture at full density),
   left/double/right click, `computer_type`, `computer_key` (combos:
   cmd+a via AX keystroke with modifiers), `computer_scroll`,
   `computer_mouse_move`, `computer_drag`, `computer_cursor_position`.
3. **Background `app_*` family** (per-app-grant-gated, no cards once
   granted, never raises windows): `computer_app_list_windows`,
   `computer_app_screenshot` (window region capture + AX element digest),
   `computer_app_ax_find` (role/title filter), `computer_app_click`
   (AXPress-structural or coordinate), `computer_app_focus` (AX focus
   without clicks), `computer_app_type` (element or `target:"focused"`),
   `computer_app_key`.
4. **Inventory + launch**: `computer_list_apps` (running first, then
   installed), `computer_open_application` (`open -a`, no force-front),
   clipboard read/write helpers in the backend (not yet exposed as tools —
   they need their grant checkboxes, deferred).
5. **Error vocabulary matches Claude's**: "no app capability grant for X
   — call computer_request_access…", "full-screen control not granted —
   call computer_request_full_control…". The N0023 trio stays as-is
   (per-call cards; migration deferred).

## Why

N0023 shipped observe/act/screenshot with per-call cards — Claude
Desktop's actual model is session grants with one dialog per consent
kind, a coordinate display family, and a background window family that
never steals focus. The study docs (Claude2/04, 05) pinned the exact
tool names, schemas, and consent split; this tranche ports the model.

## Evidence

- `g4_computer_consent_lifecycle`: ungranted app tool errors naming
  request_access; request_access card → grant; appwindows lists windows
  with NO second card; fullclick's takeover card → allow → the
  coordinate click reaches the cliclick fixture (`c:50,60` in the log);
  `/api/computer/consent` reflects both consents.
- Live workbench drive: grant Finder (card) → appwindows (no card) →
  fullclick 120,240 (takeover card) → `cliclick c:120,240` in the
  backend log; the Tools tab Computer section showed both consents
  (screenshot). `scripts/ci.sh` all gates green twice.
