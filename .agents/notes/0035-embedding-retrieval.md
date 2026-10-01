# N0035 — embedding-based skill retrieval (prompt-relevant, two tiers)

- **Status:** implemented
- **Decided:** 2026-10-01
- Closes: MASTER-PLAN §5 M5's "embedding-based tool/skill retrieval —
  the gap nobody closes."

## Decision

1. **Two tiers, one index.** `memory::retrieval` ranks items by cosine
   over L2-normalized vectors; vectors come from either tier:
   - network: `providers::embeddings::EmbeddingClient` — OpenAI-
     compatible `/embeddings` (same env/agent shape as the chat
     provider, vectors re-normalized client-side);
   - offline (default): `hashed_embedding` — deterministic hashed
     bag-of-words + bigrams, 256 dims. NOT semantic: it ranks lexical
     overlap honestly, keeps the seam keyless and hermetic, and never
     fails a turn.
2. **A relevance floor (0.18) keeps noise out of the prompt**; top-3.
3. **Suggestions are distinct from activations:** `suggest_skill`
   writes `RELEVANT — (score) first-line` into the world Skills section
   — the model can tell "you touched matching files" (ACTIVE) from
   "the ask resembles this skill" (RELEVANT). Both are byte-stable
   while unchanged.
4. Serve turns compute suggestions from the live catalog per send (a
   Tools-tab install affects the next send).

## Evidence

- `crates/memory/src/retrieval.rs` tests: the lexically relevant skill
  ranks first with a clear margin; unrelated queries stay low;
  embeddings deterministic + L2-normalized; disjoint vocabularies
  score low.
- `crates/providers/src/embeddings.rs`: mock-server test — request
  parses, vectors decode and normalize.
- Serve turns: suggestions land in the head before continuation
  (verified by build + existing continuation-head tests still green).

## Because

Path-conditional activation alone leaves a skill invisible until the
agent happens to touch a matching file; retrieval surfaces it at ask
time. The offline tier is the default deliberately: retrieval quality
should never gate on a credential, and the network tier is a drop-in
(vector source only).
