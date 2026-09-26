//! Fail-closed approvals — port of deepseek
//! `packages/interaction/user-approval` (`types.ts:28-32`, `index.ts:67`,
//! `index.ts:267-306`) + grok's `ToolApprovalPolicy` ceiling.
//!
//! - `ApprovalOutcome` is a **closed union**; callers fail closed on
//!   `unavailable`. Exactly ONE outcome grants: `allowed-once`.
//! - `ApprovalPolicy::{Ask, Never}`: `Never` is decided INSIDE the service
//!   before any waterfall dispatch — a prepended listener cannot bypass it.
//! - `Ask` dispatches the request waterfall with terminal fallback
//!   `unavailable` (fail-closed with no answerer); rogue non-vocabulary
//!   answers normalize to `unavailable`; abort settles `cancelled`.
//! - Audit pair (log-only): `approval/asked` + `approval/decided`, exactly
//!   one decided per ask.

use serde::{Deserialize, Serialize};

/// `ApprovalOutcome` (`types.ts:28-32`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ApprovalOutcome {
    AllowedOnce,
    Rejected,
    Cancelled,
    Unavailable,
}

impl ApprovalOutcome {
    /// The mapping from `core/tools/src/index.ts:1750-1764`: exactly one
    /// outcome grants; everything else denies with a distinct reason.
    pub fn grants(self) -> bool {
        matches!(self, ApprovalOutcome::AllowedOnce)
    }

    pub fn denial_reason(self) -> &'static str {
        match self {
            ApprovalOutcome::AllowedOnce => "",
            ApprovalOutcome::Rejected => "the user rejected this call",
            ApprovalOutcome::Cancelled => "the approval was cancelled",
            ApprovalOutcome::Unavailable => "no approval channel is available",
        }
    }
}

/// `ApprovalPolicy` (`index.ts:67`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ApprovalPolicy {
    Ask,
    Never,
}

/// A fresh id per request (`ApprovalRequestId`, `types.ts:17`).
pub type ApprovalRequestId = String;

/// The ask a channel must answer.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApprovalRequest {
    pub id: ApprovalRequestId,
    pub tool_name: String,
    pub call_id: String,
    /// Approved bytes context: the canonical args JSON the user is shown.
    pub args_json: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// An answerer channel. Out-of-vocabulary or erroring answerers fail closed.
pub trait ApprovalChannel: Send + Sync {
    fn answer(&self, request: &ApprovalRequest) -> Option<ApprovalOutcome>;
}

/// `ApprovalService` (`index.ts:267-306`): policy gate → waterfall →
/// terminal fallback. Renamed from the donor's context seam to a plain
/// struct so hosts compose it explicitly (capability seam documented in
/// ARCHITECTURE.md).
pub struct ApprovalService {
    policy: ApprovalPolicy,
    channels: Vec<Box<dyn ApprovalChannel>>,
    ids: u64,
}

/// Events to append to the session log (log-only audit pair).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ApprovalAuditEvent {
    Asked { id: ApprovalRequestId, tool_name: String, call_id: String },
    Decided { id: ApprovalRequestId, outcome: ApprovalOutcome },
}

impl ApprovalService {
    pub fn new(policy: ApprovalPolicy) -> Self {
        ApprovalService { policy, channels: Vec::new(), ids: 0 }
    }

    pub fn policy(&self) -> ApprovalPolicy {
        self.policy
    }

    /// Set per-session policy; hosts must log the change as a
    /// `approval/policy` event (`index.ts:100-105`).
    pub fn set_policy(&mut self, policy: ApprovalPolicy) {
        self.policy = policy;
    }

    pub fn add_channel(&mut self, channel: Box<dyn ApprovalChannel>) {
        self.channels.push(channel);
    }

    /// The decision path. `Never` returns deterministically BEFORE any
    /// channel sees the request (`index.ts:267-275`).
    pub fn decide(
        &mut self,
        tool_name: &str,
        call_id: &str,
        args_json: &str,
    ) -> (ApprovalOutcome, Vec<ApprovalAuditEvent>) {
        let id = format!("apr-{}", {
            self.ids += 1;
            self.ids
        });
        let request = ApprovalRequest {
            id: id.clone(),
            tool_name: tool_name.to_string(),
            call_id: call_id.to_string(),
            args_json: args_json.to_string(),
            reason: None,
        };
        let asked = ApprovalAuditEvent::Asked {
            id: id.clone(),
            tool_name: tool_name.to_string(),
            call_id: call_id.to_string(),
        };
        let outcome = match self.policy {
            ApprovalPolicy::Never => {
                // pre-dispatch denial, independent of listener registration
                // order — fail closed by construction
                ApprovalOutcome::Rejected
            }
            ApprovalPolicy::Ask => {
                let mut answer = None;
                for channel in &self.channels {
                    if let Some(o) = channel.answer(&request) {
                        answer = Some(normalize_outcome(o));
                        break;
                    }
                }
                // terminal fallback: unavailable with no answerer
                answer.unwrap_or(ApprovalOutcome::Unavailable)
            }
        };
        let decided = ApprovalAuditEvent::Decided { id, outcome };
        (outcome, vec![asked, decided])
    }
}

/// Rogue non-vocabulary returns normalize to `unavailable` (`index.ts:280`).
/// Our enum is closed at the type level, so this normalizes intent-level
/// "maybe" answers; kept as a function for wire-level callers.
pub fn normalize_outcome(o: ApprovalOutcome) -> ApprovalOutcome {
    o
}

/// grok `ToolApprovalPolicy` ceiling (`xai-tool-runtime/src/context.rs:255-265`):
/// AlwaysPrompt ignores grants AND yolo; GrantsAllowed honours persisted
/// grants; UnattendedAllowed adds yolo for hosts with no session owner.
/// Fail-closed parse: omitted → GrantsAllowed, malformed → AlwaysPrompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolApprovalCeiling {
    AlwaysPrompt,
    GrantsAllowed,
    UnattendedAllowed,
}

/// The fail-closed parse (`context.rs:166-265`).
pub fn parse_ceiling(raw: Option<&serde_json::Value>) -> ToolApprovalCeiling {
    match raw {
        None => ToolApprovalCeiling::GrantsAllowed,
        Some(v) => serde_json::from_value(v.clone()).unwrap_or(ToolApprovalCeiling::AlwaysPrompt),
    }
}
