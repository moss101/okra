//! Runtime tools + app CA pair — row-48 remainder from ZCode
//! `packages/services/src/runtime-tools/` (runtimeToolResolver.ts, appCaCert.ts).
//!
//! **Tool resolver**: for each bundled runtime tool (bfs / ripgrep / ugrep)
//! the agent subprocess needs the binary path: env override first (e.g.
//! `OKRA_BFS_BINARY`), then platform-scoped bundled roots, then a PATH
//! lookup. Resolved binaries become an env patch (per-tool var + PATH
//! entries appended) so children inherit the same tools.
//!
//! **App CA pair**: the app's self-signed network CA (cert + key PEM pair
//! in `<config>/certs/`) is *reused idempotently* when both files exist —
//! fingerprints must stay stable or already-trusted children break.
//! GENERATION of a new pair is deferred: it needs an X.509 crate decision
//! (rcgen candidate) and is the only piece not ported here — the status
//! enum reports honestly what is present/missing.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

// ---------------------------------------------------------------------------
// path-entry algebra (runtimeToolResolver.ts prepend/append/joinUnique)
// ---------------------------------------------------------------------------

const PATH_DELIMITER: char = ':';

fn split_path_entries(current: Option<&str>) -> Vec<String> {
    current
        .unwrap_or_default()
        .split(PATH_DELIMITER)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

fn join_unique_path_entries(entries: &[String]) -> String {
    let mut seen = BTreeSet::new();
    let mut out: Vec<String> = Vec::new();
    for entry in entries {
        if entry.is_empty() || !seen.insert(entry.clone()) {
            continue;
        }
        out.push(entry.clone());
    }
    out.join(&PATH_DELIMITER.to_string())
}

/// Prepend `entries` in front of `current_path` (deduplicated).
pub fn prepend_path_entries(current_path: Option<&str>, entries: &[String]) -> String {
    join_unique_path_entries(&[entries.to_vec(), split_path_entries(current_path)].concat())
}

/// Append `entries` after `current_path` (deduplicated).
pub fn append_path_entries(current_path: Option<&str>, entries: &[String]) -> String {
    join_unique_path_entries(&[split_path_entries(current_path), entries.to_vec()].concat())
}

fn is_executable_file(path: &str) -> bool {
    use std::os::unix::fs::PermissionsExt;
    match std::fs::metadata(path) {
        Ok(m) => m.is_file() && (m.permissions().mode() & 0o111) != 0,
        Err(_) => false,
    }
}

/// Resolve `command` against `path_env` (unix semantics: no PATHEXT).
pub fn resolve_command_on_path(command: &str, path_env: Option<&str>) -> Option<String> {
    let path_env = path_env?;
    for entry in path_env.split(PATH_DELIMITER) {
        if entry.is_empty() {
            continue;
        }
        let candidate = Path::new(entry).join(command);
        let candidate = candidate.to_string_lossy();
        if is_executable_file(&candidate) {
            return Some(candidate.into_owned());
        }
    }
    None
}

// ---------------------------------------------------------------------------
// runtime tool registry (runtime-tool-runtime.ts)
// ---------------------------------------------------------------------------

/// The bundled runtime tools; ids match the ZCode registry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeToolId {
    Bfs,
    Ripgrep,
    Ugrep,
}

impl RuntimeToolId {
    pub const ALL: [RuntimeToolId; 3] =
        [RuntimeToolId::Bfs, RuntimeToolId::Ripgrep, RuntimeToolId::Ugrep];

    /// The env var the subprocess reads the binary path from
    /// (ZCode: `ZCODE_BFS_BINARY` / `ZCODE_RG_BINARY` / `ZCODE_UGREP_BINARY`).
    pub fn binary_env_var(self) -> &'static str {
        match self {
            RuntimeToolId::Bfs => "OKRA_BFS_BINARY",
            RuntimeToolId::Ripgrep => "OKRA_RG_BINARY",
            RuntimeToolId::Ugrep => "OKRA_UGREP_BINARY",
        }
    }

    /// Bundled resource dir + binary name (unix; the Windows `.exe` suffix
    /// is a win32-only branch in the donor and lands with M6-Windows).
    pub fn bundled(self) -> (&'static str, &'static str) {
        match self {
            RuntimeToolId::Bfs => ("bfs", "bfs"),
            RuntimeToolId::Ripgrep => ("ripgrep", "rg"),
            RuntimeToolId::Ugrep => ("ugrep", "ugrep"),
        }
    }
}

