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
M2  context / memory / skills   ✔ #28 microcompaction, #29 hydration,
                                  #33 memory recall, compaction (prefire
                                  two-pass + validated install + byte-stable
                                  head), #45 hooks (20 events, deny>ask>
                                  allow), #47 MCP client + use_tool funnel
                                  — benchmark green in BOTH compaction
                                  profiles (layered + summary-only);
                                  skills + memory wired into EVERY live
                                  surface (n0028: daemon/stdio/CLI/ACP
                                  turns are continuations — world head
                                  seeded pre-install, path-conditional
                                  skill activation live, turns chain);
                                  agent-quality bench (#65) green 5/5
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
                                  fail-closed path handling); Rhai workflow
                                  ENGINE live (n0030: step seam, budget-
                                  honest cancellation, journal-first;
                                  steps run as full child turns via
                                  `okra workflow run`) + workflowRun.*
                                  protocol ops (n0032, differential-
                                  validated byte-identical vs the TS
                                  donor) + validation passes (n0036:
                                  taint + causality, pre-run); REWIND over
                                  the wire (n0029: writes capture
                                  checkpoints, `POST /api/rewind`
                                  restores files+git+context, workbench
                                  button, git reset to the captured HEAD
                                  e2e-proven); the workflow→wire LINK
                                  live (n0041: POST /api/workflow/run —
                                  validate → engine → live workflowRuns
                                  projection, change-only deltas via the
                                  ported diff to EVERY surface, protocol
                                  reducer reconstructs the run; TypeSafe
                                  Jev step gate on autonomous step text,
                                  inert by default); approval SCOPES +
                                  ruleset learning + project trust
                                  (n0043: #53 once/conversation/always —
                                  the outcome union stays the closed
                                  four; #24 granted decisions become
                                  human-confirmed project rules persisted
                                  to settings; #25 workspace content
                                  inert until the project is trusted at
                                  its content digest, drift re-gates);
                                  G2 live PASSED vs a real
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
                                  **TUI pager** (n0031: `okra tui` — ratatui
                                  block scrollback with scroll-lock over
                                  real turns; `--minimal` plain mode).
                                  **Mediation live** (n0033: first-responder/
                                  designated/consensus/local-only over
                                  scoped clients, `--mediation` flag,
                                  fail-closed). **Sessions page** (n0038:
                                  full-canvas searchable sessions surface —
                                  a peer of the transcript, Devin-style).
                                  **TUI as a daemon SURFACE** (n0042:
                                  `okra tui --attach` — see + steer +
                                  y/n-approve a LIVE session over the
                                  v4 NDJSON protocol, scroll-locked;
                                  `--smoke` drives it headless for CI;
                                  late subscribers see replayed rows).
                                  **Designated mediation DIVERGES**
                                  (n0042: live-attachment probe — no
                                  browser, no approvals, and
                                  resolveApproval refused with
                                  okra.mediation.designatedAbsent;
                                  e2e-proven both ways).
                                  #56 headless set COMPLETE (n0040:
                                  --json-schema contract, --tools globs,
                                  --worktree runs in a REAL created
                                  worktree); #58 apps/web CLOSED as
                                  superseded by the served workbench.
M5  differentiators ✔ in-turn `subagent` tool (n0034: REAL git worktree,
                                  confined child turn, inherit-nothing,
                                  grants never inherited, work collected
                                  on a branch — G5 isolation as a tool the
                                  model calls); embedding-based skill
                                  retrieval (n0035: two-tier, offline
                                  hashed default + OpenAI-compatible
                                  network tier, RELEVANT vs ACTIVE);
                                  workflow validation passes (n0036);
                                  computer control as a standalone MCP
                                  server (n0037: `okra mcp-serve --computer`,
                                  consent from launch config, fail-closed)
M6  scale                      ◐ i18n slice: daemon-side en-US/zh-CN catalog
                                  domain (locale negotiation, fallback chain,
                                  interpolation; the UI catalog stays in the
                                  reused TS UI); Windows bring-up LIVE —
                                  the repo is on GitHub
                                  (github.com/moss101/okra, pushed 2026-10-02)
                                  and the armed `.github/workflows/
                                  windows.yml` now runs on every push
                                  (triage per `docs/m6-windows-port.md`);
                                  **managed-pin
                                  loop closed end to end** — admin signs with
                                  `okra pin-sign` (Ed25519 envelope over
                                  sha256(payload), key provisioning via
                                  `--generate-key`), daemon verifies + trust-
                                  files gate, `pin-status` reports state honestly,
                                  and the pin is ENFORCED at runtime — every
                                  agent run clamps sandbox/max-turns and
                                  refuses non-allowlisted providers before
                                  any model call (fail-closed pins apply
                                  read-only confinement); serve --tcp
                                  refuses denied providers at startup;
                                  macOS→msvc cross-check attempted +
                                  documented (n0039: blocked at the
                                  rustls/aws-lc-rs C toolchain — the
                                  armed windows.yml runner stays the
                                  gate)
