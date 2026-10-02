# N0046 — memory agents: extract, dream, and MMR (#33/#34)

- **Status:** implemented
- **Decided:** 2026-10-01
- Closes the OPEN parts of MASTER-PLAN §3 #33 ("tiered memory + extract/
  dream/recall agents + secret scanning") and #34 ("vector index + MMR +
  dream consolidation"): recall, tiers, secret scanning, and the
  embedding index were already live (M2/M5, n0035); this note lands the
  extract agent, the dream agent, and MMR re-ranking.

## Decision

1. **Deterministic offline agents.** The donors run extract/dream as LLM
   calls; okra keeps the CONTRACTS with deterministic heuristics so the
   seams live without a key (mirroring `retrieval.rs`'s two-tier
   honesty):
   - `extract_memories(turn_text, existing)` proposes `MemoryCandidate`s
     (Preference / ProjectFact) from a turn's user text with per-candidate
     provenance (`source` = the sentence). Duplicates against stored
     memory are dropped by embedding cosine (≥ 0.72) — memory never
     re-learns what it knows. Standing-instruction MARKERS ("Always…",
     "Never…") stay in the stored text — the dream agent's contradiction
     detector reads them.
   - `dream(memories)` → `DreamReport { clusters, contradictions, total }`:
     greedy cosine clusters (≥ 0.62) with a suggested merge that is the
     LONGEST member (the most specific — never a fabricated synthesis),
     plus Always/Never-style contradictions over a shared topic.
   - `rank_mmr(items, query, k, lambda)` (grok memory-v2): greedy
     Maximal-Marginal-Relevance re-ranking — λ·relevance − (1−λ)·max
     redundancy with the already-selected set; below-floor items are
     never selected.
2. **Writes are never implicit.** Extract PROPOSES; a human accepts. The
   daemon extracts from the USER text of every turn (the model's text may
   echo), accumulates candidates daemon-lifetime, and exposes:
   - `GET /api/memory/suggestions` — the pending proposals;
   - `POST /api/memory {action:"accept", text, tier}` — appends ONE line
     to a memory tier (`TieredReader::append_tier`, idempotent, refuses
     empty lines, and REFUSES text the secret scanner flags — 422);
   - `POST /api/memory {action:"dismiss", text}`;
   - `POST /api/memory {action:"dream"}` — the consolidation report over
     the current tier contents (User + Team + Project lines).
3. `append_tier` appends; it never rewrites stored lines (a memory file
   is a list of lines, and an accept must not reorder or drop history).

## Evidence

- `crates/memory/src/agents.rs` (5 tests: extraction + kinds + casing,
  cosine dedupe, dream clustering with longest-member merge,
  Always/Never contradiction, MMR duplicate-suppression + floor).
- `crates/memory/src/tiers.rs`: `append_tier`/`lines_of` + idempotence
  test.
- Live daemon smoke: a turn whose user text states a preference produced
  a suggestion; accept appended to `~/.okra/memory.md`; a second turn
  stating the opposite, accepted, made `dream` report BOTH the
  contradiction and the near-duplicate cluster.
- `cargo test -p okra-memory` 22/22.
