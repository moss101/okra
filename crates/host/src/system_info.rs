//! System info + integrated terminal shells (MASTER-PLAN §3 #48, from
//! ZCode `packages/services/src/system/`): the facts surfaces and the
//! agent need about the host machine.
//!
//! Donor contracts kept:
//! - `info()`: homedir + platform (+ okra's own version) — the snapshot a
//!   surface renders on the "about" panel;
//! - **integrated terminal shells**: candidates per platform (zsh/fish/
//!   bash on unix, PowerShell/cmd/git-bash on Windows), with existence
//!   checked via an injectable `is_executable` so tests never depend on
//!   the build machine's installed shells;
//! - **intranet probe**: bounded TCP connect attempts against a target
//!   list with a required-success-count quorum and a strategy verdict
//!   (any/all) — how the donor decides "are we on the office network".

use serde::Serialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SystemInfo {
    pub homedir: PathBuf,
    pub platform: String,
    pub okra_version: String,
    /// $SHELL when set (unix), else the platform default.
    pub shell: Option<String>,
}

/// `info()` — homedir/platform plus okra's own version.
pub fn system_info(home: &Path) -> SystemInfo {
    SystemInfo {
        homedir: home.to_path_buf(),
        platform: std::env::consts::OS.to_string(),
        okra_version: env!("CARGO_PKG_VERSION").to_string(),
        shell: std::env::var("SHELL").ok().filter(|s| !s.trim().is_empty()),
    }
}

/// Candidate login shells per platform (donor ordering: the most likely
/// default shell first).
pub fn shell_candidates(platform: &str) -> Vec<&'static str> {
    match platform {
        "windows" => vec![
            "powershell.exe",
            "pwsh.exe",
            "cmd.exe",
            r"Git\bin\bash.exe",
        ],
        _ => vec!["/bin/zsh", "/bin/bash", "/bin/fish", "/usr/bin/fish"],
    }
}

/// Injectable executable check (donor `isExecutable`).
pub trait IsExecutable: Send + Sync {
    fn is_executable(&self, path: &Path) -> bool;
}

/// The real check on unix: exists, is a file, has an execute bit.
pub struct FsExecutable;

impl IsExecutable for FsExecutable {
    fn is_executable(&self, path: &Path) -> bool {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::metadata(path)
                .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
                .unwrap_or(false)
        }
        #[cfg(not(unix))]
        {
            path.is_file()
        }
    }
}

/// `listIntegratedTerminalShells`: the candidates that exist, in the
/// donor's priority order.
pub fn integrated_terminal_shells(
    platform: &str,
    is_executable: &dyn IsExecutable,
) -> Vec<PathBuf> {
    shell_candidates(platform)
        .into_iter()
        .map(PathBuf::from)
        .filter(|p| is_executable.is_executable(p))
        .collect()
}

