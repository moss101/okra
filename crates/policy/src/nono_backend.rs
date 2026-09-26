//! Kernel-enforced self-confinement via `nono` — the outcome of the N0004
//! revisit (decision N0006).
//!
//! grok's `xai-grok-sandbox` is a ~6k-line wrapper whose *kernel layer* is
//! the public crate `nono = "=0.53.0"` (Landlock on Linux, Seatbelt on
//! macOS — pinned exact per the donor's deny-precedence note). Extracting
//! the wrapper would also have dragged in two internal `xai-*` crates, so
//! okra consumes `nono` directly and ports the profile→capability mapping
//! with citations:
//!
//! - `Sandbox::apply` is **irreversible, process-wide** confinement — the
//!   honest kernel primitive for okra's in-process tool model (decision
//!   N0001). Unlike an argv wrapper, even okra's own tool code is confined.
//! - Profile mapping (`xai-grok-sandbox/src/profiles.rs:28-84` semantics):
//!   `ReadOnly`/`Strict` → workspace read-only + network blocked;
//!   `WorkspaceWrite` → workspace read-write; `DangerFullAccess` is never
//!   confinable (fail closed, `confine()` contract).
//! - Enforcement honesty: `SupportInfo` decides full vs unavailable; there
//!   is no silent passthrough.
//!
//! The sessions directory is admitted read-write so the kernel event log —
//! the only durable truth — stays writable under confinement.

use std::path::{Path, PathBuf};

use crate::confine::{SandboxEnforcement, SandboxError, SandboxExecutionPolicy, SandboxMode};
use crate::profiles::ProfileName;

/// Report returned after applying (or failing to apply) self-confinement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfinementReport {
    pub enforcement: SandboxEnforcement,
    pub platform: String,
    pub details: String,
    pub workspace: PathBuf,
    pub network_blocked: bool,
}

/// Apply kernel confinement to the CURRENT process.
pub trait SelfConfinement {
    fn apply_to_self(
        &self,
        policy: &SandboxExecutionPolicy,
        extra_writable: &[PathBuf],
    ) -> Result<ConfinementReport, SandboxError>;
}

/// The nono-backed backend (Landlock/Seatbelt).
pub struct NonoSandboxBackend {
    /// Network blocked regardless of mode (strict/offline operation).
    pub force_block_network: bool,
    /// Admit the system temp dir read-write. OFF by default: okra's atomic
    /// writes use temp siblings INSIDE the target directory, so granting the
    /// global temp dir would widen the writable surface for no benefit
    /// (and would silently cover workspaces that live under it).
    pub allow_temp: bool,
}

impl NonoSandboxBackend {
    pub fn new() -> Self {
        NonoSandboxBackend { force_block_network: false, allow_temp: false }
    }

    pub fn blocking_network() -> Self {
        NonoSandboxBackend { force_block_network: true, allow_temp: false }
    }

    fn platform_support() -> Result<nono::SupportInfo, SandboxError> {
        let info = nono::Sandbox::support_info();
        if !info.is_supported {
            return Err(SandboxError::Unavailable(format!(
                "kernel sandbox unsupported on {}: {}",
                info.platform, info.details
            )));
        }
        Ok(info)
    }

    fn capability_set(
        &self,
        policy: &SandboxExecutionPolicy,
        extra_writable: &[PathBuf],
    ) -> Result<nono::CapabilitySet, SandboxError> {
        if !crate::confine::confinable(policy) {
            return Err(SandboxError::NotConfinable(policy.mode));
        }
        let network_blocked =
            self.force_block_network || policy.mode == SandboxMode::ReadOnly;

        let mut caps = nono::CapabilitySet::new();
        // system read allows: the process must keep loading binaries/libs
        for sys_path in system_read_paths() {
            caps = caps
                .allow_path(&sys_path, nono::AccessMode::Read)
                .map_err(|e| SandboxError::Unavailable(format!("allow {sys_path:?}: {e}")))?;
        }
        // temp dir: only when requested (see field docs)
        if self.allow_temp {
            let tmp = std::env::temp_dir();
            caps = caps
                .allow_path(&tmp, nono::AccessMode::ReadWrite)
                .map_err(|e| SandboxError::Unavailable(format!("allow temp: {e}")))?;
        }
        // workspace per mode
        caps = caps
            .allow_path(
                &policy.workspace_root,
                if policy.mode == SandboxMode::ReadOnly {
                    nono::AccessMode::Read
                } else {
                    nono::AccessMode::ReadWrite
                },
            )
            .map_err(|e| SandboxError::Unavailable(format!("allow workspace: {e}")))?;
        // the kernel event log must stay writable in every mode
        for path in extra_writable {
            caps = caps
                .allow_path(path, nono::AccessMode::ReadWrite)
                .map_err(|e| SandboxError::Unavailable(format!("allow {path:?}: {e}")))?;
        }
        if network_blocked {
            caps = caps.block_network();
        }
        Ok(caps)
    }
}

