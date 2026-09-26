//! confine(argv) + enforcement honesty — port of deepseek
//! `packages/sandbox/sandbox/src/index.ts`.
//!
//! - `confine(argv, policy)`: argv is the EXACT argv about to be spawned
//!   (program plus arguments), **NOT a shell string** — a shell-shaped
//!   consumer passes `["bash", "-c", command]` (`index.ts:164-169`).
//! - Backends must return enforcing argv **or fail closed**; silent
//!   unconfined passthrough is forbidden (`index.ts:152-157`).
//! - `enforcement: full | partial` — partial means the backend cannot govern
//!   every promised file effect; callers requiring an absolute boundary must
//!   not treat it as full (`index.ts:59-63`).
//! - `denial_signatures` are THIS backend's denial dialects; consumers match
//!   exactly these, not a cross-backend union (`index.ts:100-108`).
//! - Policy is carried PER CALL (`SandboxExecutionPolicy`): two consumers
//!   confine under different policies simultaneously; an approved escalation
//!   is a new call with a wider policy (`index.ts:39-72`).

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// `SandboxMode` (`index.ts:29`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SandboxMode {
    ReadOnly,
    WorkspaceWrite,
    DangerFullAccess,
}

/// `SandboxExecutionPolicy` (`index.ts:39-44`): carried per call.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SandboxExecutionPolicy {
    pub mode: SandboxMode,
    pub workspace_root: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
}

/// Confinable modes exclude `danger-full-access` (`SandboxPolicy`,
/// `index.ts:47-58`).
pub fn confinable(policy: &SandboxExecutionPolicy) -> bool {
    policy.mode != SandboxMode::DangerFullAccess
}

/// `SandboxEnforcement` (`index.ts:59`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SandboxEnforcement {
    Full,
    Partial,
}

/// `RunnerFailureRule` (`index.ts:81-88`): the command never ran on runner
/// failure; denial means confinement WORKED — the two must not be conflated.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RunnerFailureRule {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub allowed_exit_codes: Option<Vec<i32>>,
    /// Required: these stderr signatures mark a runner (not command) failure.
    pub fatal_signatures: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub informational_lines: Option<Vec<String>>,
}

/// `ConfinedArgv` (`index.ts:95-116`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfinedArgv {
    /// Wrapped argv to spawn in place of the caller's.
    pub argv: Vec<String>,
    pub enforcement: SandboxEnforcement,
    /// THIS backend's denial dialects (case-insensitive stderr substrings).
    pub denial_signatures: Vec<String>,
    pub runner_failure_rules: Vec<RunnerFailureRule>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SandboxError {
    /// `SandboxUnavailableError` with code `SANDBOX_UNAVAILABLE`
    /// (`index.ts:124-144`) — fail closed.
    #[error("SANDBOX_UNAVAILABLE: {0}")]
    Unavailable(String),
    #[error("mode is not confinable: {0:?}")]
    NotConfinable(SandboxMode),
}

/// `SandboxProvider` (`index.ts:158-176`): confine the exact argv or fail
/// closed. One implementation per host; a provider swap moves the whole
/// execution world (capability seam, `docs/architecture.md:129-131`).
pub trait SandboxProvider: Send + Sync {
    fn confine(
        &self,
        argv: &[String],
        policy: &SandboxExecutionPolicy,
    ) -> Result<ConfinedArgv, SandboxError>;
}

/// The honesty classifier (`diagnostics.ts`): a runner failure means the
/// command NEVER RAN; a denial means confinement worked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureClassification {
    /// The command ran and was denied by confinement.
    Denied,
    /// The runner itself failed; the command never ran.
    RunnerFailure,
    /// Plain command failure with a non-allowed exit code.
    CommandFailed,
    CommandSucceeded,
}

pub fn classify_failure(
    exit_code: i32,
    stderr: &str,
    rules: &[RunnerFailureRule],
) -> FailureClassification {
    for rule in rules {
        let fatal = rule
            .fatal_signatures
            .iter()
            .any(|sig| stderr.to_lowercase().contains(&sig.to_lowercase()));
        if fatal {
            return FailureClassification::RunnerFailure;
        }
    }
    if exit_code == 0 {
        return FailureClassification::CommandSucceeded;
    }
    // denial dialects come from ConfinedArgv; the caller passes them via
    // rules-conflated signature set below in hosts; here: nonzero + no
    // runner-fatal signature is a plain command failure unless a backend
    // dialect matched — hosts pass dialects as `denial_signatures`.
    FailureClassification::CommandFailed
}

