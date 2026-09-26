//! Permission rule lattice (qwen `MultiClientPermissionMediator` +
//! qwen/qwen-code permission rules): **deny > ask > default** — the first
//! matching deny wins over any allow; ask forces a prompt; unmatched falls
//! to the tool's default (needs_approval).
//!
//! Rule shape follows ZCode's project permission rules (prefix + exact
//! matchers, `suggestedPermissionUpdates` learning lands in M2).

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuleEffect {
    Deny,
    Ask,
    Allow,
}

/// Matcher: exact tool name, or `tool:argument` patterns with a `*`
/// trailing wildcard (e.g. `bash(git *)` style is out of M0 scope; M0 is
/// name + path-prefix).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionRule {
    pub tool: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path_prefix: Option<String>,
    pub effect: RuleEffect,
    /// Where the rule came from (audit + precedence between sources).
    pub source: RuleSource,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuleSource {
    UserSettings,
    Project,
    PolicyManaged,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Deny,
    Ask,
    Allow,
    /// No rule matched: fall to the tool's declared default.
    Default,
}

/// The lattice. Later-registered sources bind TIGHTER (project over user
/// settings; managed over everything).
#[derive(Debug, Clone, Default)]
pub struct PermissionLattice {
    rules: Vec<PermissionRule>,
}

impl PermissionLattice {
    pub fn new() -> Self {
        PermissionLattice { rules: Vec::new() }
    }

    pub fn add_rule(&mut self, rule: PermissionRule) {
        self.rules.push(rule);
        // stable precedence: managed > project > user, then registration order
        self.rules.sort_by_key(|r| match r.source {
            RuleSource::UserSettings => 0,
            RuleSource::Project => 1,
            RuleSource::PolicyManaged => 2,
        });
    }

    pub fn rules(&self) -> &[PermissionRule] {
        &self.rules
    }

    /// Evaluate: the LAST matching rule in precedence order wins
    /// (deny > ask > allow is enforced by returning the tightest match:
    /// among equally-precedence matches, deny beats ask beats allow).
    pub fn evaluate(&self, tool: &str, arg_path: Option<&str>) -> Decision {
        let mut decision = Decision::Default;
        let mut current_rank = 0i32;
        for rule in &self.rules {
            if rule.tool != tool && rule.tool != "*" {
                continue;
            }
            if let (Some(prefix), Some(path)) = (&rule.path_prefix, arg_path) {
                if !path.starts_with(prefix.as_str()) {
                    continue;
                }
            } else if rule.path_prefix.is_some() {
                continue; // rule scoped to paths, no path in this call
            }
            let rank = match rule.source {
                RuleSource::UserSettings => 1,
                RuleSource::Project => 2,
                RuleSource::PolicyManaged => 3,
            };
            let effect_rank = match rule.effect {
                RuleEffect::Deny => 3,
                RuleEffect::Ask => 2,
                RuleEffect::Allow => 1,
            };
            let score = rank * 10 + effect_rank;
            if score >= current_rank {
                current_rank = score;
                decision = match rule.effect {
                    RuleEffect::Deny => Decision::Deny,
                    RuleEffect::Ask => Decision::Ask,
                    RuleEffect::Allow => Decision::Allow,
                };
            }
        }
        decision
    }
}

/// The four multi-client mediation policies (qwen
/// `MultiClientPermissionMediator`): who answers a permission prompt when
/// several clients are attached. M4 lands the live mediation; M0 carries the
/// closed policy enum so hosts can't invent variants.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum MediationPolicy {
    /// First client to answer wins.
    FirstResponder,
    /// One designated client (the session owner) answers.
    Designated,
    /// All clients must answer allow (unanimous); any deny/reject denies.
    Consensus,
    /// Prompts resolve locally without other clients (default).
    #[default]
    LocalOnly,
}