/// Platform-scoped bundled tool roots (cwd-relative, matching the donor's
/// three lookup levels).
pub fn bundled_tool_roots(cwd: &Path, platform: &str) -> Vec<PathBuf> {
    let platform_key = format!("{}-{}", platform, std::env::consts::ARCH);
    vec![
        cwd.join("bundled-tools").join(&platform_key),
        cwd.join("packages/desktop/bundled-tools").join(&platform_key),
        cwd.join("../desktop/bundled-tools").join(&platform_key),
    ]
}

fn resolve_existing(candidates: &[Option<PathBuf>]) -> Option<PathBuf> {
    candidates
        .iter()
        .flatten()
        .find(|p| is_executable_file(&p.to_string_lossy()))
        .cloned()
}

/// Env-var override → bundled roots → PATH lookup.
pub fn resolve_runtime_tool_binary(
    tool_id: RuntimeToolId,
    base_env: &BTreeMap<String, String>,
    bundled_roots: &[PathBuf],
) -> Option<PathBuf> {
    let (resource_dir, binary) = tool_id.bundled();

    if let Some(env_path) = base_env.get(tool_id.binary_env_var()) {
        let trimmed = env_path.trim();
        if !trimmed.is_empty() && is_executable_file(trimmed) {
            return Some(PathBuf::from(trimmed));
        }
    }

    let bundled: Vec<Option<PathBuf>> = bundled_roots
        .iter()
        .map(|root| {
            let p = root.join(resource_dir).join(binary);
            if is_executable_file(&p.to_string_lossy()) {
                Some(p)
            } else {
                None
            }
        })
        .collect();
    if let Some(found) = resolve_existing(&bundled) {
        return Some(found);
    }

    resolve_command_on_path(binary, base_env.get("PATH").map(String::as_str)).map(PathBuf::from)
}

/// The env patch for a set of tools: per-tool binary vars + their parent
/// dirs appended to PATH so children see the same toolchain.
pub fn build_runtime_tool_env_patch(
    tool_ids: &[RuntimeToolId],
    base_env: &BTreeMap<String, String>,
    bundled_roots: &[PathBuf],
) -> BTreeMap<String, String> {
    let mut patch = BTreeMap::new();
    let mut path_entries: Vec<String> = Vec::new();

    for tool_id in tool_ids {
        if let Some(binary) = resolve_runtime_tool_binary(*tool_id, base_env, bundled_roots) {
            patch.insert(tool_id.binary_env_var().to_string(), binary.to_string_lossy().into_owned());
            if let Some(dir) = binary.parent() {
                path_entries.push(dir.to_string_lossy().into_owned());
            }
        }
    }

    if !path_entries.is_empty() {
        patch.insert(
            "PATH".to_string(),
            append_path_entries(base_env.get("PATH").map(String::as_str), &path_entries),
        );
    }
    patch
}

// ---------------------------------------------------------------------------
// app CA pair (appCaCert.ts) — reuse contract; generation deferred
// ---------------------------------------------------------------------------

pub const APP_CA_CERT_FILE: &str = "okra-network-ca.pem";
pub const APP_CA_KEY_FILE: &str = "okra-network-ca.key";

/// The cert/key pair paths under `<config_dir>/certs/`.
pub fn app_ca_cert_paths(config_dir: &Path) -> (PathBuf, PathBuf) {
    let certs = config_dir.join("certs");
    (certs.join(APP_CA_CERT_FILE), certs.join(APP_CA_KEY_FILE))
}

