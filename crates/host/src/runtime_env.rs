//! Host domain: runtime environment — row-48 re-homed from ZCode
//! `packages/services/src/logger/serviceLogger.ts` + `runtime-tools/`
//! (runtimeLoginShellEnvCapture.ts, agentProxyEnv.ts, nodeEnv.ts).
//!
//! Three pieces, contracts ported verbatim (env-key names renamed
//! `ZCODE_*` → `OKRA_*` per N0002):
//!
//! 1. **Scoped service logger**: `[YYYY-MM-DD HH:mm:ss.mmm] [pid:N] [scope][trace:ID]`
//!    prefixes; `debug` prints only in an effective development runtime —
//!    decided by the app-injected `OKRA_RUNTIME_ENV` (never `NODE_ENV`,
//!    which user shells leak into the daemon). Timestamps are UTC here
//!    (deterministic, TZ-independent) where the TS original used local time.
//! 2. **Login-shell env capture**: GUI/remote daemons never pass through a
//!    login shell, so the user's own profile is replayed in a probe shell —
//!    `$SHELL` → /bin/zsh → /bin/bash → /bin/sh (first executable), login
//!    args (`-ilc` for zsh/bash), a minimal bootstrap PATH so the probe can
//!    start, `TERM=dumb CI=1`, marker-framed `env -0` output (NUL-separated
//!    so multi-line values survive), hard timeout (4s) + output budget
//!    (2 MiB) with POSIX process-group kill so profile-spawned descendants
//!    cannot pin the capture.
//! 3. **Agent runtime env patch**: settings-page proxy / No Proxy / custom
//!    CA translate to child env at spawn ("next agent start" semantics).
//!    Uppercase standard keys are SET so explicit settings override
//!    inherited shell variables; values without a scheme get `http://`.

use std::collections::BTreeMap;
use std::time::Duration;

// ---------------------------------------------------------------------------
// 1. scoped service logger (serviceLogger.ts + log-format.ts)
// ---------------------------------------------------------------------------

