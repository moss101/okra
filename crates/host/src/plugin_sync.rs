//! Plugin sync (MASTER-PLAN §3 #48 / row #46 remainder, from ZCode
//! `packages/services/src/plugin-sync/`): the user's configured plugins —
//! registered as directories in the okra config's `plugins.dirs` — are
//! scanned into sync candidates, exported as a gzip'd ustar archive with
//! a metadata entry, and imported on another machine with the same
//! skip-existing state machine as skills.
//!
//! Donor contracts kept:
//! - **identity is `name@inline`**; the candidate id is a stable hash of
//!   plugin id + canonical path;
//! - **enabled overrides** live in the okra config's
//!   `plugins.enabledPlugins` map (id-keyed, default enabled);
//! - **component types** are declared by manifest keys or detected from
//!   conventional directories (`skills/`, `commands/`,
//!   `hooks/hooks.json`, `.mcp.json`);
//! - **remote status reasons, in order**: `targetExists` (the directory
//!   is already at the target root) then `samePluginId` (a plugin with
//!   the same normalized id is already configured);
//! - **archives** carry a metadata entry (`.okra-plugin-sync.json`,
//!   donor: `.zcode-plugin-sync.json`) listing name/pluginId/
//!   directoryName/enabled per plugin; import skips existing targets and
//!   configured ids, bounds bytes, and contains paths.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

use crate::plugins::store::sha256_hex;
use crate::settings_sync::{read_json_file_or_empty, write_json_file, SettingsSyncService};
use crate::skill_sync::{append_tar_entry, extract_archive, gzip_tar, SkillSyncError};

