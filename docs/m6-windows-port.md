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
| Process groups (login-shell capture, terminal) | `runtime_env::RealLoginShellExecutor` (process_group(0), kill(-pid)), `terminal.rs` (portable-pty) | **LANDED**: the capture deadline kill uses `taskkill /PID /T /F` (tree kill via the OS, CREATE_NO_WINDOW); Job Objects remain the fuller shim for auto-kill-on-close semantics; portable-pty has a ConPTY backend (the rendering stall is tracked separately) |
| Safe-read (O_NOFOLLOW\|O_NONBLOCK) | `safe_fs.rs` — LANDED: windows first pass (symlink pre-check + fstat verification carry the contract; `enforcement_level()` reports `partial` and the preview API surfaces it). ACL second pass — LANDED (2026-10-03): `safe_fs::everyone_has_write_access` reads the DACL via windows-sys (GetNamedSecurityInfoW → Everyone SID trustee → GetEffectiveRightsFromAclW) and `safe_read` refuses Everyone-write files; `enforcement_level()` is `full` on both platforms, verified by the runner's own test run FILE_FLAG_OPEN_REPARSE_POINT + GetFileInformation checks; the ownership/mode ladder re-maps to ACLs (first pass: report `enforcement: partial`) |
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

## HARDENED GATE GREEN (2026-10-03, run 37112803251)

The continue-on-error tolerances are REMOVED: integration tests and
clippy (`-D warnings`) now FAIL the run on regression. First hardened
run: fully green (build + unit + 1,040 integration tests + clippy, zero
warnings). The last whack-a-mole items were windows-only dead imports in
`g5_real_sampler`/`g5_full_loop` behind the unix test gates.

## ACL SECOND PASS — implementation sketch (next session's first item)

`windows-sys =0.59.0` is already a cfg(windows) dependency of `okra-host`
(commit history: "capability pass 2"), features enabled: Foundation,
Security, Security_Authorization, Storage_FileSystem, System_JobObjects,
System_Threading + add System_Memory (LocalFree).

Design: `safe_fs::everyone_has_write_access(path) -> io::Result<bool>` —
1. wide-path → `GetNamedSecurityInfoW(path, SE_FILE_OBJECT=1,
   DACL_SECURITY_INFORMATION, …, &mut dacl, …, &mut sd)`
2. Everyone SID: `AllocateAndInitializeSid(SECURITY_WORLD_SID_AUTHORITY,
   1, 0,…, &sid)` → `BuildTrusteeWithSidW(&mut trustee, sid)`
3. `GetEffectiveRightsFromAclW(dacl, &mut trustee, &mut rights)`
4. refuse when `rights & (FILE_WRITE_DATA | FILE_APPEND_DATA |
   GENERIC_WRITE) != 0`; free via `FreeSid` + `LocalFree`
5. wire into `safe_read`'s cfg(not(unix)) branch → new
   `SafeReadError::WorldWritableAcl` variant; `enforcement_level()`
   flips to `full` when the check is wired.

