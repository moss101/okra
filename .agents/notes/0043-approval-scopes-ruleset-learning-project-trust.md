# N0043 — approval scopes, ruleset learning, and project trust (#53, #24, #25)

- **Status:** implemented
- **Decided:** 2026-10-01
- Closes: MASTER-PLAN §3 #53 ("Approval UX: Allow once / this conversation /
  always / Deny"), §3 #24 ("Project rulesets +
  `suggestedPermissionUpdates` learning"), §3 #25 ("Project trust gating:
  project content inert until trusted").

## Decision

1. **The outcome union stays the closed deepseek four** (#53). Exactly one
   outcome grants (`allowed-once`); the pinned invariant is untouched.
   The USER-CHOSEN lifetime rides beside the outcome as
   `policy::ApprovalScope::{Once, Conversation, Always}`:
   - `Once` (default) — exactly the pre-#53 behavior: an arg-hash-bound
     conversation grant (a retry of the identical approved bytes never
     re-prompts).
   - `Conversation` — that PLUS a `SessionToolGrant` bound to
     `(tool, behavior version, policy version)` WITHOUT the args hash:
     the weaker binding the user explicitly asked for, never minted
     implicitly, dies with the session (and with `revoke_persistent`).
   - `Always` — both of those, plus a persistent arg-hash grant when the
     host permits persistence, plus `suggests_rule = true`: the caller
     MUST surface a ruleset suggestion; silent cross-session persistence
     of an un-hashed tool grant is not a thing okra does.
   Channels answer through `answer_scoped` (default: `answer` → scope
   `Once`, so every pre-existing channel is unchanged and the default is
   the tightest scope). Denials always carry no scope, whatever the
   channel claimed (`normalize_answer` clamps). The audit
   `approval/decided` event gained an OPTIONAL `scope` field — logs
   written before this note replay unchanged.
2. **Ruleset learning proposes, a human disposes** (#24). Granted
   decisions (tool + coarse path prefix = first path component) feed a
   daemon-side `RulesetLearner`. `GET /api/rules/suggestions` returns
   lattice-aware, dismissal-aware proposals; `POST /api/rules
   {action:"apply"}` inserts the rule into the shared lattice AND
   persists it to workspace settings under the declared catalog key
   `permissions.rules` (workspace scope wins). A deny rule for the same
   tool+prefix refuses the apply (409). Suggestions never contradict the
   lattice: an already-answered tool+prefix (deny, ask, or allow) is not
   suggested again, and only GRANTED decisions are evidence.
3. **Project trust gates activation, not conversation** (#25). A
   user-scope store (`~/.okra/trusted-projects.json`) maps project root →
   digest over the RELATIVE paths + content hashes of the
   workspace-provided content (`.okra/skills/**`, `.zcode/commands/**`,
   `.okra/config.json`, `.okra/settings.json`). Trust binds to the
   digest: one byte of drift re-gates (a malicious PR cannot ride in on
   an old click); moving the project directory does not. Untrusted (or
   changed) → the daemon holds workspace skills INERT on every turn and
   reports honestly over `GET /api/trust`
   (`untrusted`/`changed`/`trusted`/`none`); the workbench shows a banner
   with an explicit trust action (`POST /api/trust`). User-scope content
   (`~/.okra/skills`) is NEVER gated — trust gates what the PROJECT
   brought in, not what the user installed. An empty gated set needs no
   trust. A corrupt store fails closed and quarantines (`.json.corrupt`).

## Workbench surface

- The approval card's Allow is a split button: **Allow once** (default) +
  a ▾ menu with *Allow for this conversation* / *Allow always (suggest a
  rule)*. After an allow, the UI refetches suggestions; `always` also
  re-checks trust.
- Suggestion cards render under the topbar with **Add rule** /
  **Not now** (dismiss persists per daemon lifetime).
- The trust banner renders on load when the project is untrusted or its
  gated content changed.

## Evidence

- `crates/policy/src/approval.rs` (scope + scoped waterfall + audit),
  `crates/policy/src/grants.rs` (`SessionToolGrant`,
  `record_approval_scoped`), `crates/policy/src/learning.rs`
  (`RulesetLearner` + unit tests), `crates/policy/src/trust.rs`
  (`ProjectTrustStore` + drift/regate/quarantine tests).
- `crates/policy/tests/policy_plane.rs`: scope-rides-only-on-grants,
  plain-channels-default-to-once, conversation-scope session grant +
  revocation, always-scope persistence + suggestion flag +
  AlwaysPrompt refusal, ruleset learning end-to-end (5 new tests).
- `crates/agent-core/tests/agent_loop.rs`: conversation scope grants a
  second DIFFERENT-args write without a second prompt (ask counter == 1)
  while scope once still prompts twice; `ApprovalGranted` events carry
  the scope.
- Daemon: `resolveApproval` accepts `scope`; `SurfaceApprovalChannel`
  stores scoped answers; mediator forwards scopes (consensus keeps the
  unanimous-outcome rule, scope = tightest claimed); the turn loop seeds
  its lattice from the shared one and feeds the learner.
- `cargo test -p okra-policy --test policy_plane` 16/16;
  `cargo test -p okra-agent-core --test agent_loop` 12/12.
