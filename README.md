# okra

One Rust daemon (agent + host services) that any surface drives over one typed
protocol — desktop shell, web, TUI, headless/CI, editors via ACP — with
kernel-enforced safety, an event-sourced session log as the only durable truth,
and the workbench-shell polish (notifications, file management, computer
control) that no reference product gets right.

This is the implementation of
[`work/20260925-nextgen-workbench/MASTER-PLAN.md`](../work/20260925-nextgen-workbench/MASTER-PLAN.md)
(the "workbench" plan, realized under the okra name — decision N0002).

## Status

Milestone **M0 (skeleton)** is implemented and green; **M1 (agent + CLI
parity)** is partially landed. See `ARCHITECTURE.md` for the block-by-block
map (block → source → crate) and `.agents/notes/` for binding decisions.

```text
M0  skeleton                    ✔ workspace, policy files, protocol port with
                                  TS-ground-truth convergence tests, kernel
                                  event log, read_file end-to-end, headless CLI
M0  gate G0                     ✔ ZCode's existing Electron UI displays a
                                  real turn produced by the okra daemon
                                  (okra serve + host-side bridge, N0005)
M1  agent + CLI parity          ◐ tool plane, policy stack, session log,
                                  governors, fault injection (in progress)
M2  context / memory / skills   ◐ compaction summary schema, origin tags
M3  host services + alpha       ☐      M4 surfaces ☐      M5 differentiators ☐
```

## G0 demo (2026-09-26)

The ZCode Dev Electron app, launched with `ZCODE_OKRA_DAEMON=<okra binary>
pnpm dev:runtime` (ZCode working tree carries the marked G0 bridge patch —
see `.agents/notes/0005-g0-electron-bridge.md`), rendered a full turn from
the okra daemon: user message bubble, streaming assistant text, a read_file
tool card, and the file content okra's Rust toolchain read from the
workspace — over the standard `zcode-agent` v4 channel with no renderer
changes.

## Layout

```
crates/
  protocol/    zcode-protocol-v4 port: wire frames, fragmentation codec,
               delivery profiles, delta ops + coalesce, payload caps
  kernel/      event-sourced session log (deepseek contract) + JSONL storage
               (grok loss contract) + single-writer SessionHandle
  tools/       tool runtime: [Progress*, one Terminal] streams, ToolSpec
               (idempotent flag), policy metadata, ToolAccesses scheduler,
               spill store
  policy/      fail-closed approvals, arg-hash grants, confine(argv) with
               enforcement honesty, sandbox profiles, permission lattice
  agent-core/  turn phase machine (enum + match), governors, steering inbox,
               semantic termination + backstops, one task runtime
  providers/   sampler trait + scripted model stub + message shims
  compaction/  validated summary schema, origin tags, world-state projection
  memory/      tiered memory files + secret scanning
  session/     rewind checkpoints
  workflow/    run journal + budgets (Rhai engine: M3)
  host/        notifications policy, safe file service, automation guard
  gateway/     NDJSON event bus: ring replay, Last-Event-ID, slow-client eviction
  computer/    AX-first computer control contracts (M5)
  tui/         minimal scrollback writer (ratatui pager: M4)
apps/
  okra/        headless CLI: okra --json "prompt" (NDJSON event stream)
```

## Build

```sh
cargo build            # workspace
cargo test             # unit + convergence + crash-recovery suites
cargo clippy           # day-1 bans enforced (clippy.toml)
```

The protocol crate's golden vectors are generated from the ZCode TypeScript
ground truth (`crates/protocol/tests/golden/generate.mjs`, kept with the
checked-in `golden-vectors.json`); regenerate only when the upstream contract
changes.

## Governance

- Decision records live in `.agents/notes/` (deepseek pattern); lifecycle
  `proposed → implemented → rejected|archived`, CI-checked.
- Crate dependency direction is enforced by `scripts/check-boundaries.sh`.
- Day-1 clippy bans: no raw process spawn, no raw `home_dir`, no raw
  `canonicalize`.
