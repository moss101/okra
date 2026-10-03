# M6 Windows port — inventory and plan

MASTER-PLAN §4 M6 makes Windows an explicit porting project. This is the
starting inventory: every surface that touches OS specifics, where it
lives, and the expected shape of the port. The CI runner is armed in
`.github/workflows/windows.yml` — LIVE since the repo landed on GitHub
(github.com/moss101/okra, 2026-10-02): the workflow runs on every push
to main and is the canonical bring-up gate.

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

## SECOND PASS PROGRESS (2026-10-03)

- The integration step COMPILES and RUNS on windows (436 tests passing in
  run `37097302681`; it died at rustc before this round's fixes).
- g1 kill-matrix GREEN on windows: the kill contract is now platform-
  aware (signal on unix; STATUS_CONTROL_C_EXIT hard-termination family
  on windows — a clean exit still fails).
- Remaining integration triage (expected red, continue-on-error):
  g4 computer-control tests (AX is macOS by design — gate to
  `target_os = macos`), g4 PTY (ConPTY shim = the real work), g4 MCP
  probe/tools (unix spawn fixtures in the tests).

## BRING-UP STATUS: GREEN (2026-10-03, n0050)

Run `37082086546` passed the compile gate AND the full `--lib`
unit/convergence suite on `windows-latest` (418 tests). What it took:
the lease → std file-lock API, nono as a unix-only dependency behind an
honest-unavailable backend stub, the sync_dir directory-open root cause
(os error 5), `home_dir` via USERPROFILE, safe-read first passes, and
gating the unix-semantics test batch — full story in
`.agents/notes/0050-windows-bringup-green.md`. Remaining (non-gating,
continue-on-error): windows integration triage, windows clippy, PATHEXT
resolution, ACL mapping, Job Objects tree-kill, restricted-token
sandbox.

## Cross-check status (2026-10-02 re-verification)

The dev machine cannot even TYPE-CHECK for Windows today, one step before
the n0039 TLS blockage: the active toolchain is Homebrew rust 1.97.1
(`rustc --print sysroot` → `/opt/homebrew/Cellar/rust/…`), whose sysroot
carries no `rustlib/x86_64-pc-windows-msvc` std at all; `rustup target
add` reports "up to date" against a toolchain it does not manage (a
no-op), so `cargo check --target x86_64-pc-windows-msvc` dies with
`E0463: can't find crate for core` on the first std-depending crate
(serde_core, itoa, memchr, …). Also absent: NASM, mingw, CMake — the C
toolchains both aws-lc-rs and ring need for msvc. Conclusion unchanged
and sharpened: every Windows gate (cross-check, then bring-up) requires
the user-provided runner; nothing code-side remains before it.

## Cross-check from macOS (2026-10-01, n0039)

Attempted: `rustup target add x86_64-pc-windows-msvc` + `cargo check
--target` over the workspace and over the pure-logic crate subset. Both
stop at the TLS dependency chain: `ureq → rustls 0.23 → aws-lc-rs`
(default provider) and `ring` — their build scripts need a target C
toolchain that does not exist for msvc targets on macOS (no MSVC, and
aws-lc-sys needs CMake for cross). This is a property of the dependency,
not of okra's code; rustls 0.23 has no pure-Rust provider on the default
feature path.

Consequences:
- The `.github/workflows/windows.yml` runner IS the compile gate (live
  since 2026-10-02; triage its runs in the order of work below).
- The platform shims above are unchanged in scope; nothing in this
  session's work (n0028–n0038) added new unix-only paths to the
  pure-logic crates (protocol/kernel/policy/compaction/memory/session/
  workflow/gateway/tui stay OS-free; the new sanctioned spawn sites are
  in apps/okra, which is already unix-gated via portable-pty and the
  sandbox).
