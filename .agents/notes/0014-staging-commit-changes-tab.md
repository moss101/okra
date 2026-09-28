# N0014 — the Changes tab commits: staging + staged-only commits

- **Status:** implemented
- **Decided:** 2026-09-28
- Blocks: MASTER-PLAN #48 (git domain), #52 (file management surfaces);
  completes the N0010 Changes tab loop (review → stage → commit).
- Builds on: the host git domain (status/head/diff/worktrees); N0010's
  Changes tab; N0013's toasts.

## Decision

1. **Git domain:** `stage(pathspecs)` (`add --`), `unstage(pathspecs)`
   (`restore --staged --` — index back to HEAD, working tree untouched),
   and `commit(message)` — commits the STAGED index only (never touches
   the working tree; staging is the caller's explicit act) and refuses
   honestly on empty message or nothing-to-commit. Option-looking
   pathspecs are refused (`UnsafePath`); all pathspecs also pass behind
   `--`. `status()` now keeps the RAW porcelain XY pair — trimming it
   erased the staged/unstaged distinction the split view partitions on
   (X = staged, Y = unstaged).
2. **Serve:** `POST /api/git/stage|unstage {paths}` and
   `POST /api/git/commit {message}` → `{hash, branch}`. Non-repositories
   refuse with 400; empty path lists refuse.
3. **Workbench:** the Changes tab splits into **Changes** (unstaged +
   untracked) and **Staged** sections from the raw XY codes; per-file
   `+`/`−` toggles stage/unstage; a commit box (message + Commit button,
   enabled only with staged changes and a non-empty message) commits and
   toasts the short hash + branch, then refreshes. The commit box lives
   OUTSIDE the re-rendered list (a box nested in the rebuilt nav vanished
   on every refresh — found live).

## Why

Reviewing changes (N0010) without acting on them left the daily loop
unfinished — every reference workbench stages and commits where it
diffs. The domain already had `commit_all` (stage-everything), which
defeats selective staging; the explicit stage/commit split is the honest
primitive set.

## Evidence

- `g4_git_stage_and_commit_over_the_wire`: stage ONE of two changed
  files → raw codes on the wire (`"M "` vs `" M"`) → commit → `git show`
  proves ONLY the staged file landed and the other stays dirty → honest
  400s for empty message / empty paths / option-looking pathspec / non-repo.
- Hands-on browser drive (2026-09-28): `+` on alpha.txt → it moves to
  Staged (`M  alpha.txt`), commit box appears, Commit → toast
  "Committed 04a6fbe685 on main", status refreshes to only ` M beta.txt`,
  box hides (nothing staged). Full `scripts/ci.sh` all gates green.
