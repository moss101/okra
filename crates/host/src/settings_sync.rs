//! Settings sync (MASTER-PLAN §3 #48 host-domain strangler, from ZCode
//! `packages/services/src/settings-sync/`): discover skills and commands
//! installed by EXTERNAL coding agents and import them into okra's own
//! directories. This is also the groundwork for row #46's agent
//! converters — the path tables below are facts about where each external
//! tool keeps its data, kept verbatim from the donor.
//!
//! Donor contracts kept:
//! - **discovery per agent**: counts, sorted source roots, importable vs
//!   skipped, `selectedByDefault` (skills: when anything is importable;
//!   commands: never — commands overwrite nothing silently);
//! - **skip reasons, in order**: `targetExists` (the exact target is
//!   already there) beats `sameNameExists` (a differently-sourced skill
//!   with the same normalized name lives in the target root, computed
//!   once per target root and cached);
//! - **import modes**: `copy` (recursive, error-on-exist) or `symlink`
//!   (directory symlink from an absolute source path; file symlink for
//!   commands), never overwrite;
//! - **tolerant metadata**: loose frontmatter reads; a missing name falls
//!   back to the directory basename; command names are
//!   `/<relative/path/without/.md>` of the source root.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::fsutil;
use crate::skill_sync::{
    loose_frontmatter_field, parse_skill_metadata, read_loose_frontmatter_version,
    should_walk_entry, SKILL_FILE_NAME, MAX_SKILL_SCAN_DEPTH,
};
use crate::plugins::store::sha256_hex;

/// External agents whose on-disk conventions okra can import from. Names
/// are the donor's — they identify third-party tools.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SyncAgent {
    ClaudeCode,
    CodexCli,
    OpenCode,
    OpenClaw,
    Augment,
    Continue,
    Goose,
    QwenCode,
    Qode,
    QodeCn,
    Windsurf,
    Trae,
    TraeCn,
    KiroCli,
    Roo,
    CodeBuddy,
    /// The `.agents` interop namespace itself (donor `SettingsSyncAgent`).
    Agents,
}

impl SyncAgent {
    pub fn as_str(&self) -> &'static str {
        match self {
            SyncAgent::ClaudeCode => "claudeCode",
            SyncAgent::CodexCli => "codexCli",
            SyncAgent::OpenCode => "openCode",
            SyncAgent::OpenClaw => "openClaw",
            SyncAgent::Augment => "augment",
            SyncAgent::Continue => "continue",
            SyncAgent::Goose => "goose",
            SyncAgent::QwenCode => "qwenCode",
            SyncAgent::Qode => "qode",
            SyncAgent::QodeCn => "qodeCn",
            SyncAgent::Windsurf => "windsurf",
            SyncAgent::Trae => "trae",
            SyncAgent::TraeCn => "traeCn",
            SyncAgent::KiroCli => "kiroCli",
            SyncAgent::Roo => "roo",
            SyncAgent::CodeBuddy => "codeBuddy",
            SyncAgent::Agents => "agents",
        }
    }
}

/// Where one external agent keeps one category: segments under the home
/// directory (global) and under a workspace (project). Donor path tables,
/// verbatim.
const SKILL_SOURCES: &[(&str, &[&str], &[&str])] = &[
    ("claudeCode", &[".claude", "skills"], &[".claude", "skills"]),
    ("codexCli", &[".codex", "skills"], &[".codex", "skills"]),
    ("openCode", &[".config", "opencode", "skills"], &[".opencode", "skills"]),
    ("openClaw", &[".openclaw", "skills"], &["skills"]),
    ("augment", &[".augment", "skills"], &[".augment", "skills"]),
    ("continue", &[".continue", "skills"], &[".continue", "skills"]),
    ("goose", &[".config", "goose", "skills"], &[".goose", "skills"]),
    ("qwenCode", &[".qwen", "skills"], &[".qwen", "skills"]),
    ("qode", &[".qoder", "skills"], &[".qoder", "skills"]),
    ("qodeCn", &[".qoder-cn", "skills"], &[".qoder", "skills"]),
    ("windsurf", &[".codeium", "windsurf", "skills"], &[".windsurf", "skills"]),
    ("trae", &[".trae", "skills"], &[".trae", "skills"]),
    ("traeCn", &[".trae-cn", "skills"], &[".trae", "skills"]),
    ("kiroCli", &[".kiro", "skills"], &[".kiro", "skills"]),
    ("roo", &[".roo", "skills"], &[".roo", "skills"]),
    ("codeBuddy", &[".codebuddy", "skills"], &[".codebuddy", "skills"]),
];

const COMMAND_SOURCES: &[(&str, &[&str], &[&str])] = &[
    ("claudeCode", &[".claude", "commands"], &[".claude", "commands"]),
    ("codexCli", &[".codex", "commands"], &[".codex", "commands"]),
    ("openCode", &[".config", "opencode", "commands"], &[".opencode", "commands"]),
    ("openClaw", &[".openclaw", "commands"], &["commands"]),
    ("augment", &[".augment", "commands"], &[".augment", "commands"]),
    ("continue", &[".continue", "commands"], &[".continue", "commands"]),
    ("goose", &[".config", "goose", "commands"], &[".goose", "commands"]),
    ("qwenCode", &[".qwen", "commands"], &[".qwen", "commands"]),
    ("qode", &[".qoder", "commands"], &[".qoder", "commands"]),
    ("qodeCn", &[".qoder-cn", "commands"], &[".qoder", "commands"]),
    ("windsurf", &[".codeium", "windsurf", "commands"], &[".windsurf", "commands"]),
    ("trae", &[".trae", "commands"], &[".trae", "commands"]),
    ("traeCn", &[".trae-cn", "commands"], &[".trae", "commands"]),
    ("kiroCli", &[".kiro", "commands"], &[".kiro", "commands"]),
    ("roo", &[".roo", "commands"], &[".roo", "commands"]),
    ("codeBuddy", &[".codebuddy", "commands"], &[".codebuddy", "commands"]),
];

