//! Arg-hash grants — CLEAN-ROOM (MASTER-PLAN §3 #22): every reference
//! product binds approvals to tool name (or prefix) only; okra binds grants
//! to `(tool, args hash, policy version)`.
//!
//! Invariants:
//! - A grant authorizes EXACTLY the approved bytes (canonical args JSON) for
//!   EXACTLY one behavior version of one tool. Any drift — one byte of args,
//!   a tool behavior bump, a policy-version change — invalidates it.
//! - Only `AllowedOnce` approvals may mint grants, and only when the
//!   ceiling permits (`grants_allowed` or wider).
//! - Grants are conversation-scoped; `session` grants die with the session.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashSet;

use crate::approval::{ApprovalOutcome, ApprovalScope, ToolApprovalCeiling};

/// Scope of a grant's lifetime.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GrantScope {
    /// One call (stored for dedup/idempotency, never reused for approval).
    Once,
    /// The conversation (session) lifetime.
    Conversation,
    /// Host-managed persistent grant (ZCode per-folder grants analog).
    Persistent,
}

/// A stored grant bound to the exact approved bytes.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Grant {
    pub tool: String,
    /// sha256(canonical args JSON) — the approved bytes.
    pub args_hash: String,
    /// Tool behavior version in force at approval (bytewise compare).
    pub behavior_version: String,
    /// The approval ceiling value the grant was minted under; a ceiling
    /// tightening invalidates grants.
    pub policy_version: u32,
    pub scope: GrantScope,
}

/// A WEAKER session-scoped grant (#53 "this conversation" / "always"):
/// bound to `(tool, behavior version, policy version)` WITHOUT the args
/// hash. Invariants that keep it honest:
/// - created ONLY from an explicit user scope choice — never minted
///   implicitly from a plain approval, never minted by any code path the
///   model controls;
/// - dies with the session (never serialized into durable settings);
/// - a persistent "always" STILL only takes the session grant + a
///   suggested ruleset update (#24) — silent cross-session persistence of
///   an un-hashed tool grant is not a thing okra does.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionToolGrant {
    pub tool: String,
    pub behavior_version: String,
    pub policy_version: u32,
}

/// What one scoped approval decided to record (#53).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopedRecord {
    /// The arg-hash-bound grant (always minted for a granted outcome,
    /// except under the AlwaysPrompt ceiling).
    pub grant: Option<Grant>,
    /// The weaker tool-level grant, only for scope Conversation/Always.
    pub session_tool: Option<SessionToolGrant>,
    /// True when the user chose "always": the caller should surface a
    /// suggested ruleset update (#24) for explicit confirmation.
    pub suggests_rule: bool,
}

impl Grant {
    /// Mint from approved bytes. `args_json` must be the canonical
    /// serialization stored on the ApprovedInvocation.
    pub fn mint(
        tool: &str,
        args_json: &str,
        behavior_version: &str,
        policy_version: u32,
        scope: GrantScope,
    ) -> Grant {
        let mut h = Sha256::new();
        h.update(args_json.as_bytes());
        let args_hash = format!("{:x}", h.finalize());
        Grant {
            tool: tool.to_string(),
            args_hash,
            behavior_version: behavior_version.to_string(),
            policy_version,
            scope,
        }
    }

    pub fn covers(&self, tool: &str, args_json: &str, behavior_version: &str, policy_version: u32) -> bool {
        let mut h = Sha256::new();
        h.update(args_json.as_bytes());
        let args_hash = format!("{:x}", h.finalize());
        self.tool == tool
            && self.args_hash == args_hash
            && self.behavior_version == behavior_version
            && self.policy_version == policy_version
    }
}

/// The grant store. `check` never mutates; `mint` enforces the ceiling.
#[derive(Debug, Default)]
pub struct GrantStore {
    conversation: HashSet<Grant>,
    persistent: HashSet<Grant>,
    once: HashSet<Grant>,
    session_tools: HashSet<SessionToolGrant>,
    policy_version: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrantDecision {
    Granted,
    NotGranted,
}

impl GrantStore {
    pub fn new(policy_version: u32) -> Self {
        GrantStore { policy_version, ..Default::default() }
    }

    /// Mint a grant from an approval outcome. Only AllowedOnce approvals
    /// mint; scope mapping: once → Once (dedup only), conversation →
    /// Conversation, host-persistent callers mint directly.
    pub fn record_approval(
        &mut self,
        outcome: crate::approval::ApprovalOutcome,
        tool: &str,
        args_json: &str,
        behavior_version: &str,
        ceiling: ToolApprovalCeiling,
        allow_persistent: bool,
    ) -> Option<Grant> {
        // Pre-#53 callers pass allow_persistent; map it to the equivalent
        // scope so their behavior is byte-identical (persistent arg-hash
        // grant). The extra session-tool grant and the ruleset suggestion
        // flag are additive and ignorable by legacy callers.
        let scope = if allow_persistent {
            crate::approval::ApprovalScope::Always
        } else {
            crate::approval::ApprovalScope::Once
        };
        self.record_approval_scoped(
            outcome,
            scope,
            tool,
            args_json,
            behavior_version,
            ceiling,
            allow_persistent,
        )
        .grant
    }

