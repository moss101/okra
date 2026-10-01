//! Embedding-based skill/tool retrieval (MASTER-PLAN §5 M5: "embedding-
//! based tool/skill retrieval — the gap nobody closes").
//!
//! Two-tier embeddings:
//! - **Network tier** (`okra-providers` embeddings client): real
//!   vectors from an OpenAI-compatible `/embeddings` endpoint.
//! - **Offline tier** (`hashed_embedding`, this module): a deterministic
//!   hashed bag-of-words projection — no network, no model, stable
//!   across runs. It is NOT semantic; it ranks lexical overlap
//!   honestly, which is exactly enough to make prompt-relevant skills
//!   surface before any path is touched, and it keeps the seam live
//!   without a key.
//!
//! Cosine similarity over L2-normalized vectors; the index ranks items
//! by similarity to the query and applies a relevance floor so
//! low-signal matches stay out of the prompt.

/// Offline deterministic embedding: hashed bag-of-words, `D` dims,
/// L2-normalized. Words are lowercased alphanumeric runs; bigrams are
/// included so phrase overlap outranks single-word overlap.
pub fn hashed_embedding(text: &str) -> Vec<f32> {
    const DIM: usize = 256;
    let mut v = vec![0f32; DIM];
    let words: Vec<String> = text
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_lowercase)
        .collect();
    let mut feed = |token: &str| {
        let h = fnv1a(token) as usize % DIM;
        v[h] += 1.0;
    };
    for w in &words {
        feed(w);
    }
    for pair in words.windows(2) {
        feed(&format!("{}~{}", pair[0], pair[1]));
    }
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for x in &mut v {
            *x /= norm;
        }
    }
    v
}

fn fnv1a(s: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    // vectors from this module are pre-normalized; the dot product IS
    // the cosine. Defensive zero-length handling for external vectors.
    let n = a.len().min(b.len());
    let dot: f32 = a.iter().zip(b.iter()).take(n).map(|(x, y)| x * y).sum();
    let na: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let nb: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if na == 0.0 || nb == 0.0 {
        0.0
    } else {
        dot / (na * nb)
    }
}

/// A rankable item (a skill, a tool description).
#[derive(Debug, Clone)]
pub struct RetrievalItem {
    pub id: String,
    pub text: String,
    pub vector: Vec<f32>,
}

pub struct EmbeddingIndex {
    items: Vec<RetrievalItem>,
}

/// Matches below this cosine are noise, not relevance (tuned on the
/// offline tier; the network tier reuses it — it only tightens).
pub const RELEVANCE_FLOOR: f32 = 0.18;

impl EmbeddingIndex {
    /// Build from (id, text) pairs with pre-computed vectors (either
    /// tier — the index does not care where vectors came from).
    pub fn build(vectored: Vec<(String, String, Vec<f32>)>) -> Self {
        EmbeddingIndex {
            items: vectored
                .into_iter()
                .map(|(id, text, vector)| RetrievalItem { id, text, vector })
                .collect(),
        }
    }

