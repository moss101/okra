//! The semantic wander governor (TypeSafe Jev as a judgment primitive).
//!
//! Dogfood day-1 finding #3: models wander — long stretches of
//! exploratory tool calls before producing. String matching cannot tell
//! "wandering" from "legitimate investigation" (the calls differ); a
//! semantic judgment can. At every Nth step the governor shows Jev the
//! recent activity and asks TWO independent questions over the same
//! state (parallel, they cannot see each other):
//!
//! - `progressing` (noul): is the agent making productive progress toward
//!   the user's most recent instruction?
//! - `activity` (choice): exploring | repeating | offtrack | delivering
//!
//! Policy (explicit, in code, CALIBRATED on live Jev): the activity CLASS
//! is the primary signal — `repeating`/`offtrack` nudge. The `progressing`
//! noul reads generously (measured 0.69 on a clear circle), so it is
//! reported in the reminder, not required as a gate. Fail-open everywhere:
//! any Jev error, missing key, or ambiguous answer means NO nudge — a
//! semantic signal never breaks a turn (the hooks-containment rule).

use okra_providers::jev;

/// What the governor needs from the turn: the recent activity, rendered.
pub trait SemanticJudge: Send + Sync {
    /// Whether this judge can judge at all (no key → inert, zero cost).
    fn is_active(&self) -> bool {
        true
    }

    /// Returns the raw answers map, or None (fail-open) on any error.
    fn judge_wander(&self, recent_activity: &str, latest_user_text: &str)
        -> Option<JevWanderVerdict>;
}

/// The parsed verdict for the wander question pair.
#[derive(Debug, Clone, PartialEq)]
pub struct JevWanderVerdict {
    pub progressing_probability: f64,
    pub activity: String,
}

/// The real client-backed judge. None when no key is configured.
pub struct JevSemanticJudge {
    api_key: Option<String>,
}

impl JevSemanticJudge {
    pub fn from_env() -> Self {
        JevSemanticJudge { api_key: jev::api_key_from_env() }
    }

    pub fn inert() -> Self {
        JevSemanticJudge { api_key: None }
    }

    pub fn is_active(&self) -> bool {
        self.api_key.is_some()
    }
}

impl SemanticJudge for JevSemanticJudge {
    fn is_active(&self) -> bool {
        self.api_key.is_some()
    }

    fn judge_wander(
        &self,
        recent_activity: &str,
        latest_user_text: &str,
    ) -> Option<JevWanderVerdict> {
        let key = self.api_key.as_ref()?;
        let state = serde_json::json!({
            "latestUserInstruction": latest_user_text,
            "recentActivity": recent_activity,
        });
        let questions = serde_json::json!({
            "progressing": {
                "type": "noul",
                "instructions": "Given the user's latest instruction and the agent's recent tool activity, is the agent making productive progress toward fulfilling it? Recent activity is oldest-first."
            },
            "activity": {
                "type": "choice",
                "instructions": "Which best describes the agent's recent activity pattern?",
                "criteria": {
                    "exploring": "reading or listing different things to understand the task — legitimate investigation",
                    "repeating": "substantively the same calls or observations again — going in circles",
                    "offtrack": "activity unrelated to the user's instruction",
                    "delivering": "actively producing the requested outcome"
                }
            }
        });
        let answers = jev::judge(key, state, questions).ok()?;
        let progressing = answers.get("progressing")?.noul?;
        let activity = answers.get("activity")?.choice.clone()?;
        Some(JevWanderVerdict {
            progressing_probability: progressing,
            activity,
        })
    }
}

/// The explicit policy gate: class primary (calibrated on live Jev —
/// see N0021), shared by the governor and its observability.
fn class_matches(config: &WanderConfig, verdict: &JevWanderVerdict) -> bool {
    config.nudge_activities.iter().any(|c| *c == verdict.activity)
}

/// Governance constants (explicit policy — tune here, not in the loop).
#[derive(Debug, Clone, Copy)]
pub struct WanderConfig {
    /// First step eligible for a judgment.
    pub first_step: usize,
    /// Judge every Nth step.
    pub every_n: usize,
    /// Maximum judgments per turn (bounded cost).
    pub max_judgments: usize,
    /// Nudge below this progress probability…
    pub progress_threshold: f64,
    /// …and only for these activity classes.
    pub nudge_activities: [&'static str; 2],
}

impl Default for WanderConfig {
    fn default() -> Self {
        WanderConfig {
            first_step: 4,
            every_n: 3,
            max_judgments: 6,
            progress_threshold: 0.35,
            nudge_activities: ["repeating", "offtrack"],
        }
    }
}

/// The env-configured judge: `TYPESAFE_API_KEY` enables, and
/// `OKRA_SEMANTIC_WATCH=off` disables explicitly. None → pure-code path.
pub fn from_env_config() -> Option<std::sync::Arc<dyn SemanticJudge>> {
    if std::env::var("OKRA_SEMANTIC_WATCH").as_deref() == Ok("off") {
        return None;
    }
    let judge = JevSemanticJudge::from_env();
    if judge.is_active() {
        Some(std::sync::Arc::new(judge))
    } else {
        None
    }
}

/// The per-turn governor state.
pub struct WanderGovernor {
    pub config: WanderConfig,
    judge: std::sync::Arc<dyn SemanticJudge>,
    judgments: usize,
    nudges: usize,
}

impl WanderGovernor {
    pub fn new(judge: std::sync::Arc<dyn SemanticJudge>) -> Self {
        WanderGovernor {
            config: WanderConfig::default(),
            judge,
            judgments: 0,
            nudges: 0,
        }
    }

