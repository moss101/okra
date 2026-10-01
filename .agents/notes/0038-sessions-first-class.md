# N0038 — sessions as a first-class surface (the sessions page)

- **Status:** implemented
- **Decided:** 2026-10-01
- Closes: MASTER-PLAN §3 #59 (M4): "IDE sessions as first-class surface
  (sessions page, not a side panel); agent handoff editor↔daemon"
  (Devin recon design donor — its 42 MB sessions bundle made sessions a
  peer of the editor).

## Decision

The workbench grows a full-canvas **Sessions** view (topbar button;
`Esc`/Close returns to the transcript): a searchable card grid over the
existing `/api/sessions` index — title, live/status dot, event count,
session id — clicking a card opens that session's transcript. The
periodic refresh keeps an open page live. The tasks sidebar stays (it is
the quick switcher); the page is the "sessions are the product" surface.

## Evidence

- `apps/okra/tests/g4_sessions_page.rs`: the served workbench ships the
  page markup (`sessions-page`, `sessions-view-btn`, `sessions-search`,
  `sessions-grid`), the renderer + toggle + `/api/sessions` feed in
  app.js, and the page styles in app.css.
- `/api/sessions` itself is covered by the pre-existing
  `g4_sessions_index_and_replay`.

## Because

Every reference product buries sessions behind a chat list; the Devin
study's takeaway is that the session IS the unit of work. A full-page,
searchable surface is the smallest version of that which is still true
to the idea.
