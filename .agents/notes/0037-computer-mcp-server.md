# N0037 — okra's computer control as a standalone MCP server

- **Status:** implemented
- **Decided:** 2026-10-01
- Closes: ARCHITECTURE's computer-crate row — "M0 contracts; MCP servers
  at M5" (MASTER-PLAN §3 #60's server half).

## Decision

1. `okra mcp-serve --computer [--allow-app NAME]... [--allow-full-control]`:
   a JSON-RPC 2.0 MCP server over stdio (initialize → tools/list →
   tools/call — the same wire okra's own MCP client speaks), exposing
   the REAL macOS backend: `computer_observe` (AX tree),
   `computer_act` (click element / type / key, re-observe after),
   `computer_screenshot` (PNG data URL), `computer_list_apps`,
   `computer_open_application` (background no-raise).
2. **Consent is the launch config, fail-closed.** A headless server has
   no surface to raise an approval card, so `--allow-app` seeds the
   per-app capability ledger and `--allow-full-control` grants display
   scope (screenshots; typing/keys). Every refusal names the missing
   flag. Grants hold for the server's lifetime (the session lock).

## Evidence

- `apps/okra/tests/mcp_server.rs` (real binary, stdio): handshake
  returns serverInfo + tools capability; tools/list carries the family;
  observe without consent → `isError` naming `--allow-app Finder`;
  screenshot without full control → names `--allow-full-control`;
  unknown tool → JSON-RPC error. Granted path runs through the
  env-overridable hermetic backend (`OKRA_OSASCRIPT` fixture) and is
  PAST the consent gate while other apps stay refused.

## Because

The in-app tools (N0023/N0025) serve okra's own turns; the standalone
server is the seam that lets any external MCP client use okra's
AX-first control without okra being the agent — with the same
split-consent semantics instead of a blanket grant.
