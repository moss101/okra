//! Validated compaction summary schema — CLEAN-ROOM (MASTER-PLAN §3 #30),
//! the ChatGPT/Codex dossier's #1 gap: a summary that fails schema
//! validation is rejected, never installed.
//!
//! A summary must carry: the seq range it replaces, required sections with
//! type-checked fields, and the source event seqs it cites. The compaction
//! pipeline validates BEFORE swapping the surface nodes (kernel replace op).

use serde::{Deserialize, Serialize};

/// The validated summary body (deny_unknown_fields: drift fails loudly).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct CompactionSummary {
    /// Semantic version of the summary schema itself.
    pub schema_version: u32,
    /// Inclusive seq range replaced: [startSeq, endSeq].
    pub replaces: (u64, u64),
    /// What the user asked for (verbatim anchors, preserved).
    pub user_goals: Vec<String>,
    /// Decisions taken so far, each with the tool evidence.
    pub decisions: Vec<String>,
    /// Open questions / next actions — the "why" the turn continues.
    pub open_items: Vec<String>,
    /// File states touched: path + why it matters + last observed state.
    pub file_states: Vec<FileStateNote>,
    /// Everything else that must survive (compact prose).
    pub context_digest: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct FileStateNote {
    pub path: String,
    pub note: String,
}

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum SummaryValidationError {
    #[error("summary is not valid JSON at all")]
    NotJson,
    #[error("summary schema version {found} unsupported (want 1)")]
    UnsupportedVersion { found: u32 },
    #[error("replaces range [{0},{1}] is empty or inverted")]
    BadRange(u64, u64),
    #[error("summary must state at least one user goal")]
    MissingUserGoals,
    #[error("context digest too short to be useful ({0} chars, want >= 32)")]
    DigestTooShort(usize),
}

/// Validate before install (dossier fix: reject, never install).
pub fn validate_summary(raw: &str) -> Result<CompactionSummary, SummaryValidationError> {
    let summary: CompactionSummary =
        serde_json::from_str(raw).map_err(|_| SummaryValidationError::NotJson)?;
    check(&summary)?;
    Ok(summary)
}

/// Validate an already-parsed summary.
pub fn check(summary: &CompactionSummary) -> Result<(), SummaryValidationError> {
    if summary.schema_version != 1 {
        return Err(SummaryValidationError::UnsupportedVersion { found: summary.schema_version });
    }
    let (start, end) = summary.replaces;
    if start > end {
        return Err(SummaryValidationError::BadRange(start, end));
    }
    if summary.user_goals.is_empty() {
        return Err(SummaryValidationError::MissingUserGoals);
    }
    if summary.context_digest.chars().count() < 32 {
        return Err(SummaryValidationError::DigestTooShort(summary.context_digest.chars().count()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn good() -> String {
        serde_json::json!({
            "schemaVersion": 1,
            "replaces": [3, 41],
            "userGoals": ["refactor the scheduler without changing behavior"],
            "decisions": ["kept kimi conflict semantics for write/read overlap"],
            "openItems": ["rerun the scheduler tests"],
            "fileStates": [{"path": "src/scheduler.rs", "note": "split into 3 modules"}],
            "contextDigest": "The scheduler refactor is mid-flight; tests 3-7 still reference the old module layout and must be re-pointed at scheduler/conflict.rs."
        })
        .to_string()
    }

    #[test]
    fn valid_summary_installs() {
        let s = validate_summary(&good()).unwrap();
        assert_eq!(s.replaces, (3, 41));
        assert_eq!(s.file_states.len(), 1);
    }

    #[test]
    fn invalid_summaries_are_rejected_never_installed() {
        let parse_good = || -> serde_json::Value {
            serde_json::from_str(&good()).unwrap()
        };
        // wrong schema version
        let mut bad = parse_good();
        bad["schemaVersion"] = serde_json::json!(2);
        assert!(validate_summary(&bad.to_string()).is_err());
        // inverted range
        let mut bad = parse_good();
        bad["replaces"] = serde_json::json!([9, 3]);
        assert!(matches!(validate_summary(&bad.to_string()), Err(SummaryValidationError::BadRange(9, 3))));
        // no user goals
        let mut bad = parse_good();
        bad["userGoals"] = serde_json::json!([]);
        assert!(validate_summary(&bad.to_string()).is_err());
        // thin digest
        let mut bad = parse_good();
        bad["contextDigest"] = serde_json::json!("too short");
        assert!(validate_summary(&bad.to_string()).is_err());
        // unknown field (schema drift fails loudly)
        let mut bad = parse_good();
        bad["surprise"] = serde_json::json!(true);
        assert!(validate_summary(&bad.to_string()).is_err());
        // not JSON at all
        assert!(validate_summary("the model just wrote prose").is_err());
    }
}
