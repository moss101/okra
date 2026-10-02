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

/// The lifetime the user attaches to an ALLOW (MASTER-PLAN §3 #53: "Allow
/// once / this conversation / always / Deny"). The OUTCOME union stays the
/// closed deepseek four — exactly one outcome grants — and the scope only
/// parameterizes what the grant mint may record. `Always` NEVER silently
/// persists: it takes effect for the session immediately and additionally
/// produces a suggested ruleset update (§3 #24) that a human must confirm
/// before it lands in project settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum ApprovalScope {
    /// These bytes, this session (grant is arg-hash-bound; a retry of the
    /// same approved bytes does not re-prompt).
    #[default]
    Once,
    /// This tool for the rest of the conversation (session tool grant —
    /// WEAKER than the arg-hash binding, chosen explicitly by a human,
    /// never minted implicitly, dies with the session).
    Conversation,
    /// From now on (session tool grant now + a suggested project rule
    /// pending confirmation).
    Always,
}

/// A channel answer with its scope. A plain `ApprovalOutcome` answer maps
/// to scope `Once` (the pre-#53 behavior, byte-for-byte).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ApprovalAnswer {
    pub outcome: ApprovalOutcome,
    pub scope: ApprovalScope,
}

impl ApprovalAnswer {
    pub fn new(outcome: ApprovalOutcome) -> ApprovalAnswer {
        ApprovalAnswer { outcome, scope: ApprovalScope::Once }
    }

    pub fn scoped(outcome: ApprovalOutcome, scope: ApprovalScope) -> ApprovalAnswer {
        ApprovalAnswer { outcome, scope }
    }
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

    /// Scoped answer (#53). Default: delegate to `answer` with scope
    /// `Once`, so every pre-existing channel keeps working unchanged and
    /// the default stays the tightest scope.
    fn answer_scoped(&self, request: &ApprovalRequest) -> Option<ApprovalAnswer> {
        self.answer(request).map(ApprovalAnswer::new)
    }
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
    Asked {
        id: ApprovalRequestId,
        tool_name: String,
        call_id: String,
        /// The approved bytes the user was shown (surfaces replay the
        /// proposed action from this).
        args_json: String,
    },
    Decided {
        id: ApprovalRequestId,
        outcome: ApprovalOutcome,
        /// The scope the user attached to an allow (#53). `Option` with
        /// serde default so logs written before #53 replay unchanged.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        scope: Option<ApprovalScope>,
    },
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
        let (outcome, _scope, audit) = self.decide_scoped(tool_name, call_id, args_json);
        (outcome, audit)
    }

    /// The decision path with the user's scope (#53). Same fail-closed
    /// waterfall; the scope only rides on a granted answer.
    pub fn decide_scoped(
        &mut self,
        tool_name: &str,
        call_id: &str,
        args_json: &str,
    ) -> (ApprovalOutcome, ApprovalScope, Vec<ApprovalAuditEvent>) {
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
            args_json: args_json.to_string(),
        };
        let answer = match self.policy {
            ApprovalPolicy::Never => {
                // pre-dispatch denial, independent of listener registration
                // order — fail closed by construction
                ApprovalAnswer::new(ApprovalOutcome::Rejected)
            }
            ApprovalPolicy::Ask => {
                let mut answer = None;
                for channel in &self.channels {
                    if let Some(a) = channel.answer_scoped(&request) {
                        answer = Some(normalize_answer(a));
                        break;
                    }
                }
                // terminal fallback: unavailable with no answerer
                answer.unwrap_or(ApprovalAnswer::new(ApprovalOutcome::Unavailable))
            }
        };
        let (outcome, scope) = (answer.outcome, answer.scope);
        if !outcome.grants() {
            // a denial carries no scope, whatever the channel claimed
            let scope = ApprovalScope::Once;
            let decided = ApprovalAuditEvent::Decided { id, outcome, scope: None };
            return (outcome, scope, vec![asked, decided]);
        }
        let decided = ApprovalAuditEvent::Decided { id, outcome, scope: Some(scope) };
        (outcome, scope, vec![asked, decided])
    }
}

/// Rogue non-vocabulary returns normalize to `unavailable` (`index.ts:280`).
/// Our enum is closed at the type level, so this normalizes intent-level
/// "maybe" answers; kept as a function for wire-level callers.
pub fn normalize_outcome(o: ApprovalOutcome) -> ApprovalOutcome {
    o
}

/// A rogue answer normalizes to deny, and a denial always carries the
/// tightest scope (a channel claiming scope `Always` on a denial is
/// clamped — the scope only exists where the outcome grants).
pub fn normalize_answer(a: ApprovalAnswer) -> ApprovalAnswer {
    let outcome = normalize_outcome(a.outcome);
    if outcome.grants() {
        ApprovalAnswer { outcome, scope: a.scope }
    } else {
        ApprovalAnswer::new(outcome)
    }
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