fn agent_of(name: &str) -> SyncAgent {
    match name {
        "claudeCode" => SyncAgent::ClaudeCode,
        "codexCli" => SyncAgent::CodexCli,
        "openCode" => SyncAgent::OpenCode,
        "openClaw" => SyncAgent::OpenClaw,
        "augment" => SyncAgent::Augment,
        "continue" => SyncAgent::Continue,
        "goose" => SyncAgent::Goose,
        "qwenCode" => SyncAgent::QwenCode,
        "qode" => SyncAgent::Qode,
        "qodeCn" => SyncAgent::QodeCn,
        "windsurf" => SyncAgent::Windsurf,
        "trae" => SyncAgent::Trae,
        "traeCn" => SyncAgent::TraeCn,
        "kiroCli" => SyncAgent::KiroCli,
        "roo" => SyncAgent::Roo,
        "codeBuddy" => SyncAgent::CodeBuddy,
        "agents" => SyncAgent::Agents,
        _ => unreachable!("source tables only contain known agents"),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceScope {
    Global,
    Project,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImportMode {
    Copy,
    Symlink,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkipReason {
    TargetExists,
    SameNameExists,
}

/// One discovered importable item.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncCandidate {
    pub agent: SyncAgent,
    pub name: String,
    pub name_key: String,
    pub source_root: PathBuf,
    pub source_root_scope: SourceScope,
    pub source_path: PathBuf,
    pub target_root: PathBuf,
    pub target_path: PathBuf,
    /// Skill version when the frontmatter carries one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// Command description / argument hint when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub argument_hint: Option<String>,
}

/// One agent's discovery summary.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentDiscovery {
    pub agent: SyncAgent,
    pub category: String,
    pub discovered: bool,
    pub discovered_count: usize,
    pub importable_count: usize,
    pub skipped_count: usize,
    /// Source root paths, sorted, deduped.
    pub source_paths: Vec<String>,
    pub selected_by_default: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiscoveryResult {
    pub agents: Vec<AgentDiscovery>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SyncImportStatus {
    Imported,
    Skipped,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportResult {
    pub agent: SyncAgent,
    pub name: String,
    pub source_path: PathBuf,
    pub target_path: PathBuf,
    pub status: SyncImportStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skip_reason: Option<SkipReason>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum SettingsSyncError {
    #[error("settings sync io: {0}")]
    Io(#[from] std::io::Error),
}

/// The service rooted at a home directory with an optional workspace
/// (both injectable for tests).
#[derive(Debug, Clone)]
pub struct SettingsSyncService {
    home: PathBuf,
}

impl SettingsSyncService {
    pub fn new(home: impl Into<PathBuf>) -> Self {
        SettingsSyncService { home: home.into() }
    }

    fn root_for(&self, segments: &[&str], scope: SourceScope, workspace: Option<&Path>) -> Option<PathBuf> {
        let base = match scope {
            SourceScope::Global => self.home.to_path_buf(),
            SourceScope::Project => workspace?.to_path_buf(),
        };
        let mut path = base;
        for segment in segments {
            path.push(segment);
        }
        Some(path)
    }

    fn okra_skills_root(&self, scope: SourceScope, workspace: Option<&Path>) -> Option<PathBuf> {
        self.root_for(&[".okra", "skills"], scope, workspace)
    }

    fn okra_commands_root(&self, scope: SourceScope, workspace: Option<&Path>) -> Option<PathBuf> {
        self.root_for(&[".okra", "commands"], scope, workspace)
    }

    /// Discover skills across all external agents.
    pub fn discover_skills(&self, workspace: Option<&Path>) -> DiscoveryResult {
        let candidates = self.collect_skill_candidates(workspace);
        self.build_discovery(&candidates, "skills", true)
    }

    /// Discover commands across all external agents.
    pub fn discover_commands(&self, workspace: Option<&Path>) -> DiscoveryResult {
        let candidates = self.collect_command_candidates(workspace);
        self.build_discovery(&candidates, "commands", false)
    }

    fn build_discovery(
        &self,
        candidates: &[SyncCandidate],
        category: &str,
        selected_by_default: bool,
    ) -> DiscoveryResult {
        let by_agent: BTreeMap<&str, Vec<&SyncCandidate>> = {
            let mut map: BTreeMap<&str, Vec<&SyncCandidate>> = BTreeMap::new();
            for candidate in candidates {
                map.entry(candidate.agent.as_str()).or_default().push(candidate);
            }
            map
        };
        let table: &[(&str, &[&str], &[&str])] = if category == "skills" {
            SKILL_SOURCES
        } else {
            COMMAND_SOURCES
        };
        let mut agents = Vec::new();
        for (name, _, _) in table {
            let Some(agent_candidates) = by_agent.get(name) else {
                continue;
            };
            let agent = agent_of(name);
            let mut existing_cache: HashMap<PathBuf, BTreeSet<String>> = HashMap::new();
            let mut importable_count = 0usize;
            for candidate in agent_candidates {
                if self.skip_reason(candidate, &mut existing_cache).is_none() {
                    importable_count += 1;
                }
            }
            let mut source_paths: Vec<String> = agent_candidates
                .iter()
                .map(|c| c.source_root.to_string_lossy().into_owned())
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect();
            source_paths.sort();
            agents.push(AgentDiscovery {
                agent,
                category: category.to_string(),
                discovered: !agent_candidates.is_empty(),
                discovered_count: agent_candidates.len(),
                importable_count,
                skipped_count: agent_candidates.len() - importable_count,
                source_paths,
                selected_by_default: selected_by_default && importable_count > 0,
            });
        }
        DiscoveryResult { agents }
    }

    /// The skip-reason state machine: `targetExists` first, then
    /// `sameNameExists` against the target root's existing name keys,
    /// computed once per root and cached for the discovery pass.
    fn skip_reason(
        &self,
        candidate: &SyncCandidate,
        existing_cache: &mut HashMap<PathBuf, BTreeSet<String>>,
    ) -> Option<SkipReason> {
        if candidate.target_path.exists() {
            return Some(SkipReason::TargetExists);
        }
        let name_keys = existing_cache
            .entry(candidate.target_root.clone())
            .or_insert_with(|| existing_name_keys_in(&candidate.target_root));
        if name_keys.contains(&candidate.name_key) {
            Some(SkipReason::SameNameExists)
        } else {
            None
        }
    }

    /// Import candidates (the selected set): copy or symlink, never
    /// overwrite. Returns one result per candidate.
    pub fn import(
        &self,
        candidates: &[SyncCandidate],
        mode: ImportMode,
        category: &str,
    ) -> Vec<ImportResult> {
        let mut existing_cache: HashMap<PathBuf, BTreeSet<String>> = HashMap::new();
        let mut results = Vec::new();
        for candidate in candidates {
            if let Some(reason) = self.skip_reason(candidate, &mut existing_cache) {
                results.push(ImportResult {
                    agent: candidate.agent,
                    name: candidate.name.clone(),
                    source_path: candidate.source_path.clone(),
                    target_path: candidate.target_path.clone(),
                    status: SyncImportStatus::Skipped,
                    skip_reason: Some(reason),
                    error: None,
                });
                continue;
            }
            let outcome = if category == "skills" {
                import_directory(&candidate.source_path, &candidate.target_path, mode)
            } else {
                import_file(&candidate.source_path, &candidate.target_path, mode)
            };
            results.push(match outcome {
                Ok(()) => {
                    existing_cache
                        .entry(candidate.target_root.clone())
                        .or_default()
                        .insert(candidate.name_key.clone());
                    ImportResult {
                        agent: candidate.agent,
                        name: candidate.name.clone(),
                        source_path: candidate.source_path.clone(),
                        target_path: candidate.target_path.clone(),
                        status: SyncImportStatus::Imported,
                        skip_reason: None,
                        error: None,
                    }
                }
                Err(e) => ImportResult {
                    agent: candidate.agent,
                    name: candidate.name.clone(),
                    source_path: candidate.source_path.clone(),
                    target_path: candidate.target_path.clone(),
                    status: SyncImportStatus::Failed,
                    skip_reason: None,
                    error: Some(e.to_string()),
                },
            });
        }
        results
    }

    /// Collect skill candidates across all agents (user + workspace roots).
    pub fn collect_skill_candidates(&self, workspace: Option<&Path>) -> Vec<SyncCandidate> {
        let mut candidates = Vec::new();
        for (name, global, project) in SKILL_SOURCES {
            let agent = agent_of(name);
            for (scope, segments) in [
                (SourceScope::Global, *global),
                (SourceScope::Project, *project),
            ] {
                let Some(root) = self.root_for(segments, scope, workspace) else {
                    continue;
                };
                if !root.exists() {
                    continue;
                }
                let Some(target_root) = self.okra_skills_root(scope, workspace) else {
                    continue;
                };
                for (dir, rel, _depth) in walk_dirs_bounded(&root) {
                    let skill_md = dir.join(SKILL_FILE_NAME);
                    let Ok(content) = std::fs::read_to_string(&skill_md) else {
                        continue;
                    };
                    let fallback = rel.rsplit('/').next().unwrap_or(&rel).to_string();
                    let (skill_name, _) = parse_skill_metadata(&content, &fallback);
                    let version = read_loose_frontmatter_version(&content);
                    candidates.push(SyncCandidate {
                        agent,
                        name_key: skill_name.trim().to_ascii_lowercase(),
                        name: skill_name,
                        version,
                        description: None,
                        argument_hint: None,
                        source_root: root.clone(),
                        source_root_scope: scope,
                        source_path: dir.clone(),
                        target_root: target_root.clone(),
                        target_path: target_root.join(&rel),
                    });
                }
            }
        }
        candidates.sort_by(|a, b| a.name_key.cmp(&b.name_key));
        candidates
    }

    /// Collect command candidates: recursive `.md` files under each root
    /// (dot entries skipped); names are `/<rel/path/without/.md>`.
    pub fn collect_command_candidates(&self, workspace: Option<&Path>) -> Vec<SyncCandidate> {
        let mut candidates = Vec::new();
        for (name, global, project) in COMMAND_SOURCES {
            let agent = agent_of(name);
            for (scope, segments) in [
                (SourceScope::Global, *global),
                (SourceScope::Project, *project),
            ] {
                let Some(root) = self.root_for(segments, scope, workspace) else {
                    continue;
                };
                if !root.exists() {
                    continue;
                }
                let Some(target_root) = self.okra_commands_root(scope, workspace) else {
                    continue;
                };
                for (file, rel) in markdown_files_under(&root) {
                    let command_name = format!(
                        "/{}",
                        rel.strip_suffix(".md")
                            .unwrap_or(&rel)
                            .replace('\\', "/")
                    );
                    let (description, argument_hint) = read_command_metadata(&file);
                    candidates.push(SyncCandidate {
                        agent,
                        name_key: command_name.trim().to_ascii_lowercase(),
                        name: command_name,
                        version: None,
                        description,
                        argument_hint,
                        source_root: root.clone(),
                        source_root_scope: scope,
                        source_path: file,
                        target_root: target_root.clone(),
                        target_path: target_root.join(&rel),
                    });
                }
            }
        }
        candidates.sort_by(|a, b| a.name_key.cmp(&b.name_key));
        candidates
    }

    /// Stable candidate id (sha256 of the source path) — mirrors the
    /// donor's per-candidate identity shape.
    pub fn candidate_id(candidate: &SyncCandidate) -> String {
        sha256_hex(candidate.source_path.to_string_lossy().as_bytes())
    }
}

/// Bounded directory walk reusing the shared skill scan policy.
fn walk_dirs_bounded(root: &Path) -> Vec<(PathBuf, String, usize)> {
    let mut out = Vec::new();
    let mut visited_symlinks: BTreeSet<PathBuf> = BTreeSet::new();
    let mut stack = vec![(root.to_path_buf(), String::new(), 0usize)];
    while let Some((dir, rel, depth)) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut children: Vec<(PathBuf, String)> = Vec::new();
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !should_walk_entry(&name) {
                continue;
            }
            let path = entry.path();
            let is_symlink = entry.file_type().map(|t| t.is_symlink()).unwrap_or(false);
            let is_dir = if is_symlink {
                std::fs::metadata(&path).map(|m| m.is_dir()).unwrap_or(false)
            } else {
                entry.file_type().map(|t| t.is_dir()).unwrap_or(false)
            };
            if !is_dir {
                continue;
            }
            if is_symlink {
                let canonical = fsutil::canonicalize(&path).unwrap_or_else(|_| path.clone());
                if !visited_symlinks.insert(canonical) {
                    continue;
                }
            }
            let child_rel = if rel.is_empty() { name } else { format!("{rel}/{name}") };
            children.push((path, child_rel));
        }
        children.sort_by(|a, b| a.1.cmp(&b.1));
        for (path, child_rel) in children {
            out.push((path.clone(), child_rel.clone(), depth + 1));
            if depth + 1 < MAX_SKILL_SCAN_DEPTH {
                stack.push((path, child_rel, depth + 1));
            }
        }
    }
    out
}

fn markdown_files_under(root: &Path) -> Vec<(PathBuf, String)> {
    let mut found = BTreeSet::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                continue;
            }
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if name.to_ascii_lowercase().ends_with(".md") {
                let rel = path
                    .strip_prefix(root)
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or(name);
                found.insert((path, rel));
            }
        }
    }
    found.into_iter().collect()
}

/// Existing normalized skill names inside one target root (donor
/// `collectExistingSkillNameKeys`).
fn existing_name_keys_in(root: &Path) -> BTreeSet<String> {
    let mut keys = BTreeSet::new();
    if !root.exists() {
        return keys;
    }
    for (dir, rel, _depth) in walk_dirs_bounded(root) {
        let Ok(content) = std::fs::read_to_string(dir.join(SKILL_FILE_NAME)) else {
            continue;
        };
        let fallback = rel.rsplit('/').next().unwrap_or(&rel).to_string();
        let (name, _) = parse_skill_metadata(&content, &fallback);
        keys.insert(name.trim().to_ascii_lowercase());
    }
    keys
}

fn read_command_metadata(path: &Path) -> (Option<String>, Option<String>) {
    let Ok(content) = std::fs::read_to_string(path) else {
        return (None, None);
    };
    let description = loose_frontmatter_field(&content, "description");
    let argument_hint = loose_frontmatter_field(&content, "argument-hint")
        .or_else(|| loose_frontmatter_field(&content, "argumentHint"));
    (description, argument_hint)
}

fn import_directory(source: &Path, target: &Path, mode: ImportMode) -> Result<(), std::io::Error> {
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if mode == ImportMode::Symlink {
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(fsutil::canonicalize(source)?, target)?;
            return Ok(());
        }
        #[cfg(not(unix))]
        {
            copy_dir_recursive(source, target)?;
            return Ok(());
        }
    }
    copy_dir_recursive(source, target)
}

fn import_file(source: &Path, target: &Path, mode: ImportMode) -> Result<(), std::io::Error> {
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if mode == ImportMode::Symlink {
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(fsutil::canonicalize(source)?, target)?;
            return Ok(());
        }
        #[cfg(not(unix))]
        {
            std::fs::copy(source, target)?;
            return Ok(());
        }
    }
    std::fs::copy(source, target)?;
    Ok(())
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

// ---------------------------------------------------------------------------
// Plugins category
// ---------------------------------------------------------------------------

/// Donor plugin source tables (15 agents; no `agents` interop entry).
const PLUGIN_SOURCES: &[(&str, &[&str], &[&str])] = &[
    ("claudeCode", &[".claude", "plugins"], &[".claude", "plugins"]),
    ("codexCli", &[".codex", "plugins"], &[".codex", "plugins"]),
    ("openCode", &[".config", "opencode", "plugins"], &[".opencode", "plugins"]),
    ("openClaw", &[".openclaw", "plugins"], &["plugins"]),
    ("augment", &[".augment", "plugins"], &[".augment", "plugins"]),
    ("continue", &[".continue", "plugins"], &[".continue", "plugins"]),
    ("goose", &[".config", "goose", "plugins"], &[".goose", "plugins"]),
    ("qwenCode", &[".qwen", "plugins"], &[".qwen", "plugins"]),
    ("qode", &[".qoder", "plugins"], &[".qoder", "plugins"]),
    ("qodeCn", &[".qoder-cn", "plugins"], &[".qoder", "plugins"]),
    ("windsurf", &[".codeium", "windsurf", "plugins"], &[".windsurf", "plugins"]),
    ("trae", &[".trae", "plugins"], &[".trae", "plugins"]),
    ("kiroCli", &[".kiro", "plugins"], &[".kiro", "plugins"]),
    ("roo", &[".roo", "plugins"], &[".roo", "plugins"]),
    ("codeBuddy", &[".codebuddy", "plugins"], &[".codebuddy", "plugins"]),
];

/// Manifest locations inside one plugin directory, in priority order
/// (donor: zcode→claude→codex; okra's own first).
const PLUGIN_MANIFEST_PATHS: [&str; 3] = [".okra-plugin", ".claude-plugin", ".codex-plugin"];
const INLINE_PLUGIN_MARKETPLACE: &str = "inline";

impl SettingsSyncService {
    fn okra_plugins_root(&self, scope: SourceScope, workspace: Option<&Path>) -> Option<PathBuf> {
        self.root_for(&[".okra", "plugins"], scope, workspace)
    }

    fn okra_config_path(&self, scope: SourceScope, workspace: Option<&Path>) -> Option<PathBuf> {
        self.root_for(&[".okra", "config.json"], scope, workspace)
    }

    pub(crate) fn find_plugin_manifest(plugin_path: &Path) -> Option<PathBuf> {
        for dir in PLUGIN_MANIFEST_PATHS {
            let candidate = plugin_path.join(dir).join("plugin.json");
            if candidate.exists() {
                return Some(candidate);
            }
        }
        None
    }

    /// `readPluginMetadata`: a valid manifest with a non-blank name makes
    /// the directory a plugin; its identity is `name@inline`.
    fn read_plugin_metadata(plugin_path: &Path) -> Option<(String, Option<String>)> {
        let manifest = Self::find_plugin_manifest(plugin_path)?;
        let parsed: Value = serde_json::from_str(&std::fs::read_to_string(manifest).ok()?).ok()?;
        let name = parsed.get("name")?.as_str()?.trim().to_string();
        if name.is_empty() {
            return None;
        }
        let version = parsed
            .get("version")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(str::to_string);
        Some((name, version))
    }

    fn plugin_id(name: &str) -> String {
        format!("{name}@{INLINE_PLUGIN_MARKETPLACE}").to_ascii_lowercase()
    }

    /// `collectConfiguredPluginIds`: ids of the plugins referenced by the
    /// okra config's `plugins.dirs` (absolute directory paths).
    fn collect_configured_plugin_ids(&self, config_path: &Path) -> BTreeSet<String> {
        let mut ids = BTreeSet::new();
        let Ok(content) = std::fs::read_to_string(config_path) else {
            return ids;
        };
        let Ok(parsed) = serde_json::from_str::<Value>(&content) else {
            return ids;
        };
        let Some(dirs) = parsed
            .get("plugins")
            .and_then(|p| p.get("dirs"))
            .and_then(Value::as_array)
        else {
            return ids;
        };
        for dir in dirs {
            let Some(dir) = dir.as_str() else { continue };
            if let Some((name, _)) = Self::read_plugin_metadata(Path::new(dir)) {
                ids.insert(Self::plugin_id(&name));
            }
        }
        ids
    }

    /// Collect plugin candidates across all agents. A plugin is a
    /// non-dot directory (or symlink to one) directly under the root
    /// carrying a recognizable manifest.
    pub fn collect_plugin_candidates(&self, workspace: Option<&Path>) -> Vec<SyncCandidate> {
        let mut candidates = Vec::new();
        for (name, global, project) in PLUGIN_SOURCES {
            let agent = agent_of(name);
            for (scope, segments) in [
                (SourceScope::Global, *global),
                (SourceScope::Project, *project),
            ] {
                let Some(root) = self.root_for(segments, scope, workspace) else {
                    continue;
                };
                let Ok(entries) = std::fs::read_dir(&root) else {
                    continue;
                };
                let Some(target_root) = self.okra_plugins_root(scope, workspace) else {
                    continue;
                };
                let mut dirs: Vec<PathBuf> = entries
                    .flatten()
                    .filter(|e| {
                        let name = e.file_name().to_string_lossy().into_owned();
                        !name.starts_with('.')
                    })
                    .map(|e| e.path())
                    .filter(|p| {
                        p.is_dir()
                            || std::fs::symlink_metadata(p)
                                .map(|m| m.file_type().is_symlink())
                                .unwrap_or(false)
                    })
                    .filter(|p| Self::find_plugin_manifest(p).is_some())
                    .collect();
                dirs.sort();
                for plugin_dir in dirs {
                    let dir_name = plugin_dir
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    let Some((plugin_name, version)) = Self::read_plugin_metadata(&plugin_dir)
                    else {
                        continue;
                    };
                    candidates.push(SyncCandidate {
                        agent,
                        name_key: Self::plugin_id(&plugin_name),
                        name: plugin_name,
                        version,
                        description: None,
                        argument_hint: None,
                        source_root: root.clone(),
                        source_root_scope: scope,
                        source_path: plugin_dir,
                        target_root: target_root.clone(),
                        target_path: target_root.join(&dir_name),
                    });
                }
            }
        }
        candidates.sort_by(|a, b| a.name_key.cmp(&b.name_key));
        candidates
    }

    /// `buildPluginsDiscovery`: plugins are never default-selected.
    pub fn discover_plugins(&self, workspace: Option<&Path>) -> DiscoveryResult {
        let candidates = self.collect_plugin_candidates(workspace);
        let mut by_agent: BTreeMap<&str, Vec<&SyncCandidate>> = BTreeMap::new();
        for candidate in &candidates {
            by_agent
                .entry(candidate.agent.as_str())
                .or_default()
                .push(candidate);
        }
        let mut agents = Vec::new();
        for (name, _, _) in PLUGIN_SOURCES {
            let Some(agent_candidates) = by_agent.get(name) else {
                continue;
            };
            let mut configured_cache: HashMap<SourceScope, BTreeSet<String>> = HashMap::new();
            let mut importable_count = 0usize;
            for candidate in agent_candidates {
                let scope = candidate.source_root_scope;
                if candidate.target_path.exists() {
                    continue;
                }
                let configured = configured_cache
                    .entry(scope)
                    .or_insert_with(|| {
                        self.okra_config_path(scope, workspace)
                            .map(|p| self.collect_configured_plugin_ids(&p))
                            .unwrap_or_default()
                    });
                if configured.contains(&candidate.name_key) {
                    continue;
                }
                importable_count += 1;
            }
            let mut source_paths: Vec<String> = agent_candidates
                .iter()
                .map(|c| c.source_root.to_string_lossy().into_owned())
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect();
            source_paths.sort();
            agents.push(AgentDiscovery {
                agent: agent_of(name),
                category: "plugins".to_string(),
                discovered: !agent_candidates.is_empty(),
                discovered_count: agent_candidates.len(),
                importable_count,
                skipped_count: agent_candidates.len() - importable_count,
                source_paths,
                selected_by_default: false,
            });
        }
        DiscoveryResult { agents }
    }

    /// Import plugins: copy/symlink the directory, then register the
    /// target path in the okra config's `plugins.dirs`.
    pub fn import_plugins(
        &self,
        candidates: &[SyncCandidate],
        mode: ImportMode,
        workspace: Option<&Path>,
    ) -> Vec<ImportResult> {
        let mut configured_cache: HashMap<PathBuf, BTreeSet<String>> = HashMap::new();
        let mut results = Vec::new();
        for candidate in candidates {
            let scope = candidate.source_root_scope;
            let config_path = self.okra_config_path(scope, workspace);
            // targetExists first
            if candidate.target_path.exists() {
                results.push(self.skipped_plugin(candidate, SkipReason::TargetExists));
                continue;
            }
            // then sameNameExists against the configured plugin ids
            let Some(config_path) = config_path.clone() else {
                results.push(self.skipped_plugin(candidate, SkipReason::TargetExists));
                continue;
            };
            let configured = configured_cache
                .entry(config_path.clone())
                .or_insert_with(|| self.collect_configured_plugin_ids(&config_path));
            if configured.contains(&candidate.name_key) {
                results.push(self.skipped_plugin(candidate, SkipReason::SameNameExists));
                continue;
            }
            let outcome = import_directory(&candidate.source_path, &candidate.target_path, mode)
                .and_then(|()| {
                    self.add_plugin_dir_to_config(
                        &config_path,
                        &candidate.target_path,
                        configured,
                    )
                });
            results.push(match outcome {
                Ok(()) => {
                    ImportResult {
                        agent: candidate.agent,
                        name: candidate.name.clone(),
                        source_path: candidate.source_path.clone(),
                        target_path: candidate.target_path.clone(),
                        status: SyncImportStatus::Imported,
                        skip_reason: None,
                        error: None,
                    }
                }
                Err(e) => ImportResult {
                    agent: candidate.agent,
                    name: candidate.name.clone(),
                    source_path: candidate.source_path.clone(),
                    target_path: candidate.target_path.clone(),
                    status: SyncImportStatus::Failed,
                    skip_reason: None,
                    error: Some(e.to_string()),
                },
            });
        }
        results
    }

    fn skipped_plugin(&self, candidate: &SyncCandidate, reason: SkipReason) -> ImportResult {
        ImportResult {
            agent: candidate.agent,
            name: candidate.name.clone(),
            source_path: candidate.source_path.clone(),
            target_path: candidate.target_path.clone(),
            status: SyncImportStatus::Skipped,
            skip_reason: Some(reason),
            error: None,
        }
    }

    /// `addPluginDirToConfig`: append the resolved absolute path to
    /// `plugins.dirs`, deduping by resolved path.
    fn add_plugin_dir_to_config(
        &self,
        config_path: &Path,
        plugin_path: &Path,
        configured: &mut BTreeSet<String>,
    ) -> Result<(), std::io::Error> {
        let mut parsed = read_json_file_or_empty(config_path);
        let resolved = fsutil::canonicalize(plugin_path)
            .unwrap_or_else(|_| plugin_path.to_path_buf());
        let resolved_str = resolved.to_string_lossy().into_owned();
        if configured.contains(&Self::plugin_id(
            &Self::read_plugin_metadata(&resolved)
                .map(|(n, _)| n)
                .unwrap_or_default(),
        )) {
            return Ok(());
        }
        let plugins = parsed
            .entry("plugins")
            .or_insert_with(|| Value::Object(Map::new()));
        if !plugins.is_object() {
            *plugins = Value::Object(Map::new());
        }
        let plugins = plugins.as_object_mut().unwrap_or_else(|| unreachable!());
        let dirs = plugins
            .entry("dirs")
            .or_insert_with(|| Value::Array(Vec::new()));
        if !dirs.is_array() {
            *dirs = Value::Array(Vec::new());
        }
        let already = dirs
            .as_array()
            .map(|a| {
                a.iter().any(|d| {
                    d.as_str()
                        .map(|s| {
                            Path::new(s).to_path_buf() == resolved
                                || Path::new(s) == plugin_path
                        })
                        .unwrap_or(false)
                })
            })
            .unwrap_or(false);
        if !already {
            if let Some(a) = dirs.as_array_mut() {
                a.push(Value::String(resolved_str));
            }
            configured.insert(Self::plugin_id(
                &Self::read_plugin_metadata(&resolved)
                    .map(|(n, _)| n)
                    .unwrap_or_default(),
            ));
        }
        write_json_file(config_path, &Value::Object(parsed))
    }
}

pub(crate) fn read_json_file_or_empty(path: &Path) -> Map<String, Value> {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|c| serde_json::from_str::<Value>(&c).ok())
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default()
}

