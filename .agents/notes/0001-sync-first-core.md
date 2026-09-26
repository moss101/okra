# N0001 — Sync-first core, no tokio in M0 crates

- **Status:** implemented
- **Decided:** 2026-09-26
- **Lifecycle:** proposed → implemented (same day)

## Decision

The M0/M1 core crates (kernel, tools, policy, agent-core, providers, and the
okra CLI) are synchronous: std threads + std channels, no tokio, no async
runtimes. Async may enter at the gateway (network surfaces) in M4 without
touching these contracts.

## Why

- The donors' async surfaces (grok's tokio actor loop) exist to serve network
  streaming; the M0 acceptance gate (UI/headless drives a real turn through
  the daemon) does not need it.
- Fault-injection tests (MASTER-PLAN §3 #63 — `killAtPhase` at every durable
  boundary) are dramatically more deterministic single-threaded.
- The contracts we port (event log, single-writer handle, tool stream shape,
  conflict scheduler, approval lattice) are all expressible sync; grok's own
  `Tool` trait separates *shape* (N-Progress-then-one-Terminal) from *runtime*
  (RPITIT vs erased dispatch).

## Consequences

- The ToolAccesses scheduler uses a mutex + condvar instead of futures; the
  semantics ported from kimi (conflict → queue behind blockers) are identical.
- `SessionActor` becomes a `TurnLoop` struct driven by `TurnCommand` values
  over a std mpsc channel; the gateway wraps it in threads.
- If M4 requires async, the port point is the tool dispatcher + sampler only.

## Evidence

- `crates/tools/src/scheduler.rs` (sync scheduler, kimi semantics)
- `crates/agent-core/src/loop_.rs` (sync turn loop)
- `apps/okra/src/main.rs` (threads at the edge only)
