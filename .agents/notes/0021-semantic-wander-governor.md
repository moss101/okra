# N0021 — the semantic wander governor: TypeSafe Jev in the turn loop

- **Status:** implemented
- **Decided:** 2026-09-29
- Blocks: MASTER-PLAN #11-adjacent (governors), the dogfood day-1 finding
  #3 ("exploration wander — a nudge governor is the lever"), G3's
  measure-again-on-day-2 note.
- Builds on: the stationarity nudge path (grok), the hooks-containment
  rule (a semantic signal never breaks a turn), N0018's env-credential
  posture.

## Decision

1. **`okra_providers::jev`** — a narrow TypeSafe System One client
   (`POST api.typesafe.ai/v1/systemone`, model `jev-latest`, key from
   `TYPESAFE_API_KEY`, 20 s timeout, fail-open). Typed answers decode to
   noul/choice/score values.
2. **`WanderGovernor`** in agent-core (`semantics.rs`): at steps 4, 7, 10…
   (first_step 4, every_n 3, max 6 judgments/turn — bounded cost), the
   loop shows Jev the last 6 tool calls and the latest user instruction
   and asks TWO independent questions: `progressing` (noul) and
   `activity` (choice: exploring/repeating/offtrack/delivering). The
   verdict rides the EXISTING nudge path (`LoopEvent::Nudge` + a user
   message) — no new loop machinery.
3. **Calibration (live, on a clean pair):** the CLASS signal is sharp —
   `repeating` conf 0.79 on a circular transcript, `delivering` conf 1.0
   on a productive one — but `progressing` reads generously (0.69 on the
   clear circle). Policy recalibrated accordingly: **class is the gate**
   (repeating/offtrack → nudge); progress is reported in the reminder,
   not required. `progress_threshold` stays in the config for future
   tuning on real dogfood data.
4. **Activation:** `TYPESAFE_API_KEY` present → active; absent → inert
   (zero cost, zero calls); `OKRA_SEMANTIC_WATCH=off` disables
   explicitly. Fail-open everywhere: judge errors/ambiguity → no nudge.
   The offline demo planner finishes before first_step, so offline runs
   are never judged (verified: no false nudge).

## Why

The loop cannot distinguish "going in circles" from "legitimate
investigation" with string matching — the calls differ. That is exactly
semantic understanding, which is what Jev is for: code owns the
governor, thresholds, and nudge; the model supplies the programmable
common sense. It also dogfoods the user's TypeSafe stack inside the
agent harness.

## Evidence

- Unit: governor policy table (threshold/class/fail-open/inert/bounded)
  4/4; `jev` decode roundtrip + live smoke (skipped without the key).
- Turn-level (`agent_loop.rs`): a 7-read circling turn gets exactly one
  `Nudge{reason:"semantic wander check"}`; a healthy turn gets none.
- LIVE: wandering transcript → `repeating` (conf 0.79), productive →
  `delivering` (conf 1.0); an offline CLI turn with the key set
  completes with no false nudge. `scripts/ci.sh` all gates green.
