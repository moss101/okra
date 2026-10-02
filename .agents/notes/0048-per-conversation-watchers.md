# N0048 — per-conversation watchers over the checkpoint log (M3 #52 tail)

- **Status:** implemented
- **Decided:** 2026-10-01
- Closes the "per-conversation watchers" line of MASTER-PLAN §3 #52
  (file management): what files did THIS conversation touch, and what is
  their live state?

## Decision

1. **No extra watcher state exists to drift.** The durable source is the
   checkpoint manager's per-prompt write records (N0029) — every
   conversation's writes are ALREADY recorded (before/after sha256
   snapshots, mirrored to `.okra/checkpoints.jsonl`). The watchers
   surface is a read model over that log:
   `GET /api/watchers?session=<id>` unions every path the session's
   turns recorded (last `after` snapshot per path) and reports the LIVE
   state per file: `clean` (on-disk sha256 == recorded), `changed`
   (differs), `deleted` (gone). `createdByConversation` marks files the
   conversation created (no `before` snapshot).
2. **Turns are per-session**: the manager is daemon-wide and prompt-
   indexed, so the surface reads turns `0..turns_seen` for the requested
   session id (`state.session_turns`). An unknown session answers
   honestly with zero turns and no files.
3. Poll-free UI wiring can follow; the endpoint is the domain.

## Evidence

- `crates/host/src/checkpoints.rs` (records, unchanged);
  `apps/okra/src/serve.rs` `watchers_state`;
  `apps/okra/src/serve_tcp.rs` `GET /api/watchers`.
- Live smoke: a `create notes.md` turn (approval resolved over the wire
  with the n0043 scoped `resolveApproval`) → `{"files":[{"path":
  "notes.md","state":"clean",…}]}`; an external edit → `"state":
  "changed"`; an unknown session → `{"turns":0,"files":[]}`.