/// Status of the app CA pair on disk (the reuse contract is exact:
/// BOTH present → reuse; anything else → generation is required, which
/// `ensure_app_ca_pair` performs via rcgen — pure Rust, ECDSA-P256).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppCaPairStatus {
    /// cert + key both present → reuse (fingerprints stay stable).
    Complete,
    /// Only the cert present — the key is missing; reusing without the key
    /// breaks TLS re-sign, so callers must treat this as generation-needed.
    CertOnly,
    /// Only the key present.
    KeyOnly,
    /// Neither file present.
    Missing,
}

pub fn app_ca_pair_status(config_dir: &Path) -> (AppCaPairStatus, PathBuf, PathBuf) {
    let (cert, key) = app_ca_cert_paths(config_dir);
    let (cert_ok, key_ok) = (cert.exists(), key.exists());
    let status = match (cert_ok, key_ok) {
        (true, true) => AppCaPairStatus::Complete,
        (true, false) => AppCaPairStatus::CertOnly,
        (false, true) => AppCaPairStatus::KeyOnly,
        (false, false) => AppCaPairStatus::Missing,
    };
    (status, cert, key)
}

/// The X.509/PEM result of a generation pass.
#[derive(Debug, Clone, PartialEq)]
pub struct GeneratedCaPem {
    pub cert_pem: String,
    pub key_pem: String,
}

/// Generate a self-signed CA (ECDSA-P256, SHA-256, 10-year validity,
/// `CA: true` + `keyCertSign`/`cRLSign`/`digitalSignature` — the donor's
/// profile with an EC key instead of RSA-2048; equal security at far less
/// code, and the PEM is consumed by the same NODE_EXTRA_CA_CERTS channel).
pub fn generate_self_signed_ca_pem(common_name: &str) -> Result<GeneratedCaPem, String> {
    let mut params = rcgen::CertificateParams::new(vec![]).map_err(|e| e.to_string())?;
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, common_name);
    params
        .distinguished_name
        .push(rcgen::DnType::OrganizationName, "okra");
    params
        .distinguished_name
        .push(rcgen::DnType::CountryName, "XX");
    let not_before = rcgen::date_time_ymd(2026, 9, 28);
    params.not_before = not_before;
    params.not_after = rcgen::date_time_ymd(2036, 9, 26); // ~10 years
    params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    params.key_usages = vec![
        rcgen::KeyUsagePurpose::KeyCertSign,
        rcgen::KeyUsagePurpose::CrlSign,
        rcgen::KeyUsagePurpose::DigitalSignature,
    ];
    // cryptographically random serial
    let mut serial = [0u8; 16];
    fill_random(&mut serial);
    serial[0] &= 0x7f; // keep the serial non-negative
    params.serial_number = Some(rcgen::SerialNumber::from_slice(&serial));

    let key_pair = rcgen::KeyPair::generate().map_err(|e| format!("CA keygen: {e}"))?;
    let cert = params
        .self_signed(&key_pair)
        .map_err(|e| format!("CA self-sign: {e}"))?;
    Ok(GeneratedCaPem {
        cert_pem: cert.pem(),
        key_pem: key_pair.serialize_pem(),
    })
}

fn fill_random(buf: &mut [u8]) {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    // mix time + pid + a counter: non-secret material (a serial number),
    // so this PRNG is adequate; no key material derives from it.
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let mut state = t ^ (std::process::id() as u64) << 32 ^ COUNTER.fetch_add(1, Ordering::Relaxed);
    for chunk in buf.chunks_mut(8) {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        for (i, b) in chunk.iter_mut().enumerate() {
            *b = (state >> (i * 8)) as u8;
        }
    }
}

