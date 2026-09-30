# day 03 — top friction findings

Compiled from journal/day-00.md, journal/2026-09-28-day01.md, journal/2026-09-29-day02.md.
Ranked by severity/impact; status reflects where things stand as of day 02.

## 1. No usable model credentials — real-model dogfood fully blocked
- **Severity: high (open)**
- **What:** Every cloud route expired: opencode gateway recipe lost; coding-plan
  api-keys (individual + team) and oauth token all return 401 on
  api.z.ai / zcode.z.ai / bigmodel, in both openai and anthropic formats. No local
  fallback (no Ollama, no LM Studio installed).
- **Impact:** G3-day dogfooding cannot exercise a real model at all; everything past
  day 02 runs offline/through harness plumbing only.
- **Evidence:** journal/2026-09-29-day02.md — "credential recovery EXHAUSTED 2026-09-29".
- **Need:** user-supplied openai-compatible base URL + key, or a local runner.

## 2. No `--version` flag in okra
- **Severity: medium (fixed in-flight)**
- **What:** The journal's day-00 format requires "provider/model + version" per entry,
  but the CLI had no way to report its version. Entries logged "version: okra" — a
  literal, not a version.
- **Impact:** Dogfood log couldn't be falsified against builds; the gap was only
  found by probing, not surfaced by the tool.
- **Evidence:** journal/2026-09-28-day01.md session-3 — "--version flag added to okra
  after the probe gap; version line now real" (e.g. "okra 0.1.0").
- **Lesson:** version reporting should exist before the log format demands it.

## 3. Label drift silently misfiles journal entries
- **Severity: medium (fixed and verified)**
- **What:** `scripts/dogfood-log.sh` took a label to pick the target .md file; a
  typo'd or drifted label could quietly append an entry to the wrong date file
  without any error.
- **Impact:** Journal integrity — entries erred into the wrong file instead of the
  dated one; exactly the failure day 01 predicted ("none yet; the likely one is a
  typo'd label quietly landing the entry in the wrong .md file").
- **Evidence:** day01 prediction; 2026-09-29-day02.md — "harness label-drift fix
  verified: entries now land in the dated file".
- **Related minor:** the missing-stream form test (`/nonexistent.ndjson`) showed
  downstream-path integrity is also just logged as "stream: missing" rather than
  validated loudly.

---
*Day 00 contributed no friction (setup only). Read each entry once for this synthesis.*

---

## Measurement context (N0021 governor, live)

Turn driven through the workbench (glm-5.3-flash via opencode zen gateway —
recipe RECOVERED: base URL from models.opencode.ai/api.json provider
`opencode-go` + key from opencode auth.json; requires `User-Agent:
opencode/1.0` and `x-opencode-session` headers, which okra's
`OKRA_EXTRA_HEADERS` supplies).

- Tool sequence: **9× identical `list_dir journal` → 3× `read_file` → 1×
  `write_file`** — the day-1 wander pattern, reproduced and now measurable.
- The N0021 wander governor judged at steps 4/7/10 live: progressing
  0.86 → 0.86 → 0.34, class `exploring` throughout → class-gate kept it
  quiet (Jev read the listing as legitimate investigation; the repetition
  signal appears in the decaying progress, not the class).
- Policy implication recorded for N0021 tuning: consider a decay rule
  (same-activity + falling progress across 2+ judgments → nudge) vs the
  current static class gate. Not changed yet — one session is not
  calibration data.

---

## Day 4 (2026-09-30) — real-model consent review + a live bug fixed

Task: "read day3-findings.md once, write day4-consent.md in your own
words." The model complied exactly (1 read, 1 approved write — governor
correctly silent: 2 steps < first_step 4). Its summary was accurate, and
it surfaced a REAL product risk worth tracking:

> grants are bundled and sticky — a single dialog approves a whole batch
> of apps at once with no per-app choice, and those grants persist all
> session. A user intending "just Finder" may have waved through the
> entire set, and declining screen takeover still leaves background app
> automation running — so "no" on one prompt is not "no" overall.

(This matches Claude Desktop's own dialog property — but the critique is
the dogfood signal we wanted: the model reads the consent model as a
user would.)

**Bug found live and fixed during the session:** reloading the page
during an approval pause LOST the card — the watchdog emitted only on
change, so a fresh subscriber never learned the pending approval and the
turn wedged. Fixed: pending approvals/questions now heartbeat (re-emit
every ~4 ticks while pending); `g4_late_subscriber_recovers_pending_approval`
pins the flow (subscribe mid-pause → learn the card → resolve → turn
completes).
