//! TypeSafe System One client — Jev as a judgment primitive (MASTER-PLAN
//! §3 #65-adjacent: semantic signals where string matching cannot reach).
//!
//! Jev returns TYPED answers with probabilities, not text. The client is
//! deliberately narrow: one `judge` call over a JSON state with a fixed
//! question set, fail-open on any error (a semantic signal must never
//! break a turn — the hooks-containment rule). The API key comes from
//! `TYPESAFE_API_KEY`; without it the client is `None` and callers run
//! the pure-code path.

use serde::{Deserialize, Serialize};
use std::time::Duration;

pub const JEV_API_URL: &str = "https://api.typesafe.ai/v1/systemone";
pub const JEV_MODEL: &str = "jev-latest";

/// A typed answer to one question.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct JevAnswer {
    /// noul: probability of "yes".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub noul: Option<f64>,
    /// choice: the selected option key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub choice: Option<String>,
    /// score: probability-weighted position (1-based over the legend).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub score: Option<f64>,
    /// choice/score confidence summary.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confidence: Option<f64>,
}

/// One judgment over `state` (a JSON value the questions reference).
pub fn judge(
    api_key: &str,
    state: serde_json::Value,
    questions: serde_json::Value,
) -> Result<std::collections::BTreeMap<String, JevAnswer>, String> {
    let body = serde_json::json!({
        "state": state,
        "model": JEV_MODEL,
        "questions": questions,
    });
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(20))
        .build();
    let response = agent
        .post(JEV_API_URL)
        .set("Authorization", &format!("Bearer {api_key}"))
        .set("Content-Type", "application/json")
        .send_json(body)
        .map_err(|e| format!("jev: {e}"))?;
    let parsed: serde_json::Value = response
        .into_json()
        .map_err(|e| format!("jev decode: {e}"))?;
    let answers = parsed
        .get("answers")
        .cloned()
        .ok_or_else(|| format!("jev: no answers in response: {parsed}"))?;
    serde_json::from_value(answers).map_err(|e| format!("jev answers decode: {e}"))
}

/// The API key from the environment (None → semantic features inert).
pub fn api_key_from_env() -> Option<String> {
    std::env::var("TYPESAFE_API_KEY").ok().filter(|k| !k.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Live smoke — runs only when TYPESAFE_API_KEY is set; skipped
    /// otherwise so CI stays hermetic.
    #[test]
    fn jev_live_smoke() {
        let Some(key) = api_key_from_env() else {
            eprintln!("skipped: TYPESAFE_API_KEY not set");
            return;
        };
        let answers = judge(
            &key,
            serde_json::json!({ "log": "build ok, 0 tests failed" }),
            serde_json::json!({
                "healthy": { "type": "noul", "instructions": "Does this log indicate a healthy build?" }
            }),
        )
        .expect("live jev call");
        let healthy = answers.get("healthy").expect("answer present");
        assert!(healthy.noul.unwrap_or(0.0) > 0.5, "healthy log should read as healthy: {healthy:?}");
    }

    #[test]
    fn decode_shapes_roundtrip() {
        let raw = serde_json::json!({
            "wander": { "type": "noul", "noul": 0.71 },
            "mode": { "type": "choice", "choice": "exploring", "confidence": 0.9 }
        });
        let parsed: std::collections::BTreeMap<String, JevAnswer> =
            serde_json::from_value(raw).unwrap();
        assert_eq!(parsed["wander"].noul, Some(0.71));
        assert_eq!(parsed["mode"].choice.as_deref(), Some("exploring"));
    }
}
