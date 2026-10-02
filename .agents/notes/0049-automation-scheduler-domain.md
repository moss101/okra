# N0049 — the automation scheduler domain (cron_* tools + a firing loop)

- **Status:** implemented
- **Decided:** 2026-10-02
- Closes the last missing M3 domain: MASTER-PLAN §3 #39's `scheduled`
  task kind had its runtime plumbing (`TaskKind::Scheduled`,
  `TurnDispatch::CronScheduled`) and its self-mutation guard (#40, M1)
  since the start — but nothing ever FIRED a scheduled turn, and the
  guarded `cron_*` tools did not exist. The guard denied names no tool
  carried.

## Decision

1. **Durable store (`host::automation`)**: `.okra/automation.json`,
   rewritten atomically (temp + rename — no torn table, unlike a JSONL
   log there is no append case to protect). `AutomationSpec { id, name,
   prompt, session_id, every_secs | at_hhmm, enabled, last_fired,
   fire_count }`. Schedule honesty: interval floor 10s, HH:MM range
   checks, EXACTLY ONE schedule kind, HH:MM is explicitly UTC (std has
   no tz database; an honest UTC schedule beats a wrong local one).
   `tick(now)` advances and returns due specs idempotently (a slow
   caller cannot double-fire inside an interval).
2. **Firing goes through the normal sendText path**: the daemon's 1s
   scheduler thread calls the same `command_accept` a surface uses — one
   turn gate, one projection, one approval bridge, continuations,
   checkpoints. A fired turn carries the envelope marker
   `payload.automation = "cron"` and runs with
   `TurnDispatch::CronScheduled`, so the EXISTING tool-plane guard
   denies `cron_*` inside it: **automation may not reschedule itself**
   (#40, now enforced on real turns, not just unit-level).
3. **The cron_* tools exist and are approval-gated**: `cron_create`
   (content-derived stable id, dedicated `auto-<id>` session),
   `cron_list` (read-only), `cron_delete`. Ordinary turns may create
   schedules; cron-fired and idle turns may not (the M1 guard, finally
   pointed at real tools).
4. **Fail-closed approvals apply to automations**: a scheduled write
   PAUSES on the approval bridge exactly like any write. There is no
   auto-approve path for cron turns — an unattended deployment is a
   ceiling decision (`UnattendedAllowed`), not a scheduler feature.

## Evidence

- `crates/host/src/automation.rs` (6 tests: interval math, tick fire +
  record + no double-fire, disabled/deleted never fire, validation
  matrix, daily UTC occurrence, store survives reopen).
- Live smoke: a seeded 10s spec fired into `auto-smoke1` (log line +
  `fireCount: 1` persisted); the fired write PAUSED on approval;
  resolving (with a n0043 conversation scope) let the turn complete and
  the file land.
- `cargo test -p okra-host --lib automation` 6/6.
