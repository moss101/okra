# okra

One Rust daemon (agent + host services) that any surface drives over one typed
protocol — desktop shell, web, TUI, headless/CI, editors via ACP — with
kernel-enforced safety, an event-sourced session log as the only durable truth,
and the workbench-shell polish (notifications, file management, computer
control) that no reference product gets right.

This is the implementation of
[`work/20260925-nextgen-workbench/MASTER-PLAN.md`](../work/20260925-nextgen-workbench/MASTER-PLAN.md)
(the "workbench" plan, realized under the okra name — decision N0002).

## Status

Milestone **M0 (skeleton)** is implemented and green; **M1 (agent + CLI
parity)** is partially landed. See `ARCHITECTURE.md` for the block-by-block
map (block → source → crate) and `.agents/notes/` for binding decisions.

```text
M0  skeleton                    ✔ workspace, policy files, protocol port with
                                  TS-ground-truth convergence tests, kernel
                                  event log, read_file end-to-end, headless CLI
M0  gate G0                     ✔ ZCode's existing Electron UI displays a
                                  real turn produced by the okra daemon
                                  (okra serve + host-side bridge, N0005)
M1  agent + CLI parity          ✔ multi-file coding tasks via `--task`
                                  (write/edit/verify + corrective writes);
                                  kill matrix recovers from ALL 6 durable
                                  boundaries — no double-exec, no torn state;
                                  OpenAI-compatible provider (`--provider
                                  openai`); kernel sandbox via nono
                                  (`--sandbox`, Seatbelt/Landlock)
M2  context / memory / skills   ◐ #28 microcompaction, #29 hydration,
                                  #33 memory recall, compaction (prefire
                                  two-pass + validated install + byte-stable
                                  head), #45 hooks (20 events, deny>ask>
                                  allow), #47 MCP client + use_tool funnel
                                  — benchmark green in BOTH compaction
                                  profiles (layered + summary-only);
                                  remaining: skills, agent-quality bench
M3  host services + alpha       ◐ strangler: SQLite session/task index
                                  (`okra sessions`), terminal/PTY domain
                                  (real PTY, TTY-verified), git domain
                                  (status/commit/REAL worktrees — the G5
                                  grant surface), user/slash commands domain
                                  (parser, discovery order, enable
                                  overrides, plugin command roots),
                                  BigModel entitlement + team-plan API keys
                                  (three-valued entitlement, envelope
                                  contract, ensure flow), Claude/Gemini
                                  plugin converters (#46 CLOSED — data-only,
                                  fail-closed path handling); G2 live PASSED vs a real
                                  network model (glm-5.3-flash) INCLUDING
                                  the FULL 100-turn / 1,352-read live
                                  profile: flat context (peak 19.9k ≤ 20k),
                                  4 installs / 0 emergencies, byte-stable
                                  prefixes, 2,690 hooks / 0 failures

M4  surfaces                  ◐ NDJSON two-surface + cross-surface steering,
                                  TUI render; **G4 breadth closed (2026-09-27):
                                  hands-on browser drive** — the served page
                                  sends a turn and steers it MID-TURN
                                  from a real browser engine over SSE
                                  (steeringQueued semantics + POST /steer; a
                                  concurrent sendText can no longer spawn a
                                  second parallel turn thread), while an
                                  NDJSON surface attached to the same daemon
                                  receives the identical projection frames.
                                  **Workbench web shell (2026-09-28, N0008):
                                  `okra serve --tcp` now serves a real task
                                  workbench at `/` (ui/ — tasks sidebar,
                                  streaming transcript with tool cards,
                                  send/stop composer, steering, paired
                                  light/dark design tokens), drives the REAL
                                  pipeline (`--provider openai --model`, full
                                  read/list/write/edit tool plane), lists
                                  tasks from the SQLite index
                                  (`GET /api/sessions`), replays any task
                                  from the kernel log after reload/restart,
                                  and cancels a live turn from the Stop
                                  button (CancellationCategory::UserRequested
                                  — honest interrupted phase, standard
                                  repair path).
                                  **ACP gateway live** (`okra serve --acp`:
                                  initialize/version negotiation, session/new,
                                  session/prompt on a worker thread with
                                  streamed session/update, honest stop
                                  reasons, and a REAL session/cancel —
                                  mid-turn abort via the same stop-flag seam
                                  as the web Stop button, session recovers
                                  on the next prompt; scripted-editor e2e
                                  green; a real editor drive awaits a
                                  user-armed environment per N0007)
                                  + **leader/roster** (one leader per daemon,
                                  term-bumped claims, `roster/claim`).
M5  differentiators ◐ (subagent kernel isolation)
M6  scale                      ◐ i18n slice: daemon-side en-US/zh-CN catalog
                                  domain (locale negotiation, fallback chain,
                                  interpolation; the UI catalog stays in the
                                  reused TS UI); Windows bring-up armed —
                                  `.github/workflows/windows.yml` +
                                  `docs/m6-windows-port.md` inventory (runner
                                  itself is a user decision); **managed-pin
                                  loop closed end to end** — admin signs with
                                  `okra pin-sign` (Ed25519 envelope over
                                  sha256(payload), key provisioning via
                                  `--generate-key`), daemon verifies + trust-
                                  files gate, `pin-status` reports state honestly
```


## G3 dogfooding week — started 2026-09-28

