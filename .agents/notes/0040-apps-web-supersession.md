# N0040 — `apps/web` is superseded by the daemon's served workbench; plus the follow-up pass (tier wiring, ACP numbering, #56 flags, stdio checkpoints)

- **Status:** implemented
- **Decided:** 2026-10-01

## apps/web (#58) — SUPERSEDED, not pending

MASTER-PLAN §2/§3 #58 planned a separate `apps/web` ("same UI, replayable
profile, loopback-only"). Decision N0008 (the workbench web shell) already
delivered that AS the daemon: `okra serve --tcp` serves the same embedded
UI at `/`, sessions replay from the kernel log over HTTP
(`g4_sessions_index_and_replay` proves the replayable profile), and the
bind refuses non-loopback (serve_tcp's loopback gate). A second binary
would duplicate the surface with no contract of its own. The take-list
line is CLOSED as superseded-by-N0008 rather than left as phantom scope.

## Evidence

- Because: two artifacts serving the same UI with the same profile is
  drift, not breadth.
- Evidence: `GET /` + `/api/sessions/<id>/rows` replay (apps/okra
  tests g4_sessions_index_and_replay, g4_http_surface), loopback-only
  bind check in `serve --tcp` startup.

## Follow-up pass (same note; small honesty gaps found in the
n0028–n0039 audit, each closed with tests)

1. **Embeddings network tier actually wired** — the n0035 comment
   promised `OKRA_EMBEDDINGS=on` engagement that the code never
   performed. `memory::retrieval::rank_relevant` now takes an injected
   network embedder (tests: network used when healthy, fallback on
   error AND on malformed output); serve builds the provider-backed
   closure only when the env gate + credential exist.
2. **ACP turn numbering** — prompts rebuilt the Agent, so kernel
   `turn/start` restarted at 1 every prompt (replay collapsed turns);
   `AcpSession` carries a monotonic ordinal and seeds the counter.
3. **#56 headless flags completed** — `--json-schema` prints the NDJSON
   LoopEvent contract (a drift-guard test serializes EVERY variant and
   asserts coverage); `--tools PATTERNS` filters the registry
   (`Registry::retain_matching`, `*` wildcards, drops REPORTED,
   empty-match refused); `--worktree` now REALLY runs the task in a
   worktree (existing dir used as-is, missing path CREATED via
   `worktree_add` — branch `worktree-<id>`, e2e-proven) instead of
   being validated and ignored.
4. **stdio checkpoints** — the stdio surface captures write checkpoints
   like the TCP daemon (per-session ordinals + durable mirror), so
   rewind is not TCP-only.
