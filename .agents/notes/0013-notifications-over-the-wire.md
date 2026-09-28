# N0013 — the 3-class notification boundary is live over the wire

- **Status:** implemented
- **Decided:** 2026-09-28
- Blocks: MASTER-PLAN #51 (notifications: 3 native classes + body
  redaction + tray unread model), UI-SHELL-PLAN U2 (notifications rebuild)
- Builds on: the notifications policy domain (classify/redact/decide —
  unit-tested since the domain landed); N0008's toasts; the task sidebar.

## Decision

1. **The daemon classifies and redacts; the surface decides delivery.**
   `run_turn_streaming` emits a `v4/notification` frame on every turn end
   (`turn_complete`; stopped/failed carry their wording), and the
   approval watchdog emits `permission_request` once per new ask. Labels
   go through the domain's `classify`/`redact_body` — content (the
   prompt, args, tool output) never leaves the process. The `question`
   class stays reserved (no question flow exists yet).
2. **Surface policy = the domain's decide(), client-side.** Focus is a
   surface concept: focused → in-app toast only; unfocused → Web
   Notification API (when granted) with the REDACTED label as the body —
   never the richer local task title, which is app-internal — plus the
   in-app toast. `tag` is the session id (one notification per task);
   clicking focuses and selects the task.
3. **Tray unread model:** notifications for background tasks set an
   unread dot on the sidebar entry; selecting the task clears it. Focus
   suppression falls out naturally (background = dot, not noise).

## Why

The policy domain existed and was unit-tested, but nothing emitted over
the wire and no surface consumed it — the workbench only had ad-hoc
toasts with no classification and no redaction contract. This closes the
donor's lock-screen-leak class of bugs for the workbench surface: native
bodies are structurally limited to redacted daemon labels.

## Evidence

- `g4_notifications_classify_and_redact_over_the_wire`: a completed turn
  produces a `v4/notification` frame with class `turn_complete`, the
  right session, and a label that provably does NOT contain the prompt
  (redaction leak assertion) and stays bounded.
- The approvals test now also asserts a `permission_request` notification
  fires for every ask.
- Hands-on browser drive (2026-09-28): real turn → "Task finished" toast
  with the local title; `Notification.permission` granted and a native
  notification constructs; a background-task notification shows the
  sidebar unread dot and selecting the task clears it. Screenshot in the
  session record. `scripts/ci.sh` all gates green.