impl Default for NonoSandboxBackend {
    fn default() -> Self {
        Self::new()
    }
}

fn system_read_paths() -> Vec<PathBuf> {
    if cfg!(target_os = "macos") {
        vec![
            PathBuf::from("/usr"),
            PathBuf::from("/bin"),
            PathBuf::from("/sbin"),
            PathBuf::from("/System"),
            PathBuf::from("/private/var"),
            PathBuf::from("/dev"),
        ]
    } else {
        vec![
            PathBuf::from("/usr"),
            PathBuf::from("/bin"),
            PathBuf::from("/sbin"),
            PathBuf::from("/lib"),
            PathBuf::from("/lib64"),
            PathBuf::from("/etc"),
            PathBuf::from("/dev"),
        ]
    }
}

impl SelfConfinement for NonoSandboxBackend {
    fn apply_to_self(
        &self,
        policy: &SandboxExecutionPolicy,
        extra_writable: &[PathBuf],
    ) -> Result<ConfinementReport, SandboxError> {
        let info = Self::platform_support()?;
        let caps = self.capability_set(policy, extra_writable)?;
        let network_blocked =
            self.force_block_network || policy.mode == SandboxMode::ReadOnly;
        nono::Sandbox::apply(&caps)
            .map_err(|e| SandboxError::Unavailable(format!("kernel apply failed: {e}")))?;
        Ok(ConfinementReport {
            enforcement: SandboxEnforcement::Full,
            platform: info.platform.to_string(),
            details: info.details,
            workspace: policy.workspace_root.clone(),
            network_blocked,
        })
    }
}

/// Profile-name → mode mapping helper (`profiles.rs:69-84` spellings).
pub fn mode_for_profile_name(name: &ProfileName) -> Option<SandboxMode> {
    match name {
        ProfileName::ReadOnly | ProfileName::Strict => Some(SandboxMode::ReadOnly),
        ProfileName::Workspace | ProfileName::Devbox => Some(SandboxMode::WorkspaceWrite),
        ProfileName::Off => Some(SandboxMode::DangerFullAccess),
        ProfileName::Custom(_) => Some(SandboxMode::WorkspaceWrite),
    }
}

/// True if `path` sits inside `dir` or is that directory (lexical check).
pub fn path_within(path: &Path, dir: &Path) -> bool {
    path.starts_with(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn platform_reports_support_honestly() {
        // macOS: Seatbelt supported; Linux: Landlock (kernel dependent);
        // either way the backend must FAIL CLOSED when unsupported.
        let info = NonoSandboxBackend::platform_support();
        match info {
            Ok(info) => assert!(info.is_supported),
            Err(SandboxError::Unavailable(_)) => {} // honest unavailability
            Err(e) => panic!("unexpected error: {e}"),
        }
    }

    #[test]
    fn danger_full_access_is_never_confinable() {
        let backend = NonoSandboxBackend::new();
        let policy = SandboxExecutionPolicy {
            mode: SandboxMode::DangerFullAccess,
            workspace_root: std::path::PathBuf::from("/tmp"),
            session_id: None,
        };
        let err = backend
            .apply_to_self(&policy, &[])
            .expect_err("full access must not confine");
        assert!(matches!(
            err,
            SandboxError::NotConfinable(SandboxMode::DangerFullAccess)
        ));
    }

    #[test]
    fn profile_names_map_to_modes() {
        assert_eq!(
            mode_for_profile_name(&crate::profiles::parse_profile_name("strict")),
            Some(SandboxMode::ReadOnly)
        );
        assert_eq!(
            mode_for_profile_name(&crate::profiles::parse_profile_name("workspace")),
            Some(SandboxMode::WorkspaceWrite)
        );
        assert_eq!(
            mode_for_profile_name(&crate::profiles::parse_profile_name("off")),
            Some(SandboxMode::DangerFullAccess)
        );
    }

    #[test]
    fn path_within_basic() {
        assert!(path_within(Path::new("/w/src/a.rs"), Path::new("/w")));
        assert!(!path_within(Path::new("/etc/passwd"), Path::new("/w")));
    }
}