pub(crate) fn write_json_file(path: &Path, value: &Value) -> Result<(), std::io::Error> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut text = serde_json::to_string_pretty(value)?;
    text.push('\n');
    std::fs::write(path, text)
}

// ---------------------------------------------------------------------------
// MCP servers category
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum McpFormat {
    /// Top-level `mcpServers` object (Claude Code and most others).
    McpServersJson,
    /// openCode: top-level `mcp` object, array-form commands.
    McpJson,
    /// Codex CLI: TOML `mcp_servers` tables.
    CodexToml,
}

struct McpSource {
    agent: &'static str,
    project_files: &'static [&'static [&'static str]],
    global_files: &'static [&'static [&'static str]],
    format: McpFormat,
}

/// Donor MCP source table, verbatim.
const MCP_SOURCES: &[McpSource] = &[
    McpSource {
        agent: "claudeCode",
        project_files: &[&[".claude", "settings.json"], &[".mcp.json"]],
        global_files: &[&[".claude", "settings.json"]],
        format: McpFormat::McpServersJson,
    },
    McpSource {
        agent: "codexCli",
        project_files: &[&[".codex", "config.toml"]],
        global_files: &[&[".codex", "config.toml"]],
        format: McpFormat::CodexToml,
    },
    McpSource {
        agent: "openCode",
        project_files: &[&[".opencode", "opencode.json"]],
        global_files: &[&[".config", "opencode", "opencode.json"]],
        format: McpFormat::McpJson,
    },
    McpSource {
        agent: "openClaw",
        project_files: &[&["settings.json"]],
        global_files: &[&[".openclaw", "settings.json"]],
        format: McpFormat::McpServersJson,
    },
    McpSource {
        agent: "qwenCode",
        project_files: &[&[".qwen", "settings.json"]],
        global_files: &[&[".qwen", "settings.json"]],
        format: McpFormat::McpServersJson,
    },
    McpSource {
        agent: "qode",
        project_files: &[&[".qoder", "settings.json"]],
        global_files: &[&[".qoder", "settings.json"]],
        format: McpFormat::McpServersJson,
    },
    McpSource {
        agent: "qodeCn",
        project_files: &[&[".qoder", "settings.json"]],
        global_files: &[&[".qoder-cn", "settings.json"]],
        format: McpFormat::McpServersJson,
    },
    McpSource {
        agent: "trae",
        project_files: &[&[".trae", "settings.json"]],
        global_files: &[&[".trae", "settings.json"]],
        format: McpFormat::McpServersJson,
    },
    McpSource {
        agent: "kiroCli",
        project_files: &[&[".kiro", "settings.json"]],
        global_files: &[&[".kiro", "settings.json"]],
        format: McpFormat::McpServersJson,
    },
    McpSource {
        agent: "roo",
        project_files: &[&[".roo", "settings.json"]],
        global_files: &[&[".roo", "settings.json"]],
        format: McpFormat::McpServersJson,
    },
    McpSource {
        agent: "codeBuddy",
        project_files: &[&[".codebuddy", "settings.json"]],
        global_files: &[&[".codebuddy", "settings.json"]],
        format: McpFormat::McpServersJson,
    },
    McpSource {
        agent: "agents",
        project_files: &[&[".agents", "mcp.json"]],
        global_files: &[&[".agents", "mcp.json"]],
        format: McpFormat::McpServersJson,
    },
];

/// One discovered MCP server with its extracted config.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpCandidate {
    pub agent: SyncAgent,
    pub name: String,
    pub name_key: String,
    pub config: Value,
    /// `<file>#<server>` — the donor's per-server source identity.
    pub source_path: String,
    pub source_root_scope: SourceScope,
    pub source_root_path: PathBuf,
}

/// `stripExternalMcpTimeoutFields`: external-only timeout knobs must not
/// leak into okra's config shape.
fn strip_external_timeout_fields(config: &mut Value) {
    if let Some(obj) = config.as_object_mut() {
        obj.remove("timeout");
        obj.remove("startup_timeout_sec");
    }
}

fn normalize_mcp_server_map(value: &Value) -> Vec<(String, Value)> {
    let mut servers = Vec::new();
    let Some(map) = value.as_object() else {
        return servers;
    };
    for (name, config) in map {
        if name.trim().is_empty() || !config.is_object() {
            continue;
        }
        servers.push((name.clone(), config.clone()));
    }
    servers
}

/// `readOpenCodeMcpServers`: normalize openCode's array-form commands and
/// local/remote type vocabulary onto okra's stdio/http shape.
fn normalize_opencode_config(mut config: Value) -> Value {
    let obj = match config.as_object() {
        Some(o) => o,
        None => return config,
    };
    let raw_command = obj.get("command").cloned();
    if let Some(Value::Array(items)) = raw_command {
        let strings: Vec<String> = items
            .iter()
            .filter_map(|v| v.as_str().map(str::to_string))
            .collect();
        if let Some(first) = strings.first() {
            let type_value = obj.get("type").and_then(Value::as_str).map(str::to_string);
            let args: Vec<Value> =
                strings.iter().skip(1).map(|s| Value::String(s.clone())).collect();
            if let Some(o) = config.as_object_mut() {
                o.remove("type");
                o.insert("command".into(), Value::String(first.clone()));
                o.insert("args".into(), Value::Array(args));
                match type_value.as_deref() {
                    Some("local") => {
                        o.insert("type".into(), Value::String("stdio".into()));
                    }
                    Some("remote") => {
                        o.insert("type".into(), Value::String("http".into()));
                    }
                    _ => {}
                }
            }
        }
        return config;
    }
    let local_string_command = obj.get("type").and_then(Value::as_str) == Some("local")
        && obj
            .get("command")
            .and_then(Value::as_str)
            .is_some_and(|c| !c.trim().is_empty());
    if local_string_command
        && let Some(o) = config.as_object_mut()
    {
        o.insert("type".into(), Value::String("stdio".into()));
    }
    config
}

fn read_mcp_servers_from_source_file(
    path: &Path,
    format: McpFormat,
) -> Vec<(String, Value)> {
    let Ok(raw) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    match format {
        McpFormat::CodexToml => {
            let parsed: toml::Value = match toml::from_str(&raw) {
                Ok(v) => v,
                Err(_) => return Vec::new(),
            };
            let Ok(json) = serde_json::to_value(&parsed) else {
                return Vec::new();
            };
            let table = json
                .get("mcp_servers")
                .or_else(|| json.get("mcpServers"))
                .cloned()
                .unwrap_or(Value::Null);
            normalize_mcp_server_map(&table)
                .into_iter()
                .map(|(name, mut config)| {
                    strip_external_timeout_fields(&mut config);
                    (name, config)
                })
                .collect()
        }
        McpFormat::McpJson => {
            let parsed: Value = match serde_json::from_str(&raw) {
                Ok(v) => v,
                Err(_) => return Vec::new(),
            };
            normalize_mcp_server_map(parsed.get("mcp").unwrap_or(&Value::Null))
                .into_iter()
                .map(|(name, config)| (name, normalize_opencode_config(config)))
                .collect()
        }
        McpFormat::McpServersJson => {
            let parsed: Value = match serde_json::from_str(&raw) {
                Ok(v) => v,
                Err(_) => return Vec::new(),
            };
            normalize_mcp_server_map(parsed.get("mcpServers").unwrap_or(&Value::Null))
                .into_iter()
                .map(|(name, mut config)| {
                    strip_external_timeout_fields(&mut config);
                    (name, config)
                })
                .collect()
        }
    }
}

impl SettingsSyncService {
    /// `collectMcpImportCandidates`: per source file, extract the server
    /// map, dedupe by `agent#sourcePath`, strip external timeout fields.
    pub fn collect_mcp_candidates(&self, workspace: Option<&Path>) -> Vec<McpCandidate> {
        let mut candidates = Vec::new();
        let mut seen: BTreeSet<String> = BTreeSet::new();
        for source in MCP_SOURCES {
            let agent = agent_of(source.agent);
            for (scope, files) in [
                (SourceScope::Global, source.global_files),
                (SourceScope::Project, source.project_files),
            ] {
                for segments in files {
                    let Some(path) = self.root_for(segments, scope, workspace) else {
                        continue;
                    };
                    if !path.exists() {
                        continue;
                    }
                    for (name, config) in read_mcp_servers_from_source_file(&path, source.format) {
                        let source_path = format!("{}#{name}", path.display());
                        let seen_key = format!("{}:{source_path}", agent.as_str());
                        if !seen.insert(seen_key) {
                            continue;
                        }
                        let name_key = name.trim().to_ascii_lowercase();
                        candidates.push(McpCandidate {
                            agent,
                            name,
                            name_key,
                            config,
                            source_path,
                            source_root_scope: scope,
                            source_root_path: path.clone(),
                        });
                    }
                }
            }
        }
        candidates
    }