Day 1: the harness (`scripts/dogfood-log.sh` + `docs/dogfood/day-01-2026-09-28.md`)
was produced BY okra — `--provider openai` (glm-5.3-flash) driving
write_file/list_dir/read_file through the real pipeline. Run 1 cancelled
honestly on a gateway timeout; run 2 completed (16 steps, 15k tokens).
Findings live in `docs/dogfood/`; week complete = 7 day entries.
Both fixable day-1 findings closed same day (write mode preservation
`282e511`; harness wc-padding `3e0b011`). Day-2 pre-flight (09-28
15:40 +03): binary current with all fixes, `--version` clean, key
parses (never printed), gateway 200 in 0.6 s, workspace + docs +
script bits verified, automation armed for 07:30 +03.

## Real-Zed ACP drive (2026-09-27)

Zed 1.21.0 drove the okra daemon end-to-end over the Agent Client
Protocol — the G4 editor leg with the real editor. `~/.config/zed/
settings.json` registers okra as a custom agent server launching
`scripts/zed-acp-wrapper.sh` (tees both wire directions to
`~/.okra-zed-acp/`). Evidence in one drive: Zed → `initialize`
(protocolVersion 1 negotiated, clientInfo zed/1.21.0) → `session/new`
(workspace cwd honored — the editor's project, not the daemon's launch
dir) → two `session/prompt` turns on ONE session id, okra streaming
`agent_message_chunk` + tool_call updates and replying `end_turn`;
Zed's Agent Panel rendered the user row, the read_file/list_dir tool
cards, and the assistant markdown. Wire logs + full-turn screenshots
captured.

## Workbench web shell (2026-09-28)

`okra serve --tcp` serves a real task workbench at `http://127.0.0.1:<port>/`
(decision N0008): tasks sidebar + streaming transcript + send/stop composer,
paired light/dark design tokens (ChatGPT2 docs/07 token architecture), tool
cards with status/duration/**and real output bodies** (tool outputs are
logged — model-visible means logged — and replayed; pre-output logs fall
back to status-only cards), steered-message chips, turn dividers, in-app
turn-complete toasts. It drives the full pipeline — `--provider openai
--model NAME` for a real network model, the same four-tool registry as the
CLI — and survives reloads: tasks list from the SQLite index
(`/api/sessions`), transcripts replay from the kernel log
(`/api/sessions/<id>/rows`). Stop is a first-class seam
(`CancellationCategory::UserRequested`, checked at every step boundary;
interrupted turns recover through the standard repair path). The UI is
embedded in the binary at compile time (`ui/`, dependency-free — no build
step, no node_modules).

## G0 demo (2026-09-26)

The ZCode Dev Electron app, launched with `ZCODE_OKRA_DAEMON=<okra binary>
pnpm dev:runtime` (ZCode working tree carries the marked G0 bridge patch —
see `.agents/notes/0005-g0-electron-bridge.md`), rendered a full turn from
the okra daemon: user message bubble, streaming assistant text, a read_file
tool card, and the file content okra's Rust toolchain read from the
workspace — over the standard `zcode-agent` v4 channel with no renderer
changes.

## Layout

```
crates/
  protocol/    zcode-protocol-v4 port: wire frames, fragmentation codec,
               delivery profiles, delta ops + coalesce, payload caps
  kernel/      event-sourced session log (deepseek contract) + JSONL storage
               (grok loss contract) + single-writer SessionHandle
  tools/       tool runtime: [Progress*, one Terminal] streams, ToolSpec
               (idempotent flag), policy metadata, ToolAccesses scheduler,
               spill store
  policy/      fail-closed approvals, arg-hash grants, confine(argv) with
               enforcement honesty, sandbox profiles, permission lattice
  agent-core/  turn phase machine (enum + match), governors, steering inbox,
               semantic termination + backstops, one task runtime
  providers/   sampler trait + scripted model stub + message shims
  compaction/  validated summary schema, origin tags, world-state projection
  memory/      tiered memory files + secret scanning
  session/     rewind checkpoints
  workflow/    run journal + budgets (Rhai engine: M3)
  host/        notifications policy, safe file service, automation guard
  gateway/     NDJSON event bus: ring replay, Last-Event-ID, slow-client eviction
  computer/    AX-first computer control contracts (M5)
  tui/         minimal scrollback writer (ratatui pager: M4)
apps/
  okra/        headless CLI: okra --json "prompt" (NDJSON event stream)
ui/            workbench web shell served by `okra serve --tcp` at /
               (vanilla HTML/CSS/JS, embedded at compile time — N0008)
```

## Build

```sh
cargo build            # workspace
cargo test             # unit + convergence + crash-recovery suites
cargo clippy           # day-1 bans enforced (clippy.toml)
```

The protocol crate's golden vectors are generated from the ZCode TypeScript
ground truth (`crates/protocol/tests/golden/generate.mjs`, kept with the
checked-in `golden-vectors.json`); regenerate only when the upstream contract
changes.

## Governance

- Decision records live in `.agents/notes/` (deepseek pattern); lifecycle
  `proposed → implemented → rejected|archived`, CI-checked.
- Crate dependency direction is enforced by `scripts/check-boundaries.sh`.
- Day-1 clippy bans: no raw process spawn, no raw `home_dir`, no raw
  `canonicalize`.
