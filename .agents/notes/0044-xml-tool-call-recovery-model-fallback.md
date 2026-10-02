# N0044 — XML tool-call recovery and model-fallback events (#37)

- **Status:** implemented
- **Decided:** 2026-10-01
- Closes: MASTER-PLAN §3 #37 ("Model-fallback events, XML tool-call
  recovery, output clamping" — qwen `turn.ts:747-761`,
  `xml-tool-call-fallback`).

## Decision

1. **XML tool-call recovery (`providers::recovery`).** Some models answer
   with the call embedded in TEXT instead of the structured field. After
   every sample, when the response has NO structured calls but the text
   contains `<tool_call`, the loop recovers them:
   - `<tool_call>{"name":…,"arguments":{…}}</tool_call>` (also
     `parameters`/`args` spellings) and the attributed form
     `<tool_call name="x">{…}</tool_call>`;
   - recovered calls are REAL calls — they execute through the same
     normalize → hooks → lattice → grants → approval pipeline as any
     sampled call, with ids `recovered-N`. A recovered write still ASKS;
     recovery never bypasses policy;
   - recovered blocks are stripped from the model-visible text (the model
     does not see stale blocks next turn); a MALFORMED block stays in the
     text verbatim (the model can see its own mistake) and produces
     nothing;
   - the receipt is honest: `LoopEvent::ToolCallsRecovered` + a log-only
     kernel event `assistant/tool_calls_recovered` (names + count).
2. **Model fallback with recorded switches (`providers::fallback`).**
   `FallbackSampler` walks a candidate chain (primary first):
   - fallback-able: `Unauthorized` (credential does not cover the model)
     and `RateLimited` (the model's quota, not the task's fault);
   - NOT fallback-able: `Transient` (the retry budget owns it),
     `ContextLength` (a compaction concern — switching would hide it),
     `Permanent`, and the LAST candidate (its error passes through);
   - every switch is a `FallbackEvent { from, to, reason }`: drained via
     the `Sampler::drain_fallback_events` trait seam (default: none), the
     turn loop emits `LoopEvent::ModelFallback` AND a log-only kernel
     event `model/fallback` — a model change is NEVER silent;
   - an empty candidate list fails closed (`Permanent`).
3. **Output clamping** stays where it already lives (output byte/token
   budgets + spill in `tools`, `truncate_at` fault injection in the
   scripted stub); nothing new here — the donor's clamping concern is
   covered by the existing budget plane.

## Non-goal (this pass)

The workbench does not yet render `model/fallback` /
`assistant/tool_calls_recovered` as transcript rows; both are in the
kernel log (durable, replay-safe) and stream as `LoopEvent`s to NDJSON
surfaces. Row rendering can follow the next UI pass.

## Evidence

- `crates/providers/src/recovery.rs` (6 unit tests: JSON body, attributed
  form, ordering, malformed-stays, unterminated, plain text untouched).
- `crates/providers/src/fallback.rs` (6 unit tests: 401 falls back + one
  event, transient never falls back, last-candidate passes through, empty
  chain fails closed, chain walks a→b→c with two events, response shape
  intact).
- `crates/agent-core/tests/agent_loop.rs`: `xml_tool_calls_in_text…` —
  the recovered `write_file` REALLY executes (file lands on disk) through
  the approval seam; `model_fallback_switches_are_surfaced_and_logged` —
  the switch surfaces as `LoopEvent::ModelFallback`.
- `cargo test -p okra-providers` 22/22; agent_loop 14/14.
