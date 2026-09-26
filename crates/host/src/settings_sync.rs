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
        _ => unreachable!("source tables only contain known agents"),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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
}