/// "YYYY-MM-DD HH:mm:ss.mmm" in UTC (the TS original formats local time;
/// the daemon logs UTC so traces compare across machines).
pub fn format_timestamp_utc_ms(unix_ms: u128) -> String {
    let secs = (unix_ms / 1000) as i64;
    let millis = (unix_ms % 1000) as u32;
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let (h, m, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    // civil-from-days (Howard Hinnant's algorithm)
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mth = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if mth <= 2 { y + 1 } else { y };
    format!("{y:04}-{mth:02}-{d:02} {h:02}:{m:02}:{s:02}.{millis:03}")
}

/// `[YYYY-MM-DD HH:mm:ss.mmm] [pid:N] [source]` (log-format.ts).
pub fn format_log_prefix_at(source: &str, pid: Option<u32>, unix_ms: u128) -> String {
    let ts = format_timestamp_utc_ms(unix_ms);
    match pid {
        Some(pid) => format!("[{ts}] [pid:{pid}] [{source}]"),
        None => format!("[{ts}] [{source}]"),
    }
}

/// `NODE_ENV` is a general variable that user shells leak; only an explicit
/// app-injected `OKRA_RUNTIME_ENV=development` enables development behavior
/// (nodeEnv.ts, same rule).
pub fn is_effective_development_env(env: &BTreeMap<String, String>) -> bool {
    env.get("OKRA_RUNTIME_ENV").map(String::as_str) == Some("development")
}

/// Where log lines go. The TS service injects a `console`-like sink.
pub trait LogSink: Send + Sync {
    fn write(&self, level: &str, line: &str);
}

/// Writes to stderr.
pub struct StderrSink;

impl LogSink for StderrSink {
    fn write(&self, _level: &str, line: &str) {
        eprintln!("{line}");
    }
}

/// Scoped, level-gated logger (serviceLogger.ts). Cloneable; sinks shared.
#[derive(Clone)]
pub struct ServiceLogger {
    scope: String,
    pid: Option<u32>,
    sink: std::sync::Arc<dyn LogSink>,
    is_debug_enabled: DebugGate,
}

#[derive(Clone)]
enum DebugGate {
    /// default: development runtime only
    RuntimeEnv,
    Always(bool),
}

impl ServiceLogger {
    pub fn new(scope: &str) -> Self {
        ServiceLogger {
            scope: scope.to_string(),
            pid: Some(std::process::id()),
            sink: std::sync::Arc::new(StderrSink),
            is_debug_enabled: DebugGate::RuntimeEnv,
        }
    }

    pub fn with_pid(mut self, pid: Option<u32>) -> Self {
        self.pid = pid;
        self
    }

    pub fn with_sink(mut self, sink: std::sync::Arc<dyn LogSink>) -> Self {
        self.sink = sink;
        self
    }

    /// TS `isDebugEnabled` override (bool or closure; here a fixed state).
    pub fn with_debug(self, enabled: bool) -> Self {
        ServiceLogger {
            is_debug_enabled: DebugGate::Always(enabled),
            ..self
        }
    }

    fn write(
        &self,
        env: &BTreeMap<String, String>,
        level: &str,
        trace_id: Option<&str>,
        message: &str,
        unix_ms: u128,
    ) {
        if level == "debug" {
            match &self.is_debug_enabled {
                DebugGate::RuntimeEnv if !is_effective_development_env(env) => return,
                DebugGate::Always(false) => return,
                _ => {}
            }
        }
        // trace-carrying scope nests into the bracketed source:
        // `[scope][trace:ID]`
        let source = match trace_id {
            Some(id) => format!("{}][trace:{}", self.scope, id),
            None => self.scope.clone(),
        };
        self.sink.write(
            level,
            &format!("{} {message}", format_log_prefix_at(&source, self.pid, unix_ms)),
        );
    }

    pub fn debug(&self, env: &BTreeMap<String, String>, trace_id: Option<&str>, message: &str) {
        self.write(env, "debug", trace_id, message, now_ms());
    }

    pub fn info(&self, trace_id: Option<&str>, message: &str) {
        self.write(&BTreeMap::new(), "info", trace_id, message, now_ms());
    }

    pub fn warn(&self, trace_id: Option<&str>, message: &str) {
        self.write(&BTreeMap::new(), "warn", trace_id, message, now_ms());
    }

    pub fn error(&self, trace_id: Option<&str>, message: &str) {
        self.write(&BTreeMap::new(), "error", trace_id, message, now_ms());
    }

    /// Deterministic write for tests: supply the clock and env. Gated like
    /// `write` — a suppressed line is an EMPTY string.
    pub fn write_at(
        &self,
        env: &BTreeMap<String, String>,
        level: &str,
        trace_id: Option<&str>,
        message: &str,
        unix_ms: u128,
    ) -> String {
        if level == "debug" {
            match &self.is_debug_enabled {
                DebugGate::RuntimeEnv if !is_effective_development_env(env) => {
                    return String::new()
                }
                DebugGate::Always(false) => return String::new(),
                _ => {}
            }
        }
        let source = match trace_id {
            Some(id) => format!("{}][trace:{}", self.scope, id),
            None => self.scope.clone(),
        };
        format!(
            "{level} {} {message}",
            format_log_prefix_at(&source, self.pid, unix_ms)
        )
    }
}

fn now_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// 2. login-shell env capture (runtimeLoginShellEnvCapture.ts)
// ---------------------------------------------------------------------------

pub const LOGIN_ENV_CAPTURE_PREFIX: &str = "__OKRA_LOGIN_ENV_START__";
pub const LOGIN_ENV_CAPTURE_SUFFIX: &str = "__OKRA_LOGIN_ENV_END__";

/// darwin-first bootstrap PATH (linux list follows the TS fallback).
pub const DEFAULT_POSIX_BOOTSTRAP_PATH: &str =
    "/usr/local/bin:/opt/homebrew/bin:/usr/bin:/bin:/usr/sbin:/sbin";

/// Default output budget: 2 MiB (the TS maxBuffer).
pub const DEFAULT_MAX_BUFFER: usize = 2 * 1024 * 1024;
/// Default capture deadline: 4s (the TS timeout).
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(4);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CaptureError {
    Timeout { ms: u64 },
    BufferLimit { bytes: usize },
    Exit { code: Option<i32>, signal: Option<String> },
    Io(String),
}

impl std::fmt::Display for CaptureError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CaptureError::Timeout { ms } => {
                write!(f, "login shell environment capture timed out after {ms}ms")
            }
            CaptureError::BufferLimit { bytes } => write!(
                f,
                "login shell environment capture exceeded {bytes} bytes"
            ),
            CaptureError::Exit { code, signal } => write!(
                f,
                "login shell environment capture exited with code {}, signal {}",
                code.map(|c| c.to_string()).unwrap_or_else(|| "null".into()),
                signal.clone().unwrap_or_else(|| "none".into())
            ),
            CaptureError::Io(e) => write!(f, "login shell capture: {e}"),
        }
    }
}

