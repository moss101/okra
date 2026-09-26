//! MCP sync (MASTER-PLAN §3 #48 host-domain strangler, from ZCode
//! `packages/services/src/mcp-sync/`): reads and writes the user/workspace
//! MCP server directories so surfaces, remote hosts, and CLI installs stay
//! in sync.
//!
//! Donor contracts kept:
//! - **two config sources with preferred-source fallback**: okra's own
//!   config first; if it defines no servers, the `agents` interop file is
//!   read (`collectEffectiveUserMcpRecords`);
//! - **legacy `enable` migration**: the old misspelled field folds into
//!   the contract `enabled` field on read; when the two conflict, DISABLED
//!   wins (a server the user switched off must never be resurrected by
//!   imported `enabled: true` residue). Enabled is the default state and
//!   is never persisted; the legacy field is stripped on rewrite;
//! - **strict reads**: a missing file is empty; a corrupt one is a hard
//!   error — import must never treat a broken config as blank and
//!   silently overwrite provider/secret settings;
//! - **skip-existing import** (overwrite is not supported): a server with
//!   the same name (exact, then case-insensitive) is skipped, not merged;
//! - **filesystem path rewriting** for stdio filesystem servers on
//!   cross-machine import, with remote-style path joining (never use the
//!   local platform to decide separators);
//! - **atomic 0o600 writes**: configs persist env/header/token secrets.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::plugins::store::sha256_hex;

/// Which config file a record came from / should go to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum McpSyncSource {
    Okra,
    Agents,
}

impl McpSyncSource {
    pub fn as_str(&self) -> &'static str {
        match self {
            McpSyncSource::Okra => "okra",
            McpSyncSource::Agents => "agents",
        }
    }
}

/// One config-file descriptor (the donor's `DirectoryMcpDescriptor`):
/// where the file lives in user/workspace scope and which JSON key holds
/// the server map.
#[derive(Debug, Clone)]
pub struct McpSyncDescriptor {
    pub source: McpSyncSource,
    pub user_dir_segments: &'static [&'static str],
    pub workspace_dir_segments: &'static [&'static str],
    pub file_name: &'static str,
    /// `"mcp.servers"` (nested) or `"mcpServers"` (flat).
    pub config_key: &'static str,
}

pub const OKRA_DESCRIPTOR: McpSyncDescriptor = McpSyncDescriptor {
    source: McpSyncSource::Okra,
    user_dir_segments: &[".okra"],
    workspace_dir_segments: &[".okra"],
    file_name: "config.json",
    config_key: "mcp.servers",
};

pub const AGENTS_DESCRIPTOR: McpSyncDescriptor = McpSyncDescriptor {
    source: McpSyncSource::Agents,
    user_dir_segments: &[".agents"],
    workspace_dir_segments: &[".agents"],
    file_name: "mcp.json",
    config_key: "mcpServers",
};

const ENABLED_KEY: &str = "enabled";
const LEGACY_ENABLE_KEY: &str = "enable";
/// Donor `SECRET_CONFIG_FILE_MODE`: temp files must not leak secrets via umask.
#[cfg(unix)]
const SECRET_CONFIG_FILE_MODE: u32 = 0o600;

/// One server read from a config file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpServerRecord {
    pub name: String,
    pub config: Value,
    pub enabled: bool,
    pub source: McpSyncSource,
    pub file_path: PathBuf,
    /// Set when read from workspace scope.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_path: Option<PathBuf>,
}

