# N0016 — the desktop shell: a thin Electron main over `okra serve`

- **Status:** implemented (window path verified by design review + smoke
  mode; sandbox cannot launch GUI processes — see Evidence)
- **Decided:** 2026-09-28
- Blocks: MASTER-PLAN #50 (Electron main = pure window/IPC bridge)
- Builds on: N0008's workbench (the entire product surface lives behind
  `okra serve --tcp`).

## Decision

1. **Thin by construction.** `apps/desktop/main.js` does exactly three
   things: spawn the loopback daemon (`okra serve --tcp --cwd`), wait for
   the handshake (read the bound port from stderr, poll `/health`), and
   point a BrowserWindow at the workbench. Closing the window stops the
   daemon. There is no IPC surface and no business logic in the shell —
   the web UI already speaks loopback HTTP/SSE, so nothing in the
   product branches on "desktop vs web" (the multi-surface claim, taken
   literally).
2. **Testable handshake.** The only nontrivial logic (spawn → stderr port
   parse → health poll, with honest failure modes: early exit, never
   binds, never answers) lives in exported pure functions; electron is an
   OPTIONAL require so plain `node --test` runs the acceptance without
   the GUI runtime. A `--smoke` mode runs the same handshake inside the
   real Electron runtime without a window.
3. **Caught a real daemon bug:** `serve --tcp` never validated `--cwd` —
   a bogus directory still bound a daemon (tools/PTYs would then operate
   on garbage). Now exit(2), matching the single-turn path.
4. **Scope honesty:** the smoke/window path could not be executed inside
   this session's sandboxed shell (GUI subprocess blocked); its
   verification is the node handshake suite plus `npm run smoke` /
   `npm start` as user-run steps. Node 26 runner note: `node --test
   --test-force-exit` — the daemon's stderr pipe keeps the runner alive
   after completion without it.

## Why

The workbench became feature-complete on the web (N0008–N0015); the
desktop shell is pure packaging, and block #50's thinness rule keeps it
that way forever: chrome around the daemon, nothing else.

## Evidence

- `apps/desktop/test/handshake.test.cjs` — 4/4 green via `npm test`
  (node 26, `--test-force-exit`): parse/resolve facts, a real handshake
  against the compiled binary (bind → /health with correct
  daemon/version/cwd), and an honest early-exit rejection (which caught
  the `--cwd` validation bug above).
- `node --check main.js`; electron installed (`npm install`); the GUI
  smoke is documented as a user-run step (`npm start` / `npm run smoke`).
- Full `scripts/ci.sh` all gates green.
