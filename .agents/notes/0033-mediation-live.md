# N0033 — multi-client mediation goes live (the four policies, fail-closed)

- **Status:** implemented
- **Decided:** 2026-10-01
- Closes: MASTER-PLAN §3 #26 (M4): "Multi-client permission mediation:
  first-responder / designated / consensus / local-only" — the closed
  enum existed since M0 (`lattice.rs`); the mediator did not.

## Decision

1. `policy::mediation::Mediator` implements `ApprovalChannel` over N
   scoped clients (`Client { id, local, channel }`):
   - `first_responder`: first answer wins, in attach order; later
     clients are not consulted.
   - `designated`: ONLY the named client's channel is asked; a missing
     designation or a missing client answers NOTHING (the ask falls to
     the service waterfall → unavailable → deny). Never falls back to
     another client.
   - `consensus`: every attached client must answer `allowed-once`;
     one rejection rejects; one silence is not consent (no verdict).
   - `local_only`: remote clients are never asked; locals answer
     first-responder among themselves.
2. **Daemon wiring:** `okra serve --tcp --mediation
   first-responder|designated|consensus|local-only [--designated ID]`.
   The workbench bridge registers as the one attached client ("workbench",
   local — loopback); first-responder with one client is byte-identical
   to the old direct-bridge behavior. `designated` without
   `--designated` refuses at STARTUP (a config error, not a hanging ask).

## Evidence

- `crates/policy/src/mediation.rs` tests: first answer wins and later
  clients are never consulted; designated-only (missing designation →
  no verdict, never another client); consensus (all-yes grants,
  silence-withholds, one-no rejects, empty attaches nothing);
  local-only never asks remotes (ask-counter asserted zero).

## Because

One attached client makes every policy coincide today — the deliverable
is the fail-closed SEMANTICS (unit-proven) and the live seam: the next
attached surface registers as a client and the policies diverge
immediately, with no further changes to the turn path.