    /// Discover MCP servers per agent (category `mcpServers`, never
    /// default-selected).
    pub fn discover_mcp(&self, workspace: Option<&Path>) -> DiscoveryResult {
        let candidates = self.collect_mcp_candidates(workspace);
        let mut by_agent: BTreeMap<&str, Vec<&McpCandidate>> = BTreeMap::new();
        for candidate in &candidates {
            by_agent
                .entry(candidate.agent.as_str())
                .or_default()
                .push(candidate);
        }
        let mut agents = Vec::new();
        for source in MCP_SOURCES {
            let Some(agent_candidates) = by_agent.get(source.agent) else {
                continue;
            };
            let mut existing_cache: HashMap<SourceScope, BTreeSet<String>> = HashMap::new();
            let mut importable_count = 0usize;
            for candidate in agent_candidates {
                if self
                    .mcp_skip_reason(candidate, workspace, &mut existing_cache)
                    .is_none()
                {
                    importable_count += 1;
                }
            }
            let mut source_paths: Vec<String> = agent_candidates
                .iter()
                .map(|c| c.source_root_path.to_string_lossy().into_owned())
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect();
            source_paths.sort();
            agents.push(AgentDiscovery {
                agent: agent_of(source.agent),
                category: "mcpServers".to_string(),
                discovered: !agent_candidates.is_empty(),
                discovered_count: agent_candidates.len(),
                importable_count,
                skipped_count: agent_candidates.len() - importable_count,
                source_paths,
                selected_by_default: false,
            });
        }
        DiscoveryResult { agents }
    }