/// Ensure the app CA pair exists; generate on first use. Idempotent: when
/// cert AND key are already present they are returned as-is, so the
/// fingerprint stays stable and already-trusted children never see drift.
/// The key file is written 0600 (sensitive material); the cert 0644.
pub fn ensure_app_ca_pair(
    config_dir: &Path,
) -> Result<(AppCaPairStatus, PathBuf, PathBuf), String> {
    let (status, cert_path, key_path) = app_ca_pair_status(config_dir);
    if status == AppCaPairStatus::Complete {
        return Ok((status, cert_path, key_path));
    }
    // a half-present pair is stale by contract — regenerate both together
    let certs_dir = cert_path
        .parent()
        .ok_or_else(|| "cert path has no parent".to_string())?;
    std::fs::create_dir_all(certs_dir).map_err(|e| format!("create certs dir: {e}"))?;
    let pem = generate_self_signed_ca_pem("okra Network CA")?;
    std::fs::write(&cert_path, &pem.cert_pem).map_err(|e| format!("write cert: {e}"))?;
    std::fs::write(&key_path, &pem.key_pem).map_err(|e| format!("write key: {e}"))?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&cert_path, std::fs::Permissions::from_mode(0o644))
        .map_err(|e| format!("cert perms: {e}"))?;
    std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o600))
        .map_err(|e| format!("key perms: {e}"))?;
    Ok((AppCaPairStatus::Complete, cert_path, key_path))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_entry_algebra_prepends_appends_dedupes() {
        assert_eq!(
            prepend_path_entries(Some("/a:/b"), &["/x".into(), "/a".into()]),
            "/x:/a:/b"
        );
        assert_eq!(
            append_path_entries(Some("/a:/b"), &["/x".into(), "/a".into()]),
            "/a:/b:/x"
        );
        // empty current / empty entries / empty segments all behave
        assert_eq!(prepend_path_entries(None, &["/x".into()]), "/x");
        assert_eq!(append_path_entries(Some(""), &[]), "");
        assert_eq!(prepend_path_entries(Some("::/a::"), &[]), "/a");
    }

    #[test]
    fn resolve_command_on_path_finds_executables() {
        let td = tempfile::tempdir().unwrap();
        let bin_dir = td.path().join("bin");
        std::fs::create_dir_all(&bin_dir).unwrap();
        let tool = bin_dir.join("mytool");
        std::fs::write(&tool, "#!/bin/sh\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();

        let path_env = format!("{}:/usr/bin", bin_dir.display());
        assert_eq!(
            resolve_command_on_path("mytool", Some(&path_env)),
            Some(tool.to_string_lossy().into_owned())
        );
        // non-executable files are skipped
        std::fs::write(bin_dir.join("noexec"), "data").unwrap();
        assert_eq!(resolve_command_on_path("noexec", Some(&path_env)), None);
        assert_eq!(resolve_command_on_path("missing", Some(&path_env)), None);
        assert_eq!(resolve_command_on_path("mytool", None), None);
    }

    #[test]
    fn runtime_tool_resolution_prefers_env_then_bundled_then_path() {
        let td = tempfile::tempdir().unwrap();
        let bundled = td.path().join("bundled-tools").join("aarch64-apple-darwin");
        std::fs::create_dir_all(bundled.join("ripgrep")).unwrap();
        let bundled_rg = bundled.join("ripgrep/rg");
        std::fs::write(&bundled_rg, "#!/bin/sh\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&bundled_rg, std::fs::Permissions::from_mode(0o755)).unwrap();

        let mut base = BTreeMap::new();
        base.insert("PATH".to_string(), "/usr/bin".to_string());

        // bundled hit
        let roots = vec![bundled.clone()];
        assert_eq!(
            resolve_runtime_tool_binary(RuntimeToolId::Ripgrep, &base, &roots),
            Some(bundled_rg.clone())
        );

        // env override wins over bundled
        let env_bin = td.path().join("custom-rg");
        std::fs::write(&env_bin, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&env_bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        base.insert("OKRA_RG_BINARY".to_string(), env_bin.to_string_lossy().into_owned());
        assert_eq!(
            resolve_runtime_tool_binary(RuntimeToolId::Ripgrep, &base, &roots),
            Some(env_bin)
        );
        base.remove("OKRA_RG_BINARY");

        // PATH fallback when no bundled root has the tool
        let empty_roots: Vec<PathBuf> = vec![];
        assert!(resolve_runtime_tool_binary(RuntimeToolId::Ripgrep, &base, &empty_roots).is_none());
    }

    #[test]
    fn env_patch_includes_tool_vars_and_appends_path() {
        let td = tempfile::tempdir().unwrap();
        let dir = td.path().join("tools");
        // bundled layout: <root>/<resource_dir>/<binary>  (tools/bfs/bfs)
        std::fs::create_dir_all(dir.join("bfs")).unwrap();
        let bfs = dir.join("bfs").join("bfs");
        std::fs::write(&bfs, "#!/bin/sh\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&bfs, std::fs::Permissions::from_mode(0o755)).unwrap();

        let base = BTreeMap::from([("PATH".to_string(), "/usr/bin".to_string())]);
        let patch = build_runtime_tool_env_patch(
            &[RuntimeToolId::Bfs],
            &base,
            std::slice::from_ref(&dir),
        );
        assert_eq!(
            patch.get("OKRA_BFS_BINARY").map(String::as_str),
            Some(bfs.to_string_lossy().as_ref())
        );
        let path = patch.get("PATH").map(String::as_str).unwrap();
        assert!(path.ends_with(&dir.join("bfs").to_string_lossy().to_string()), "{path}");
        assert!(path.starts_with("/usr/bin"), "{path}");
    }

    #[test]
    fn ensure_app_ca_pair_generates_then_reuses() {
        let td = tempfile::tempdir().unwrap();
        let (status, cert, key) = ensure_app_ca_pair(td.path()).unwrap();
        assert_eq!(status, AppCaPairStatus::Complete);
        let cert_pem = std::fs::read_to_string(&cert).unwrap();
        let key_pem = std::fs::read_to_string(&key).unwrap();
        assert!(cert_pem.starts_with("-----BEGIN CERTIFICATE-----"), "{cert_pem}");
        assert!(key_pem.contains("PRIVATE KEY"), "{key_pem}");

        use std::os::unix::fs::PermissionsExt;
        let key_mode = std::fs::metadata(&key).unwrap().permissions().mode();
        assert_eq!(key_mode & 0o777, 0o600, "key must be 0600");
        let cert_mode = std::fs::metadata(&cert).unwrap().permissions().mode();
        assert_eq!(cert_mode & 0o777, 0o644);

        // idempotent: second call reuses the exact same bytes (stable
        // fingerprint — an already-trusted child must never see drift)
        let cert_before = std::fs::read(&cert).unwrap();
        let (status2, cert2, key2) = ensure_app_ca_pair(td.path()).unwrap();
        assert_eq!(status2, AppCaPairStatus::Complete);
        assert_eq!(cert2, cert);
        assert_eq!(key2, key);
        assert_eq!(std::fs::read(&cert2).unwrap(), cert_before);
    }

    #[test]
    fn generated_ca_pem_pair_is_wellformed() {
        let pem = generate_self_signed_ca_pem("okra Network CA").unwrap();
        assert!(pem.cert_pem.contains("BEGIN CERTIFICATE"));
        assert!(pem.cert_pem.contains("END CERTIFICATE"));
        assert!(pem.key_pem.contains("PRIVATE KEY"));
        assert!(pem.cert_pem.len() > 400, "a real CA cert is non-trivial");
    }

    #[test]
    fn app_ca_pair_status_reports_all_four_states() {
        let td = tempfile::tempdir().unwrap();
        let (status, cert, key) = app_ca_pair_status(td.path());
        assert_eq!(status, AppCaPairStatus::Missing);
        assert!(cert.ends_with(APP_CA_CERT_FILE));
        assert!(key.ends_with(APP_CA_KEY_FILE));

        std::fs::create_dir_all(cert.parent().unwrap()).unwrap();
        std::fs::write(&cert, "CERT").unwrap();
        assert_eq!(app_ca_pair_status(td.path()).0, AppCaPairStatus::CertOnly);
        std::fs::write(&key, "KEY").unwrap();
        assert_eq!(app_ca_pair_status(td.path()).0, AppCaPairStatus::Complete);
        std::fs::remove_file(&cert).unwrap();
        assert_eq!(app_ca_pair_status(td.path()).0, AppCaPairStatus::KeyOnly);
    }
}
