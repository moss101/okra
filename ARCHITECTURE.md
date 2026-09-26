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
| `kernel` | #4 event-log contract ("model-visible means logged", surface vs log-only, `surfaceOp` replace) · #5 JSONL adapter + loss contract + `.corrupt` quarantine · #6 single-writer `SessionHandle` + torn-tail repair · #7 SQLite projections | deepseek `packages/core/session/src/{types,surface,invariant}.ts`, `packages/session/session-persistence*`, grok `xai-grok-shell/src/session/storage/jsonl` | **M0 done** (12 tests: crash recovery, double-open rejection, quarantine, heal-on-append); SQLite at M3 |
| `tools` | #13 stream shape `[Progress*, one Terminal]` · #14 ToolEntry metadata + normalize-before-hooks (approved bytes = executed bytes) · #15 `ToolAccesses` conflict scheduler · #17 output budgets + spill · #18 `ToolSpec.idempotent` (clean-room) | grok `xai-tool-runtime/src/tool.rs`, ZCode `apps/zcode-cli/packages/core/src/tool/types.ts:64-91` + `call-runner.ts`, kimi `agent-core-v2/src/tool/toolContract.ts` + `toolExecutor/toolScheduler.ts`, deepseek `packages/spill` | **M0 done** |
| `policy` | #20 `confine(argv)` + enforcement honesty + denial dialects · #21 fail-closed approvals (closed outcome union, `never` pre-dispatch) · #22 arg-hash grants (clean-room) · #24 permission lattice deny>ask>default · #26 mediation policies · grok ceiling parse (fail-closed) | deepseek `packages/sandbox/sandbox/src/index.ts`, `packages/interaction/user-approval`, grok `xai-tool-runtime/src/context.rs:166-265`, `xai-grok-sandbox/src/profiles.rs`, qwen `acp-bridge` mediator | **M0 done** (14 tests) |
| `agent-core` | #9 turn loop + governors (stationarity 4/8-8/12/4-noop, length salvage, rate-limit park, 401 uncharged resubmit) · #10 phase machine enum+match · #11 semantic termination + wall-clock/no-progress/spend backstops · #12 steering at step boundaries (stranded → fallback) · #39 one task runtime + automation self-mutation guard · #63 `killAtPhase` fault injection | grok `acp_session_impl/{turn,length_salvage,rate_limit_waits,auth_retry,interjection}`, ZCode `agent/turn-state.ts` + `automationToolPolicy.ts`, Codex dossier fixes | **M0 done** (8 unit + 6 e2e tests incl. child-process crash recovery) |
| `providers` | #35 sampler seam + closed error taxonomy (401/429/context-length/transient/permanent) · #36 kosong shims (merge-consecutive-users, tool-call-id normalization, pairing) · #63 scripted model stub (`truncate_at`, `delay_ms`) + doom-loop guard + transient retry budget | grok sampler shape, kimi `kosong`, grok `AuthRetrySchedule` | **M0 done**; real wire APIs at M1 |
| `compaction` | #30 validated summary schema (reject, never install — clean-room) · #31 origin-tagged context (13-member closed union) · #32 byte-stable `world_state` projection | kimi `contextMemory/types.ts:114-126`, Codex dossier #1 gap fix | **M0 partial**; two-pass (#27), microcompact (#28), hydration (#29) at M2 |
| `memory` | #33 tiered memory files (user/project/team) + secret scanning | qwen `packages/core/src/memory/` | **M0 partial** (tiers + regex secret redaction); extract/dream/recall agents at M2 |
| `session` | #38 rewind checkpoints (FS snapshot + sha256 manifest + fail-closed restore) | grok `checkpoint.rs` | **M0 partial** (FS+manifest); git+hunk checkpoints at M3 |
| `workflow` | #41 run journal (NDJSON, torn-tail tolerant) + budgets | grok `xai-workflow` (engine at M3, N0004), ZCode dynamic-workflow journal | **M0 partial** |
| `host` | #51 notifications: 3 native classes + body redaction + focus suppression + generation counter (ChatGPT leak fixed) · #52 safe-read (`O_NOFOLLOW\|O_NONBLOCK`, regular-file, world-writable refusal) · fsutil (sanctioned canonicalize/home) | ChatGPT2 `docs/02` + `docs/03` studies | **M0 partial**; ~50-domain strangler starts M3 |
| `gateway` | #8 NDJSON event bus: ring replay, Last-Event-ID reconnect, slow-client eviction (protocol caps) | qwen `packages/acp-bridge/eventBus`, zcode-v4 caps | **M0 done**; ACP edge at M4 |
| `computer` | #60 contracts: split consent, batch stop-on-first-error, `user_actively_typing`, pixel guard honest failure, no-raise | Claude2 `docs/02-03` | **M0 contracts**; MCP servers at M5 |
| `tui` | #55 `--minimal` scrollback writer (O(N) streaming) | grok pager (ratatui at M4) | **M0 partial** |
| `okra` (bin) | #56 headless CLI: `--json` NDJSON, `--cwd`, `--max-turns`, `--kill-at-phase`, `--fork-session`, `--worktree` | grok headless flag set | **M0 done** with offline demo sampler; real providers at M1 |

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