pub const INLINE_PLUGIN_MARKETPLACE: &str = "inline";
pub const DEFAULT_MAX_ARCHIVE_BYTES: usize = 50 * 1024 * 1024;
/// The archive's metadata entry (donor: `.zcode-plugin-sync.json`).
pub const METADATA_ARCHIVE_PATH: &str = ".okra-plugin-sync.json";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ComponentType {
    Skills,
    Commands,
    Hooks,
    Mcp,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginSyncCandidate {
    /// sha256 over `{pluginId}:{canonical path}` — stable across restarts.
    pub id: String,
    pub name: String,
    pub plugin_id: String,
    pub directory_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    pub path: PathBuf,
    pub size_bytes: u64,
    pub enabled: bool,
    /// Set only when the config carries an explicit override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled_override: Option<bool>,
    pub component_types: Vec<ComponentType>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncStatus {
    Synced,
    Skipped,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoteSkipReason {
    TargetExists,
    SamePluginId,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RemoteStatus {
    pub plugin_id: String,
    pub directory_name: String,
    pub exists: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<RemoteSkipReason>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportOutcome {
    pub name: String,
    pub plugin_id: String,
    pub directory_name: String,
    pub status: SyncStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum PluginSyncError {
    #[error("plugin sync candidate not found: {0}")]
    UnknownCandidate(String),
    #[error("plugin sync size limit exceeded: phase={phase}, actual={actual}, max={max}")]
    SizeLimit {
        phase: &'static str,
        actual: u64,
        max: u64,
    },
    #[error("invalid plugin sync archive: {0}")]
    BadArchive(String),
    #[error("plugin sync io: {0}")]
    Io(#[from] std::io::Error),
    #[error("plugin sync codec: {0}")]
    Codec(#[from] serde_json::Error),
}

/// The plugin id for an inline marketplace plugin: `name@inline`.
fn plugin_id(name: &str) -> String {
    format!("{name}@{INLINE_PLUGIN_MARKETPLACE}")
}

fn plugin_id_key(plugin_id: &str) -> String {
    plugin_id.trim().to_ascii_lowercase()
}

/// The plugin-sync service over one home directory (injectable).
#[derive(Debug, Clone)]
pub struct PluginSyncService {
    home: PathBuf,
    max_archive_bytes: usize,
}

/// One plugin directory's manifest info: (name, description, version,
/// component types).
type ManifestInfo = (String, Option<String>, Option<String>, Vec<ComponentType>);

impl PluginSyncService {
    pub fn new(home: impl Into<PathBuf>) -> Self {
        PluginSyncService {
            home: home.into(),
            max_archive_bytes: DEFAULT_MAX_ARCHIVE_BYTES,
        }
    }

    pub fn with_max_archive_bytes(mut self, max: usize) -> Self {
        self.max_archive_bytes = max;
        self
    }

    fn config_path(&self) -> PathBuf {
        self.home.join(".okra").join("config.json")
    }

    fn plugin_root(&self) -> PathBuf {
        self.home.join(".okra").join("plugins")
    }

    fn read_config(&self) -> Map<String, Value> {
        read_json_file_or_empty(&self.config_path())
    }

    /// `readUserPluginConfigState`: `plugins.dirs` + `plugins.enabledPlugins`.
    fn read_config_state(&self) -> (Vec<String>, BTreeMap<String, bool>) {
        let config = self.read_config();
        let dirs: Vec<String> = config
            .get("plugins")
            .and_then(|p| p.get("dirs"))
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        let mut overrides = BTreeMap::new();
        if let Some(enabled) = config
            .get("plugins")
            .and_then(|p| p.get("enabledPlugins"))
            .and_then(Value::as_object)
        {
            for (id, value) in enabled {
                if let Some(enabled) = value.as_bool() {
                    overrides.insert(plugin_id_key(id), enabled);
                }
            }
        }
        (dirs, overrides)
    }

    /// Manifest info for one plugin directory (name required; id =
    /// `name@inline`); component types declared by keys or detected from
    /// conventional directories.
    fn read_manifest_info(plugin_root: &Path) -> Option<ManifestInfo> {
        let manifest_path = SettingsSyncService::find_plugin_manifest(plugin_root)?;
        let parsed: Value =
            serde_json::from_str(&std::fs::read_to_string(manifest_path).ok()?).ok()?;
        let name = parsed.get("name")?.as_str()?.trim().to_string();
        if name.is_empty() {
            return None;
        }
        let description = parsed
            .get("description")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        let version = parsed
            .get("version")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string);
        let mut components = Vec::new();
        let has = |key: &str| parsed.get(key).is_some();
        if has("skills") || plugin_root.join("skills").is_dir() {
            components.push(ComponentType::Skills);
        }
        if has("commands") || plugin_root.join("commands").is_dir() {
            components.push(ComponentType::Commands);
        }
        if has("hooks") || plugin_root.join("hooks").join("hooks.json").is_file() {
            components.push(ComponentType::Hooks);
        }
        if has("mcpServers") || plugin_root.join(".mcp.json").is_file() {
            components.push(ComponentType::Mcp);
        }
        Some((name, description, version, components))
    }

    fn recursive_size(path: &Path) -> u64 {
        let Ok(meta) = std::fs::symlink_metadata(path) else {
            return 0;
        };
        if meta.is_file() {
            return meta.len();
        }
        if !meta.is_dir() {
            return 0;
        }
        let mut total = 0u64;
        if let Ok(entries) = std::fs::read_dir(path) {
            for entry in entries.flatten() {
                total += Self::recursive_size(&entry.path());
            }
        }
        total
    }

    /// `listLocalUserPluginCandidates`: config `plugins.dirs`, realpath
    /// deduped, sorted by name.
    pub fn candidates(&self) -> Vec<PluginSyncCandidate> {
        let (dirs, overrides) = self.read_config_state();
        let mut candidates = Vec::new();
        let mut seen: BTreeSet<PathBuf> = BTreeSet::new();
        for raw_dir in dirs {
            let plugin_root = PathBuf::from(&raw_dir);
            let canonical =
                crate::fsutil::canonicalize(&plugin_root).unwrap_or_else(|_| plugin_root.clone());
            if !seen.insert(canonical.clone()) {
                continue;
            }
            let Some((name, description, version, components)) =
                Self::read_manifest_info(&canonical)
            else {
                continue;
            };
            let id = plugin_id(&name);
            let enabled_override =
                overrides.get(&plugin_id_key(&id)).copied();
            candidates.push(PluginSyncCandidate {
                id: sha256_hex(format!("{id}:{}", canonical.display()).as_bytes()),
                enabled: enabled_override.unwrap_or(true),
                enabled_override,
                name,
                plugin_id: id,
                directory_name: canonical
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default(),
                description,
                version,
                path: canonical.clone(),
                size_bytes: Self::recursive_size(&canonical),
                component_types: components,
            });
        }
        candidates.sort_by(|a, b| a.name.cmp(&b.name));
        candidates
    }

    /// `setPluginEnabled`: write the override into `plugins.enabledPlugins`.
    pub fn set_enabled(&self, plugin_id: &str, enabled: bool) -> Result<(), PluginSyncError> {
        let mut config = self.read_config();
        let plugins = config
            .entry("plugins")
            .or_insert_with(|| Value::Object(Map::new()));
        if !plugins.is_object() {
            *plugins = Value::Object(Map::new());
        }
        let obj = plugins.as_object_mut().unwrap_or_else(|| unreachable!());
        let overrides = obj
            .entry("enabledPlugins")
            .or_insert_with(|| Value::Object(Map::new()));
        if !overrides.is_object() {
            *overrides = Value::Object(Map::new());
        }
        overrides
            .as_object_mut()
            .unwrap_or_else(|| unreachable!())
            .insert(plugin_id.to_string(), Value::Bool(enabled));
        write_json_file(&self.config_path(), &Value::Object(config))?;
        Ok(())
    }

    /// `listRemoteUserPluginStatuses`: targetExists first, then
    /// samePluginId against the configured id map.
    pub fn remote_statuses(
        &self,
        requested: &[(String, String)], // (pluginId, directoryName)
    ) -> Vec<RemoteStatus> {
        let (dirs, _) = self.read_config_state();
        let mut configured: BTreeMap<String, PathBuf> = BTreeMap::new();
        for raw_dir in dirs {
            let canonical =
                crate::fsutil::canonicalize(Path::new(&raw_dir)).unwrap_or_else(|_| PathBuf::from(&raw_dir));
            if let Some((name, _, _, _)) = Self::read_manifest_info(&canonical) {
                configured
                    .entry(plugin_id_key(&plugin_id(&name)))
                    .or_insert(canonical);
            }
        }
        let target_root = self.plugin_root();
        requested
            .iter()
            .map(|(pid, directory_name)| {
                let target = target_root.join(directory_name);
                if target.exists() {
                    return RemoteStatus {
                        plugin_id: pid.clone(),
                        directory_name: directory_name.clone(),
                        exists: true,
                        path: Some(target),
                        reason: Some(RemoteSkipReason::TargetExists),
                    };
                }
                match configured.get(&plugin_id_key(pid)) {
                    Some(existing) => RemoteStatus {
                        plugin_id: pid.clone(),
                        directory_name: directory_name.clone(),
                        exists: true,
                        path: Some(existing.clone()),
                        reason: Some(RemoteSkipReason::SamePluginId),
                    },
                    None => RemoteStatus {
                        plugin_id: pid.clone(),
                        directory_name: directory_name.clone(),
                        exists: false,
                        path: None,
                        reason: None,
                    },
                }
            })
            .collect()
    }

    /// `exportSkillsArchive` analog: select by candidate id, verify size
    /// budgets, emit the gzip'd ustar archive with the metadata entry
    /// first, then each selected plugin directory.
    pub fn export_archive(
        &self,
        candidate_ids: &[String],
    ) -> Result<(Vec<u8>, Vec<PluginSyncCandidate>), PluginSyncError> {
        let candidates = self.candidates();
        let mut selected = Vec::new();
        for id in candidate_ids {
            match candidates.iter().find(|c| &c.id == id) {
                Some(c) => selected.push(c.clone()),
                None => return Err(PluginSyncError::UnknownCandidate(id.clone())),
            }
        }
        let selected_bytes: u64 = selected.iter().map(|c| c.size_bytes).sum();
        if selected_bytes > self.max_archive_bytes as u64 {
            return Err(PluginSyncError::SizeLimit {
                phase: "selected-content",
                actual: selected_bytes,
                max: self.max_archive_bytes as u64,
            });
        }

        let mut tar = Vec::new();
        // metadata entry first (fixed archive path)
        let metadata = json!({
            "plugins": selected
                .iter()
                .map(|c| json!({
                    "name": c.name,
                    "pluginId": c.plugin_id,
                    "directoryName": c.directory_name,
                    "enabled": c.enabled,
                }))
                .collect::<Vec<_>>(),
        });
        let metadata_bytes = serde_json::to_vec_pretty(&metadata)?;
        tar.extend_from_slice(&tar_entry_bytes(METADATA_ARCHIVE_PATH, &metadata_bytes)?);
        for candidate in &selected {
            append_tar_entry(&mut tar, &candidate.path, &candidate.directory_name)
                .map_err(|e| PluginSyncError::BadArchive(e.to_string()))?;
        }
        tar.extend_from_slice(&[0u8; 1024]); // two zero end blocks
        let archive = gzip_tar(tar).map_err(|e| PluginSyncError::BadArchive(e.to_string()))?;
        if archive.len() > self.max_archive_bytes {
            return Err(PluginSyncError::SizeLimit {
                phase: "archive",
                actual: archive.len() as u64,
                max: self.max_archive_bytes as u64,
            });
        }
        Ok((archive, selected))
    }

    /// `importArchive` analog: skip-existing by target directory and
    /// configured plugin id; imported plugins register in `plugins.dirs`
    /// and inherit the archive's enabled state as an override when false.
    pub fn import_archive(&self, archive: &[u8]) -> Result<Vec<ImportOutcome>, PluginSyncError> {
        if archive.len() > self.max_archive_bytes {
            return Err(PluginSyncError::SizeLimit {
                phase: "archive",
                actual: archive.len() as u64,
                max: self.max_archive_bytes as u64,
            });
        }
        let target_root = self.plugin_root();
        std::fs::create_dir_all(&target_root)?;
        let temp = target_root.join(format!(
            ".sync-tmp-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.subsec_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&temp)?;
        let result = self.import_archive_inner(archive, &target_root, &temp);
        let _ = std::fs::remove_dir_all(&temp);
        result
    }

    fn import_archive_inner(
        &self,
        archive: &[u8],
        target_root: &Path,
        temp: &Path,
    ) -> Result<Vec<ImportOutcome>, PluginSyncError> {
        extract_archive(archive, temp, self.max_archive_bytes as u64)
            .map_err(|e| PluginSyncError::BadArchive(e.to_string()))?;
        let metadata_path = temp.join(METADATA_ARCHIVE_PATH);
        let metadata: Value = serde_json::from_slice(
            &std::fs::read(&metadata_path)
                .map_err(|_| PluginSyncError::BadArchive("metadata entry missing".into()))?,
        )?;
        let Some(entries) = metadata.get("plugins").and_then(Value::as_array) else {
            return Err(PluginSyncError::BadArchive(
                "metadata entry has no plugin list".into(),
            ));
        };

        // configured plugin ids for the samePluginId rule
        let mut configured: BTreeSet<String> = BTreeSet::new();
        for candidate in self.candidates() {
            configured.insert(plugin_id_key(&candidate.plugin_id));
        }

        let mut outcomes = Vec::new();
        for entry in entries {
            let Some(name) = entry.get("name").and_then(Value::as_str) else {
                continue;
            };
            let Some(plugin_id) = entry.get("pluginId").and_then(Value::as_str) else {
                continue;
            };
            let Some(directory_name) = entry.get("directoryName").and_then(Value::as_str) else {
                continue;
            };
            // path containment on the directory name
            if directory_name.is_empty()
                || directory_name.starts_with('/')
                || directory_name.contains("..")
            {
                outcomes.push(ImportOutcome {
                    name: name.to_string(),
                    plugin_id: plugin_id.to_string(),
                    directory_name: directory_name.to_string(),
                    status: SyncStatus::Failed,
                    path: None,
                    error: Some("unsafe directory name".into()),
                });
                continue;
            }
            let target = target_root.join(directory_name);
            if target.exists() {
                outcomes.push(ImportOutcome {
                    name: name.to_string(),
                    plugin_id: plugin_id.to_string(),
                    directory_name: directory_name.to_string(),
                    status: SyncStatus::Skipped,
                    path: Some(target),
                    error: None,
                });
                continue;
            }
            if configured.contains(&plugin_id_key(plugin_id)) {
                outcomes.push(ImportOutcome {
                    name: name.to_string(),
                    plugin_id: plugin_id.to_string(),
                    directory_name: directory_name.to_string(),
                    status: SyncStatus::Skipped,
                    path: None,
                    error: None,
                });
                continue;
            }
            let source = temp.join(directory_name);
            match copy_dir_recursive(&source, &target) {
                Ok(()) => {
                    // register the dir + inherit a false enabled state
                    self.register_plugin_dir(&target)?;
                    let enabled = entry
                        .get("enabled")
                        .and_then(Value::as_bool)
                        .unwrap_or(true);
                    if !enabled {
                        self.set_enabled(plugin_id, false)?;
                    }
                    outcomes.push(ImportOutcome {
                        name: name.to_string(),
                        plugin_id: plugin_id.to_string(),
                        directory_name: directory_name.to_string(),
                        status: SyncStatus::Synced,
                        path: Some(target),
                        error: None,
                    });
                }
                Err(e) => outcomes.push(ImportOutcome {
                    name: name.to_string(),
                    plugin_id: plugin_id.to_string(),
                    directory_name: directory_name.to_string(),
                    status: SyncStatus::Failed,
                    path: Some(target),
                    error: Some(e.to_string()),
                }),
            }
        }
        Ok(outcomes)
    }

    /// Append the resolved absolute plugin path to `plugins.dirs`
    /// (resolved-path dedupe).
    fn register_plugin_dir(&self, plugin_path: &Path) -> Result<(), PluginSyncError> {
        let mut config = self.read_config();
        let plugins = config
            .entry("plugins")
            .or_insert_with(|| Value::Object(Map::new()));
        if !plugins.is_object() {
            *plugins = Value::Object(Map::new());
        }
        let obj = plugins.as_object_mut().unwrap_or_else(|| unreachable!());
        let dirs = obj
            .entry("dirs")
            .or_insert_with(|| Value::Array(Vec::new()));
        if !dirs.is_array() {
            *dirs = Value::Array(Vec::new());
        }
        let resolved = crate::fsutil::canonicalize(plugin_path)
            .unwrap_or_else(|_| plugin_path.to_path_buf());
        let resolved_str = resolved.to_string_lossy().into_owned();
        let already = dirs
            .as_array()
            .map(|a| {
                a.iter().any(|d| {
                    d.as_str()
                        .map(|s| Path::new(s) == resolved || Path::new(s) == plugin_path)
                        .unwrap_or(false)
                })
            })
            .unwrap_or(false);
        if !already
            && let Some(a) = dirs.as_array_mut()
        {
            a.push(Value::String(resolved_str));
        }
        write_json_file(&self.config_path(), &Value::Object(config))?;
        Ok(())
    }
}

/// One file's tar entry bytes (header + content + padding) for a path
/// that is not a directory.
fn tar_entry_bytes(archive_path: &str, content: &[u8]) -> Result<Vec<u8>, PluginSyncError> {
    let mut tar = Vec::new();
    let mut header = [0u8; 512];
    let name_bytes = archive_path.as_bytes();
    let len = name_bytes.len().min(100);
    header[..len].copy_from_slice(&name_bytes[..len]);
    tar_octal_field(&mut header[100..108], 0o644);
    tar_octal_field(&mut header[124..136], content.len() as u64);
    header[156] = b'0';
    header[257..263].copy_from_slice(b"ustar\0");
    for byte in &mut header[148..156] {
        *byte = b' ';
    }
    let sum: u64 = header.iter().map(|b| *b as u64).sum();
    tar_octal_field(&mut header[148..155], sum);
    header[155] = b' ';
    tar.extend_from_slice(&header);
    tar.extend_from_slice(content);
    let padding = (512 - content.len() % 512) % 512;
    tar.extend(std::iter::repeat_n(0u8, padding));
    Ok(tar)
}

fn tar_octal_field(field: &mut [u8], value: u64) {
    let octal = format!("{:0width$o}", value, width = field.len() - 1);
    let bytes = octal.as_bytes();
    let len = bytes.len().min(field.len() - 1);
    field[..len].copy_from_slice(&bytes[..len]);
    field[len] = 0;
}

fn copy_dir_recursive(source: &Path, target: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(target)?;
    for entry in std::fs::read_dir(source)? {
        let entry = entry?;
        let entry_type = entry.file_type()?;
        let dest = target.join(entry.file_name());
        if entry_type.is_dir() {
            copy_dir_recursive(&entry.path(), &dest)?;
        } else if entry_type.is_symlink() {
            let Ok(target_meta) = std::fs::metadata(entry.path()) else {
                continue;
            };
            if target_meta.is_file() {
                std::fs::copy(entry.path(), dest)?;
            }
        } else {
            std::fs::copy(entry.path(), dest)?;
        }
    }
    Ok(())
}

// keep SkillSyncError referenced for the re-exported helper signatures
#[allow(unused_imports)]
use SkillSyncError as _SkillSyncError;

#[cfg(test)]
mod tests {
    use super::*;

    fn write_plugin(home: &Path, abs_dir: &Path, name: &str, version: &str) -> PathBuf {
        let dir = abs_dir.join(name);
        std::fs::create_dir_all(dir.join(".claude-plugin")).unwrap();
        std::fs::write(
            dir.join(".claude-plugin").join("plugin.json"),
            format!(r#"{{ "name": "{name}", "version": "{version}", "description": "plugin {name}" }}"#),
        )
        .unwrap();
        std::fs::create_dir_all(dir.join("skills")).unwrap();
        std::fs::write(dir.join("skills").join("SKILL.md"), "---\nname: s\n---\nx").unwrap();
        std::fs::write(dir.join("index.js"), "module.exports = {};").unwrap();
        // register in the config
        let mut config = read_json_file_or_empty(&home.join(".okra/config.json"));
        let plugins = config
            .entry("plugins")
            .or_insert_with(|| Value::Object(Map::new()));
        let obj = plugins.as_object_mut().unwrap();
        let dirs = obj.entry("dirs").or_insert_with(|| Value::Array(Vec::new()));
        dirs.as_array_mut()
            .unwrap()
            .push(Value::String(dir.to_string_lossy().into_owned()));
        write_json_file(&home.join(".okra/config.json"), &Value::Object(config)).unwrap();
        dir
    }

    fn service(home: &Path) -> PluginSyncService {
        PluginSyncService::new(home)
    }

    #[test]
    fn candidates_read_config_and_detect_components() {
        let td = tempfile::tempdir().unwrap();
        let home = td.path();
        std::fs::create_dir_all(home.join(".okra")).unwrap();
        write_plugin(home, &td.path().join("plugins"), "formatter", "1.0");

        let candidates = service(home).candidates();
        assert_eq!(candidates.len(), 1);
        let c = &candidates[0];
        assert_eq!(c.name, "formatter");
        assert_eq!(c.plugin_id, "formatter@inline");
        assert_eq!(c.directory_name, "formatter");
        assert_eq!(c.version.as_deref(), Some("1.0"));
        assert!(c.enabled);
        assert!(c.enabled_override.is_none());
        assert_eq!(c.component_types, vec![ComponentType::Skills]);
        assert!(c.size_bytes > 0);

        // enabled override flips the flag and is reported separately
        service(home).set_enabled("formatter@inline", false).unwrap();
        let c = &service(home).candidates()[0];
        assert!(!c.enabled);
        assert_eq!(c.enabled_override, Some(false));
    }

    #[test]
    fn export_import_round_trip_with_skip_rules() {
        let local = tempfile::tempdir().unwrap();
        let home = local.path();
        std::fs::create_dir_all(home.join(".okra")).unwrap();
        let plugin_dir = write_plugin(home, &td_plugins(local.path()), "formatter", "1.0");
        let candidates = service(home).candidates();
        let ids: Vec<String> = candidates.iter().map(|c| c.id.clone()).collect();
        let (archive, exported) = service(home).export_archive(&ids).unwrap();
        assert_eq!(exported.len(), 1);
        assert_eq!(&archive[..2], &[0x1f, 0x8b]);
        let _ = plugin_dir;

        // remote: the same plugin id already configured → skipped
        let remote_home = tempfile::tempdir().unwrap();
        let remote = service(remote_home.path());
        std::fs::create_dir_all(remote_home.path().join(".okra")).unwrap();
        write_plugin(
            remote_home.path(),
            &remote_home.path().join("existing"),
            "formatter",
            "0.9",
        );
        let outcomes = remote.import_archive(&archive).unwrap();
        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0].status, SyncStatus::Skipped);

        // a fresh remote: synced, registered in dirs, enabled state carried
        let fresh = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(fresh.path().join(".okra")).unwrap();
        let outcomes = service(fresh.path()).import_archive(&archive).unwrap();
        assert_eq!(outcomes[0].status, SyncStatus::Synced);
        let installed = fresh.path().join(".okra/plugins/formatter/.claude-plugin/plugin.json");
        assert!(installed.exists());
        let config: Value = serde_json::from_str(
            &std::fs::read_to_string(fresh.path().join(".okra/config.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(config["plugins"]["dirs"].as_array().unwrap().len(), 1);
        assert!(remote_statuses_fresh(fresh.path()));
    }

    fn td_plugins(base: &Path) -> PathBuf {
        let dir = base.join("plugins-src");
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn remote_statuses_fresh(home: &Path) -> bool {
        let statuses = service(home).remote_statuses(&[(
            "formatter@inline".to_string(),
            "formatter".to_string(),
        )]);
        statuses.len() == 1 && statuses[0].exists && statuses[0].reason == Some(RemoteSkipReason::TargetExists)
    }

    #[test]
    fn remote_statuses_order_target_exists_then_same_plugin_id() {
        let td = tempfile::tempdir().unwrap();
        let home = td.path();
        std::fs::create_dir_all(home.join(".okra")).unwrap();
        write_plugin(home, &td.path().join("plugins"), "alpha", "1.0");
        // a target dir that exists but is not configured
        std::fs::create_dir_all(home.join(".okra/plugins/alpha")).unwrap();

        let statuses = service(home).remote_statuses(&[
            ("alpha@inline".to_string(), "alpha".to_string()),
            ("beta@inline".to_string(), "beta".to_string()),
        ]);
        assert_eq!(statuses.len(), 2);
        assert_eq!(statuses[0].reason, Some(RemoteSkipReason::TargetExists));
        assert!(!statuses[1].exists);
        assert_eq!(statuses[1].reason, None);
    }

    #[test]
    fn archive_caps_hold() {
        let td = tempfile::tempdir().unwrap();
        let home = td.path();
        std::fs::create_dir_all(home.join(".okra")).unwrap();
        write_plugin(home, &td.path().join("plugins"), "big", "1.0");
        let svc = service(home);
        let id = svc.candidates()[0].id.clone();
        let capped = PluginSyncService::new(tempfile::tempdir().unwrap().path())
            .with_max_archive_bytes(16);
        let _ = svc;
        // a capped service on an empty home cannot resolve the id
        assert!(matches!(
            capped.export_archive(&[id]),
            Err(PluginSyncError::UnknownCandidate(_))
        ));
        let capped_same_home = service(home).with_max_archive_bytes(1);
        assert!(matches!(
            capped_same_home.export_archive(&[svc.candidates()[0].id.clone()]),
            Err(PluginSyncError::SizeLimit { .. })
        ));
        assert!(matches!(
            capped_same_home.import_archive(b"junk"),
            Err(PluginSyncError::SizeLimit { .. })
        ));
    }
}
