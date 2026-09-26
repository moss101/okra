//! okra-policy — the safety plane (MASTER-PLAN §3 #19-#26, M1/M2).
//!
//! Fused blocks:
//! - deepseek fail-closed approvals: closed outcome union, exactly one
//!   granting outcome, `never` decided pre-dispatch, audit pair
//!   (`approval.rs`, packages/interaction/user-approval)
//! - CLEAN-ROOM arg-hash grants bound to `(tool, args hash, policy
//!   version)` — no reference product has this (`grants.rs`, §3 #22)
//! - deepseek confine(argv) + enforcement honesty types + denial dialects
//!   (`confine.rs`, packages/sandbox)
//! - grok sandbox profiles + anti-hollow-out project merge
//!   (`profiles.rs`, xai-grok-sandbox/src/profiles.rs)
//! - qwen permission lattice deny>ask>default + mediation policies
//!   (`lattice.rs`)
//! - grok managed policy ceiling with fail-closed parse (`approval.rs`,
//!   xai-tool-runtime/src/context.rs:166-265)

pub mod approval;
pub mod confine;
pub mod grants;
pub mod lattice;
pub mod profiles;

pub use approval::{
    normalize_outcome, parse_ceiling, ApprovalAuditEvent, ApprovalChannel, ApprovalOutcome,
    ApprovalPolicy, ApprovalRequest, ApprovalService, ToolApprovalCeiling,
};
pub use confine::{
    confinable, classify_failure, matches_denial, ConfinedArgv, FailureClassification, PartialWrapperBackend,
    RunnerFailureRule, SandboxEnforcement, SandboxError, SandboxExecutionPolicy, SandboxMode,
    SandboxProvider,
};
pub use grants::{Grant, GrantDecision, GrantScope, GrantStore};
pub use lattice::{Decision, MediationPolicy, PermissionLattice, RuleEffect, RuleSource, PermissionRule};
pub use profiles::{
    merge_configs, parse_profile_name, resolve_profile, strict_profile, PathAccess, ProfileName,
    ProfileConfig, SandboxConfig, SandboxProfile,
};
