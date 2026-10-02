# okra — architecture & block provenance

One Rust daemon (agent + host services) that any surface drives over one
typed protocol, with kernel-enforced safety and an event-sourced session log
as the only durable truth. Realized from
`../work/20260925-nextgen-workbench/MASTER-PLAN.md` (the "workbench" plan;
name decision N0002).

## The two movements

1. **Port** — ZCode's *contracts* (protocol, state machines, service map),
   not its agent code.
2. **Assemble** — the best block from each donor, each with a named source
   location and destination crate, re-implemented behind cited contracts
   (decision N0004) until wholesale vendoring pays for itself (M1 gate).

## Crate tree and block map

| Crate | Blocks (MASTER-PLAN §3 #) | Donor source (verified paths) | Status |
|---|---|---|---|
| `protocol` | #1 wire/snapshot/delta, payload caps, `continuous`/`replayable` profiles · #2 binding-generation seam · #3 byte-convergence golden tests (recreated) | ZCode `packages/shared/src/zcode-protocol-v4/` (core/wire/wire-codec/wire-binary/coalesce/rows/delta/profiles) | **M0 done** — golden vectors generated from the TS originals (`tests/golden/`); workflowRun ops deferred to M3 (N0003) |
| `kernel` | #7 **SQLite projections** (sessions + task index, rebuildable from the log — M3 strangler first domain) · #4 event-log contract · #5 JSONL adapter + loss contract + `.corrupt` quarantine · #6 single-writer `SessionHandle` + torn-tail repair · interrupted-turn **repair** (deepseek repair.ts: outcome-unknown closers) · #7 SQLite projections | deepseek `packages/core/session/src/{types,surface,invariant,repair}.ts`, `session-persistence*`, grok JSONL storage | **M1/M3 started** (14 tests + sqlite projection tests) |
| `tools` | #45 **hooks**: 20-event set (kimi names), command/HTTP kinds, deny>ask>allow precedence, prompt-gate routing, containment (never crash a turn) · #47 **MCP client**: JSON-RPC stdio + in-process transports, `use_tool` single dispatch funnel, deferred schemas via `tool_directory`; #16 LIVE (n0045): `tool_search` — the full-registry discovery directory, schemas on demand | kimi `externalHooks/types.ts:3-24`, grok `xai-grok-mcp` + qwen deferral | **M2 done** — hook verdict/gate tests, MCP roundtrip + funnel + real stdio tests; benchmark: 100 mcp_calls, 1600 hook events, 0 failures |
| `tools` | #13 stream shape `[Progress*, one Terminal]` · #14 ToolEntry metadata + normalize-before-hooks (approved bytes = executed bytes) · #15 `ToolAccesses` conflict scheduler · #17 output budgets + spill · #18 `ToolSpec.idempotent` (clean-room) · write_file / edit_file builtins with atomic temp+fsync+rename writes | grok `xai-tool-runtime/src/tool.rs`, ZCode `tool/types.ts:64-91` + `call-runner.ts`, kimi `toolContract.ts` + `toolScheduler.ts`, deepseek `packages/spill` | **M1 done** |
| `policy` | #20 `confine(argv)` + enforcement honesty + denial dialects · #21 fail-closed approvals (closed outcome union, `never` pre-dispatch) · #22 arg-hash grants (clean-room) · #24 permission lattice deny>ask>default + **ruleset learning LIVE** (n0043: `RulesetLearner` → suggested rules → human-confirmed apply → workspace settings `permissions.rules`) · #25 **project trust gating LIVE** (n0043: digest-bound store, workspace content inert until trusted, drift re-gates) · #26 mediation policies LIVE (n0033: `Mediator` over scoped clients, serve `--mediation` flag) · #53 **approval scopes LIVE** (n0043: `ApprovalScope` once/conversation/always, session tool grants, outcome union unchanged) · grok ceiling parse (fail-closed) | deepseek `packages/sandbox/sandbox/src/index.ts`, `packages/interaction/user-approval`, grok `xai-tool-runtime/src/context.rs:166-265`, `xai-grok-sandbox/src/profiles.rs`, qwen `acp-bridge` mediator | **M0 done + mediation, scopes, learning, trust live** |
| `agent-core` | #9 turn loop + governors · #10 phase machine enum+match · #11 semantic termination + backstops · #12 steering · #39 task runtime + automation guard · #63 fault injection at 6 durable log boundaries (`OKRA_KILL_AT_BOUNDARY`) | grok `acp_session_impl/*`, ZCode `turn-state.ts` + `automationToolPolicy.ts` | **M1: G1 kill matrix green** (3 e2e tests: all 6 boundaries abort → recovery → no double-exec, no torn state) |
| `providers` | #35 sampler seam + closed error taxonomy (401/429/context-length/transient/permanent) · #36 kosong shims (merge-consecutive-users, tool-call-id normalization, pairing) · #63 scripted model stub (`truncate_at`, `delay_ms`) + doom-loop guard + transient retry budget · #37 LIVE (n0044): XML tool-call recovery (text-embedded calls become REAL calls through the same approval pipeline) + model-fallback candidate chain with recorded, surfaced switch events | grok sampler shape, kimi `kosong`, grok `AuthRetrySchedule`, qwen `turn.ts:747-761` + `xml-tool-call-fallback` | **M0 done; real wire APIs at M1; #37 recovery + fallback live** |
| `compaction` | #30 validated summary schema (reject, never install — clean-room) · #31 origin-tagged context (13-member closed union) · #32 byte-stable `world_state` projection | kimi `contextMemory/types.ts:114-126`, Codex dossier #1 gap fix | **M0 partial**; two-pass (#27), microcompact (#28), hydration (#29) at M2 |
| `memory` | #33 tiered memory files (user/project/team) + secret scanning; recall injected into the context head via agent-core continuations · #33/#34 LIVE (n0046): extract agent (proposes durable facts from user turns, cosine-deduped, human-accepted via `/api/memory`) + dream consolidation (clusters + contradiction report) + MMR re-ranking (`rank_mmr`) | qwen `packages/core/src/memory/`, grok `xai-grok-memory` | **M2 done** for recall+redaction; extract/dream/MMR live (n0046) |
| `session` | #38 rewind checkpoints (FS snapshot + sha256 manifest + fail-closed restore) | grok `checkpoint.rs` | live path is `host::checkpoints` (git+hunk+content cache, n0029 wires capture + `/api/rewind` + workbench UI); this crate's store stays the M1 kernel-adjacent core |
| `workflow` | #41 run journal (NDJSON, torn-tail tolerant) + budgets · Rhai engine (n0030: `step` seam, budget-honest cancellation, journal-first) · #42 validation passes (n0036: taint + causality graph, pre-run) | grok `xai-workflow` contract, ZCode dynamic-workflow journal/analysis | **M3/M5 done** — engine + validate live, wired via `okra workflow run/validate` |
| `host` | #51 notifications: 3 native classes + body redaction + focus suppression + generation counter (ChatGPT leak fixed) · #52 safe-read (`O_NOFOLLOW\|O_NONBLOCK`, regular-file, world-writable refusal) + per-conversation WATCHERS live (n0048: read model over the checkpoint log, live clean/changed/deleted per file) · fsutil (sanctioned canonicalize/home) | ChatGPT2 `docs/02` + `docs/03` studies | **M0 partial → watchers live (n0048)**; strangler domains largely re-homed (see notes) |
| `gateway` | #8 NDJSON event bus: ring replay, Last-Event-ID reconnect, slow-client eviction (protocol caps) | qwen `packages/acp-bridge/eventBus`, zcode-v4 caps | **M0 done**; ACP edge at M4 |
| `computer` | #60 contracts: split consent, batch stop-on-first-error, `user_actively_typing`, pixel guard honest failure, no-raise + the standalone MCP server (n0037: `okra mcp-serve --computer`, launch-config consent, fail-closed) | Claude2 `docs/02-03` | **M5 done** (contracts + macOS executor + in-app tools n0023/n0025 + MCP server) |
| `tui` | #55 `--minimal` scrollback writer + the M4 pager (n0031: `pager::Scrollback` — block model, scroll-lock, O(N) streaming; ratatui renderer in apps/okra) | grok pager | **M4 done** — `okra tui` + `--minimal` |
| `okra` (bin) | #56 headless CLI: `--json` NDJSON, `--cwd`, `--max-turns`, `--kill-at-phase`, `--fork-session`, `--worktree` · **G0 daemon**: `serve --stdio` streams zcode-v4 row projections (N0005) | grok headless flag set | **M0 done + G0 gate PASSED** — ZCode Dev Electron UI displayed a real okra turn (2026-09-26) |

## Dependency direction (enforced by `scripts/check-boundaries.sh`)

```
protocol (leaf)   policy (leaf)   tools (leaf)   providers (leaf)
memory / workflow / computer / tui (leaves)
kernel → protocol
compaction → providers, protocol
gateway → protocol
session → kernel, protocol
host → policy, protocol, kernel
agent-core → tools, policy, providers, kernel, protocol, compaction
apps/okra → everything
```

## Safety invariants pinned by tests

1. **Approved bytes = executed bytes** — normalize-before-hooks; hooks never
   see pre-normalization args, the executor never sees post-approval edits.
2. **Exactly one granting approval outcome** (`allowed-once`); `never` is
   decided before any channel is consulted; no channel = deny.
3. **Grants bind to (tool, sha256(args), behavior version, policy version)**
   — one byte of drift un-grants (clean-room, no donor has it).
4. **Model-visible means logged** — tool results and assistant text are
   surface events in the kernel log before the turn continues.
5. **Torn tails are invisible to readers** and repaired by the write path;
   corrupt lines are skipped + quarantined once, never fatal.
6. **No side-effecting tool executes twice with identical approved bytes**
   — non-idempotent re-execution is refused at the executor.
7. **Illegal phase transitions are rejected**, and the machine resets only
   through Completing/Error → Idle.
8. **Backstop-stopped turns are reported as cancelled**, never as answers.

## Deferred (with revisit triggers)

- grok wholesale vendoring → M1 gate (N0004): sandbox (nono) + JSONL leaf
  extraction before building kernel sandboxing.
- `workflowRun.*` protocol ops → M3 with the workflow-runs host domain (N0003).
- Windows → M6 (canonicalize/home bans already encode the lessons).