```


## G3 dogfooding week — DEFERRED BY USER DECISION (2026-09-28, note N0009)

Dogfooding will restart once the application is complete (user: "we
will start dog food once the app is complete — that would allow us to
fetch and track more bugs"). The daily automation was deleted before
its first fire; the M3 gate review moves with it.

What stands: day 1 (2026-09-28) ran for real — `--provider openai`
(glm-5.3-flash) driving write_file/list_dir/read_file through the
actual pipeline; run 1 cancelled honestly on a gateway timeout, run 2
completed (16 steps, 15k tokens). Both fixable day-1 findings were
closed same day (write mode preservation `282e511`; harness wc-padding
`3e0b011`). Day-1 artifacts remain banked in `docs/dogfood/` and the
journal; the harness (`scripts/dogfood-log.sh`) is ready for the
restart. **App-completion pass (2026-10-01, n0028–n0039):** the
then-pending engineering items closed — skills/memory wired into every
surface, Rhai workflow engine + workflowRun ops + validation, rewind
over the wire, the TUI pager, live mediation, the in-turn subagent
tool, embedding retrieval, the standalone computer MCP server, the
sessions page, and the Windows cross-check documented. Dogfooding
restart is the user's call per the original decision (it needs their
credentials and daily usage).

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
(decisions N0008 + N0009): tasks sidebar + streaming transcript + send/stop
composer, paired light/dark design tokens (ChatGPT2 docs/07 token
architecture), tool cards with status/duration/**and real output bodies**
(tool outputs are logged — model-visible means logged — and replayed;
pre-output logs fall back to status-only cards), steered-message chips,
turn dividers, in-app turn-complete toasts. It drives the full pipeline —
`--provider openai --model NAME` for a real network model, the same
four-tool registry as the CLI — and survives reloads: tasks list from the
SQLite index (`/api/sessions`), transcripts replay from the kernel log
(`/api/sessions/<id>/rows`). Stop is a first-class seam
(`CancellationCategory::UserRequested`, checked at every step boundary;
interrupted turns recover through the standard repair path). The UI is
embedded in the binary at compile time (`ui/`, dependency-free — no build
step, no node_modules).

**Transcript virtualization (U1, N0011):** the transcript is a custom
windowed renderer with layout checkpoints (ChatGPT2 docs/07 design): rows
are keyed by stable id and reconciled by mutation signature — markdown is
rendered exactly once per row, tool-card expansion survives re-renders —
and only the viewport ± overscan exists in the DOM, positioned by top/
bottom spacers over a checkpoint map of real measured heights (scroll-
anchored so content never jumps when checkpoints refine). A 2k-row
streaming task stays bounded and smooth; verified live at 127 rows with a
24-node window and across replay.

**Semantic wander governor (N0021):** with `TYPESAFE_API_KEY` set, the
turn loop asks TypeSafe's Jev (every 3rd step from step 4, max 6
judgments) whether the agent is progressing or going in circles; the
verdict feeds the existing nudge path. Class-gated (`repeating`/
`offtrack` — calibrated live), fail-open, inert without the key
(`OKRA_SEMANTIC_WATCH=off` disables).

**Computer control (N0023):** the Claude Desktop flagship, real on
macOS — `computer_observe` (AX tree: element ids/roles/labels/positions),
`computer_act` (batch click/type/press_key, AXPress-structural when the
element reports it, coordinate fallback from AX positions, stop-on-first
error, re-observe after every action, user_actively_typing guard), and
`computer_screenshot` (PNG data URL rendered in tool cards). All three
prompt — the approval card IS the split consent (per-app capability for
act, screen-takeover for screenshot); read-only marking was deliberately
avoided so observing the screen prompts too. Binaries env-overridable
(`OKRA_OSASCRIPT`/`OKRA_CLICKER`/`OKRA_SCREENCAPTURE`) for hermetic tests.

**Skills management (N0024):** the Tools tab manages skills — install
(writes `.okra/skills/SKILL-<name>.md` with sanitized names), disable /
enable (`.disabled` suffix rename — disabled skills leave the activation
path with zero extra state), and delete. Files are the database:
inspectable and git-committable with the workspace.

**Persistent MCP sessions (N0022):** one live child process per MCP
server for the daemon's lifetime (spawn-once piped stdio in the
sanctioned module, reader thread, bounded requests) — stateful servers
keep state and stateless ones pay startup once. Proven by an
initialize-exactly-once assertion and a cross-turn stateful counter.

**MCP tools in turns + the question flow (N0019/N0020):** probed MCP
tools register into every turn as `mcp__<server>__<tool>` and are
APPROVAL-GATED (one-shot tools/call through the sanctioned runner) —
"mcp tool" flows through the same card as native writes. The `ask_user`
tool lets a task ask the user mid-turn: a question card (answer input)
blocks until answered; the answer returns into the turn as the tool
result, and the question-class notification (the last reserved class)
fires on ask.

**MCP runtime status (N0018):** the Tools tab can now answer "does it
connect?" — `POST /api/mcp/probe` runs initialize + tools/list against
configured stdio servers on a bounded thread and caches the status
(connected · tool count / timeout / error) that the listing merges. 
Probing is explicit POST, listing stays pure.

**Composer mentions + attachments (N0017):** `@` in the composer
completes workspace file paths (daemon-side recursive search, confined);
attached files fold their CONTENT into the logged, model-visible user
message (16 KB/file, 48 KB/turn), with the attachment list on the row for
the transcript chips. Traversal pathspecs are refused pre-surface.
Steering (mid-turn sends) carries attachments too: entries fold into the
model context at the next step boundary via the loop's own steering inbox
(verified by receipt rows with attachment chips).

**Tools tab (N0015):** a fourth sidebar tab projects the installed
skills (`.okra/skills/*.md` — name, description, path-conditional
patterns) and the configured MCP servers (workspace + user scopes,
enabled flag, source, launch summary) as read-only surfaces. All
`/api/*` routes now live at the top level of the HTTP dispatch — the
route-nesting trap that 404'd POSTs is structurally gone.

**Staging + commit (N0014):** the Changes tab acts — staged/unstaged
sections from the raw porcelain codes, per-file `+`/`−` stage toggles,
and a commit box over `POST /api/git/stage|unstage|commit`. Commits are
STAGED-ONLY (the working tree is never swept in); refusals are honest
(empty message, nothing staged, non-repo).

**Notifications (N0013):** the 3-class boundary is live — the daemon
classifies and REDACTS (`v4/notification` frames on turn completion and
permission asks; labels are metadata, content never leaves the process)
and the surface applies the focus policy: focused → in-app toast,
unfocused → Web Notification with only the redacted label. Background
tasks accrue a sidebar unread dot that clears on selection.

**Terminal pane (N0012):** a collapsible PTY pane below the transcript —
a real interactive shell in the workspace, streamed over
`GET /api/term/<id>/sse` with keystrokes typed through
`POST /api/term/<id>/keys` (serialized client-side; ^C, arrows and paste
work). The renderer is a small ANSI/CSI-stripping text processor — the
dogfood loop (ls, git status, echo, ^C), not a full curses emulator.
Sessions persist across reloads; `/close` prunes.

**Changes tab (N0010):** the third sidebar tab answers "what did the
agent change?" — branch + working-tree status with code badges, and a
per-file unified diff in the preview drawer (`/api/git`, `/api/git/diff`;
read-only; `.okra-sessions` never surfaces; honest `repository:false`
outside a repo). An approved write shows up here within seconds.

**Attended approvals (N0009):** the workbench ASKS — side-effecting tools
pause the turn on an inline approval card (approved bytes shown, Allow
once / Deny), resolved over the same v4 seam (`resolveApproval` command);
stopping cancels a pending ask; the audit pair (`approval/asked` +
`approval/decided`) lands in the kernel log and replays. Read-only tools
never prompt; the stdio G0 bridge stays unattended. **File surfaces:** a
Files tab (workspace-confined tree — dot entries and symlink-following
excluded) and a safe-read preview drawer (`/api/file`, O_NOFOLLOW, 256 KB
cap); file tool cards link straight to the preview.

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

## Desktop shell (dev)

`apps/desktop` is a thin Electron main (MASTER-PLAN block #50: window +
daemon lifecycle only — every product behavior stays in the daemon and its
web workbench):

```sh
cargo build --release          # or target/debug is found too
cd apps/desktop
npm install                    # electron
npm start -- --cwd /path/to/project
```

The shell spawns `okra serve --tcp --cwd`, waits for the handshake (bind
+ `/health`), and points the window at the workbench. Closing the window
stops the daemon. `npm run smoke` verifies the handshake without a window;
`npm test` runs the handshake acceptance against the real binary with
plain node (no electron needed).
