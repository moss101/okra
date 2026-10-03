//! Memory agents (MASTER-PLAN §3 #33/#34, qwen memory + grok memory-v2):
//! the EXTRACT and DREAM agents as deterministic, offline functions.
//!
//! The donors run these as LLM calls; okra's offline tier keeps the same
//! CONTRACTS with deterministic heuristics so the seams stay live without
//! a key (mirroring `retrieval.rs`'s two-tier honesty):
//! - **extract** proposes durable facts from a conversation turn
//!   (preferences, project facts, standing instructions), deduped against
//!   what memory already stores by embedding cosine;
//! - **dream** consolidates stored memory: clusters near-duplicates and
//!   flags contradictions — a maintenance report a human applies;
//! - **MMR** (grok memory-v2) re-ranks retrieval for DIVERSITY so
//!   near-identical items do not crowd the head.
//!
//! Writes are never implicit: extract PROPOSES, a human (or an explicit
//! host action) accepts into a tier.

use crate::retrieval::{cosine, hashed_embedding};

/// A proposed memory from the extract agent.
#[derive(Debug, Clone, PartialEq)]
pub struct MemoryCandidate {
    pub text: String,
    pub kind: MemoryKind,
    /// The sentence it came from (audit: never propose without provenance).
    pub source: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryKind {
    /// "always/never/prefer X" — a standing instruction.
    Preference,
    /// "we use X for Y", "the project is X" — a durable project fact.
    ProjectFact,
}

/// Extract memory candidates from one turn's text. `existing` is what the
/// memory tiers already store (split into lines by the caller); a
/// candidate whose cosine to an existing line is >= `dup_threshold` is
/// dropped (memory does not re-learn what it knows).
pub fn extract_memories(turn_text: &str, existing: &[String]) -> Vec<MemoryCandidate> {
    const DUP_THRESHOLD: f32 = 0.72;
    let existing_vecs: Vec<(String, Vec<f32>)> = existing
        .iter()
        .filter(|l| !l.trim().is_empty())
        .map(|l| (l.trim().to_ascii_lowercase(), hashed_embedding(l)))
        .collect();
    let mut out: Vec<MemoryCandidate> = Vec::new();
    for sentence in turn_text.split(['\n', '.', '?', '!']) {
        let s = sentence.trim();
        if s.len() < 8 {
            continue;
        }
        let lower = s.to_ascii_lowercase();
        // preference patterns → a standing instruction; "we use X for Y"
        // shapes → a durable project fact; anything else is chatter
        let is_pref = lower.starts_with("always ")
            || lower.starts_with("never ")
            || lower.starts_with("prefer ")
            || lower.starts_with("please always")
            || lower.contains(" remember ")
            || lower.starts_with("remember that ")
            || lower.starts_with("don't use ")
            || lower.starts_with("avoid ");
        if !is_pref {
            let fact_topic = lower.starts_with("we use ")
                || lower.contains(" we use ")
                || lower.contains(" the project ")
                || lower.contains(" this repo ");
            let fact_link =
                lower.contains(" for ") || lower.contains(" is ") || lower.contains(" uses ");
            if !(fact_topic && fact_link) {
                continue;
            }
        }
        let vec = hashed_embedding(s);
        let dup = existing_vecs
            .iter()
            .any(|(l, ev)| {
                l == &lower || cosine(&vec, ev) >= DUP_THRESHOLD
            });
        if dup {
            continue;
        }
        let candidate_text = if is_pref {
            // normalize into a durable imperative (memory reads as rules)
            // the standing-instruction MARKER stays (the dream agent's
            // contradiction detector reads it); only polite wrappers come off
            s.trim_start_matches("Please always ")
                .trim_start_matches("please always ")
                .trim_start_matches("Remember that ")
                .trim_start_matches("remember that ")
                .trim()
                .to_string()
        } else {
            s.to_string()
        };
        let text = if candidate_text
            .chars()
            .next()
            .map(|c| c.is_ascii_lowercase())
            .unwrap_or(false)
        {
            // sentence-case the stored rule
            let upped = candidate_text.chars().next().unwrap().to_ascii_uppercase();
            format!("{}{}", upped, &candidate_text[1..])
        } else {
            candidate_text
        };
        let kind = if is_pref { MemoryKind::Preference } else { MemoryKind::ProjectFact };
        // dedupe within the same extraction pass too
        if out.iter().any(|c| {
            cosine(&hashed_embedding(&c.text), &vec) >= DUP_THRESHOLD
        }) {
            continue;
        }
        out.push(MemoryCandidate { text, kind, source: s.to_string() });
    }
    out
}

/// One dream cluster: near-duplicate memories with a suggested merge.
#[derive(Debug, Clone, PartialEq)]
pub struct DreamCluster {
    pub members: Vec<String>,
    /// A suggested consolidated statement (the LONGEST member — the most
    /// specific — never a fabricated synthesis).
    pub suggested_merge: String,
}

/// A detected contradiction: two stored lines that disagree.
#[derive(Debug, Clone, PartialEq)]
pub struct DreamContradiction {
    pub a: String,
    pub b: String,
}

/// The dream report (#34 dream consolidation).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DreamReport {
    pub clusters: Vec<DreamCluster>,
    pub contradictions: Vec<DreamContradiction>,
    pub total: usize,
}

