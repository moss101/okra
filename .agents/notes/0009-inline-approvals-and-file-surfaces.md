# N0009 — the workbench asks: inline approvals + file surfaces

- **Status:** implemented
- **Decided:** 2026-09-28
- Blocks: MASTER-PLAN #53 (approval UX), #52 (file management UI slice), UI-SHELL-PLAN U1 (approval hierarchy), U2 (file surfaces)
- Builds on: N0008 (the workbench shell), the runtime stop seam
  (UserRequested), the executor's fail-closed approval waterfall.

## Decision

The served surface is now **attended**: it no longer auto-approves
side-effecting tools.

1. **Approval bridge.** `SurfaceApprovalChannel` implements the policy
   crate's `ApprovalChannel`: `answer` registers the ask, then blocks
   (bounded waits) until a surface resolves it via the `resolveApproval`
   command (allow → `AllowedOnce`, deny → `Rejected`) or the session's
   stop flag flips (→ `Cancelled` — stopping cancels a pending ask). The
   outcome union is untouched; exactly `AllowedOnce` grants, and the
   arg-hash grant is still minted (same bytes never re-ask).
2. **Ceilings per surface kind.** The TCP serve runs
   `ToolApprovalCeiling::GrantsAllowed` (attended); the stdio G0 bridge
   stays `UnattendedAllowed` (no approver exists behind it). Read-only
   tools never prompt (donor contract).
3. **Live UX.** A watchdog emits projection frames while the turn is
   paused: `control.awaitingApproval` carries the pending asks and the
   phase flips to `awaitingApproval`. Pending cards render straight from
   control (the kernel row only exists post-decision); the card shows the
   approved bytes inline (path + content) with Allow once / Deny. Decided
   outcomes replay from the kernel audit pair: the executor now drains
   `approval/asked` + `approval/decided` (log-only, ignorable, args
   capped at 400 chars) into the session log, and replay renders approval
   rows with their outcome.
4. **Tool-call args on the wire.** `LoopEvent::ToolCallStarted` carries
   the raw sampled `args_json`, so tool cards show their target path and
   clicking it opens the file preview.
5. **File surfaces.** `GET /api/files?path=` (workspace-confined listing:
   dot entries never surface — including `.okra-sessions` — symlinks
   reported but never followed, `..` refused, canonical-escape check) and
   `GET /api/file?path=` (host `safe_fs::safe_read`: O_NOFOLLOW,
   regular-file verification, world-writable refusal; 256 KB preview cap;
   404/400 honest errors). The sidebar grows Tasks/Files tabs; the Files
   tab is a lazy tree; files open in a preview drawer (Esc/backdrop
   closes).
6. **Offline approvable demo.** The demo planner gained a write path
   ("create FILE") so the approval flow is demonstrable without a network
   model (and without weakening policy to prove UI).

Deferred on purpose: "Always allow" beyond the arg-hash grant (needs the
grants/lattice management UI, blocks #24/#53 remainder); decided-card
display in the live transcript (they appear on replay); approval prompt
for MCP/deferred tools.

## Why

Dogfooding (G3) cannot run a workbench that silently writes files, and
the approval UX is the plan's own gate for attended use (block #53,
UI-SHELL-PLAN U1). File surfaces are the other half of the daily loop:
without a tree/preview the workbench cannot answer "what did the agent
actually change?" without leaving the app.

## Evidence

- `g4_approvals_pause_turn_until_resolved` (allow → write lands + audit
  replays `allowed`; deny → honest `approval denied` tool error + nothing
  on disk; unknown approval id rejected) and
  `g4_files_api_lists_and_previews_safely` (listing, nesting, preview,
  `..` refused, traversal rejected, honest 404) in the HTTP surface suite.
- Hands-on browser drive (2026-09-28): pending card with inline proposed
  action under the "needs approval" chip → Allow once → write_file
  success + task completed; second ask → Deny → `approval denied: the
  user rejected this call`, file absent; Files tab lists the workspace;
  preview drawer renders notes.md in dark theme. Screenshots in the
  session record. `scripts/ci.sh` all gates green.
