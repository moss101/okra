# N0018 — runtime MCP status: the Tools tab can now answer "does it connect?"

- **Status:** implemented
- **Decided:** 2026-09-29
- Blocks: MASTER-PLAN #47 (MCP client → serve visibility), N0015's honest
  caveat ("no fabricated connected state — runtime connections land with
  the #47 client wiring") — the client existed; only the wiring was
  missing.
- Builds on: `McpClient::stdio` (one-shot JSON-RPC per request through the
  sanctioned runner), the mcp-sync records, N0015's Tools tab.

## Decision

1. **Probing is explicit.** `POST /api/mcp/probe {name?}` connects to one
   or all configured stdio servers (`initialize` + `tools/list`) on a
   bounded thread (8 s recv timeout; a hung server reports `timeout`,
   never blocks the daemon). Listing stays pure — `GET /api/mcp` merges
   the CACHED runtime status when present and is unchanged otherwise.
2. **Status vocabulary:** `connected` (with protocolVersion, serverInfo,
   tool names + count) | `error` | `timeout` | `no-command`. Cached per
   server name; a probe refreshes one or all.
3. **Tools tab:** a `probe` button on the MCP section; entries show a
   status chip (connected · N tools / timeout / error / no-command) and
   tool-name chips when connected.
4. **Dogfood harness fix (G3 session-4 finding):**
   `scripts/dogfood-log.sh` now writes dated journal files
   (`journal/<date>-<label>.md`, the day-1 convention) instead of the
   drifted bare label; verified end-to-end including the date rollover
   to `2026-09-29-day02.md`.

## Why

N0015 deliberately refused to fabricate a connected state. This tranche
completes it: the client existed since M2 — probing is the last wiring,
and explicit POST probing keeps listing side-effect-free (spawning
user-configured commands is an action, not a view).

## Evidence

- `g4_mcp_probe_connects_and_caches_status`: a REAL JSON-RPC-over-stdio
  responder fixture — probe → `connected` with `probe-tool`, status
  cached into `/api/mcp`, named probe of a broken server reports `error`.
- Hands-on browser drive (2026-09-29): the Tools tab's probe button →
  `connected · 1 tools` + tool-name chip for the fixture server.
  `scripts/ci.sh` all gates green.
