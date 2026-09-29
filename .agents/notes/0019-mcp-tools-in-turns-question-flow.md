# N0019/N0020 — MCP tools run inside turns; the task can ask the user

- **Status:** implemented
- **Decided:** 2026-09-29
- Blocks: MASTER-PLAN #47 (MCP client + use_tool funnel → serve turns),
  #51 (the question notification class), #53-adjacent (interaction UX).
- Builds on: N0018's bounded probe; the approval bridge (N0009); the
  question class reserved in the notifications domain since its port.

## Decision

### N0019 — MCP tools inside turns

1. **Registration per turn.** `register_mcp_tools` probes every enabled
   stdio server (initialize + tools/list, 8 s bound — the N0018
   contract) and registers each tool as `mcp__<server>__<tool>` with the
   server's input schema. Unknown side-effectors: `ResourceAccess::All`
   + read_only=false, so **the approval card fires before any call
   executes** — an MCP server never runs unattended.
2. **One-shot calls.** Each execution is a fresh `tools/call` through
   the sanctioned runner (the transport's documented scope;
   persistent-session servers land with the gateway at M4).
3. **Offline-drivable acceptance.** The demo planner gained an `mcp
   <tool> [text]` branch (looks the tool up in the sampler's tool
   views), so the full flow — command → approval card → one-shot
   tools/call → result in the transcript — is testable without a
   network model.

### N0020 — the question flow (ask_user)

1. **`ask_user` tool** (attended surfaces only; an unattended bridge
   would block forever): blocks on `SurfaceQuestionChannel` (the
   approval-bridge pattern) until a surface answers or the stop flag
   flips (→ a cancelled marker returns to the model as the tool result).
   read_only=true — the question card IS the interaction; no approval on
   top of it.
2. **The seam.** `answerQuestion {questionId, answer}` resolves the
   ask; a watchdog surfaces `control.awaitingQuestion` + the
   **question-class notification** (the last reserved class, now live)
   and flips the phase to `awaitingQuestion`; the ask/answer pair is
   durable (tool/call + tool/result events), so replay shows the
   question and the answer.
3. **UI.** A question card (info-tinted) with the question and an
   answer input; submit posts the command. The virtualizer routes
   `__pending` question rows to the question builder (found live: the
   pending-router only knew approval cards).

## Why

Both close the loop the Tools tab opened: probing told you a server
exists; now the task can actually use it, under the same approval
contract as native tools. And a task that cannot ask is a task that
guesses — the question class was reserved in the domain since its port
with nothing to fire it.

## Evidence

- `g4_mcp_tools_run_inside_turns` (real JSON-RPC/stdio fixture answering
  initialize/tools/list/tools/call): approval-gated call, result
  (`echo: hello there`) in the transcript and replay. Live drive:
  approval card for `mcp__fake_probe-tool` → allow → `echo: live drive`.
- `g4_ask_user_question_flow`: question surfaces (control +
  `question`-class notification), answer returns INTO the turn
  (replayed tool result contains the answer), unknown ids rejected.
  Live drive: card + "waiting for your answer" chip → answered through
  the real input → turn completes with the answer as the tool result.
- `scripts/ci.sh` all gates green (twice consecutively).
