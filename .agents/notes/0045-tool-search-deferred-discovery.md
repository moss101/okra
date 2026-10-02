# N0045 — tool_search: deferred discovery generalized to the full registry (#16)

- **Status:** implemented
- **Decided:** 2026-10-01
- Closes: MASTER-PLAN §3 #16 ("Deferred discovery: `shouldDefer` +
  `tool_search`, lazy registration" — qwen `tools.ts:300-320`,
  `tool-registry.ts`), generalizing the MCP-only `tool_directory`
  (n0021/n0047-era funnel directory) to EVERY registered tool.

## Decision

1. **One directory, every source.** `tools::DiscoveryIndex` holds a
   compact entry per REGISTERED tool — native builtins, interaction
   tools (ask_user), computer tools, the subagent tool, and MCP tools
   (`source: "mcp:<server>"`). The daemon refreshes the index at the
   registration moment of every turn (after MCP tools join), so what
   search returns is exactly what dispatch can execute. No stale
   directory: the index is rebuilt per turn from the registry itself.
2. **Schemas on demand.** The world head keeps descriptions; FULL JSON
   schemas are delivered only in `tool_search` results (the qwen
   deferred-discovery contract). `query` ranks by token overlap — name
   hits (weight 3, exact-name bonus 10) over description hits (weight 1);
   an empty query BROWSES in stable name order; `limit` caps at 25.
3. **Honest empty results.** No match → an empty `results` array (the
   model learns there is no such tool), never a fabricated tool. The
   builtin itself is read-only, idempotent, and allowed in plan mode.

## Relationship to the donors

qwen hides deferred tools behind `shouldDefer` flags on individual tools
and discovers them via a separate `tool_search` server call. okra's
registry has no "deferred" class (every registered tool is dispatchable),
so the generalization is: the directory indexes ALL tools, and the model
carries only the compact projection by default. The MCP-only
`tool_directory` stays (it serves server/tool MANAGEMENT views); the
turn's discovery surface is `tool_search`.

## Evidence

- `crates/tools/src/discovery.rs` (5 unit tests: name-beats-description,
  mcp source labels, stable browse, honest empty, exact-name rank).
- Daemon wiring: `run_turn_streaming` refreshes the index from
  `registry.entries()` after all registrations and registers
  `tool_search` (read-only, no accesses).
- `cargo test -p okra-tools` 18/18; the g4 http-surface suite exercises
  the turn path with the builtin registered.