    /// MCP skip state: the server's normalized name already in the target
    /// config's `mcp.servers` (cached per target scope).
    fn mcp_skip_reason(
        &self,
        candidate: &McpCandidate,
        workspace: Option<&Path>,
        existing_cache: &mut HashMap<SourceScope, BTreeSet<String>>,
    ) -> Option<SkipReason> {
        let Some(config_path) = self.okra_config_path(candidate.source_root_scope, workspace)
        else {
            return Some(SkipReason::SameNameExists);
        };
        let keys = existing_cache
            .entry(candidate.source_root_scope)
            .or_insert_with(|| {
                let parsed = read_json_file_or_empty(&config_path);
                let servers = parsed
                    .get("mcp")
                    .and_then(|m| m.get("servers"))
                    .cloned()
                    .unwrap_or(Value::Null);
                normalize_mcp_server_map(&servers)
                    .into_iter()
                    .map(|(n, _)| n.trim().to_ascii_lowercase())
                    .collect()
            });
        if keys.contains(&candidate.name_key) {
            Some(SkipReason::SameNameExists)
        } else {
            None
        }
    }

    /// Import MCP servers into okra's config `mcp.servers` (target scope
    /// mirrors each candidate's source scope), preserving unrelated keys.
    pub fn import_mcp(
        &self,
        candidates: &[McpCandidate],
        workspace: Option<&Path>,
    ) -> Vec<ImportResult> {
        let mut existing_cache: HashMap<SourceScope, BTreeSet<String>> = HashMap::new();
        let mut results = Vec::new();
        for candidate in candidates {
            let target = ImportResult {
                agent: candidate.agent,
                name: candidate.name.clone(),
                source_path: PathBuf::from(&candidate.source_path),
                target_path: self
                    .okra_config_path(candidate.source_root_scope, workspace)
                    .unwrap_or_default(),
                status: SyncImportStatus::Skipped,
                skip_reason: None,
                error: None,
            };
            if let Some(reason) =
                self.mcp_skip_reason(candidate, workspace, &mut existing_cache)
            {
                results.push(ImportResult {
                    skip_reason: Some(reason),
                    ..target
                });
                continue;
            }
            let config_path =
                match self.okra_config_path(candidate.source_root_scope, workspace) {
                    Some(p) => p,
                    None => {
                        results.push(ImportResult {
                            skip_reason: Some(SkipReason::SameNameExists),
                            ..target
                        });
                        continue;
                    }
                };
            let mut parsed = read_json_file_or_empty(&config_path);
            let mcp = parsed
                .entry("mcp")
                .or_insert_with(|| Value::Object(Map::new()));
            if !mcp.is_object() {
                *mcp = Value::Object(Map::new());
            }
            let servers = mcp
                .as_object_mut()
                .unwrap_or_else(|| unreachable!("just normalized"))
                .entry("servers")
                .or_insert_with(|| Value::Object(Map::new()));
            if !servers.is_object() {
                *servers = Value::Object(Map::new());
            }
            servers
                .as_object_mut()
                .unwrap_or_else(|| unreachable!("just normalized"))
                .insert(candidate.name.clone(), candidate.config.clone());
            match write_json_file(&config_path, &Value::Object(parsed)) {
                Ok(()) => {
                    existing_cache
                        .entry(candidate.source_root_scope)
                        .or_default()
                        .insert(candidate.name_key.clone());
                    results.push(ImportResult {
                        status: SyncImportStatus::Imported,
                        ..target
                    });
                }
                Err(e) => {
                    results.push(ImportResult {
                        status: SyncImportStatus::Failed,
                        error: Some(e.to_string()),
                        ..target
                    });
                }
            }
        }
        results
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_skill(root: &Path, rel: &str, name: &str) {
        let dir = root.join(rel);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(SKILL_FILE_NAME),
            format!("---\nname: {name}\n---\n\nBody.\n"),
        )
        .unwrap();
    }