impl std::error::Error for CaptureError {}

#[cfg(unix)]
fn is_executable_file(path: &str) -> bool {
    let c = match std::ffi::CString::new(path) {
        Ok(c) => c,
        Err(_) => return false,
    };
    unsafe { libc::access(c.as_ptr(), libc::X_OK) == 0 }
}

/// Windows first pass (docs/m6-windows-port.md): treat an existing file
/// with an executable-ish extension as runnable; the login-shell probe
/// falls through to PowerShell via the caller's candidate list anyway.
#[cfg(not(unix))]
fn is_executable_file(path: &str) -> bool {
    std::path::Path::new(path).is_file()
}

/// `$SHELL` → /bin/zsh → /bin/bash → /bin/sh, first executable.
pub fn resolve_shell_path(base_env: &BTreeMap<String, String>) -> Option<String> {
    let candidates = [
        base_env.get("SHELL").map(String::as_str),
        Some("/bin/zsh"),
        Some("/bin/bash"),
        Some("/bin/sh"),
    ];
    for candidate in candidates.into_iter().flatten() {
        if !candidate.is_empty() && is_executable_file(candidate) {
            return Some(candidate.to_string());
        }
    }
    None
}

/// GUI/remote daemons inherit a PATH that never went through a login
/// shell: give the probe shell a minimal system PATH first, and let the
/// user's own profile replay their command paths in front.
pub fn build_shell_bootstrap_path(current_path: Option<&str>) -> String {
    match current_path.filter(|p| !p.is_empty()) {
        Some(existing) => format!("{DEFAULT_POSIX_BOOTSTRAP_PATH}:{existing}"),
        None => DEFAULT_POSIX_BOOTSTRAP_PATH.to_string(),
    }
}

/// login+interactive for zsh/bash (profile scripts), plain login otherwise.
pub fn build_login_shell_args(shell_path: &str) -> Vec<String> {
    let command = format!(
        "printf '%s\\0' '{LOGIN_ENV_CAPTURE_PREFIX}'; env -0; printf '%s\\0' '{LOGIN_ENV_CAPTURE_SUFFIX}'"
    );
    let interactive = shell_path.ends_with("/zsh") || shell_path.ends_with("/bash");
    vec![
        if interactive { "-ilc".to_string() } else { "-lc".to_string() },
        command,
    ]
}

/// The probe-shell execution seam (the TS injects `executeShell` for
/// tests; production uses the real process runner).
pub trait LoginShellExecutor {
    fn execute(
        &self,
        shell_path: &str,
        args: &[String],
        env: &BTreeMap<String, String>,
        timeout: Duration,
        max_buffer: usize,
    ) -> Result<String, CaptureError>;
}