/// Consolidate stored memories: cluster lines whose embeddings sit within
/// `cluster_threshold` of each other (greedy, stable order) and flag
/// preference contradictions ("Always X" vs "Never X" on a shared topic).
pub fn dream(memories: &[String]) -> DreamReport {
    const CLUSTER_THRESHOLD: f32 = 0.62;
    let lines: Vec<&String> = memories.iter().filter(|l| !l.trim().is_empty()).collect();
    let mut report = DreamReport { total: lines.len(), ..Default::default() };
    let vecs: Vec<Vec<f32>> = lines.iter().map(|l| hashed_embedding(l)).collect();

    // greedy clustering in stored order
    let mut assigned: Vec<bool> = vec![false; lines.len()];
    for i in 0..lines.len() {
        if assigned[i] {
            continue;
        }
        let mut members = vec![lines[i].to_string()];
        assigned[i] = true;
        for j in (i + 1)..lines.len() {
            if assigned[j] {
                continue;
            }
            if cosine(&vecs[i], &vecs[j]) >= CLUSTER_THRESHOLD {
                members.push(lines[j].to_string());
                assigned[j] = true;
            }
        }
        if members.len() > 1 {
            // the longest member is the most specific — suggest it, never
            // synthesize
            let merge = members
                .iter()
                .cloned()
                .max_by_key(|m| m.len())
                .unwrap_or_default();
            report.clusters.push(DreamCluster { members, suggested_merge: merge });
        }
    }

    // contradictions: opposing markers over a shared content word
    for i in 0..lines.len() {
        for j in (i + 1)..lines.len() {
            if let Some(c) = contradiction(lines[i], lines[j], &vecs[i], &vecs[j]) {
                report.contradictions.push(c);
            }
        }
    }
    report
}

fn contradiction(a: &str, b: &str, va: &[f32], vb: &[f32]) -> Option<DreamContradiction> {
    let la = a.to_ascii_lowercase();
    let lb = b.to_ascii_lowercase();
    let (neg_a, pos_a) = (starts_neg(&la), starts_pos(&la));
    let (neg_b, pos_b) = (starts_neg(&lb), starts_pos(&lb));
    let opposed = (pos_a && neg_b) || (neg_a && pos_b);
    if !opposed {
        return None;
    }
    // "shares a topic": at least one non-marker content word in common
    let stop = ["always", "never", "prefer", "avoid", "use", "the", "a", "an", "to", "for", "with"];
    let words_a: Vec<String> = la
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|w| !w.is_empty() && !stop.contains(w))
        .map(str::to_string)
        .collect();
    let overlap = words_a.iter().any(|w| lb.contains(w.as_str()));
    if !overlap && cosine(va, vb) < 0.3 {
        return None;
    }
    Some(DreamContradiction { a: a.to_string(), b: b.to_string() })
}

fn starts_neg(l: &str) -> bool {
    l.starts_with("never ") || l.starts_with("don't ") || l.starts_with("avoid ")
}

fn starts_pos(l: &str) -> bool {
    l.starts_with("always ") || l.starts_with("prefer ") || l.starts_with("use ")
}