    /// The offline index: hashed embeddings, no network.
    pub fn build_offline(items: Vec<(String, String)>) -> Self {
        EmbeddingIndex {
            items: items
                .into_iter()
                .map(|(id, text)| {
                    let vector = hashed_embedding(&text);
                    RetrievalItem { id, text, vector }
                })
                .collect(),
        }
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Top-k ids above the relevance floor, best first.
    pub fn rank(&self, query_vector: &[f32], k: usize) -> Vec<(String, f32)> {
        let mut scored: Vec<(String, f32)> = self
            .items
            .iter()
            .map(|item| (item.id.clone(), cosine(query_vector, &item.vector)))
            .filter(|(_, score)| *score >= RELEVANCE_FLOOR)
            .collect();
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        scored.truncate(k);
        scored
    }

    /// The vector the offline tier produces for a query.
    pub fn offline_query(text: &str) -> Vec<f32> {
        hashed_embedding(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn catalog() -> EmbeddingIndex {
        EmbeddingIndex::build_offline(vec![
            (
                "docker-build".into(),
                "container build conventions dockerfile buildkit image layers".into(),
            ),
            (
                "rust-testing".into(),
                "rust testing conventions unit tests modules cargo test".into(),
            ),
            (
                "release-checklist".into(),
                "release process tagging changelog publishing version bump".into(),
            ),
        ])
    }

    #[test]
    fn ranks_the_lexically_relevant_skill_first() {
        let idx = catalog();
        let hits = idx.rank(&EmbeddingIndex::offline_query("how should I write rust unit tests for this module"), 3);
        assert!(!hits.is_empty(), "at least one relevant skill surfaces");
        assert_eq!(hits[0].0, "rust-testing", "best match: {hits:?}");
        let second = hits.get(1).map(|h| h.1).unwrap_or(0.0);
        assert!(hits[0].1 > second + 0.05, "clear margin: {hits:?}");
    }

    #[test]
    fn irrelevant_queries_stay_under_the_floor() {
        let idx = catalog();
        let hits = idx.rank(&EmbeddingIndex::offline_query("what color is the sky today"), 3);
        assert!(hits.iter().all(|(_, s)| *s < 0.5), "no strong match for unrelated text: {hits:?}");
    }

    #[test]
    fn hashed_embedding_is_deterministic_and_normalized() {
        let a = hashed_embedding("rust testing conventions");
        let b = hashed_embedding("rust testing conventions");
        assert_eq!(a, b);
        let norm: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-5, "L2 normalized: {norm}");
    }

    #[test]
    fn cosine_bounds_hold_for_disjoint_texts() {
        let a = hashed_embedding("docker buildkit layers");
        let b = hashed_embedding("release changelog tagging");
        assert!(cosine(&a, &b) < 0.3, "disjoint vocabularies score low");
    }
}

/// The network embedder seam (injected so callers/tests choose the tier
/// — `okra-providers`' client is wired in at the app layer).
pub type NetworkEmbed = dyn Fn(&[&str]) -> Result<Vec<Vec<f32>>, String>;

/// Two-tier retrieval: use the network embedder when provided and
/// healthy; fall back to the offline hashed tier on ANY failure
/// (retrieval must never fail a turn). The last text in the batch is
/// the query.
pub fn rank_relevant(
    items: Vec<(String, String)>,
    query_text: &str,
    network: Option<&NetworkEmbed>,
    k: usize,
) -> Vec<(String, f32)> {
    if let Some(embed) = network {
        let mut texts: Vec<&str> = items.iter().map(|(_, t)| t.as_str()).collect();
        texts.push(query_text);
        if let Ok(vectors) = embed(&texts)
            && vectors.len() == texts.len()
        {
            let query_vector = vectors.last().cloned().unwrap_or_default();
            let vectored: Vec<(String, String, Vec<f32>)> = items
                .into_iter()
                .zip(vectors)
                .map(|((id, text), vector)| (id, text, vector))
                .collect();
            return EmbeddingIndex::build(vectored).rank(&query_vector, k);
        }
        // any error or shape mismatch: offline, never a failed turn
    }
    let index = EmbeddingIndex::build_offline(items);
    let query_vector = EmbeddingIndex::offline_query(query_text);
    index.rank(&query_vector, k)
}

#[cfg(test)]
mod tier_tests {
    use super::*;

    fn items() -> Vec<(String, String)> {
        vec![
            ("docker-build".into(), "container dockerfile buildkit".into()),
            ("rust-testing".into(), "rust unit tests cargo".into()),
        ]
    }

    #[test]
    fn offline_tier_is_the_default() {
        let hits = rank_relevant(items(), "how do I write rust unit tests", None, 3);
        assert_eq!(hits.first().map(|(id, _)| id.as_str()), Some("rust-testing"));
    }

    #[test]
    fn network_tier_is_used_when_healthy() {
        // a fake embedder that "knows" docker: near-parallel vectors
        let embed = |texts: &[&str]| -> Result<Vec<Vec<f32>>, String> {
            Ok(texts
                .iter()
                .map(|t| {
                    if t.contains("docker") || t.contains("ship containers") {
                        vec![1.0, 0.0]
                    } else {
                        vec![0.0, 1.0]
                    }
                })
                .collect())
        };
        let hits = rank_relevant(items(), "ship containers for me", Some(&embed), 3);
        assert_eq!(hits.first().map(|(id, _)| id.as_str()), Some("docker-build"));
    }

    #[test]
    fn network_failure_falls_back_offline() {
        let broken = |_texts: &[&str]| -> Result<Vec<Vec<f32>>, String> { Err("503".into()) };
        let hits = rank_relevant(items(), "how do I write rust unit tests", Some(&broken), 3);
        assert_eq!(hits.first().map(|(id, _)| id.as_str()), Some("rust-testing"));
    }

    #[test]
    fn malformed_network_output_falls_back_offline() {
        let short = |_texts: &[&str]| -> Result<Vec<Vec<f32>>, String> { Ok(vec![vec![1.0]]) };
        let hits = rank_relevant(items(), "how do I write rust unit tests", Some(&short), 3);
        assert_eq!(hits.first().map(|(id, _)| id.as_str()), Some("rust-testing"));
    }
}