    #[test]
    fn source_tables_match_donor_paths() {
        let claude = SKILL_SOURCES.iter().find(|(n, _, _)| *n == "claudeCode").unwrap();
        assert_eq!(claude.1, &[".claude", "skills"]);
        assert_eq!(claude.2, &[".claude", "skills"]);
        let opencode = SKILL_SOURCES.iter().find(|(n, _, _)| *n == "openCode").unwrap();
        assert_eq!(opencode.1, &[".config", "opencode", "skills"]);
        assert_eq!(opencode.2, &[".opencode", "skills"]);
        assert_eq!(SKILL_SOURCES.len(), 16);
        assert_eq!(COMMAND_SOURCES.len(), 16);
    }

    #[test]
    fn discovery_finds_global_and_project_sources() {
        let td = tempfile::tempdir().unwrap();
        let home = td.path();
        let ws = td.path().join("work");
        write_skill(&home.join(".claude/skills"), "review", "review");
        write_skill(&home.join(".claude/skills"), "refactor", "refactor");
        write_skill(&home.join(".codex/skills"), "deploy", "deploy");
        std::fs::create_dir_all(ws.join(".claude/skills")).unwrap();
        write_skill(&ws.join(".claude/skills"), "project-only", "proj");

        let svc = SettingsSyncService::new(home);
        let discovery = svc.discover_skills(Some(&ws));
        let claude = discovery
            .agents
            .iter()
            .find(|a| a.agent == SyncAgent::ClaudeCode)
            .unwrap();
        assert_eq!(claude.discovered_count, 3, "global 2 + project 1");
        assert_eq!(claude.importable_count, 3);
        assert!(claude.selected_by_default);
        // codex has no workspace source in this setup
        let codex = discovery
            .agents
            .iter()
            .find(|a| a.agent == SyncAgent::CodexCli)
            .unwrap();
        assert_eq!(codex.discovered_count, 1);
        // undiscovered agents are absent
        assert!(!discovery.agents.iter().any(|a| a.agent == SyncAgent::Goose));
    }