CAUTION (this session's lesson): the draft in git history ("capability
pass 1" revert, c-series) guessed several API details and could not
compile-verify locally — write it against the windows-sys 0.59 docs and
let CI converge over 1–2 cycles; the hardened gate will red until it
compiles.

## SECOND PASS: CLEAN (2026-10-03, run 37103997439)

The windows pipeline is fully green with NO annotations: build ✓,
`--lib` ✓ (all suites), integration ✓ (all non-gated suites), clippy ✓
(zero warnings — the windows-only dead-code batch from the test gates
was swept). Everything remaining (PATHEXT resolution, ACL mapping,
Job Objects tree-kill, restricted-token sandbox, the ConPTY
terminal-emulator layer) is NEW capability work, not red tests.

## SECOND PASS PROGRESS (2026-10-03)

- The integration step COMPILES and RUNS on windows (436 tests passing in
  run `37097302681`; it died at rustc before this round's fixes).
- g1 kill-matrix GREEN on windows: the kill contract is now platform-
  aware (signal on unix; STATUS_CONTROL_C_EXIT hard-termination family
  on windows — a clean exit still fails).
- Integration triage DONE (second pass): MCP fixtures moved to a
  cross-platform `fake-mcp` test binary; computer-control tests gate to
  macOS (AX by design); the PTY test gates to unix pending the ConPTY
  terminal-emulator layer.
- Restricted-token sandbox SCOPE (the g5 unlock): the confined-child
  spawn sites are `apps/okra/src/subagent.rs` (2 `Command::new` sites).
  Windows shape: `OpenProcessToken(GetCurrentProcess)` →
  `CreateRestrictedToken` (drop every privilege via
  `RemoveAllPrivileges`, add `S-1-1-0` deny for write bits) →
  `CreateProcessAsUserW` with the restricted token; features
  `Win32_Security` + `Win32_System_Threading` are already enabled in the
  windows-sys dependency.
  VERIFIED SIGNATURES (windows-sys 0.59 source, local registry):
  - simplest strong form: `CreateRestrictedToken(tok,
    DISABLE_MAX_PRIVILEGE, 0, null, 0, null, 0, null, &new)` — dropping
    ALL privileges needs no SID/LUID list plumbing;
  - `CreateProcessAsUserW(token, app, cmdline(PWSTR — MUTABLE wide
    buffer; args must be re-quoted), sec_attrs, sec_attrs,
    inherit_handles, creation_flags, env(wide, double-NUL), dir,
    startupinfo, &procinfo)` — the env block + stdout/stderr pipe
    inheritance via STARTUPINFOW are the remaining plumbing;
  - `OpenProcessToken(GetCurrentProcess,
    TOKEN_DUPLICATE|TOKEN_QUERY, &tok)` first.
  Honest limit: the daemon cannot PROVE the kernel enforcement from the
  parent — the g5 kernel-verdict tests (run a probe write outside the
  grant, expect refusal) stay the gate and would move from
  fail-closed-refused to pass-with-token.
- Subagent launches on windows: **LANDED** — the confined child spawns
  under a restricted token (every privilege dropped via
  DISABLE_MAX_PRIVILEGE) through CreateProcessAsUserW with pipe
  inheritance; the g5 confined-launch tests are un-gated and pass on the
  runner.
- Subagent launches FAIL CLOSED on windows (by design): the nono stub
  reports Unavailable and the g5 launcher refuses confined children —
  the G5 contract holding. Unlocks with the restricted-token sandbox.
- Rewind-removal triage RESOLVED: the original red was the gate-release
  race (409 "turn in flight" right after completedSuccess, before the
  turn thread releases running_turns) — the tests retry on 409 and PASS
  on windows (run 37114039717: g4_rewind 3/3 green).
- Rewind-removal triage: `POST /api/rewind` returns 200 but an
  absent-before file survives on windows (run 37099210769,
  g4_rewind.rs:228) — candidates: composed-before recording on the
  windows write path, or a remove_file sharing violation. Next fix.
- ConPTY diagnostic RESULT (runs 37106132295/37107367246): the DSR
  reply (with bounded retries) does not unstick rendering — the session
  streams only the probe. One run passed the full flow (timing luck).
  Conclusion: the terminal-emulator layer (win32-input-mode, sequence
  handling) is REQUIRED for windows PTY, not a probe reply.
- ConPTY next diagnostic step: with the DSR reply in place, the typed
  marker still never echoes within 30s (the PTY streams only the probe
  frame) — the next cycle should instrument the keys path (does the
  ConPTY input pipe accept the write? does cmd.exe echo?) with the test
  re-enabled on windows and FULL frame dumps.
- Gate-release race (timing note for test authors): the projection flips
  to `completedSuccess` BEFORE the turn thread releases `running_turns`
  — an immediate follow-up call can get the honest 409 "turn in flight".
  Tests retry on 409 (see g4_rewind).
- ConPTY findings: conhost opens every session with a DSR probe
  (ESC[6n); the pump answers it, but rendering still stalls — the real
  work is a terminal-emulator layer (win32-input-mode / sequence
  handling), not just the probe reply. Terminal open/resize/keys/close
  endpoints themselves work (the PTY test's earlier gates passed).

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
