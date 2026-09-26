# N0005 — G0 gate: ZCode Electron UI driven by the okra daemon via a host-side bridge

- **Status:** implemented
- **Decided:** 2026-09-26
- **Lifecycle:** proposed → implemented (same day)

## Decision

The G0 gate ("ZCode's existing Electron UI displays a real turn produced by
the Rust daemon") is implemented as a two-piece bridge instead of a full
wire-protocol daemon:

1. **okra side** — `okra serve --stdio` (apps/okra/src/serve.rs): a
   line-delimited JSON-RPC daemon that runs turns through the normal
   agent-core loop and streams live `v4/projection` notifications whose
   rows are zcode-protocol-v4 `conversationRowSchema`-shaped JSON
   (turnHeader / userInput / assistantText / toolCall, field-for-field per
   donor rows.ts).
2. **ZCode side** — `packages/desktop/src/host/okraBridge.ts` (new file,
   env-gated by `ZCODE_OKRA_DAEMON`) plus a 7-line decorator hook in
   `exposeServicesOnMessagePort` (host/index.ts): the bridge spawns the
   daemon per workspace, diffs projections into `ConversationDelta` ops
   (row.appended / row.delta / row.upserted / state.updated), assembles a
   full `ConversationSnapshot`, wraps both in `TopicWireFrame` envelopes and
   serves the `zcode-agent` v4 surface (subscribe / resync / command / ack /
   rows-range / sessions-index / workspace-config / frame events) through
   the existing `createZCodeAgentConnectionScope` facade. The renderer is
   untouched.

## Why

- The renderer requires the full Channel RPC + ~40 services at boot;
  replacing the host wholesale would have reimplemented all of them. The
  decorator keeps settings/model-selection/storage REAL and swaps only the
  conversation surface — the plan's "rendering path untouched" constraint.
- The v4 wire contract (rows, deltas, snapshots, acks) is zod-validated on
  the renderer side; emitting donor-shaped rows from Rust and diffing in TS
  keeps each side's contract surface minimal and machine-checked.
- Demo evidence (2026-09-26): ZCode Dev app (Electron 41, dev runtime)
  displayed a full okra turn — user bubble, streaming assistant text,
  read_file tool card with the okra README content read by okra's Rust
  toolchain from the workspace — driven end-to-end over the bridge.

## Known G0 limitations (deliberate)

- Row projections are in-memory per daemon instance; history replay from
  the kernel log on daemon restart lands with M3's session domain.
- Non-conversation agent commands (editUserQuery, retry, attachments, …)
  are rejected with `okra.g0.unsupportedCommand`.
- The bridge is removed when okra's own gateway speaks the full channel
  protocol (M4).

## Evidence

- apps/okra/src/serve.rs + apps/okra/src/main.rs (`serve` subcommand)
- ZCode working tree: packages/desktop/src/host/okraBridge.ts,
  packages/desktop/src/host/index.ts (marked G0 patch)
- Screenshot transcript captured over CDP during the 2026-09-26 session