    /// Inert governor (no key): never judges, never nudges.
    pub fn inert() -> Self {
        WanderGovernor::new(std::sync::Arc::new(JevSemanticJudge::inert()))
    }

    pub fn is_active(&self) -> bool {
        self.judgments < self.config.max_judgments
    }

    pub fn judgments(&self) -> usize {
        self.judgments
    }

    pub fn nudges(&self) -> usize {
        self.nudges
    }

    /// Called by the loop at each step boundary (step is 1-based). Returns
    /// Some(reminder) when the semantic verdict warrants the nudge.
    pub fn observe_step(
        &mut self,
        step: usize,
        recent_activity: &str,
        latest_user_text: &str,
    ) -> Option<String> {
        if !self.judge.is_active()
            || step < self.config.first_step
            || !(step - self.config.first_step).is_multiple_of(self.config.every_n)
            || self.judgments >= self.config.max_judgments
        {
            return None;
        }
        self.judgments += 1;
        // fail-open: any error/ambiguity → no nudge (verdict logged either
        // way — a silent semantic signal can't be tuned or trusted)
        let verdict = self.judge.judge_wander(recent_activity, latest_user_text)?;
        eprintln!(
            "[wander] step {step}: progressing={:.2} activity={} -> {}",
            verdict.progressing_probability,
            verdict.activity,
            if class_matches(&self.config, &verdict) { "NUDGE" } else { "quiet" },
        );
        if class_matches(&self.config, &verdict) {
            self.nudges += 1;
            Some(format!(
                "A semantic progress check suggests you may be going in circles \
                 (progress {:.0}%, activity: {}). Re-read the user's request, state \
                 what you still need, and move toward delivering it — avoid repeating \
                 similar calls.",
                verdict.progressing_probability * 100.0,
                verdict.activity,
            ))
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct StubJudge {
        verdicts: Mutex<Vec<Option<JevWanderVerdict>>>,
    }

    impl SemanticJudge for StubJudge {
        fn judge_wander(
            &self,
            _recent: &str,
            _user: &str,
        ) -> Option<JevWanderVerdict> {
            let mut v = self.verdicts.lock().unwrap();
            if v.is_empty() {
                return None;
            }
            v.remove(0)
        }
    }

    fn wandering() -> JevWanderVerdict {
        // measured live: class is sharp, progress reads generously
        JevWanderVerdict { progressing_probability: 0.69, activity: "repeating".into() }
    }

    fn fine() -> JevWanderVerdict {
        JevWanderVerdict { progressing_probability: 0.9, activity: "exploring".into() }
    }

    #[test]
    fn nudges_only_on_threshold_and_class() {
        let g = WanderGovernor::new(std::sync::Arc::new(StubJudge {
            verdicts: Mutex::new(vec![Some(fine()), Some(wandering())]),
        }));
        let mut g = g;
        // steps 1..3: below first_step → no judgment
        assert!(g.observe_step(1, "a", "u").is_none());
        assert!(g.observe_step(2, "a", "u").is_none());
        assert!(g.observe_step(3, "a", "u").is_none());
        // step 4: first judgment — healthy → no nudge
        assert!(g.observe_step(4, "a", "u").is_none());
        // step 7: second judgment — wandering → nudge
        let nudge = g.observe_step(7, "a", "u").expect("nudge");
        assert!(nudge.contains("going in circles"));
        assert_eq!(g.nudges(), 1);
        assert_eq!(g.judgments(), 2);
    }

    #[test]
    fn fail_open_on_judge_error() {
        let g = WanderGovernor::new(std::sync::Arc::new(StubJudge {
            verdicts: Mutex::new(vec![None]),
        }));
        let mut g = g;
        assert!(g.observe_step(4, "a", "u").is_none());
        assert_eq!(g.nudges(), 0);
    }

    #[test]
    fn inert_governor_never_judges() {
        let g = WanderGovernor::inert();
        let mut g = g;
        for step in [4usize, 7, 10] {
            assert!(g.observe_step(step, "a", "u").is_none());
        }
        assert_eq!(g.judgments(), 0);
    }

    #[test]
    fn bounded_judgments_per_turn() {
        let verdicts = (0..20).map(|_| Some(wandering())).collect();
        let g = WanderGovernor::new(std::sync::Arc::new(StubJudge {
            verdicts: Mutex::new(verdicts),
        }));
        let mut g = g;
        let mut nudges = 0;
        for step in 1..=40usize {
            if g.observe_step(step, "a", "u").is_some() {
                nudges += 1;
            }
        }
        assert_eq!(g.judgments(), 6, "max_judgments caps the cost");
        assert_eq!(nudges, 6);
    }
}
