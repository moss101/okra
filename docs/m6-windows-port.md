# M6 Windows port — inventory and plan

MASTER-PLAN §4 M6 makes Windows an explicit porting project. This is the
starting inventory: every surface that touches OS specifics, where it
lives, and the expected shape of the port. The CI runner is armed in
`.github/workflows/windows.yml` (inactive until the repo has a remote —
providing the runner/machine is a user decision).

## Already portable (expected to just work)

- **protocol / kernel / compaction / memory / gateway** — pure logic +
  rusqlite (bundled); JSONL storage uses plain file IO. Path handling is
  the only risk (see "path discipline").
- **providers (openai/ureq)** — rustls; no OS trust-store dependency.
- **tools** — the tool traits and scheduler are OS-free; builtins are
  covered under safe_fs below.

## Needs platform shims (the actual port)

| Surface | Location | Windows shape |
|---|---|---|
| Sandbox (nono: Seatbelt/Landlock) | `okra_policy`, N0006 | No direct equivalent. Options: restricted-token + AppContainer (large), or ship Windows with `enforcement: partial` and honest denial dialects (deepseek honesty types already model this) |
| Process groups (login-shell capture, terminal) | `runtime_env::RealLoginShellExecutor` (process_group(0), kill(-pid)), `terminal.rs` (portable-pty) | Job Objects for tree kill; portable-pty has a ConPTY backend; the `detached`/group code paths are unix-gated today |
| Safe-read (O_NOFOLLOW\|O_NONBLOCK) | `safe_fs.rs` | FILE_FLAG_OPEN_REPARSE_POINT + GetFileInformation checks; the ownership/mode ladder re-maps to ACLs (first pass: report `enforcement: partial`) |
| fsutil canonicalize (verbatim `\\?\` paths) | `fsutil.rs` | The M6 note in the file is the design slot; path-equality keys must normalize verbatim prefixes |
| home dir / env | `fsutil::home_dir` | SHGetKnownFolderPath or `USERPROFILE`; the clippy ban on `std::env::home_dir` already forces one call site |
| Login-shell probe | `runtime_env` | PowerShell `-NoProfile -Command` capture or Git-Bash if present; markers/NUL parsing unchanged |
| Signals / kill | subagent + governors | Taskkill / Job Objects; governor budget logic is OS-free |

## Path discipline (cross-cutting)

- clippy.toml already bans raw `canonicalize` for exactly this reason;
  every path key that crosses a persistence boundary goes through
  `fsutil::normalize_lexical` — keep it that way and add a verbatim-prefix
  strip in the M6 `canonicalize` slot.
- Marker/preflight files (`remote_access`, JSONL temp+rename) use
  `create_new` + rename — both fine on Windows, but rename-over-existing
  needs `std::fs::rename` retries while AV scanners hold handles (known
  flake class; add a bounded retry helper in `fsutil`).

## Order of work once a runner exists

1. `cargo build --workspace` green (expect: sandbox + safe_fs + terminal).
2. `--lib` tests green (pure logic should pass immediately).
3. Integration triage in g1/g2/g5 order (kill-matrix uses signals — shim
   first), recording every shim as a decision note.
4. Clippy windows run; fold new path lessons into clippy.toml bans.
5. Only then: sandbox enforcement work (restricted token) — until then
   Windows ships `enforcement: partial` (honest, by contract).
