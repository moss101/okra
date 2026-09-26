# N0002 — Product name "okra", crate namespace `okra-*`

- **Status:** implemented
- **Decided:** 2026-09-26

## Decision

The plan's "workbench" product is realized as **okra** at `/Users/mohsin/zee/okra`
(user instruction, 2026-09-26). All crates carry the `okra-` prefix; the
donor `x.ai/*` namespace never appears in our tree (MASTER-PLAN §1: "renamed
to ours"; enforced by `deny.toml` wildcard ban).

## Why

A single owned namespace keeps crate names available and makes the
fork-ownership boundary mechanically checkable.
