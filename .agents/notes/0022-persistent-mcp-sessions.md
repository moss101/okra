# N0022 — persistent MCP sessions: one live child per server, across turns

- **Status:** implemented
- **Decided:** 2026-09-29
- Blocks: MASTER-PLAN #47 (persistent-session servers — pulled forward
  from "lands with the gateway at M4"); supersedes N0019's one-shot scope.
- Builds on: the sanctioned process module (N0022 extends it),
  register_mcp_tools (N0019), the daemon session-state pattern
  (approval/question bridges).

## Decision

1. **`PersistentChild`** in the sanctioned process module: spawn-once
   piped stdio, a reader thread dispatching JSON-RPC responses by id,
   bounded `request` (timeout, dead-session fast-fail), best-effort
   shutdown via stdin close (servers exit on EOF).
2. **`PersistentTransport` + `McpClient::stdio_persistent`**: same
   transport contract as the one-shot (FULL envelope; `rpc` extracts) —
   the contract mismatch was found by the first failing fixture.
3. **Session lifetime = the DAEMON, not the turn.** `register_mcp_tools`
   now consults a daemon-level cache (`TcpServeState.mcp_sessions`,
   keyed by server name): the first turn connects and caches; later
   turns refresh `tools/list` over the live session and reuse it; a dead
   session is dropped and respawned once. The stdio G0 bridge passes a
   throwaway map (single-turn scope).
4. **Approvals unchanged.** Sessions persist; the approval gate does not
   weaken — every call still fires the card (unknown side-effector).

## Why

One-shot sessions re-spawn the server per call (and per turn): real
servers pay startup cost every time (uvx, node), and stateful servers
were impossible. Persistent sessions make MCP a first-class peer of
native tools — at the price of child lifecycle management, which the
sanctioned module already owns.

## Evidence

- `g4_mcp_tools_run_inside_turns` now proves it: the fixture counts
  `initialize` invocations to a file and the assertion is
  **exactly once** across initialize + tools_list + tools/call (the
  one-shot transport would show multiple). The fixture itself had to
  become a persistent responder (loop to EOF) — the one-shot fixture
  failed `tools_list` after `initialize`, which is how the transport
  contract mismatch surfaced.
- Live drive (stateful counter server): two separate turns through the
  workbench → `session counter now: 1` then `2` — server state survived
  across turns. `scripts/ci.sh` all gates green.
