# N0015 — the Tools tab: skills and MCP servers as read-only surfaces

- **Status:** implemented
- **Decided:** 2026-09-28
- Blocks: MASTER-PLAN #48 (skills + mcp-sync domains → surfaces), #47 (MCP
  visibility); UI-SHELL-PLAN U0 panes.
- Builds on: the memory skills domain (SkillCatalog, path-conditional
  activation), the host mcp-sync domain (records across scopes), N0008's
  sidebar tabs.

## Decision

1. **Read-only projections, no new state.** `GET /api/skills` lists the
   workspace's `.okra/skills/*.md` catalog (name, description, match
   patterns — disclosure layer 1; bodies stay layer 2). `GET /api/mcp`
   lists configured MCP servers via `McpSyncService::load(Some(workspace))`
   (workspace `.okra/config.json` `mcp.servers` wins over user-level
   sources): name, enabled, source (okra/agents), scope (workspace/user),
   and a launch summary (command + args, or url).
2. **Honest status semantics.** The MCP surface reflects the sync domain's
   truth — the enabled flag and origin — not a fabricated "connected"
   state: the daemon does not hold runtime MCP connections to project
   (that lands with the #47 client wiring into serve turns).
3. **Workbench:** a fourth sidebar tab (Tasks / Files / Changes / Tools)
   with the two sections, pattern chips for skills, enable dots + scope
   labels + launch summaries for servers, and empty states that say where
   to install things.
4. **Structural fix (the route-nesting trap, fifth strike):** every
   `/api/*` route group now lives at the TOP level of `http_handle`; the
   `if method == "GET"` block holds only the static assets. New routes can
   no longer silently 404 by being anchored inside a method guard.

## Why

Skills and MCP servers were invisible in the workbench — the agent used
them (activation is in the turn loop) but the user had no surface to see
what is installed, what activates when, or which servers are configured.
Read-only first: management (install/disable flows) rides on the same
projections later.

## Evidence

- `g4_tools_surface_projects_skills_and_mcp`: a workspace with a real
  skill (frontmatter + pattern) and a workspace MCP server projects
  name/description/patterns and enabled=false + `uvx mcp-tester` +
  workspace scope; an empty workspace projects honest empties. (Fixture
  lesson recorded: the okra MCP source reads `.okra/config.json` with
  nested `mcp.servers`, not `.okra/mcp.json`.)
- Hands-on browser drive (2026-09-28): the Tools tab lists two skills with
  pattern chips and two servers with enable dots, scopes and launch
  summaries. Full `scripts/ci.sh` all gates green.