/// A user-level server offered as a sync candidate (the export surface).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpSyncCandidate {
    /// sha256 over `{source}:{path}:{name}` — stable across restarts.
    pub id: String,
    pub name: String,
    pub config: Value,
    pub enabled: bool,
    pub source: McpSyncSource,
    pub path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportedServer {
    pub id: String,
    pub name: String,
    pub config: Value,
    pub enabled: bool,
    pub source: McpSyncSource,
    pub path: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImportStatus {
    Synced,
    Skipped,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportOutcome {
    pub name: String,
    pub status: ImportStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Cross-machine path rewrite for stdio filesystem servers: args under the
/// local home/workspace are re-based onto the remote equivalents.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PathRewrite {
    pub local_home: PathBuf,
    pub local_workspace: Option<PathBuf>,
    pub remote_home: PathBuf,
    pub remote_workspace: Option<PathBuf>,
}

#[derive(Debug, thiserror::Error)]
pub enum McpSyncError {
    #[error("mcp sync candidate not found: {0}")]
    UnknownCandidate(String),
    #[error("cannot read MCP config file {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("cannot parse MCP config file {path}: {message}")]
    Parse { path: PathBuf, message: String },
    #[error("MCP config file {path} must be a JSON object")]
    NotAnObject { path: PathBuf },
    #[error("mcp sync io: {0}")]
    Io(#[from] std::io::Error),
    #[error("mcp sync codec: {0}")]
    Codec(#[from] serde_json::Error),
}

/// The sync service rooted at a home directory (injectable for tests;
/// production passes the real home).
#[derive(Debug, Clone)]
pub struct McpSyncService {
    home: PathBuf,
}

impl McpSyncService {
    pub fn new(home: impl Into<PathBuf>) -> Self {
        McpSyncService { home: home.into() }
    }

    // ---------- path building ----------

    fn config_path(
        &self,
        d: &McpSyncDescriptor,
        workspace: Option<&Path>,
    ) -> PathBuf {
        match workspace {
            Some(ws) => {
                let mut path = ws.to_path_buf();
                for segment in d.workspace_dir_segments {
                    path = path.join(segment);
                }
                path.join(d.file_name)
            }
            None => {
                let mut path = self.home.clone();
                for segment in d.user_dir_segments {
                    path = path.join(segment);
                }
                path.join(d.file_name)
            }
        }
    }

    /// The import/upsert target: okra's own user config.
    fn target_path(&self) -> PathBuf {
        self.config_path(&OKRA_DESCRIPTOR, None)
    }

    // ---------- JSON map plumbing ----------

    /// Strict read: missing → `Ok(None)`; corrupt → hard error (a broken
    /// config must never read as blank and get overwritten by a later write).
    fn read_json_object(&self, path: &Path) -> Result<Option<Map<String, Value>>, McpSyncError> {
        let raw = match std::fs::read_to_string(path) {
            Ok(raw) => raw,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => {
                return Err(McpSyncError::Read {
                    path: path.to_path_buf(),
                    source: e,
                })
            }
        };
        let parsed: Value = serde_json::from_str(&raw).map_err(|e| McpSyncError::Parse {
            path: path.to_path_buf(),
            message: e.to_string(),
        })?;
        match parsed {
            Value::Object(map) => Ok(Some(map)),
            _ => Err(McpSyncError::NotAnObject {
                path: path.to_path_buf(),
            }),
        }
    }

    fn server_map_from(config: &Map<String, Value>, key: &str) -> Map<String, Value> {
        let empty = Map::new();
        if key == "mcp.servers" {
            return config
                .get("mcp")
                .and_then(Value::as_object)
                .and_then(|m| m.get("servers"))
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or(empty);
        }
        config
            .get(key)
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or(empty)
    }

    fn server_map_into(mut config: Map<String, Value>, key: &str, servers: Map<String, Value>) -> Map<String, Value> {
        if key == "mcp.servers" {
            let mcp = config
                .entry("mcp")
                .or_insert_with(|| Value::Object(Map::new()));
            if !mcp.is_object() {
                *mcp = Value::Object(Map::new());
            }
            if let Some(mcp) = mcp.as_object_mut() {
                mcp.insert("servers".into(), Value::Object(servers));
            }
            return config;
        }
        config.insert(key.to_string(), Value::Object(servers));
        config
    }

    /// `readServerEnabled`: absence means enabled; only `enabled: false`
    /// disables.
    fn server_enabled(config: &Value) -> bool {
        config.get(ENABLED_KEY) != Some(&Value::Bool(false))
    }

    /// `setServerEnabled`: enabled is the default and leaves no residue;
    /// the legacy `enable` field is stripped so contradictory configs
    /// (`enable:false, enabled:true`) can never be written again.
    fn set_server_enabled(config: &mut Value, enabled: bool) {
        if let Some(obj) = config.as_object_mut() {
            obj.remove(ENABLED_KEY);
            obj.remove(LEGACY_ENABLE_KEY);
            if !enabled {
                obj.insert(ENABLED_KEY.into(), Value::Bool(false));
            }
        }
    }

    /// `migrateLegacyEnableFlag`: fold `enable` into `enabled`; on conflict
    /// the disabled state wins.
    fn migrate_legacy(server_map: &mut Map<String, Value>) -> bool {
        let mut changed = false;
        for (_name, config) in server_map.iter_mut() {
            let Some(obj) = config.as_object() else {
                continue;
            };
            let has_legacy = obj.contains_key(LEGACY_ENABLE_KEY);
            if !has_legacy {
                continue;
            }
            let disabled = obj.get(LEGACY_ENABLE_KEY) == Some(&Value::Bool(false))
                || obj.get(ENABLED_KEY) == Some(&Value::Bool(false));
            Self::set_server_enabled(config, !disabled);
            changed = true;
        }
        changed
    }

    /// Read a file's server map, migrating legacy `enable` flags in place
    /// (best-effort write-back: a failed migration write never blocks the
    /// in-memory result, which is already the correct reading).
    fn read_server_map(
        &self,
        path: &Path,
        key: &str,
    ) -> Result<Map<String, Value>, McpSyncError> {
        let Some(config) = self.read_json_object(path)? else {
            return Ok(Map::new());
        };
        let mut servers = Self::server_map_from(&config, key);
        if Self::migrate_legacy(&mut servers) {
            let next = Self::server_map_into(config, key, servers.clone());
            let text = serde_json::to_string_pretty(&Value::Object(next))
                .map(|mut t| {
                    t.push('\n');
                    t
                })
                .unwrap_or_default();
            let _ = atomic_write_0600(path, text.as_bytes());
        }
        Ok(servers)
    }

    /// One descriptor's records at a scope, sorted by name.
    fn read_records(
        &self,
        d: &McpSyncDescriptor,
        workspace: Option<&Path>,
    ) -> Result<Vec<McpServerRecord>, McpSyncError> {
        let path = self.config_path(d, workspace);
        let map = self.read_server_map(&path, d.config_key)?;
        let mut pairs: Vec<(String, Value)> = map.into_iter().collect();
        pairs.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(pairs
            .into_iter()
            .map(|(name, config)| McpServerRecord {
                enabled: Self::server_enabled(&config),
                name,
                config,
                source: d.source,
                file_path: path.clone(),
                workspace_path: workspace.map(Path::to_path_buf),
            })
            .collect())
    }

    /// Preferred-source read (the donor's fallback rule): okra's config
    /// first; the agents interop file only when okra's has no servers.
    fn read_preferred(
        &self,
        workspace: Option<&Path>,
    ) -> Result<Vec<McpServerRecord>, McpSyncError> {
        let okra = self.read_records(&OKRA_DESCRIPTOR, workspace)?;
        if !okra.is_empty() {
            return Ok(okra);
        }
        self.read_records(&AGENTS_DESCRIPTOR, workspace)
    }

    // ---------- public API ----------

    /// `loadMcpFromUserDirectory`: workspace servers first, then user.
    pub fn load(&self, workspace: Option<&Path>) -> Result<Vec<McpServerRecord>, McpSyncError> {
        let mut servers = Vec::new();
        if let Some(ws) = workspace {
            servers.extend(self.read_preferred(Some(ws))?);
        }
        servers.extend(self.read_preferred(None)?);
        Ok(servers)
    }

    /// `listLocalUserMcpCandidates`: user-level records across sources,
    /// sorted by name, each with a stable sha256 id.
    pub fn candidates(&self) -> Result<Vec<McpSyncCandidate>, McpSyncError> {
        let mut candidates: Vec<McpSyncCandidate> = self
            .read_preferred(None)?
            .into_iter()
            .map(|r| McpSyncCandidate {
                id: sha256_hex(
                    format!("{}:{}:{}", r.source.as_str(), r.file_path.display(), r.name)
                        .as_bytes(),
                ),
                name: r.name,
                config: r.config,
                enabled: r.enabled,
                source: r.source,
                path: r.file_path,
            })
            .collect();
        candidates.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(candidates)
    }

    /// `exportMcpServers` by candidate id; unknown ids are hard errors.
    pub fn export(&self, server_ids: &[String]) -> Result<Vec<ExportedServer>, McpSyncError> {
        let candidates = self.candidates()?;
        server_ids
            .iter()
            .map(|id| {
                candidates
                    .iter()
                    .find(|c| &c.id == id)
                    .map(|c| ExportedServer {
                        id: c.id.clone(),
                        name: c.name.clone(),
                        config: c.config.clone(),
                        enabled: c.enabled,
                        source: c.source,
                        path: c.path.clone(),
                    })
                    .ok_or_else(|| McpSyncError::UnknownCandidate(id.clone()))
            })
            .collect()
    }

    /// `saveMcpToUserDirectory` upsert action: write one server into
    /// okra's config (user or workspace scope). The config is stored
    /// verbatim — enabled-state edits go through `set_enabled`.
    pub fn upsert(
        &self,
        name: &str,
        config: Value,
        workspace: Option<&Path>,
    ) -> Result<(), McpSyncError> {
        let path = self.config_path(&OKRA_DESCRIPTOR, workspace);
        let cfg = self
            .read_json_object(&path)?
            .unwrap_or_default();
        let mut servers = Self::server_map_from(&cfg, OKRA_DESCRIPTOR.config_key);
        servers.insert(name.to_string(), config);
        let next = Self::server_map_into(cfg, OKRA_DESCRIPTOR.config_key, servers);
        self.write_config(&path, &next)
    }

    /// `saveMcpToUserDirectory` remove action.
    pub fn delete(&self, name: &str, workspace: Option<&Path>) -> Result<(), McpSyncError> {
        let path = self.config_path(&OKRA_DESCRIPTOR, workspace);
        let Some(cfg) = self.read_json_object(&path)? else {
            return Ok(());
        };
        let mut servers = Self::server_map_from(&cfg, OKRA_DESCRIPTOR.config_key);
        if servers.remove(name).is_none() {
            return Ok(());
        }
        let next = Self::server_map_into(cfg, OKRA_DESCRIPTOR.config_key, servers);
        self.write_config(&path, &next)
    }

    /// `set-enabled` action: the disabled state is stored INSIDE the
    /// server's config object (never as a path-keyed overlay that could
    /// collide with real MCP config); enabling strips all residue.
    /// `set-enabled` action: the disabled state is stored INSIDE the
    /// server's config object (never as a path-keyed overlay that could
    /// collide with real MCP config); enabling strips all residue. The
    /// descriptor resolves across sources — the edit lands in the file
    /// where the server actually lives (donor `findDescriptorByLocation`),
    /// preferring okra's config over the agents interop file.
    pub fn set_enabled(
        &self,
        name: &str,
        enabled: bool,
        workspace: Option<&Path>,
    ) -> Result<(), McpSyncError> {
        for d in [&OKRA_DESCRIPTOR, &AGENTS_DESCRIPTOR] {
            let path = self.config_path(d, workspace);
            let Some(cfg) = self.read_json_object(&path)? else {
                continue;
            };
            let mut servers = Self::server_map_from(&cfg, d.config_key);
            let Some(mut server) = servers.get(name).cloned() else {
                continue;
            };
            Self::set_server_enabled(&mut server, enabled);
            servers.insert(name.to_string(), server);
            let next = Self::server_map_into(cfg, d.config_key, servers);
            self.write_config(&path, &next)?;
            return Ok(());
        }
        Ok(())
    }

    /// `importMcpServers` — skip-existing, never overwrite. Imported
    /// stdio filesystem servers get their path arguments re-based onto
    /// `rewrite` when cross-machine mapping is supplied.
    pub fn import(
        &self,
        servers: &[ExportedServer],
        rewrite: Option<&PathRewrite>,
    ) -> Result<Vec<ImportOutcome>, McpSyncError> {
        let path = self.target_path();
        let cfg = self.read_json_object(&path)?.unwrap_or_default();
        let mut target_map = Self::server_map_from(&cfg, OKRA_DESCRIPTOR.config_key);
        let existing: std::collections::BTreeSet<String> = self
            .candidates()?
            .into_iter()
            .map(|c| c.name.trim().to_ascii_lowercase())
            .collect();

        let mut outcomes = Vec::new();
        let mut changed = false;
        for server in servers {
            if target_map.contains_key(&server.name) {
                outcomes.push(ImportOutcome {
                    name: server.name.clone(),
                    status: ImportStatus::Skipped,
                    error: None,
                });
                continue;
            }
            let name_key = server.name.trim().to_ascii_lowercase();
            if existing.contains(&name_key) {
                outcomes.push(ImportOutcome {
                    name: server.name.clone(),
                    status: ImportStatus::Skipped,
                    error: None,
                });
                continue;
            }
            let mut config = server.config.clone();
            Self::set_server_enabled(&mut config, server.enabled);
            if let Some(rw) = rewrite {
                config = rewrite_filesystem_config(&server.name, config, rw);
            }
            target_map.insert(server.name.clone(), config);
            outcomes.push(ImportOutcome {
                name: server.name.clone(),
                status: ImportStatus::Synced,
                error: None,
            });
            changed = true;
        }

        if changed {
            let next = Self::server_map_into(cfg, OKRA_DESCRIPTOR.config_key, target_map);
            self.write_config(&path, &next)?;
        }
        Ok(outcomes)
    }

    fn write_config(&self, path: &Path, config: &Map<String, Value>) -> Result<(), McpSyncError> {
        let mut text = serde_json::to_string_pretty(&Value::Object(config.clone()))?;
        text.push('\n');
        atomic_write_0600(path, text.as_bytes())?;
        Ok(())
    }
}

/// Atomic write with donor `SECRET_CONFIG_FILE_MODE`: stage a 0o600 temp
/// file in the target directory, then rename over the target.
fn atomic_write_0600(path: &Path, content: &[u8]) -> std::io::Result<()> {
    use std::io::Write as _;
    let dir = path
        .parent()
        .ok_or_else(|| std::io::Error::other("config path has no parent"))?;
    std::fs::create_dir_all(dir)?;
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("config");
    let tmp = dir.join(format!(
        "{file_name}.okra-mcp-{}.{}.tmp",
        std::process::id(),
        nanos
    ));
    {
        let mut file = std::fs::File::create(&tmp)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(std::fs::Permissions::from_mode(SECRET_CONFIG_FILE_MODE))?;
        }
        file.write_all(content)?;
    }
    match std::fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

/// Donor `isFilesystemMcpServer`: the known filesystem servers, by name
/// or by well-known package names in command/args.
fn is_filesystem_server(name: &str, config: &Value) -> bool {
    let normalized = name.trim().to_ascii_lowercase();
    if matches!(normalized.as_str(), "filesystem" | "file-system" | "fs") {
        return true;
    }
    let Some(obj) = config.as_object() else {
        return false;
    };
    let mut haystack = String::new();
    if let Some(cmd) = obj.get("command").and_then(Value::as_str) {
        haystack.push_str(cmd);
        haystack.push(' ');
    }
    if let Some(args) = obj.get("args").and_then(Value::as_array) {
        for arg in args {
            if let Some(s) = arg.as_str() {
                haystack.push_str(s);
                haystack.push(' ');
            }
        }
    }
    let hay = haystack.to_ascii_lowercase();
    hay.contains("@modelcontextprotocol/server-filesystem")
        || hay.contains("mcp-server-filesystem")
}

fn is_stdio_config(config: &Value) -> bool {
    let Some(obj) = config.as_object() else {
        return false;
    };
    let ty = obj
        .get("type")
        .and_then(Value::as_str)
        .map(|t| t.trim().to_ascii_lowercase())
        .unwrap_or_default();
    if ty.is_empty() {
        return obj
            .get("command")
            .and_then(Value::as_str)
            .is_some_and(|c| !c.trim().is_empty());
    }
    ty == "stdio"
}

/// Rewrite args of stdio filesystem servers only; everything else passes
/// through untouched.
fn rewrite_filesystem_config(name: &str, config: Value, rw: &PathRewrite) -> Value {
    if !is_stdio_config(&config) || !is_filesystem_server(name, &config) {
        return config;
    }
    let Some(obj) = config.as_object() else {
        return config;
    };
    let Some(args) = obj.get("args").and_then(Value::as_array).cloned() else {
        return config;
    };
    let mut next = config;
    if let Some(obj) = next.as_object_mut() {
        obj.insert(
            "args".into(),
            Value::Array(
                args.iter()
                    .map(|arg| match arg.as_str() {
                        Some(s) => Value::String(rewrite_path_arg(s, rw)),
                        None => arg.clone(),
                    })
                    .collect(),
            ),
        );
    }
    next
}

/// Donor `rewritePathArgForRemote`: workspace-relative wins over
/// home-relative; untouched args pass through.
fn rewrite_path_arg(arg: &str, rw: &PathRewrite) -> String {
    if let Some(rel) = relative_if_within(rw.local_workspace.as_deref(), arg)
        && let Some(remote_ws) = rw.remote_workspace.as_deref()
        && !remote_ws.as_os_str().is_empty()
    {
        return join_remote(remote_ws, &rel);
    }
    if let Some(rel) = relative_if_within(Some(rw.local_home.as_path()), arg) {
        return join_remote(rw.remote_home.as_path(), &rel);
    }
    arg.to_string()
}

/// Donor `getRelativePathIfWithin`, POSIX-normalized (okra's supported
/// surfaces are POSIX; Windows drive mapping returns None like the donor's
/// unpathable inputs).
fn relative_if_within(base: Option<&Path>, candidate: &str) -> Option<String> {
    let base = base?;
    if base.as_os_str().is_empty() || candidate.trim().is_empty() {
        return None;
    }
    let base_str = base.to_str()?.replace('\\', "/");
    if !base_str.starts_with('/') {
        return None;
    }
    let cand = candidate.trim().replace('\\', "/");
    if !cand.starts_with('/') {
        return None;
    }
    let base_norm = base_str.trim_end_matches('/');
    if cand == base_norm {
        return Some(String::new());
    }
    let prefix = format!("{base_norm}/");
    cand.strip_prefix(&prefix).map(str::to_string)
}

/// Donor `joinRemotePath`: join in the REMOTE's style — decided by the
/// remote base's shape, never by the local platform.
fn join_remote(remote_base: &Path, relative: &str) -> String {
    let base = remote_base.to_string_lossy().replace('\\', "/");
    if relative.is_empty() {
        return base;
    }
    let segments: Vec<&str> = relative
        .split(['\\', '/'])
        .filter(|s| !s.is_empty())
        .collect();
    let mut joined = base.trim_end_matches('/').to_string();
    for segment in segments {
        joined.push('/');
        joined.push_str(segment);
    }
    joined
}

#[cfg(test)]
mod tests {
    use super::*;

    fn service() -> (McpSyncService, tempfile::TempDir) {
        let td = tempfile::tempdir().unwrap();
        (McpSyncService::new(td.path()), td)
    }

    fn write_config(path: &Path, json: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, json).unwrap();
    }

    #[test]
    fn reads_okra_then_falls_back_to_agents() {
        let (svc, td) = service();
        // nothing anywhere
        assert!(svc.load(None).unwrap().is_empty());
        // agents file alone provides records
        write_config(
            &td.path().join(".agents/mcp.json"),
            r#"{ "mcpServers": { "alpha": { "command": "a" } } }"#,
        );
        let records = svc.load(None).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].source, McpSyncSource::Agents);
        assert!(records[0].enabled, "absence means enabled");
        // once okra's config has servers, it wins outright
        write_config(
            &td.path().join(".okra/config.json"),
            r#"{ "mcp": { "servers": { "beta": { "command": "b" } } } }"#,
        );
        let records = svc.load(None).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].name, "beta");
        assert_eq!(records[0].source, McpSyncSource::Okra);
    }

    #[test]
    fn legacy_enable_migrates_with_disabled_wins() {
        let (svc, td) = service();
        write_config(
            &td.path().join(".okra/config.json"),
            r#"{ "mcp": { "servers": {
                "old-off": { "command": "a", "enable": false },
                "old-on": { "command": "b", "enable": true },
                "conflict": { "command": "c", "enable": false, "enabled": true },
                "plain": { "command": "d" }
            } } }"#,
        );
        let records = svc.load(None).unwrap();
        let enabled: Vec<(&str, bool)> = records
            .iter()
            .map(|r| (r.name.as_str(), r.enabled))
            .collect();
        assert_eq!(
            enabled,
            vec![
                ("conflict", false),
                ("old-off", false),
                ("old-on", true),
                ("plain", true)
            ]
        );
        // migration folded the field on disk; a reread is stable and clean
        let raw = std::fs::read_to_string(td.path().join(".okra/config.json")).unwrap();
        assert!(!raw.contains("\"enable\""), "{raw}");
        assert!(raw.contains("\"enabled\": false"), "{raw}");
    }

    #[test]
    fn corrupt_config_is_a_hard_error_never_blank() {
        let (svc, td) = service();
        write_config(&td.path().join(".okra/config.json"), "{ not json");
        assert!(matches!(
            svc.load(None),
            Err(McpSyncError::Parse { .. })
        ));
        assert!(matches!(
            svc.import(&[], None),
            Err(McpSyncError::Parse { .. })
        ));
        // array, not object
        write_config(&td.path().join(".okra/config.json"), "[]");
        assert!(matches!(svc.load(None), Err(McpSyncError::NotAnObject { .. })));
    }

    #[test]
    fn upsert_set_enabled_delete_round_trip() {
        let (svc, td) = service();
        svc.upsert("fs1", serde_json::json!({ "command": "npx", "args": ["fs"] }), None)
            .unwrap();
        svc.upsert(
            "fs2",
            serde_json::json!({ "command": "npx" }),
            None,
        )
        .unwrap();
        svc.set_enabled("fs1", false, None).unwrap();
        let records = svc.load(None).unwrap();
        assert_eq!(records.len(), 2);
        let fs1 = records.iter().find(|r| r.name == "fs1").unwrap();
        assert!(!fs1.enabled);
        assert_eq!(
            fs1.config.get("enabled"),
            Some(&Value::Bool(false)),
            "disabled persists inside the server object"
        );
        // enabling strips residue entirely
        svc.set_enabled("fs1", true, None).unwrap();
        let records = svc.load(None).unwrap();
        let fs1 = records.iter().find(|r| r.name == "fs1").unwrap();
        assert!(fs1.enabled);
        assert!(fs1.config.get("enabled").is_none());
        assert!(fs1.config.get("enable").is_none());
        svc.delete("fs2", None).unwrap();
        assert_eq!(svc.load(None).unwrap().len(), 1);
        // deleting an unknown server is a no-op
        svc.delete("ghost", None).unwrap();
        let _ = td;
    }

    #[test]
    fn workspace_scope_is_separate_from_user() {
        let (svc, td) = service();
        let ws = td.path().join("project");
        std::fs::create_dir_all(ws.join(".okra")).unwrap();
        svc.upsert("ws-only", serde_json::json!({ "command": "x" }), Some(&ws))
            .unwrap();
        assert_eq!(svc.load(Some(&ws)).unwrap().len(), 1);
        assert!(svc.load(None).unwrap().is_empty());
        assert_eq!(
            svc.load(Some(&ws)).unwrap()[0].workspace_path.as_deref(),
            Some(ws.as_path())
        );
    }

    #[test]
    fn candidates_export_and_import_skip_existing() {
        let (svc, td) = service();
        write_config(
            &td.path().join(".okra/config.json"),
            r#"{ "mcp": { "servers": { "Alpha": { "command": "a" } } } }"#,
        );
        let candidates = svc.candidates().unwrap();
        assert_eq!(candidates.len(), 1);
        let exported = svc.export(&[candidates[0].id.clone()]).unwrap();
        assert_eq!(exported[0].name, "Alpha");
        assert!(matches!(
            svc.export(&["nope".into()]),
            Err(McpSyncError::UnknownCandidate(_))
        ));

        // import into a fresh home: same name (case-insensitive) skips
        let other_home = tempfile::tempdir().unwrap();
        let remote = McpSyncService::new(other_home.path());
        // the remote also has an agents-file "alpha" from another source
        write_config(
            &other_home.path().join(".agents/mcp.json"),
            r#"{ "mcpServers": { "alpha": { "command": "other" } } }"#,
        );
        let outcomes = remote.import(&exported, None).unwrap();
        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0].status, ImportStatus::Skipped);
        assert_eq!(remote.load(None).unwrap()[0].config["command"], "other");

        // a genuinely new server syncs
        let mut fresh = exported[0].clone();
        fresh.name = "brand-new".into();
        let outcomes = remote.import(std::slice::from_ref(&fresh), None).unwrap();
        assert_eq!(outcomes[0].status, ImportStatus::Synced);
        // preferred-source rule: okra's config now has a server, so it
        // shadows the agents file in effective reads (donor behavior)
        let loaded = remote.load(None).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].name, "brand-new");
        assert_eq!(remote.candidates().unwrap().len(), 1);
    }

    #[test]
    fn filesystem_path_rewrite_rebases_onto_remote() {
        let (svc, td) = service();
        let local_home = td.path().join("local");
        let local_ws = local_home.join("work");
        write_config(
            &local_ws.join(".okra/config.json"),
            r#"{}"#,
        );
        let rw = PathRewrite {
            local_home: local_home.clone(),
            local_workspace: Some(local_ws.clone()),
            remote_home: PathBuf::from("/srv/users/ada"),
            remote_workspace: Some(PathBuf::from("/srv/work/proj")),
        };
        let servers = vec![ExportedServer {
            id: "x".into(),
            name: "filesystem".into(),
            config: serde_json::json!({
                "type": "stdio",
                "command": "npx",
                "args": ["-y", "@modelcontextprotocol/server-filesystem", format!("{}/notes", local_ws.display()), format!("{}/docs", local_home.display()), "--untouched"]
            }),
            enabled: true,
            source: McpSyncSource::Okra,
            path: PathBuf::from("/nowhere"),
        }];
        svc.import(&servers, Some(&rw)).unwrap();
        let imported = &svc.load(None).unwrap()[0];
        let args = imported.config["args"].as_array().unwrap();
        assert_eq!(args[2], "/srv/work/proj/notes", "workspace rebase");
        assert_eq!(args[3], "/srv/users/ada/docs", "home rebase");
        assert_eq!(args[4], "--untouched", "flags pass through");

        // non-filesystem stdio servers are never rewritten
        let servers = vec![ExportedServer {
            id: "y".into(),
            name: "grep".into(),
            config: serde_json::json!({
                "type": "stdio",
                "command": "rg",
                "args": [format!("{}", local_home.display())]
            }),
            enabled: true,
            source: McpSyncSource::Okra,
            path: PathBuf::from("/nowhere"),
        }];
        svc.import(&servers, Some(&rw)).unwrap();
        let grep = &svc.load(None).unwrap()[1];
        assert_eq!(
            grep.config["args"][0],
            format!("{}", local_home.display()),
            "non-filesystem args untouched"
        );
    }

    #[test]
    fn writes_are_atomic_0600_and_pretty() {
        let (svc, td) = service();
        svc.upsert("s", serde_json::json!({ "command": "c", "env": { "TOKEN": "x" } }), None)
            .unwrap();
        let path = td.path().join(".okra/config.json");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "secrets-bearing config is user-only");
        }
        let raw = std::fs::read_to_string(&path).unwrap();
        assert!(raw.ends_with('\n'));
        assert!(raw.contains("TOKEN"), "{raw}");
        // no temp files left behind
        let dir: Vec<_> = std::fs::read_dir(td.path().join(".okra")).unwrap().collect();
        assert_eq!(dir.len(), 1, "{dir:?}");
    }
}