    #[test]
    fn skip_reasons_and_import_copy_mode() {
        let td = tempfile::tempdir().unwrap();
        let home = td.path();
        write_skill(&home.join(".claude/skills"), "review", "review");
        write_skill(&home.join(".claude/skills"), "notes", "REVIEW");
        write_skill(&home.join(".claude/skills"), "deploy", "deploy");
        // a target already present, and a same-name skill already imported
        let target_root = home.join(".okra/skills");
        write_skill(&target_root, "existing-name", "review");

        let svc = SettingsSyncService::new(home);
        let candidates = svc.collect_skill_candidates(None);
        let by_name: HashMap<&str, &SyncCandidate> =
            candidates.iter().map(|c| (c.name.as_str(), c)).collect();
        let review = by_name["review"].clone();
        let notes = by_name["REVIEW"].clone();
        assert_eq!(review.target_path, target_root.join("review"));
        assert_eq!(notes.name_key, "review", "loose name normalization");

        let results = svc.import(&candidates, ImportMode::Copy, "skills");
        let by_import_name: HashMap<&str, &ImportResult> =
            results.iter().map(|r| (r.name.as_str(), r)).collect();
        assert_eq!(by_import_name["deploy"].status, SyncImportStatus::Imported);
        assert_eq!(by_import_name["review"].status, SyncImportStatus::Skipped);
        assert_eq!(
            by_import_name["review"].skip_reason,
            Some(SkipReason::SameNameExists)
        );
        assert_eq!(by_import_name["REVIEW"].status, SyncImportStatus::Skipped);
        assert_eq!(
            by_import_name["REVIEW"].skip_reason,
            Some(SkipReason::SameNameExists)
        );
        // the imported skill landed whole
        assert!(target_root.join("deploy/SKILL.md").exists());
    }

    #[test]
    fn target_exists_beats_same_name() {
        let td = tempfile::tempdir().unwrap();
        let home = td.path();
        write_skill(&home.join(".claude/skills"), "review", "review");
        let target_root = home.join(".okra/skills");
        write_skill(&target_root, "review", "other-name");
        let svc = SettingsSyncService::new(home);
        let candidates = svc.collect_skill_candidates(None);
        let mut cache = HashMap::new();
        assert_eq!(
            svc.skip_reason(&candidates[0], &mut cache),
            Some(SkipReason::TargetExists)
        );
    }

    #[test]
    fn command_discovery_names_and_metadata() {
        let td = tempfile::tempdir().unwrap();
        let home = td.path();
        let commands = home.join(".claude/commands");
        std::fs::create_dir_all(commands.join("nested")).unwrap();
        std::fs::write(
            commands.join("review.md"),
            "---\ndescription: review code\nargument-hint: <file>\n---\nDo review.\n",
        )
        .unwrap();
        std::fs::write(commands.join("nested/deep.md"), "plain body\n").unwrap();
        // dot entries skipped
        std::fs::create_dir_all(commands.join(".hidden")).unwrap();
        std::fs::write(commands.join(".hidden/skip.md"), "x").unwrap();

        let svc = SettingsSyncService::new(home);
        let candidates = svc.collect_command_candidates(None);
        let names: Vec<&str> = candidates.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["/nested/deep", "/review"]);
        let review = candidates.iter().find(|c| c.name == "/review").unwrap();
        assert_eq!(review.description.as_deref(), Some("review code"));
        assert_eq!(review.argument_hint.as_deref(), Some("<file>"));
        // commands never selected by default
        let discovery = svc.discover_commands(None);
        assert!(discovery
            .agents
            .iter()
            .all(|a| !a.selected_by_default));
    }

    #[test]
    fn symlink_mode_links_instead_of_copies() {
        let td = tempfile::tempdir().unwrap();
        let home = td.path();
        write_skill(&home.join(".claude/skills"), "review", "review");
        let svc = SettingsSyncService::new(home);
        let candidates = svc.collect_skill_candidates(None);
        let results = svc.import(&candidates, ImportMode::Symlink, "skills");
        assert!(results.iter().all(|r| r.status == SyncImportStatus::Imported));
        let target = home.join(".okra/skills/review");
        let meta = std::fs::symlink_metadata(&target).unwrap();
        assert!(meta.file_type().is_symlink(), "symlink mode links the skill dir");
    }

