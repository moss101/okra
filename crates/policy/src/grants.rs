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

use crate::approval::ToolApprovalCeiling;

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
        if !outcome.grants() {
            return None;
        }
        if ceiling == ToolApprovalCeiling::AlwaysPrompt {
            // every mutating call prompts; grants are ignored
            return None;
        }
        let scope = if allow_persistent { GrantScope::Persistent } else { GrantScope::Conversation };
        let grant = Grant::mint(tool, args_json, behavior_version, self.policy_version, scope);
        match scope {
            GrantScope::Persistent => self.persistent.insert(grant.clone()),
            _ => self.conversation.insert(grant.clone()),
        };
        Some(grant)
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
    /// analog, ZCode `workspaceHookSettingsModel.ts`).
    pub fn revoke_persistent(&mut self) {
        self.persistent.clear();
    }

    pub fn len(&self) -> usize {
        self.conversation.len() + self.persistent.len() + self.once.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}
