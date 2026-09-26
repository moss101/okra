//! ToolSpec + ToolEntry — grok descriptor fields, ZCode policy metadata, and
//! the clean-room `idempotent` flag.
//!
//! - `ToolDescription` shape: grok `xai-tool-types/src/types.rs:10-46`
//!   (`behavior_version` is **bytewise-compared, NOT semver** — `capabilities.rs:33-35`).
//! - `ToolMetadata`: ZCode `apps/zcode-cli/packages/core/src/tool/types.ts:64-91`
//!   (readOnly/destructive/concurrentSafe/riskLevel/needsApproval/
//!   stopTurnOnSuccess).
//! - `idempotent`: clean-room (MASTER-PLAN §3 #18) — every reference product
//!   lacks it; safe auto-retry needs it declared per tool.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// `ToolDescription` (grok) + wire identity.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub description: String,
    /// JSON Schema for arguments.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arguments_schema: Option<Value>,
    /// Stable snake_case grouping (grok `kind`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// Bytewise-compared behavior version; a change invalidates caches and
    /// re-approvals (grok `capabilities.rs:33-35`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub behavior_version: Option<String>,
    /// CLEAN-ROOM (MASTER-PLAN §3 #18): the tool's effect is safe to
    /// auto-retry after a transport-level failure with identical arguments.
    /// Only idempotent tools may be retried without a fresh approval.
    #[serde(default)]
    pub idempotent: bool,
    /// Convenience read-only flag folded from capabilities
    /// (grok `is_read_only`; absence reads as Read for multi-agent routing).
    #[serde(default)]
    pub read_only: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_concurrency: Option<u32>,
}

/// `ModelToolSideEffectScope` (ZCode tool/types.ts) — where a side effect
/// lands, used by policy and subagent FS isolation (M5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SideEffectScope {
    None,
    Workspace,
    Machine,
    Network,
    External,
}

/// `RiskLevel` (ZCode tool/types.ts).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RiskLevel {
    None,
    Low,
    Medium,
    High,
}

/// ZCode `ToolMetadata` (`tool/types.ts:64-91`) — policy metadata the
/// executor reads instead of guessing from tool names.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolMetadata {
    pub read_only: bool,
    pub destructive: bool,
    pub concurrent_safe: bool,
    pub needs_approval: bool,
    pub side_effect_scope: SideEffectScope,
    pub risk_level: RiskLevel,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_bytes: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_in_plan_mode: Option<bool>,
    /// ZCode `stopTurnOnSuccess` (`tool/types.ts:73-85`): "success ends the
    /// turn" is an intrinsic capability declaration read by the executor —
    /// never inferred at call sites by tool name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_turn_on_success: Option<bool>,
    #[serde(default)]
    pub provider_visible: bool,
}

impl Default for ToolMetadata {
    fn default() -> Self {
        ToolMetadata {
            read_only: false,
            destructive: false,
            concurrent_safe: false,
            needs_approval: false,
            side_effect_scope: SideEffectScope::Workspace,
            risk_level: RiskLevel::Low,
            timeout_ms: None,
            max_output_bytes: None,
            allowed_in_plan_mode: None,
            stop_turn_on_success: None,
            provider_visible: true,
        }
    }
}

/// The registry entry: wire spec + policy metadata, kept in one place so the
/// two views can never drift apart.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolEntry {
    pub spec: ToolSpec,
    pub metadata: ToolMetadata,
}

impl ToolEntry {
    pub fn new(spec: ToolSpec, metadata: ToolMetadata) -> Self {
        ToolEntry { spec, metadata }
    }
}
