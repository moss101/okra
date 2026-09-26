//! Origin-tagged context messages — port of kimi
//! `contextMemory/types.ts:114-126` (12-member origin union), extended with
//! okra's steering origin (13 members). Origins ride on every injected
//! context message so compaction can treat each class by its own retention
//! rules (M2).

use serde::{Deserialize, Serialize};

/// The closed origin union (`types.ts:114-126` + okra extension).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Origin {
    /// The user typed it.
    User,
    /// Steering injected mid-turn (okra: interjections).
    Steering,
    /// A hook emitted it.
    Hook,
    /// System reminder (stationarity nudges, salvage reminders).
    SystemReminder,
    /// Compaction summary.
    CompactionSummary,
    /// File-state hydration after compaction (ZCode block #29).
    FileHydration,
    /// Memory recall injected into context.
    MemoryRecall,
    /// Skill activation content.
    Skill,
    /// Tool result context.
    ToolContext,
    /// Subagent projection (inherit-nothing boundary marker, §3 #43).
    SubagentProjection,
    /// Workflow run observation.
    WorkflowObservation,
    /// Provider-level recovery (XML tool-call recovery etc.).
    ProviderRecovery,
    /// World-state projection section (§3 #32).
    WorldState,
}

impl Origin {
    /// Origins eligible for microcompaction eviction (oldest first) in M2.
    pub fn evictable(self) -> bool {
        matches!(
            self,
            Origin::ToolContext
                | Origin::FileHydration
                | Origin::WorkflowObservation
                | Origin::MemoryRecall
        )
    }

    /// Origins that must survive compaction verbatim (user intent, goals).
    pub fn preserved(self) -> bool {
        matches!(self, Origin::User)
    }
}

/// A tagged context message: the unit compaction reasons about.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OriginTaggedMessage {
    pub origin: Origin,
    pub text: String,
    /// Absolute seq of the log event this came from (traceability).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_seq: Option<u64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origin_union_is_closed_and_extensible_by_code_only() {
        // serialization is snake_case; unknown values on the wire are
        // rejected (closed union) — a new origin means editing this enum
        assert!(serde_json::from_value::<Origin>(serde_json::json!("steering")).is_ok());
        assert!(serde_json::from_value::<Origin>(serde_json::json!("mystery")).is_err());
    }

    #[test]
    fn eviction_classes() {
        assert!(Origin::ToolContext.evictable());
        assert!(!Origin::User.evictable());
        assert!(Origin::User.preserved());
    }
}