/// Maximal Marginal Relevance re-ranking (#34 grok memory-v2): greedy
/// selection balancing query relevance against redundancy with the
/// already-selected set. `lambda` 1.0 = pure relevance, 0.0 = pure
/// diversity. Items below the relevance floor are never selected.
pub fn rank_mmr(
    items: &[(String, String)],
    query_vector: &[f32],
    k: usize,
    lambda: f32,
) -> Vec<(String, f32)> {
    const RELEVANCE_FLOOR: f32 = 0.18;
    let vecs: Vec<(String, Vec<f32>, Vec<f32>)> = items
        .iter()
        .map(|(id, text)| (id.clone(), hashed_embedding(text), hashed_embedding(text)))
        .collect();
    let mut selected: Vec<usize> = Vec::new();
    let mut remaining: Vec<usize> = (0..items.len()).collect();
    while selected.len() < k && !remaining.is_empty() {
        let mut best: Option<(usize, f32)> = None;
        for &i in &remaining {
            let rel = cosine(query_vector, &vecs[i].1);
            if rel < RELEVANCE_FLOOR {
                continue;
            }
            let redundancy = selected
                .iter()
                .map(|&s| cosine(&vecs[i].1, &vecs[s].1))
                .fold(0.0f32, f32::max);
            let mmr = lambda * rel - (1.0 - lambda) * redundancy;
            if best.map(|(_, bs)| mmr > bs).unwrap_or(true) {
                best = Some((i, mmr));
            }
        }
        match best {
            Some((i, mmr_score)) => {
                selected.push(i);
                remaining.retain(|&r| r != i);
                let _ = mmr_score;
            }
            None => break,
        }
    }
    // report the QUERY relevance of each selected item (what callers render)
    selected
        .into_iter()
        .map(|i| {
            let rel = cosine(query_vector, &vecs[i].1);
            (items[i].0.clone(), rel)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_pulls_preferences_and_project_facts() {
        let cands = extract_memories(
            "Always run cargo clippy before committing. We use okra crates for the daemon. What color is the sky.",
            &[],
        );
        assert_eq!(cands.len(), 2, "{cands:?}");
        assert_eq!(cands[0].kind, MemoryKind::Preference);
        // the standing-instruction MARKER stays in the stored text (the
        // dream agent's contradiction detector reads it)
        assert!(cands[0].text.starts_with("Always run cargo clippy"), "{}", cands[0].text);
        assert_eq!(cands[1].kind, MemoryKind::ProjectFact);
        // provenance is kept
        assert!(cands[0].source.contains("clippy"));
    }

    #[test]
    fn extract_never_reproposes_what_memory_stores() {
        let existing = vec!["Run cargo clippy before committing".to_string()];
        let cands = extract_memories("Always run cargo clippy before committing.", &existing);
        assert!(cands.is_empty(), "dedupe by cosine: {cands:?}");
    }

    #[test]
    fn dream_clusters_near_duplicates_and_suggests_the_longest() {
        let report = dream(&[
            "Run cargo clippy before every commit".into(),
            "Run cargo clippy before every commit to main".into(),
            "The project uses rust edition 2024".into(),
        ]);
        assert_eq!(report.clusters.len(), 1, "{report:?}");
        assert_eq!(report.clusters[0].members.len(), 2);
        assert_eq!(
            report.clusters[0].suggested_merge,
            "Run cargo clippy before every commit to main",
            "the longest (most specific) member is the merge"
        );
        assert_eq!(report.total, 3);
    }

    #[test]
    fn dream_flags_always_never_contradictions() {
        let report = dream(&[
            "Always run the full test suite before pushing".into(),
            "Never run the full test suite before pushing".into(),
        ]);
        assert_eq!(report.contradictions.len(), 1, "{report:?}");
        assert!(report.contradictions[0].a.starts_with("Always"));
        assert!(report.contradictions[0].b.starts_with("Never"));
    }

    #[test]
    fn mmr_suppresses_near_duplicate_crowding() {
        let items = vec![
            ("rust-a".into(), "rust cargo modules unit tests".into()),
            // an EXACT duplicate of rust-a: maximal redundancy
            ("rust-b".into(), "rust cargo modules unit tests".into()),
            // same query topic, different content words
            ("rust-ci".into(), "rust cargo security audit pipeline".into()),
        ];
        let q = hashed_embedding("rust cargo");
        // pure relevance (lambda 1.0) ranks the duplicate second
        let greedy = rank_mmr(&items, &q, 2, 1.0);
        assert_eq!(greedy[0].0, "rust-a");
        assert_eq!(greedy[1].0, "rust-b", "pure relevance keeps the duplicate: {greedy:?}");
        // diversity trades the redundant duplicate for topical coverage
        let diverse = rank_mmr(&items, &q, 2, 0.6);
        assert_eq!(
            diverse.iter().find(|(id, _)| id == "rust-ci").map(|_| ()),
            Some(()),
            "MMR picks the distinct item over the duplicate: {diverse:?}"
        );
        // below-floor items are never selected
        let none = rank_mmr(&items, &hashed_embedding("quantum chromodynamics"), 3, 0.7);
        assert!(none.is_empty());
    }
}
