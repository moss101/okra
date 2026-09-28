# N0010 — the Changes tab: the workbench answers "what did the agent change?"

- **Status:** implemented
- **Decided:** 2026-09-28
- Blocks: MASTER-PLAN #52 (file management, git-aware reads), UI-SHELL-PLAN U2
  (file surfaces); complements N0009's approvals (allow a write → see its diff).
- Builds on: the host git domain (status/head/commit/worktrees — the G5
  grant surface), N0008's preview drawer.

## Decision

1. **Git domain gains `diff_file`** — unified working-tree diff vs HEAD
   for one repo-relative path (`--` guards option parsing; option-looking
   paths are refused as `UnsafePath`). Untracked files have no diff by
   git's contract: the endpoint returns an honest EMPTY diff (git exits 0
   for a path with no change), and the status list carries them.
2. **Serve endpoints (read-only):** `GET /api/git` → `{repository, branch,
   hash, changes:[{code,path}]}` — outside a repository this is honest
   (`repository:false`, not an error). `GET /api/git/diff?path=` → the
   unified diff, `..`-segments refused. The daemon's own `.okra-sessions`
   bookkeeping is filtered out of the change list — it is never a
   user-visible change.
3. **Workbench:** a third sidebar tab (Tasks / Files / **Changes**) shows
   the branch, a dirty/clean count, and the working-tree entries with
   status-code badges (M/??/D/R). Clicking an entry opens the working-tree
   diff in the preview drawer with per-line coloring (+/−/@@/meta). The
   tab refreshes on the same cadence as the task list (writes land as
   diffs within seconds of an approved turn).

Deferred on purpose: staging/commit UI (the domain has commit_all but a
workbench commit button is a decisions-heavy surface — ZCode keeps commit
in the terminal/git pane, block #48's terminal/PTY + git domains UI);
branch switching; diff-of-a-approval (pre-image capture per turn).

## Why

Approvals made the workbench safe to work in (N0009); the missing half of
the daily loop is *reviewing what the agent did*. ZCode's strongest shell
surfaces are exactly this (GitPane + git-graph); the daemon already had
the git domain — only the read-only projection and the tab were missing.

## Evidence

- `g4_git_surfaces_report_branch_status_and_diff` in the HTTP surface
  suite: branch + status over a REAL repo (committed edit + untracked
  file), real unified diff body, honest empty diff for unchanged paths,
  `..` refused, `repository:false` outside a repo, diff refuses there.
- Hands-on browser drive (2026-09-28): Changes tab lists `M committed.md`
  + `?? untracked.txt` under `main`; the drawer renders the colored
  unified diff; then a real APPROVED `create agent-made.md` turn through
  the workbench → the file appears in Changes within the refresh cadence,
  `.okra-sessions` filtered. `scripts/ci.sh` all gates green.