// ---------------------------------------------------------------------------
// Intranet probe
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProbeTarget {
    pub host: String,
    pub port: u16,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProbeOutcome {
    pub target: ProbeTarget,
    pub reachable: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IntranetProbeResult {
    pub is_intranet: bool,
    pub reached_count: u32,
    pub required_count: u32,
    pub total_targets: u32,
    pub strategy: &'static str,
    pub results: Vec<ProbeOutcome>,
}

/// The TCP connect seam — injectable so probes are hermetic in tests.
pub trait TcpProbe: Send + Sync {
    fn probe(&self, host: &str, port: u16) -> bool;
}

/// The real TCP probe with a bounded connect timeout.
pub struct TcpConnect {
    pub timeout: std::time::Duration,
}

impl TcpProbe for TcpConnect {
    fn probe(&self, host: &str, port: u16) -> bool {
        use std::net::ToSocketAddrs as _;
        match (host, port).to_socket_addrs() {
            Ok(mut addrs) => match addrs.next() {
                Some(addr) => std::net::TcpStream::connect_timeout(&addr, self.timeout).is_ok(),
                None => false,
            },
            Err(_) => false,
        }
    }
}

/// `probeIntranet`: targets are reachable when `required_count` of them
/// answer. `strategy` is "all" when the quorum equals the target count,
/// else "any" (donor `resolveProbeStrategy`).
pub fn probe_intranet(
    targets: &[ProbeTarget],
    required_count: u32,
    probe: &dyn TcpProbe,
) -> IntranetProbeResult {
    let total = targets.len() as u32;
    let required = required_count.min(total);
    let mut results = Vec::new();
    let mut reached = 0u32;
    for target in targets {
        let reachable = probe.probe(&target.host, target.port);
        if reachable {
            reached += 1;
        }
        results.push(ProbeOutcome {
            target: target.clone(),
            reachable,
        });
    }
    let strategy = if required > 0 && required == total {
        "all"
    } else {
        "any"
    };
    IntranetProbeResult {
        is_intranet: reached >= required && total > 0,
        reached_count: reached,
        required_count: required,
        total_targets: total,
        strategy,
        results,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    #[test]
    fn info_carries_platform_and_version() {
        let info = system_info(Path::new("/home/u"));
        assert_eq!(info.homedir, Path::new("/home/u"));
        assert_eq!(info.platform, std::env::consts::OS);
        assert!(!info.okra_version.is_empty());
    }

    struct FixedExec(bool);
    impl IsExecutable for FixedExec {
        fn is_executable(&self, _path: &Path) -> bool {
            self.0
        }
    }

    #[test]
    fn shell_candidates_filtered_by_existence_and_ordered() {
        let all = integrated_terminal_shells("macos", &FixedExec(true));
        assert_eq!(
            all,
            vec![
                PathBuf::from("/bin/zsh"),
                PathBuf::from("/bin/bash"),
                PathBuf::from("/bin/fish"),
                PathBuf::from("/usr/bin/fish"),
            ]
        );
        let none = integrated_terminal_shells("macos", &FixedExec(false));
        assert!(none.is_empty());
        // windows candidates ordered powershell-first
        let win = shell_candidates("windows");
        assert_eq!(win.first(), Some(&"powershell.exe"));
    }

    /// succeed for the first N calls, then fail
    struct CountingProbe {
        remaining: AtomicU32,
    }
    impl TcpProbe for CountingProbe {
        fn probe(&self, _host: &str, _port: u16) -> bool {
            let previous = self.remaining.load(Ordering::SeqCst);
            previous > 0
                && self
                    .remaining
                    .compare_exchange(previous, previous - 1, Ordering::SeqCst, Ordering::SeqCst)
                    .is_ok()
        }
    }

    fn targets(n: u32) -> Vec<ProbeTarget> {
        (1..=n)
            .map(|i| ProbeTarget {
                host: format!("10.0.0.{i}"),
                port: 22,
            })
            .collect()
    }

    #[test]
    fn intranet_quorum_and_strategy() {
        // 1 of 2 reachable, quorum any(1) → intranet
        let probe = CountingProbe { remaining: AtomicU32::new(1) };
        let result = probe_intranet(&targets(2), 1, &probe);
        assert!(result.is_intranet);
        assert_eq!(result.strategy, "any");
        assert_eq!(result.reached_count, 1);

        // quorum all(2) with only 1 reachable → not intranet
        let probe = CountingProbe { remaining: AtomicU32::new(1) };
        let result = probe_intranet(&targets(2), 2, &probe);
        assert!(!result.is_intranet);
        assert_eq!(result.strategy, "all");
        assert_eq!(result.reached_count, 1);

        // zero targets: never intranet
        let probe = CountingProbe { remaining: AtomicU32::new(1) };
        let result = probe_intranet(&[], 1, &probe);
        assert!(!result.is_intranet);
    }

    #[test]
    fn tcp_connect_rejects_unroutable() {
        let probe = TcpConnect { timeout: std::time::Duration::from_millis(300) };
        assert!(!probe.probe("203.0.113.1", 1), "TEST-NET is unroutable");
    }
}
