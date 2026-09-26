# N0006 — N0004 revisit: kernel sandbox via nono directly; OpenAI-compatible provider

- **Status:** implemented
- **Decided:** 2026-09-26

## Decision (sandbox half — the N0004 revisit trigger)

N0004 said: re-evaluate extracting grok's `xai-grok-sandbox` at M1. The
extraction was inspected and REJECTED in its wholesale form, with a better
finding: the wrapper's kernel layer is the **public crate `nono = "=0.53.0"`**
(Landlock on Linux, Seatbelt on macOS; pinned exact per the donor's
deny-precedence note). okra now depends on `nono` directly
(`okra-policy/src/nono_backend.rs`) and ports the profile→capability mapping
with citations — avoiding the 6k-line extraction and its two internal `xai-*`
dependencies while using the identical kernel primitives.

- `SelfConfinement` trait: `Sandbox::apply` is irreversible process-wide
  confinement — the honest primitive for okra's in-process tool model
  (N0001). The whole process, including okra's own tool code, is confined.
- `--sandbox read-only|workspace-write` on the CLI applies it before the
  turn; network follows the mode (blocked under read-only/strict — the
  model provider needs it under workspace-write).
- System temp dir is NOT admitted by default: okra's atomic writes use temp
  siblings inside the target directory, so the global temp grant would
  widen the surface and silently cover workspaces living under it.
- Enforcement honesty: unsupported platforms fail closed
  (`SANDBOX_UNAVAILABLE`); `workspace-write` reports Full only when the
  kernel layer confirms support.

## Evidence (real kernel verdicts, macOS)

- `kernel_confinement.rs`: probe child applies read-only confinement, then
  `read_inside=ok write_inside=denied write_outside=denied` — Seatbelt
  denies the writes with EPERM.
- CLI: `--sandbox workspace-write` completes a write task
  (`enforcement=Full`); `--sandbox read-only` makes the same write fail with
  "Operation not permitted (os error 1)" — the kernel speaking through the
  tool error.

## Decision (provider half)

`okra-providers/src/openai.rs`: sync `ureq` (rustls) client speaking the
OpenAI `/chat/completions` wire behind the `Sampler` seam; errors map into
the closed taxonomy (401 → uncharged park, 429 + Retry-After → rate-limit
park governor, context-length → compaction path, 5xx → transient retry,
else permanent). Tools travel as function definitions; responses map to
`StopReason` 1:1. Tested against a local HTTP server (7 tests); live use via
`okra --provider openai --model <m>` with `OKRA_API_KEY`.

## Evidence

- crates/providers/src/openai.rs (7 tests, local HTTP server)
- crates/policy/src/nono_backend.rs + crates/policy/tests/kernel_confinement.rs
- apps/okra/src/main.rs (`--provider`, `--sandbox` flags)
