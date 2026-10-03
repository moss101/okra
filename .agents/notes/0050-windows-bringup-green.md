# N0050 — Windows bring-up: the CI gate is green

- **Status:** implemented
- **Decided:** 2026-10-03
- Closes the M6 gate that blocked rounds 1–3: the repo is on GitHub
  (moss101/okra), the armed `windows.yml` runs on every push, and run
  `37082086546` is **GREEN** — workspace build ✓ + the full `--lib`
  unit/convergence suite on a real `windows-latest` runner (**418 tests
  passed**). The integration + clippy steps remain `continue-on-error`
  by design: they are the second-pass work queue.

## What the bring-up took (7 CI cycles, in order)

1. `crates/kernel/src/lease.rs` was the ONLY compile blocker (run
   `37078261049`): flock + (dev,ino) identity. Ported to the std
   file-lock API (`File::try_lock` — flock on unix, `LockFileEx` on
   windows), keeping the donor's identity-revalidation contract
   ((creation_time, len) on windows — the lock file is never written,
   so recreation changes both).
2. `nono` is unix-only by upstream contract (SCM_RIGHTS/CMSG/umask in
   its socket supervisor): it became a `[target.'cfg(unix)']`
   dependency, and `NonoSandboxBackend` grew an honest Windows stub
   under the SAME public surface — `confinable()` refuses
   DangerFullAccess identically, confined modes answer
   `SandboxError::Unavailable` (windows NEVER claims
   `enforcement: full`), and a cfg(windows) gate test runs in CI.
3. THE runtime root cause (run `37079773172`, os error 5):
   `sync_dir` opened a DIRECTORY via `std::fs::File::open` — windows
   refuses that outright. Unix keeps the real dir fsync; windows
   degrades honestly (NTFS metadata journaling covers rename
   durability; recorded in the port doc). The same best-effort
   dir-fsync in `builtins::write_file` is unix-gated too.
4. Smaller shims: `home_dir` reads `USERPROFILE` on windows; `safe_read`
   /`safe_open` first passes without O_NOFOLLOW (the symlink pre-check +
   fstat regular-file verification carry the contract); `is_executable_file`
   extension fallback; `runtime_env` process-group + group-kill gated.
5. Test-semantics gates (each carries a triage comment): the unix test
   batch (tar extraction, symlink fixtures, readonly-mode refusal, shell
   ladder, PATH resolution, computer-backend fixture) — these test unix
   SEMANTICS, not an oversight; windows equivalents arrive with the
   PATHEXT/ACL second pass.

## Second-pass queue (from the continue-on-error steps)

- Integration tests on windows (g1/g2/g5 triage per the port doc).
- Clippy windows run.
- PATHEXT command resolution, ACL mapping for safe-read, Job Objects
  tree-kill, restricted-token sandbox (enforcement: partial → full).

## Evidence

- Run `37082086546`: ✓ build, ✓ unit + convergence (418 passed).
- Unix side unchanged: 432 lib tests green locally; clippy
  `-D warnings` green; check-notes + check-boundaries green.
