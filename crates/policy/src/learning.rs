//! Ruleset learning — `suggestedPermissionUpdates` (MASTER-PLAN §3 #24,
//! ZCode project permission rules). The permission lattice holds rules
//! from settings; this module turns the decisions users actually make
//! during turns into PROPOSED rules.
//!
//! Invariants:
//! - A suggestion is never auto-applied. The host surfaces it; a human
//!   confirms; only then does [`RulesetLearner::apply`] insert it (as a
//!   `Project`-source rule the caller persists to settings).
//! - Suggestions never contradict the lattice: a tool+prefix that already
//!   matches ANY rule (deny, ask, or allow) is not suggested again.
//! - Denials are not evidence for allows: only granted decisions count.

use std::collections::HashMap;

use crate::approval::{ApprovalOutcome, ApprovalScope};
use crate::lattice::{PermissionLattice, PermissionRule, RuleEffect, RuleSource};

/// Coarse path prefix derived from a tool argument path: the FIRST path
/// component (`src/main.rs` → `src/`), so suggested rules stay readable.
/// A top-level file (`a.rs`) or no path suggests a tool-wide rule.
pub fn path_prefix(path: &str) -> Option<String> {
    let trimmed = path.trim_start_matches("./");
    let mut parts = trimmed.split('/');
    let first = parts.next()?;
    match parts.next() {
        Some(rest) if !rest.is_empty() || trimmed.ends_with('/') => {
            Some(format!("{first}/"))
        }
        _ => None,
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
struct ObservationKey {
    tool: String,
    path_prefix: Option<String>,
}

/// A proposed rule with its evidence.
#[derive(Debug, Clone, PartialEq)]
pub struct SuggestedPermissionUpdate {
    pub rule: PermissionRule,
    /// How many granted decisions back it.
    pub occurrences: u32,
    /// Human-readable rationale (surfaces render this on the card).
    pub because: String,
}

/// The learner. `record` is cheap and never panics; `suggest` is pure
/// with respect to the lattice it is shown.
#[derive(Debug, Default)]
pub struct RulesetLearner {
    counts: HashMap<ObservationKey, u32>,
    total_granted: u32,
}

impl RulesetLearner {
    pub fn new() -> Self {
        RulesetLearner::default()
    }

    /// Record one approval outcome. Only GRANTED decisions are evidence
    /// for an allow suggestion; denials are still counted for honesty in
    /// `total_decided` but never propose rules.
    pub fn observe(&mut self, tool: &str, arg_path: Option<&str>, outcome: ApprovalOutcome, _scope: ApprovalScope) {
        if !outcome.grants() {
            return;
        }
        let key = ObservationKey { tool: tool.to_string(), path_prefix: arg_path.and_then(path_prefix) };
        *self.counts.entry(key).or_insert(0) += 1;
        self.total_granted += 1;
    }

    pub fn total_granted(&self) -> u32 {
        self.total_granted
    }

    /// Proposed updates, strongest evidence first, capped at `max`.
    /// Skips anything the lattice already answers (any effect at that
    /// tool+prefix), so suggestions never fight existing rules.
    pub fn suggest(&self, lattice: &PermissionLattice, min_occurrences: u32, max: usize) -> Vec<SuggestedPermissionUpdate> {
        let mut out = Vec::new();
        let mut keys: Vec<&ObservationKey> = self.counts.keys().collect();
        keys.sort();
        // strongest evidence first
        keys.sort_by_key(|k| std::cmp::Reverse(self.counts[k]));
        for key in keys {
            if out.len() >= max {
                break;
            }
            let occurrences = self.counts[key];
            if occurrences < min_occurrences {
                continue;
            }
            if lattice_already_answers(lattice, &key.tool, key.path_prefix.as_deref()) {
                continue;
            }
            let because = format!(
                "you allowed {occurrences} {} call{}{} this session — add a project rule?",
                key.tool,
                if occurrences == 1 { "" } else { "s" },
                match &key.path_prefix {
                    Some(p) => format!(" under `{p}`"),
                    None => String::new(),
                }
            );
            out.push(SuggestedPermissionUpdate {
                rule: PermissionRule {
                    tool: key.tool.clone(),
                    path_prefix: key.path_prefix.clone(),
                    effect: RuleEffect::Allow,
                    source: RuleSource::Project,
                },
                occurrences,
                because,
            });
        }
        out
    }

    /// Apply a confirmed suggestion: insert as a Project rule and return
    /// the rule so the caller persists it to settings. (Insertion uses the
    /// lattice's own precedence sort — managed rules still win.)
    pub fn apply(suggestion: &SuggestedPermissionUpdate, lattice: &mut PermissionLattice) -> PermissionRule {
        lattice.add_rule(suggestion.rule.clone());
        suggestion.rule.clone()
    }
}

/// Does the lattice already have a rule that would answer this tool at
/// this prefix (or a wider one)? Exact tool match or `*`; a rule scoped
/// to paths does NOT answer a pathless call (the lattice itself skips
/// it); a prefix-less rule answers every path.
fn lattice_already_answers(lattice: &PermissionLattice, tool: &str, prefix: Option<&str>) -> bool {
    lattice.rules().iter().any(|r| {
        let tool_hit = r.tool == tool || r.tool == "*";
        let prefix_hit = match (&r.path_prefix, prefix) {
            (None, _) => true,           // tool-wide rule answers everything
            (Some(_), None) => false,    // path-scoped rule never answers a pathless call
            (Some(rp), Some(p)) => p.starts_with(rp.as_str()), // rule as wide as, or wider than, the observation
        };
        tool_hit && prefix_hit
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_granted_decisions_are_evidence() {
        let mut l = RulesetLearner::new();
        l.observe("write_file", Some("src/main.rs"), ApprovalOutcome::Rejected, ApprovalScope::Once);
        l.observe("write_file", Some("src/other.rs"), ApprovalOutcome::Unavailable, ApprovalScope::Once);
        assert_eq!(l.total_granted(), 0);
        assert!(l.suggest(&PermissionLattice::new(), 1, 5).is_empty());
    }

    #[test]
    fn prefix_is_first_component_and_top_level_is_tool_wide() {
        assert_eq!(path_prefix("src/main.rs").as_deref(), Some("src/"));
        assert_eq!(path_prefix("./src/a/b.rs").as_deref(), Some("src/"));
        assert_eq!(path_prefix("a.rs"), None);
        assert_eq!(path_prefix("src/").as_deref(), Some("src/"));
    }

    #[test]
    fn suggestions_rank_by_evidence_and_skip_existing_rules() {
        let allow = || ApprovalOutcome::AllowedOnce;
        let mut l = RulesetLearner::new();
        for _ in 0..3 {
            l.observe("write_file", Some("src/a.rs"), allow(), ApprovalScope::Once);
        }
        l.observe("bash", None, allow(), ApprovalScope::Conversation);

        let mut lattice = PermissionLattice::new();
        lattice.add_rule(PermissionRule {
            tool: "bash".into(),
            path_prefix: None,
            effect: RuleEffect::Ask,
            source: RuleSource::Project,
        });

        let suggestions = l.suggest(&lattice, 1, 5);
        assert_eq!(suggestions.len(), 1, "bash already answered by a project ask rule");
        let s = &suggestions[0];
        assert_eq!(s.rule.tool, "write_file");
        assert_eq!(s.rule.path_prefix.as_deref(), Some("src/"));
        assert_eq!(s.rule.effect, RuleEffect::Allow);
        assert_eq!(s.occurrences, 3);
        assert!(s.because.contains("write_file"));

        // min_occurrences filters weak evidence
        assert!(l.suggest(&lattice, 4, 5).is_empty());

        // applying inserts a project rule that answers future evaluations
        let rule = RulesetLearner::apply(&suggestions[0], &mut lattice);
        assert_eq!(rule.source, RuleSource::Project);
        assert_eq!(lattice.evaluate("write_file", Some("src/main.rs")), crate::lattice::Decision::Allow);
        // and it stops being suggested
        assert!(l.suggest(&lattice, 1, 5).is_empty());
    }

    #[test]
    fn managed_rule_blocks_suggestion_even_wider() {
        let mut l = RulesetLearner::new();
        l.observe("write_file", Some("src/a.rs"), ApprovalOutcome::AllowedOnce, ApprovalScope::Once);
        let mut lattice = PermissionLattice::new();
        lattice.add_rule(PermissionRule {
            tool: "write_file".into(),
            path_prefix: None,
            effect: RuleEffect::Deny,
            source: RuleSource::PolicyManaged,
        });
        assert!(l.suggest(&lattice, 1, 5).is_empty(), "a managed rule answers the tool; never suggest");
    }
}
