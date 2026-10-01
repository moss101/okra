# N0039 — the Windows cross-check from macOS: blocked at the TLS chain, documented

- **Status:** implemented
- **Decided:** 2026-10-01
- Because: M6's inventory (docs/m6-windows-port.md) needs a compile
  signal; the runner is a user decision, so the cross-check was the
  available signal.

## Decision

Attempted `cargo check --target x86_64-pc-windows-msvc` (full workspace
and the pure-logic crate subset). Both stop at `ureq → rustls 0.23 →
aws-lc-rs`/`ring` build scripts — they need a target C toolchain absent
on macOS for msvc targets. rustls 0.23 has no pure-Rust provider on the
default path; this is a dependency property, not okra code. The result
and its consequences are recorded IN the M6 doc (dated section):
the armed `.github/workflows/windows.yml` runner stays the gate, and
n0028–n0038 added no new unix-only paths outside the already-gated
apps layer.

## Evidence

- `cargo tree -i aws-lc-sys --target x86_64-pc-windows-msvc`: the chain
  resolves through okra-host/okra-providers (the ureq users) only.
- docs/m6-windows-port.md § "Cross-check from macOS (2026-10-01)".