    /// The scoped recording path (#53). Scope mapping:
    /// - `Once` → arg-hash conversation grant (pre-#53 behavior: a retry
    ///   of the identical approved bytes does not re-prompt);
    /// - `Conversation` → that PLUS a session tool grant (no args hash —
    ///   the weaker binding the user explicitly asked for);
    /// - `Always` → both of those, plus a persistent arg-hash grant when
    ///   the host permits persistence, plus `suggests_rule = true` (the
    ///   caller must STILL surface a ruleset suggestion for explicit
    ///   confirmation — never auto-persist).
    ///
    /// Everything fails closed: non-granting outcomes record nothing; the
    /// `AlwaysPrompt` ceiling records nothing at all.
    #[allow(clippy::too_many_arguments)] // the (tool, bytes, versions, ceiling) tuple IS the contract
    pub fn record_approval_scoped(
        &mut self,
        outcome: ApprovalOutcome,
        scope: ApprovalScope,
        tool: &str,
        args_json: &str,
        behavior_version: &str,
        ceiling: ToolApprovalCeiling,
        allow_persistent: bool,
    ) -> ScopedRecord {
        let none = ScopedRecord { grant: None, session_tool: None, suggests_rule: false };
        if !outcome.grants() {
            return none;
        }
        if ceiling == ToolApprovalCeiling::AlwaysPrompt {
            // every mutating call prompts; grants are ignored
            return none;
        }
        let session_tool = match scope {
            ApprovalScope::Once => None,
            ApprovalScope::Conversation | ApprovalScope::Always => {
                Some(SessionToolGrant {
                    tool: tool.to_string(),
                    behavior_version: behavior_version.to_string(),
                    policy_version: self.policy_version,
                })
            }
        };
        // The arg-hash grant: Once/Conversation live for the session;
        // Always additionally persists (arg-hash bound, so even the
        // persistent record authorizes exactly the approved bytes).
        let grant_scope = if scope == ApprovalScope::Always && allow_persistent {
            GrantScope::Persistent
        } else {
            GrantScope::Conversation
        };
        let grant = Grant::mint(tool, args_json, behavior_version, self.policy_version, grant_scope);
        match grant_scope {
            GrantScope::Persistent => {
                self.persistent.insert(grant.clone());
            }
            _ => {
                self.conversation.insert(grant.clone());
            }
        }
        if let Some(st) = &session_tool {
            self.session_tools.insert(st.clone());
        }
        ScopedRecord {
            grant: Some(grant),
            session_tool,
            suggests_rule: scope == ApprovalScope::Always,
        }
    }

    /// Does a session tool grant cover this call? (#53) The policy version
    /// is the store's own — a ceiling change invalidates session grants.
    pub fn check_session_tool(&self, tool: &str, behavior_version: &str) -> bool {
        self.session_tools.contains(&SessionToolGrant {
            tool: tool.to_string(),
            behavior_version: behavior_version.to_string(),
            policy_version: self.policy_version,
        })
    }

    pub fn session_tool_grants(&self) -> usize {
        self.session_tools.len()
    }

    /// Idempotency record for auto-retry: exact-once call tracking (§3 #63's
    /// "no side-effecting tool executes twice per call ID" bookkeeping).
    pub fn record_once(&mut self, tool: &str, args_json: &str, behavior_version: &str) -> Grant {
        let grant = Grant::mint(tool, args_json, behavior_version, self.policy_version, GrantScope::Once);
        self.once.insert(grant.clone());
        grant
    }

    pub fn seen_once(&self, tool: &str, args_json: &str, behavior_version: &str) -> bool {
        let g = Grant::mint(tool, args_json, behavior_version, self.policy_version, GrantScope::Once);
        self.once.contains(&g)
    }

    pub fn check(
        &self,
        tool: &str,
        args_json: &str,
        behavior_version: &str,
        scope: GrantScope,
    ) -> GrantDecision {
        let probe = Grant::mint(tool, args_json, behavior_version, self.policy_version, scope);
        let hit = match scope {
            GrantScope::Once => self.once.contains(&probe),
            GrantScope::Conversation => self.conversation.contains(&probe),
            GrantScope::Persistent => self.persistent.contains(&probe),
        };
        if hit { GrantDecision::Granted } else { GrantDecision::NotGranted }
    }

    /// Revocation: clearing persistent grants (workspace-hook trust revocation
    /// analog, ZCode `workspaceHookSettingsModel.ts`). Session tool grants
    /// die with it too — a revocation never leaves the weaker grants
    /// behind.
    pub fn revoke_persistent(&mut self) {
        self.persistent.clear();
        self.session_tools.clear();
    }

    pub fn len(&self) -> usize {
        self.conversation.len() + self.persistent.len() + self.once.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}