    fn write_plugin(root: &Path, dir_name: &str, name: &str, version: &str) -> PathBuf {
        let dir = root.join(dir_name);
        std::fs::create_dir_all(dir.join(".claude-plugin")).unwrap();
        std::fs::write(
            dir.join(".claude-plugin").join("plugin.json"),
            format!(r#"{{ "name": "{name}", "version": "{version}" }}"#),
        )
        .unwrap();
        std::fs::write(dir.join("index.js"), "module.exports = {};").unwrap();
        dir
    }

    #[test]
    fn plugin_discovery_and_config_registration() {
        let td = tempfile::tempdir().unwrap();
        let home = td.path();
        let plugins = home.join(".claude/plugins");
        write_plugin(&plugins, "formatter", "formatter", "1.2.0");
        write_plugin(&plugins, "linter", "linter", "0.1.0");
        // dot dirs and manifest-less directories are not plugins
        std::fs::create_dir_all(plugins.join(".stash")).unwrap();
        std::fs::create_dir_all(plugins.join("empty")).unwrap();

        let svc = SettingsSyncService::new(home);
        let candidates = svc.collect_plugin_candidates(None);
        assert_eq!(candidates.len(), 2);
        let names: Vec<&str> = candidates.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["formatter", "linter"]);
        assert_eq!(candidates[0].name_key, "formatter@inline");

        let discovery = svc.discover_plugins(None);
        let claude = discovery.agents.first().unwrap();
        assert_eq!(claude.category, "plugins");
        assert_eq!(claude.importable_count, 2);
        assert!(!claude.selected_by_default);

        // import: copies land and get registered in the okra config
        let results = svc.import_plugins(&candidates, ImportMode::Copy, None);
        assert!(results.iter().all(|r| r.status == SyncImportStatus::Imported));
        let config = home.join(".okra/config.json");
        let parsed: Value =
            serde_json::from_str(&std::fs::read_to_string(&config).unwrap()).unwrap();
        let dirs = parsed["plugins"]["dirs"].as_array().unwrap();
        assert_eq!(dirs.len(), 2);
        assert!(dirs[0].as_str().unwrap().ends_with(".okra/plugins/formatter"));
        assert!(home.join(".okra/plugins/formatter/.claude-plugin/plugin.json").exists());

        // a plugin already configured under the same id skips
        let dup = write_plugin(&home.join(".codex/plugins"), "formatter2", "formatter", "9.9");
        let dup_candidates = svc.collect_plugin_candidates(None);
        assert_eq!(dup_candidates.len(), 3);
        let results = svc.import_plugins(
            &dup_candidates
                .iter()
                .filter(|c| c.source_path == dup)
                .cloned()
                .collect::<Vec<_>>(),
            ImportMode::Copy,
            None,
        );
        assert_eq!(results[0].status, SyncImportStatus::Skipped);
        assert_eq!(results[0].skip_reason, Some(SkipReason::SameNameExists));

        // re-import of the originals: targetExists (dirs already there)
        let again = svc.import_plugins(&candidates, ImportMode::Copy, None);
        assert!(again
            .iter()
            .all(|r| r.skip_reason == Some(SkipReason::TargetExists)));
        // and the config was not polluted with duplicate dir entries
        let parsed: Value =
            serde_json::from_str(&std::fs::read_to_string(&config).unwrap()).unwrap();
        assert_eq!(parsed["plugins"]["dirs"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn mcp_import_reads_all_three_formats() {
        let td = tempfile::tempdir().unwrap();
        let home = td.path();
        // claude: wrapped mcpServers json with an external timeout field
        std::fs::create_dir_all(home.join(".claude")).unwrap();
        std::fs::write(
            home.join(".claude/settings.json"),
            r#"{ "mcpServers": { "fetch": { "command": "npx", "args": ["fetch"], "timeout": 30 } } }"#,
        )
        .unwrap();
        // codex: TOML mcp_servers with startup_timeout_sec
        std::fs::create_dir_all(home.join(".codex")).unwrap();
        std::fs::write(
            home.join(".codex/config.toml"),
            "[mcp_servers.docs]\ncommand = \"docs-server\"\nargs = [\"--port\", \"9\"]\nstartup_timeout_sec = 5\n",
        )
        .unwrap();
        // openCode: array-form command + local/remote vocabulary
        std::fs::create_dir_all(home.join(".config/opencode")).unwrap();
        std::fs::write(
            home.join(".config/opencode/opencode.json"),
            r#"{ "mcp": { "tools": { "type": "local", "command": ["bun", "run", "tools"] } } }"#,
        )
        .unwrap();

        let svc = SettingsSyncService::new(home);
        let candidates = svc.collect_mcp_candidates(None);
        let by_name: HashMap<&str, &McpCandidate> =
            candidates.iter().map(|c| (c.name.as_str(), c)).collect();
        assert_eq!(candidates.len(), 3);
        // claude format: wrapped + timeout stripped
        let fetch = by_name["fetch"];
        assert_eq!(fetch.agent, SyncAgent::ClaudeCode);
        assert!(fetch.config.get("timeout").is_none());
        assert_eq!(fetch.config["command"], "npx");
        // codex format: toml table + startup_timeout_sec stripped
        let docs = by_name["docs"];
        assert_eq!(docs.agent, SyncAgent::CodexCli);
        assert_eq!(docs.config["command"], "docs-server");
        assert_eq!(docs.config["args"], serde_json::json!(["--port", "9"]));
        assert!(docs.config.get("startup_timeout_sec").is_none());
        // openCode format: array command normalized to command + args, local→stdio
        let tools = by_name["tools"];
        assert_eq!(tools.config["type"], "stdio");
        assert_eq!(tools.config["command"], "bun");
        assert_eq!(tools.config["args"], serde_json::json!(["run", "tools"]));
        assert_eq!(
            candidates.iter().filter(|c| c.agent == SyncAgent::CodexCli).count(),
            1
        );

        // discovery + import into the okra config
        let discovery = svc.discover_mcp(None);
        let total_importable: usize = discovery
            .agents
            .iter()
            .map(|a| a.importable_count)
            .sum();
        assert_eq!(total_importable, 3);
        let results = svc.import_mcp(&candidates, None);
        assert!(results.iter().all(|r| r.status == SyncImportStatus::Imported));
        let config: Value = serde_json::from_str(
            &std::fs::read_to_string(home.join(".okra/config.json")).unwrap(),
        )
        .unwrap();
        let servers = &config["mcp"]["servers"];
        assert_eq!(servers["fetch"]["command"], "npx");
        assert_eq!(servers["docs"]["command"], "docs-server");
        assert_eq!(servers["tools"]["type"], "stdio");
        // re-import: all sameNameExists
        let again = svc.import_mcp(&candidates, None);
        assert!(again
            .iter()
            .all(|r| r.skip_reason == Some(SkipReason::SameNameExists)));
    }

    #[test]
    fn mcp_import_preserves_unrelated_config_keys() {
        let td = tempfile::tempdir().unwrap();
        let home = td.path();
        let config = home.join(".okra/config.json");
        std::fs::create_dir_all(home.join(".okra")).unwrap();
        std::fs::create_dir_all(home.join(".agents")).unwrap();
        std::fs::write(
            &config,
            r#"{ "provider": "openai", "mcp": { "servers": { "keep": { "command": "k" } } } }"#,
        )
        .unwrap();
        std::fs::write(
            home.join(".agents/mcp.json"),
            r#"{ "mcpServers": { "new": { "command": "n" } } }"#,
        )
        .unwrap();
        let svc = SettingsSyncService::new(home);
        let results = svc.import_mcp(&svc.collect_mcp_candidates(None), None);
        assert!(results.iter().all(|r| r.status == SyncImportStatus::Imported));
        let parsed: Value = serde_json::from_str(&std::fs::read_to_string(&config).unwrap()).unwrap();
        assert_eq!(parsed["provider"], "openai", "unrelated top-level keys survive");
        assert_eq!(parsed["mcp"]["servers"]["keep"]["command"], "k");
        assert_eq!(parsed["mcp"]["servers"]["new"]["command"], "n");
    }
}