/// Real runner. SANCTIONED SPAWN SITE: the probe executes a host-resolved
/// shell from a fixed candidate list with host-built arguments — never
/// model text; the captured output is data, not instructions.
pub struct RealLoginShellExecutor;

impl LoginShellExecutor for RealLoginShellExecutor {
    fn execute(
        &self,
        shell_path: &str,
        args: &[String],
        env: &BTreeMap<String, String>,
        timeout: Duration,
        max_buffer: usize,
    ) -> Result<String, CaptureError> {
        use std::process::{Command, Stdio};

        // sanctioned site (see above)
        #[allow(clippy::disallowed_methods)]
        let mut command: std::process::Command = Command::new(shell_path);
        command
            .args(args)
            .env_clear()
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        for (k, v) in env {
            command.env(k, v);
        }
        // POSIX: own process group so profile-spawned descendants that
        // inherit stdout/stderr die with the group at the deadline
        // (Windows: direct-child kill only — Job Objects are the recorded
        // second-pass shim)
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        // sanctioned site: same probe spawn, see the comment on Command::new
        #[allow(clippy::disallowed_methods)]
        let mut child = command.spawn().map_err(|e| CaptureError::Io(e.to_string()))?;

        let mut stdout = Vec::new();
        let deadline = std::time::Instant::now() + timeout;
        let exited;
        loop {
            if let Ok(status) = child.try_wait() {
                if let Some(status) = status {
                    exited = status;
                    break;
                }
                continue;
            }
            if std::time::Instant::now() >= deadline {
                // kill the whole group, then the direct child fallback
                // (unix: group SIGKILL; windows: direct kill — Job Objects
                // tree-kill is the recorded second-pass shim)
                #[cfg(unix)]
                {
                    let pid = child.id();
                    let _ = unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
                }
                let _ = child.kill();
                let _ = child.wait();
                return Err(CaptureError::Timeout {
                    ms: timeout.as_millis() as u64,
                });
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        // reap the pipes after exit
        use std::io::Read;
        if let Some(mut out) = child.stdout.take() {
            let mut buf = Vec::new();
            let _ = out.read_to_end(&mut buf);
            stdout = buf;
        }
        if stdout.len() > max_buffer {
            return Err(CaptureError::BufferLimit { bytes: max_buffer });
        }
        let status = exited;
        if !status.success() {
            return Err(CaptureError::Exit {
                code: status.code(),
                signal: None,
            });
        }
        String::from_utf8(stdout).map_err(|e| CaptureError::Io(e.to_string()))
    }
}

/// `env -0` body → map: `KEY=VALUE` entries, key must be a valid name
/// (bad entries are skipped, never fatal); values keep embedded `=`.
pub fn parse_null_separated_env_snapshot(raw: &str) -> BTreeMap<String, String> {
    let mut result = BTreeMap::new();
    for entry in raw.split('\0') {
        if entry.is_empty() {
            continue;
        }
        let Some(sep) = entry.find('=') else { continue };
        if sep == 0 {
            continue;
        }
        let key = entry[..sep].trim();
        if !valid_env_name(key) {
            continue;
        }
        result.insert(key.to_string(), entry[sep + 1..].to_string());
    }
    result
}

fn valid_env_name(key: &str) -> bool {
    let mut chars = key.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Cut the capture body between the LAST start/end markers (profiles echo
/// marker-like text; the framing markers win).
pub fn extract_captured_env_snapshot(raw: &str) -> Option<BTreeMap<String, String>> {
    let start_marker = format!("{LOGIN_ENV_CAPTURE_PREFIX}\0");
    let end_marker = format!("{LOGIN_ENV_CAPTURE_SUFFIX}\0");
    let start = raw.rfind(&start_marker)?;
    let end = raw.rfind(&end_marker).filter(|e| *e > start)?;
    Some(parse_null_separated_env_snapshot(
        &raw[start + start_marker.len()..end],
    ))
}

/// Orchestration: resolve shell → build probe env → execute → extract.
pub fn capture_login_shell_env_snapshot(
    executor: &dyn LoginShellExecutor,
    options: &CaptureOptions,
) -> Result<BTreeMap<String, String>, CaptureError> {
    let base = &options.base_env;
    let Some(shell_path) = (options.shell_path.as_deref())
        .map(str::to_string)
        .or_else(|| resolve_shell_path(base))
    else {
        return Err(CaptureError::Io("no executable login shell found".into()));
    };
    let mut env = base.clone();
    env.insert(
        "PATH".to_string(),
        build_shell_bootstrap_path(base.get("PATH").map(String::as_str)),
    );
    env.insert("TERM".to_string(), "dumb".to_string());
    env.insert("CI".to_string(), "1".to_string());
    let args = build_login_shell_args(&shell_path);
    let raw = executor.execute(
        &shell_path,
        &args,
        &env,
        options.timeout,
        options.max_buffer,
    )?;
    extract_captured_env_snapshot(&raw).ok_or(CaptureError::Exit {
        code: None,
        signal: None,
    })
}

#[derive(Debug, Clone)]
pub struct CaptureOptions {
    pub base_env: BTreeMap<String, String>,
    pub shell_path: Option<String>,
    pub timeout: Duration,
    pub max_buffer: usize,
}

impl Default for CaptureOptions {
    fn default() -> Self {
        CaptureOptions {
            base_env: BTreeMap::new(),
            shell_path: None,
            timeout: DEFAULT_TIMEOUT,
            max_buffer: DEFAULT_MAX_BUFFER,
        }
    }
}

// ---------------------------------------------------------------------------
// 3. agent runtime env patch (agentProxyEnv.ts)
// ---------------------------------------------------------------------------

/// The settings page's proxy / No Proxy / custom CA → child env patch,
/// applied at spawn ("next agent start"). Uppercase standard keys are SET
/// (overriding inherited shell variables); `OKRA_HTTP_PROXY` /
/// `OKRA_NO_PROXY` / `OKRA_AGENT_CA_CERT` carry the same values for
/// runtime code that must not re-read user shell variables.
pub fn build_agent_runtime_env(
    http_proxy: Option<&str>,
    no_proxy: Option<&str>,
    ca_cert_path: Option<&str>,
) -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    if let Some(v) = normalize_proxy_value(http_proxy) {
        for key in ["HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY", "OKRA_HTTP_PROXY"] {
            env.insert(key.to_string(), v.clone());
        }
    }
    let tokens: Vec<String> = no_proxy
        .unwrap_or_default()
        .split(',')
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .map(str::to_string)
        .collect();
    if !tokens.is_empty() {
        let joined = tokens.join(",");
        for key in ["NO_PROXY", "no_proxy", "OKRA_NO_PROXY"] {
            env.insert(key.to_string(), joined.clone());
        }
    }
    if let Some(ca) = ca_cert_path.map(str::trim).filter(|c| !c.is_empty()) {
        // NODE_EXTRA_CA_CERTS must exist before the Node process starts;
        // OKRA_AGENT_CA_CERT keeps the path available across runtimes
        env.insert("NODE_EXTRA_CA_CERTS".to_string(), ca.to_string());
        env.insert("OKRA_AGENT_CA_CERT".to_string(), ca.to_string());
    }
    env
}

/// The host's resolved API origin is INJECTED (not re-derived) so both
/// sides compute the same trust decisions; spawn-time semantics like the
/// rest of the patch.
pub fn build_agent_endpoint_origin_env(endpoint_origin: Option<&str>) -> BTreeMap<String, String> {
    let trimmed = endpoint_origin.map(str::trim).filter(|s| !s.is_empty());
    match trimmed {
        Some(origin) => BTreeMap::from([("OKRA_BASE_URL".to_string(), origin.to_string())]),
        None => BTreeMap::new(),
    }
}

fn normalize_proxy_value(value: Option<&str>) -> Option<String> {
    let trimmed = value?.trim();
    if trimmed.is_empty() {
        return None;
    }
    // scheme already present (http://, socks5://, …) passes through;
    // bare host:port gets http://
    let has_scheme = trimmed
        .split("://")
        .next()
        .map(|scheme| {
            !scheme.is_empty()
                && scheme
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '.' | '-'))
                && trimmed.len() > scheme.len()
        })
        .unwrap_or(false);
    if has_scheme {
        Some(trimmed.to_string())
    } else {
        Some(format!("http://{trimmed}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[test]
    fn timestamp_and_prefix_format_match_the_ts_shape() {
        // 2026-09-27T12:00:00.123Z
        let ms: u128 = 1_790_510_400_123;
        assert_eq!(format_timestamp_utc_ms(ms), "2026-09-27 12:00:00.123");
        assert_eq!(
            format_log_prefix_at("mcp-sync", Some(4242), ms),
            "[2026-09-27 12:00:00.123] [pid:4242] [mcp-sync]"
        );
        assert_eq!(
            format_log_prefix_at("mcp-sync", None, ms),
            "[2026-09-27 12:00:00.123] [mcp-sync]"
        );
    }

    #[test]
    fn debug_gating_follows_runtime_env_and_override() {
        let mut env = BTreeMap::new();
        let logger = ServiceLogger::new("svc").with_pid(Some(7));
        // production (default): debug suppressed, info passes
        let line = logger.write_at(&env, "debug", Some("t1"), "hello", 1_790_510_400_123);
        assert!(line.is_empty(), "{line}");
        let line = logger.write_at(&env, "info", Some("t1"), "hello", 1_790_510_400_123);
        assert_eq!(
            line,
            "info [2026-09-27 12:00:00.123] [pid:7] [svc][trace:t1] hello"
        );
        // development runtime: debug passes with the trace-carrying source
        env.insert("OKRA_RUNTIME_ENV".into(), "development".into());
        let line = logger.write_at(&env, "debug", Some("t1"), "hello", 1_790_510_400_123);
        assert_eq!(
            line,
            "debug [2026-09-27 12:00:00.123] [pid:7] [svc][trace:t1] hello"
        );
        // NODE_ENV leaks from shells and must NOT enable debug
        env.clear();
        env.insert("NODE_ENV".into(), "development".into());
        assert!(logger.write_at(&env, "debug", None, "x", 1).is_empty());
        // explicit override wins over the runtime
        let forced = ServiceLogger::new("svc").with_debug(true);
        assert!(!forced.write_at(&env, "debug", None, "x", 1).is_empty());
    }

    #[test]
    #[cfg(unix)]
    fn shell_resolution_and_args_follow_the_candidate_ladder() {
        let mut env = BTreeMap::new();
        env.insert("SHELL".into(), "/bin/bash".into());
        assert_eq!(resolve_shell_path(&env).as_deref(), Some("/bin/bash"));
        // SHELL pointing nowhere falls to zsh then bash then sh
        let mut env2 = BTreeMap::new();
        env2.insert("SHELL".into(), "/nonexistent/shell".into());
        let resolved = resolve_shell_path(&env2);
        assert!(matches!(
            resolved.as_deref(),
            Some("/bin/zsh") | Some("/bin/bash") | Some("/bin/sh")
        ));
        assert_eq!(
            build_login_shell_args("/bin/zsh")[0],
            "-ilc",
            "zsh gets login+interactive"
        );
        assert_eq!(
            build_login_shell_args("/bin/sh")[0],
            "-lc",
            "non-zsh/bash gets plain login"
        );
        assert!(build_login_shell_args("/bin/sh")[1].contains("env -0"));
    }

    #[test]
    fn bootstrap_path_prepends_the_minimal_system_paths() {
        assert_eq!(
            build_shell_bootstrap_path(Some("/custom/bin")),
            format!("{DEFAULT_POSIX_BOOTSTRAP_PATH}:/custom/bin")
        );
        assert_eq!(
            build_shell_bootstrap_path(None),
            DEFAULT_POSIX_BOOTSTRAP_PATH
        );
        assert_eq!(build_shell_bootstrap_path(Some("")), DEFAULT_POSIX_BOOTSTRAP_PATH);
    }

    #[test]
    fn marker_extraction_cuts_profile_noise_and_rejects_bad_names() {
        let noisy = format!(
            "zsh: compinit stuff\necho from profile\n{LOGIN_ENV_CAPTURE_PREFIX}\0\
             PATH=/usr/bin:/bin\0\
             EDITOR=vim\0\
             =novalue\0\
             9BAD=x\0\
             MULTILINE=line1\nline2\0\
             EMPTY=\0\
             {LOGIN_ENV_CAPTURE_SUFFIX}\0some trailing echo"
        );
        let snap = extract_captured_env_snapshot(&noisy).unwrap();
        assert_eq!(snap.get("PATH").map(String::as_str), Some("/usr/bin:/bin"));
        assert_eq!(snap.get("MULTILINE").map(String::as_str), Some("line1\nline2"));
        assert_eq!(snap.get("EMPTY").map(String::as_str), Some(""));
        assert!(!snap.contains_key("9BAD"));
        assert!(!snap.contains_key(""));
        // values keep embedded '=' (only the FIRST '=' splits)
        let with_eq = format!("{LOGIN_ENV_CAPTURE_PREFIX}\0VAR=a=b=c\0{LOGIN_ENV_CAPTURE_SUFFIX}\0");
        let snap = extract_captured_env_snapshot(&with_eq).unwrap();
        assert_eq!(snap.get("VAR").map(String::as_str), Some("a=b=c"));
        // missing markers → None (the profile swallowed the probe)
        assert!(extract_captured_env_snapshot("no markers here").is_none());
        // end before start → None
        let reversed = format!("{LOGIN_ENV_CAPTURE_SUFFIX}\0{LOGIN_ENV_CAPTURE_PREFIX}\0");
        assert!(extract_captured_env_snapshot(&reversed).is_none());
    }

    struct FakeExecutor(Result<String, CaptureError>);
    impl LoginShellExecutor for FakeExecutor {
        fn execute(&self, _s: &str, _a: &[String], _e: &BTreeMap<String, String>, _t: Duration, _m: usize) -> Result<String, CaptureError> {
            self.0.clone()
        }
    }

    #[test]
    #[cfg(unix)]
    fn capture_orchestration_builds_probe_env_and_extracts() {
        let mut base = BTreeMap::new();
        base.insert("HOME".to_string(), "/Users/dev".to_string());
        base.insert("PATH".to_string(), "/custom".to_string());
        let noisy = format!(
            "{LOGIN_ENV_CAPTURE_PREFIX}\0HOME=/Users/dev\0PATH=/Users/dev/bin:/usr/bin\0{LOGIN_ENV_CAPTURE_SUFFIX}\0"
        );
        let seen: Mutex<Option<BTreeMap<String, String>>> = Mutex::new(None);
        let exec = InspectExecutor {
            inner: FakeExecutor(Ok(noisy)),
            seen: &seen,
        };
        let opts = CaptureOptions { base_env: base.clone(), ..Default::default() };
        let snap = capture_login_shell_env_snapshot(&exec, &opts).unwrap();
        assert_eq!(snap.get("HOME").map(String::as_str), Some("/Users/dev"));
        let env = seen.lock().unwrap().clone().unwrap();
        // probe env: bootstrap PATH prepended, TERM=dumb, CI=1
        assert!(env["PATH"].starts_with(DEFAULT_POSIX_BOOTSTRAP_PATH));
        assert!(env["PATH"].ends_with(":/custom"));
        assert_eq!(env["TERM"], "dumb");
        assert_eq!(env["CI"], "1");
        assert_eq!(env["HOME"], "/Users/dev");

        // executor failure surfaces verbatim (timeout / budget / exit)
        let err = capture_login_shell_env_snapshot(&FakeExecutor(Err(CaptureError::Timeout { ms: 4000 })), &opts).unwrap_err();
        assert_eq!(err, CaptureError::Timeout { ms: 4000 });
        let err = capture_login_shell_env_snapshot(
            &FakeExecutor(Ok("unframed output".into())),
            &opts,
        )
        .unwrap_err();
        assert!(matches!(err, CaptureError::Exit { .. }), "unframed output is a failed capture, not silent success");
    }

    struct InspectExecutor<'a> {
        inner: FakeExecutor,
        seen: &'a Mutex<Option<BTreeMap<String, String>>>,
    }
    impl LoginShellExecutor for InspectExecutor<'_> {
        fn execute(&self, shell: &str, args: &[String], env: &BTreeMap<String, String>, t: Duration, m: usize) -> Result<String, CaptureError> {
            assert!(shell.ends_with("sh") || shell.ends_with("bash") || shell.ends_with("zsh"));
            assert_eq!(args.len(), 2);
            assert_eq!((t, m), (DEFAULT_TIMEOUT, DEFAULT_MAX_BUFFER));
            *self.seen.lock().unwrap() = Some(env.clone());
            self.inner.execute(shell, args, env, t, m)
        }
    }

    #[test]
    #[cfg(unix)]
    fn real_shell_capture_round_trip() {
        // a REAL capture through /bin/sh: PATH must exist and no markers leak
        let snap = capture_login_shell_env_snapshot(
            &RealLoginShellExecutor,
            &CaptureOptions::default(),
        )
        .unwrap_or_else(|e| panic!("real capture failed: {e}"));
        assert!(snap.contains_key("PATH"), "PATH always present: {snap:?}");
        assert!(!snap.iter().any(|(k, _)| k.starts_with("__OKRA_LOGIN_ENV")));
    }

    #[test]
    fn agent_env_patch_normalizes_and_prioritizes_settings() {
        // bare host:port gets http://; uppercase four keys set together
        let patch = build_agent_runtime_env(Some("proxy.local:8080"), None, None);
        for key in ["HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY", "OKRA_HTTP_PROXY"] {
            assert_eq!(patch.get(key).map(String::as_str), Some("http://proxy.local:8080"), "{key}");
        }
        // existing scheme passes through untouched
        let patch = build_agent_runtime_env(Some("socks5://gate:1080"), Some(" a.local, b.local "), None);
        assert_eq!(patch.get("HTTPS_PROXY").map(String::as_str), Some("socks5://gate:1080"));
        assert_eq!(patch.get("NO_PROXY").map(String::as_str), Some("a.local,b.local"));
        assert_eq!(patch.get("no_proxy").map(String::as_str), Some("a.local,b.local"));
        // empty/whitespace inputs produce no keys at all
        let empty = build_agent_runtime_env(Some("   "), Some(","), None);
        assert!(empty.is_empty(), "{empty:?}");
        // CA path: trimmed, both keys
        let ca = build_agent_runtime_env(None, None, Some(" /certs/custom.pem "));
        assert_eq!(ca.get("NODE_EXTRA_CA_CERTS").map(String::as_str), Some("/certs/custom.pem"));
        assert_eq!(ca.get("OKRA_AGENT_CA_CERT").map(String::as_str), Some("/certs/custom.pem"));
        // endpoint origin: injected verbatim, empty → absent
        let origin = build_agent_endpoint_origin_env(Some(" https://api.example.com "));
        assert_eq!(origin.get("OKRA_BASE_URL").map(String::as_str), Some("https://api.example.com"));
        assert!(build_agent_endpoint_origin_env(Some("  ")).is_empty());
    }
}