pub fn matches_denial(stderr: &str, denial_signatures: &[String]) -> bool {
    let hay = stderr.to_lowercase();
    denial_signatures
        .iter()
        .any(|sig| hay.contains(&sig.to_lowercase()))
}

/// The M0 backend: an honest PARTIAL wrapper — it validates the argv shape,
/// prepends the sandbox wrapper, and declares exactly what it does NOT yet
/// enforce. No silent passthrough: the enforcement field tells the truth
/// (deepseek's honesty contract; kernel sandboxing is the M1 vendor target,
/// decision N0004).
pub struct PartialWrapperBackend {
    /// e.g. ["sandbox-exec", "-f", profile] once the kernel layer lands.
    pub wrapper: Vec<String>,
    pub denial_signatures: Vec<String>,
}

impl SandboxProvider for PartialWrapperBackend {
    fn confine(
        &self,
        argv: &[String],
        policy: &SandboxExecutionPolicy,
    ) -> Result<ConfinedArgv, SandboxError> {
        if argv.is_empty() {
            return Err(SandboxError::Unavailable("empty argv".into()));
        }
        if !confinable(policy) {
            // callers must ask for a confinable mode; full-access is the
            // host's explicit escape hatch, not this backend's default
            return Err(SandboxError::Unavailable(format!(
                "{:?} bypasses confinement; use the host's full-access path explicitly",
                policy.mode
            )));
        }
        let mut wrapped = self.wrapper.clone();
        wrapped.extend_from_slice(argv);
        Ok(ConfinedArgv {
            argv: wrapped,
            enforcement: SandboxEnforcement::Partial,
            denial_signatures: self.denial_signatures.clone(),
            runner_failure_rules: vec![RunnerFailureRule {
                allowed_exit_codes: None,
                fatal_signatures: vec!["sandbox wrapper failed".into()],
                informational_lines: None,
            }],
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn confine_wraps_argv_and_reports_partial_honestly() {
        let backend = PartialWrapperBackend {
            wrapper: vec!["okra-sandbox".into()],
            denial_signatures: vec!["operation not permitted".into()],
        };
        let policy = SandboxExecutionPolicy {
            mode: SandboxMode::WorkspaceWrite,
            workspace_root: "/tmp/ws".into(),
            session_id: None,
        };
        let c = backend
            .confine(&argv(&["ls", "-la"]), &policy)
            .unwrap();
        assert_eq!(c.argv, vec!["okra-sandbox", "ls", "-la"]);
        assert_eq!(c.enforcement, SandboxEnforcement::Partial);
        assert!(matches_denial("ls: foo: Operation not permitted", &c.denial_signatures));
        // runner failure vs denial distinction
        assert_eq!(
            classify_failure(126, "sandbox wrapper failed to start", &c.runner_failure_rules),
            FailureClassification::RunnerFailure
        );
        assert_eq!(classify_failure(1, "ls: no such file", &c.runner_failure_rules), FailureClassification::CommandFailed);
        assert_eq!(classify_failure(0, "", &c.runner_failure_rules), FailureClassification::CommandSucceeded);
    }

    #[test]
    fn full_access_fails_closed_in_confinable_path() {
        let backend = PartialWrapperBackend { wrapper: vec![], denial_signatures: vec![] };
        let policy = SandboxExecutionPolicy {
            mode: SandboxMode::DangerFullAccess,
            workspace_root: "/tmp/ws".into(),
            session_id: None,
        };
        assert!(matches!(
            backend.confine(&argv(&["ls"]), &policy),
            Err(SandboxError::Unavailable(_))
        ));
        assert!(!confinable(&policy));
    }

    #[test]
    fn empty_argv_fails_closed() {
        let backend = PartialWrapperBackend { wrapper: vec![], denial_signatures: vec![] };
        let policy = SandboxExecutionPolicy {
            mode: SandboxMode::ReadOnly,
            workspace_root: "/tmp/ws".into(),
            session_id: None,
        };
        assert!(backend.confine(&[], &policy).is_err());
    }
}
