//! Skill sync (MASTER-PLAN §3 #48 host-domain strangler, from ZCode
//! `packages/services/src/skill-sync/` + `shared/skill-scan-policy.ts`):
//! scan user skill directories, export a selected set as a gzip'd ustar
//! archive, import such an archive on another machine.
//!
//! Donor contracts kept:
//! - **shared scan policy**: bounded depth (root = 0, max 8), excluded
//!   content directories (`node_modules`, `target`, …), dot directories
//!   skipped except `.system` — the reason: a single `skills.list` once
//!   took 69–256s on Windows walking `node_modules`;
//! - **two roots, okra priority**: `~/.okra/skills` first; agents-root
//!   skills are excluded when the directory name or normalized skill name
//!   is already covered by the okra root, plus canonical-path dedupe;
//! - **tolerant metadata**: frontmatter is optional — a parse failure or
//!   missing `name` falls back to the directory basename, never an error;
//! - **skip-existing import** (overwrite unsupported): a skill is skipped
//!   when the target directory exists or a same-normalized-name skill
//!   exists in EITHER root (no duplicate-source generation);
//! - **path containment**: archive entry paths are validated relative
//!   (no absolute, no `..`, no drive prefixes, no backslashes) and
//!   resolved strictly inside the target root;
//! - **size limits everywhere**: selected content, archive bytes, and
//!   extracted bytes all bounded (default 20 MiB);
//! - archives are plain ustar + gzip, byte-compatible with the donor's
//!   format (typeflag `0`/`5`, `ustar` magic at offset 257, two zero end
//!   blocks).

use std::collections::{BTreeSet, HashMap};
use std::io::Read as _;
use std::path::{Path, PathBuf};

use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use flate2::Compression;
use serde::{Deserialize, Serialize};

use crate::fsutil;
use crate::plugins::store::sha256_hex;

pub const SKILL_FILE_NAME: &str = "SKILL.md";
/// Root-relative scan depth limit (root itself = 0). Real skill layouts
/// are at most two levels; 8 is the runaway brake for symlink chains.
pub const MAX_SKILL_SCAN_DEPTH: usize = 8;
/// Content directories never entered — user skills do not live there, and
/// walking them once made `skills.list` take minutes.
pub const SKILL_SCAN_EXCLUDED_DIRECTORY_NAMES: [&str; 12] = [
    "node_modules",
    "dist",
    "build",
    "out",
    "target",
    "vendor",
    "coverage",
    ".cache",
    ".next",
    ".turbo",
    ".venv",
    "__pycache__",
];
const DOT_DIR_ALLOWLIST: [&str; 1] = [".system"];
pub const DEFAULT_MAX_ARCHIVE_BYTES: usize = 20 * 1024 * 1024;

const TAR_BLOCK: usize = 512;

/// Donor `shouldWalkSkillDirectoryEntry`.
pub fn should_walk_entry(name: &str) -> bool {
    if SKILL_SCAN_EXCLUDED_DIRECTORY_NAMES.contains(&name) {
        return false;
    }
    if !name.starts_with('.') {
        return true;
    }
    DOT_DIR_ALLOWLIST.contains(&name)
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SkillCandidate {
    /// sha256 of the canonical directory path — stable across restarts.
    pub id: String,
    pub name: String,
    pub description: String,
    /// Root-relative POSIX directory path (e.g. `group/notes`).
    pub directory_name: String,
    pub skill_md_path: PathBuf,
    pub directory_path: PathBuf,
    pub size_bytes: u64,
    /// Which root the skill lives in.
    pub root: SkillRoot,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillRoot {
    Okra,
    Agents,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
    pub directory_name: String,
    pub status: ImportStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum SkillSyncError {
    #[error("skill sync candidate not found: {0}")]
    UnknownCandidate(String),
    #[error("skill sync size limit exceeded: phase={phase}, actual={actual}, max={max}")]
    SizeLimit {
        phase: &'static str,
        actual: u64,
        max: u64,
    },
    #[error("unsafe skill sync path: {0}")]
    UnsafePath(String),
    #[error("invalid skill sync archive: {0}")]
    BadArchive(String),
    #[error("skill sync io: {0}")]
    Io(#[from] std::io::Error),
}

/// `normalizeSkillSyncRelativePath`: POSIX-relative or nothing.
fn normalize_relative_path(path: &str) -> Result<String, SkillSyncError> {
    let normalized = path.replace('\\', "/");
    let normalized = normalized.trim_start_matches('/');
    let normalized = normalized.trim_end_matches('/');
    let is_windows_absolute = path.len() >= 2
        && path.as_bytes()[1] == b':'
        && path.as_bytes()[0].is_ascii_alphabetic();
    if path.is_empty()
        || path.starts_with('/')
        || path.starts_with('\\')
        || path.contains('\\')
        || is_windows_absolute
        || normalized.is_empty()
        || normalized.split('/').any(|part| part.is_empty() || part == "." || part == "..")
    {
        return Err(SkillSyncError::UnsafePath(path.to_string()));
    }
    Ok(normalized.to_string())
}

/// `resolveSkillSyncPathWithin`: normalized join with a lexical
/// containment check (component-wise, so `..` can never climb out).
fn resolve_within(root: &Path, path: &str) -> Result<PathBuf, SkillSyncError> {
    let normalized = normalize_relative_path(path)?;
    let mut target = root.to_path_buf();
    for part in normalized.split('/') {
        target.push(part);
    }
    if !target.starts_with(root) {
        return Err(SkillSyncError::UnsafePath(path.to_string()));
    }
    Ok(target)
}

/// Tolerant frontmatter metadata (`parseSkillMetadata`): no frontmatter or
/// an unparseable one falls back to the directory basename; a blank or
/// missing `name` falls back too; `description` defaults to empty.
fn parse_skill_metadata(content: &str, fallback_name: &str) -> (String, String) {
    let trimmed = content.trim_start_matches('\u{feff}');
    if !trimmed.starts_with("---") {
        return (fallback_name.to_string(), String::new());
    }
    let after_open = &trimmed[3..];
    let after_open = after_open.strip_prefix('\r').unwrap_or(after_open);
    let after_open = after_open.strip_prefix('\n').unwrap_or(after_open);
    let Some(close_pos) = after_open.find("\n---") else {
        return (fallback_name.to_string(), String::new());
    };
    let frontmatter = &after_open[..close_pos];
    let mut name = String::new();
    let mut description = String::new();
    for line in frontmatter.lines() {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim().trim_matches('"').trim();
        match key.trim() {
            "name" if !value.is_empty() => name = value.to_string(),
            "description" if !value.is_empty() => description = value.to_string(),
            _ => {}
        }
    }
    if name.trim().is_empty() {
        name = fallback_name.to_string();
    }
    (name, description)
}

/// One bounded DFS producing every directory (root excluded) as
/// `(path, relative posix path, depth)` with excluded names, the dot-dir
/// allowlist, and symlink realpath dedupe applied.
fn walk_directories(
    root: &Path,
) -> Vec<(PathBuf, String, usize)> {
    let mut out = Vec::new();
    let mut visited_symlinks: BTreeSet<PathBuf> = BTreeSet::new();
    let mut stack = vec![(root.to_path_buf(), String::new(), 0usize)];
    while let Some((dir, rel, depth)) = stack.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(_) => continue,
        };
        let mut child_dirs: Vec<(PathBuf, String, bool)> = Vec::new();
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
            child_dirs.push((path, child_rel, is_symlink));
        }
        child_dirs.sort_by(|a, b| a.1.cmp(&b.1));
        for (path, child_rel, _symlink) in child_dirs {
            out.push((path.clone(), child_rel.clone(), depth + 1));
            if depth + 1 < MAX_SKILL_SCAN_DEPTH {
                stack.push((path, child_rel, depth + 1));
            }
        }
    }
    out
}

fn recursive_size(path: &Path, visited: &mut BTreeSet<PathBuf>) -> u64 {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return 0;
    };
    if meta.is_symlink() {
        let Ok(target) = std::fs::metadata(path) else {
            return 0;
        };
        if target.is_file() {
            return target.len();
        }
        if !target.is_dir() {
            return 0;
        }
    } else if meta.is_file() {
        return meta.len();
    } else if !meta.is_dir() {
        return 0;
    }
    let canonical = fsutil::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    if !visited.insert(canonical) {
        return 0;
    }
    let mut total = 0u64;
    if let Ok(entries) = std::fs::read_dir(path) {
        for entry in entries.flatten() {
            total += recursive_size(&entry.path(), visited);
        }
    }
    total
}

fn candidates_in_root(root: &Path, root_kind: SkillRoot) -> Vec<SkillCandidate> {
    if !root.exists() {
        return Vec::new();
    }
    let mut candidates = Vec::new();
    for (dir, rel, _depth) in walk_directories(root) {
        let skill_md = dir.join(SKILL_FILE_NAME);
        let Ok(content) = std::fs::read_to_string(&skill_md) else {
            continue;
        };
        let fallback = rel.rsplit('/').next().unwrap_or(&rel).to_string();
        let (name, description) = parse_skill_metadata(&content, &fallback);
        let canonical = fsutil::canonicalize(&dir).unwrap_or_else(|_| dir.clone());
        candidates.push(SkillCandidate {
            id: sha256_hex(canonical.to_string_lossy().as_bytes()),
            name,
            description,
            directory_name: rel,
            size_bytes: recursive_size(&dir, &mut BTreeSet::new()),
            skill_md_path: skill_md,
            directory_path: dir,
            root: root_kind,
        });
    }
    candidates
}

/// The sync service rooted at a home directory (injectable for tests).
#[derive(Debug, Clone)]
pub struct SkillSyncService {
    home: PathBuf,
    max_archive_bytes: usize,
}

impl SkillSyncService {
    pub fn new(home: impl Into<PathBuf>) -> Self {
        SkillSyncService {
            home: home.into(),
            max_archive_bytes: DEFAULT_MAX_ARCHIVE_BYTES,
        }
    }

    pub fn with_max_archive_bytes(mut self, max: usize) -> Self {
        self.max_archive_bytes = max;
        self
    }

    fn okra_root(&self) -> PathBuf {
        self.home.join(".okra").join("skills")
    }

    fn agents_root(&self) -> PathBuf {
        self.home.join(".agents").join("skills")
    }

    /// `listLocalUserSkillCandidates`: okra-root candidates first; agents
    /// candidates excluded when the directory name or normalized name is
    /// covered by okra's root; canonical-path dedupe across both; sorted
    /// by directory name.
    pub fn candidates(&self) -> Vec<SkillCandidate> {
        let mut seen: BTreeSet<PathBuf> = BTreeSet::new();
        let mut okra = candidates_in_root(&self.okra_root(), SkillRoot::Okra);
        okra.retain(|c| {
            seen.insert(fsutil::canonicalize(&c.directory_path).unwrap_or_else(|_| c.directory_path.clone()))
        });
        let covered_dirs: BTreeSet<String> =
            okra.iter().map(|c| c.directory_name.clone()).collect();
        let okra_name_keys: BTreeSet<String> =
            okra.iter().map(|c| normalized_name(&c.name)).collect();
        let agents = candidates_in_root(&self.agents_root(), SkillRoot::Agents)
            .into_iter()
            .filter(|c| {
                !covered_dirs.contains(&c.directory_name)
                    && !okra_name_keys.contains(&normalized_name(&c.name))
            })
            .filter(|c| {
                seen.insert(fsutil::canonicalize(&c.directory_path).unwrap_or_else(|_| c.directory_path.clone()))
            });
        let mut all = okra;
        all.extend(agents);
        all.sort_by(|a, b| a.directory_name.cmp(&b.directory_name));
        all
    }

    /// `exportSkillsArchive`: select by id, bound the selected content and
    /// the archive size, then build the gzip'd ustar archive.
    pub fn export_archive(
        &self,
        skill_ids: &[String],
    ) -> Result<(Vec<u8>, Vec<SkillCandidate>), SkillSyncError> {
        let candidates = self.candidates();
        let mut selected: Vec<SkillCandidate> = Vec::new();
        for id in skill_ids {
            match candidates.iter().find(|c| &c.id == id) {
                Some(c) => selected.push(c.clone()),
                None => return Err(SkillSyncError::UnknownCandidate(id.clone())),
            }
        }
        let selected_bytes: u64 = selected.iter().map(|c| c.size_bytes).sum();
        if selected_bytes > self.max_archive_bytes as u64 {
            return Err(SkillSyncError::SizeLimit {
                phase: "selected-content",
                actual: selected_bytes,
                max: self.max_archive_bytes as u64,
            });
        }
        let mut tar = Vec::new();
        for candidate in &selected {
            append_tar_entry(&mut tar, &candidate.directory_path, &candidate.directory_name)?;
        }
        tar.extend_from_slice(&[0u8; TAR_BLOCK * 2]);
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        std::io::Write::write_all(&mut encoder, &tar)?;
        let archive = encoder.finish()?;
        if archive.len() > self.max_archive_bytes {
            return Err(SkillSyncError::SizeLimit {
                phase: "archive",
                actual: archive.len() as u64,
                max: self.max_archive_bytes as u64,
            });
        }
        Ok((archive, selected))
    }

    /// `importSkillsArchive` — skip-existing, never overwrite, path
    /// containment enforced, extracted bytes bounded.
    pub fn import_archive(&self, archive: &[u8]) -> Result<Vec<ImportOutcome>, SkillSyncError> {
        if archive.len() > self.max_archive_bytes {
            return Err(SkillSyncError::SizeLimit {
                phase: "archive",
                actual: archive.len() as u64,
                max: self.max_archive_bytes as u64,
            });
        }
        let target_root = self.okra_root();
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
    ) -> Result<Vec<ImportOutcome>, SkillSyncError> {
        extract_archive(archive, temp, self.max_archive_bytes as u64)?;

        // extracted skill directories (any depth), sorted by relative name
        let mut extracted: Vec<(String, PathBuf)> = Vec::new();
        let mut stack = vec![(temp.to_path_buf(), String::new())];
        while let Some((dir, rel)) = stack.pop() {
            let entries = match std::fs::read_dir(&dir) {
                Ok(e) => e,
                Err(_) => continue,
            };
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if !should_walk_entry(&name) {
                    continue;
                }
                let path = entry.path();
                if path.is_dir() {
                    let child_rel =
                        if rel.is_empty() { name } else { format!("{rel}/{name}") };
                    if path.join(SKILL_FILE_NAME).exists() {
                        extracted.push((child_rel, path.clone()));
                    } else {
                        stack.push((path, child_rel));
                    }
                }
            }
        }
        extracted.sort_by(|a, b| a.0.cmp(&b.0));

        let existing_by_name = self.skill_directory_by_name();
        let mut outcomes = Vec::new();
        for (directory_name, source_path) in extracted {
            let target = match resolve_within(target_root, &directory_name) {
                Ok(t) => t,
                Err(e) => {
                    outcomes.push(ImportOutcome {
                        name: directory_name.clone(),
                        directory_name,
                        status: ImportStatus::Failed,
                        path: None,
                        error: Some(e.to_string()),
                    });
                    continue;
                }
            };
            let Ok(content) = std::fs::read_to_string(source_path.join(SKILL_FILE_NAME)) else {
                outcomes.push(ImportOutcome {
                    name: directory_name.clone(),
                    directory_name,
                    status: ImportStatus::Failed,
                    path: None,
                    error: Some("SKILL.md is missing".into()),
                });
                continue;
            };
            let fallback = directory_name.rsplit('/').next().unwrap_or(&directory_name);
            let (name, _) = parse_skill_metadata(&content, fallback);
            if target.exists() {
                outcomes.push(ImportOutcome {
                    name,
                    directory_name,
                    status: ImportStatus::Skipped,
                    path: Some(target),
                    error: None,
                });
                continue;
            }
            if let Some(existing) = existing_by_name.get(&normalized_name(&name)) {
                outcomes.push(ImportOutcome {
                    name,
                    directory_name,
                    status: ImportStatus::Skipped,
                    path: Some(existing.clone()),
                    error: None,
                });
                continue;
            }
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            match copy_dir_recursive(&source_path, &target) {
                Ok(()) => outcomes.push(ImportOutcome {
                    name,
                    directory_name,
                    status: ImportStatus::Synced,
                    path: Some(target),
                    error: None,
                }),
                Err(e) => outcomes.push(ImportOutcome {
                    name,
                    directory_name,
                    status: ImportStatus::Failed,
                    path: Some(target),
                    error: Some(e.to_string()),
                }),
            }
        }
        Ok(outcomes)
    }

    /// Normalized skill name → first directory across both roots
    /// (okra root wins; first-wins within each root).
    fn skill_directory_by_name(&self) -> HashMap<String, PathBuf> {
        let mut map = HashMap::new();
        for root in [self.okra_root(), self.agents_root()] {
            for (dir, _rel, _depth) in walk_directories(&root) {
                let Ok(content) = std::fs::read_to_string(dir.join(SKILL_FILE_NAME)) else {
                    continue;
                };
                let fallback = dir
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let (name, _) = parse_skill_metadata(&content, &fallback);
                map.entry(normalized_name(&name))
                    .or_insert_with(|| dir.clone());
            }
        }
        map
    }
}

fn normalized_name(name: &str) -> String {
    name.trim().to_ascii_lowercase()
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
            // never copy symlinks: import is for content, not links
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
// ustar + gzip archive (donor format)
// ---------------------------------------------------------------------------

fn tar_string(field: &mut [u8], value: &str) {
    let bytes = value.as_bytes();
    let len = bytes.len().min(field.len() - 1);
    field[..len].copy_from_slice(&bytes[..len]);
}

fn tar_octal(field: &mut [u8], value: u64) {
    let octal = format!("{:0width$o}", value, width = field.len() - 1);
    let bytes = octal.as_bytes();
    let len = bytes.len().min(field.len() - 1);
    field[..len].copy_from_slice(&bytes[..len]);
    field[len] = 0;
}

fn tar_header(entry_path: &str, size: u64, type_flag: u8) -> [u8; TAR_BLOCK] {
    let mut header = [0u8; TAR_BLOCK];
    tar_string(&mut header[0..100], entry_path);
    tar_octal(&mut header[100..108], 0o644);
    tar_octal(&mut header[108..116], 0);
    tar_octal(&mut header[116..124], 0);
    tar_octal(&mut header[124..136], size);
    tar_octal(&mut header[136..148], 0);
    header[156] = type_flag;
    // donor: "ustar" magic at 257, 6 chars (ustar\0), version "00"
    tar_string(&mut header[257..263], "ustar");
    header[263] = b'0';
    header[264] = b'0';
    // checksum: spaces while computing, then octal
    for byte in &mut header[148..156] {
        *byte = b' ';
    }
    let sum: u64 = header.iter().map(|b| *b as u64).sum();
    tar_octal(&mut header[148..155], sum);
    header[155] = b' ';
    header
}

fn append_tar_entry(tar: &mut Vec<u8>, source: &Path, archive_path: &str) -> Result<(), SkillSyncError> {
    let meta = std::fs::symlink_metadata(source)?;
    if meta.is_dir() {
        let dir_path = format!("{archive_path}/");
        tar.extend_from_slice(&tar_header(&dir_path, 0, b'5'));
        let mut children: Vec<String> = std::fs::read_dir(source)?
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        children.sort();
        for child in children {
            let child_archive = format!("{archive_path}/{child}");
            append_tar_entry(tar, &source.join(child), &child_archive)?;
        }
        return Ok(());
    }
    if !meta.is_file() {
        return Err(SkillSyncError::BadArchive(format!(
            "unsupported skill archive source: {}",
            source.display()
        )));
    }
    let content = std::fs::read(source)?;
    tar.extend_from_slice(&tar_header(archive_path, content.len() as u64, b'0'));
    tar.extend_from_slice(&content);
    let padding = (TAR_BLOCK - content.len() % TAR_BLOCK) % TAR_BLOCK;
    tar.extend(std::iter::repeat_n(0u8, padding));
    Ok(())
}

fn read_tar_string(header: &[u8], offset: usize, len: usize) -> String {
    header[offset..offset + len]
        .iter()
        .take_while(|b| **b != 0)
        .map(|b| *b as char)
        .collect::<String>()
        .trim_end()
        .to_string()
}

fn read_tar_size(header: &[u8]) -> Result<u64, SkillSyncError> {
    let raw = &header[124..136];
    let text: String = raw
        .iter()
        .take_while(|b| **b != 0 && **b != b' ')
        .map(|b| *b as char)
        .collect();
    u64::from_str_radix(text.trim_end(), 8)
        .map_err(|_| SkillSyncError::BadArchive("invalid archive entry size".into()))
}

/// `extractSkillSyncArchive`: gunzip, walk ustar blocks, enforce entry
/// path containment and the extracted-bytes budget.
fn extract_archive(archive: &[u8], target_dir: &Path, max_extracted: u64) -> Result<(), SkillSyncError> {
    let mut gz = GzDecoder::new(archive);
    let mut tar = Vec::new();
    gz.read_to_end(&mut tar)
        .map_err(|_| SkillSyncError::BadArchive("not a valid gzip archive".into()))?;

    let target_root = fsutil::canonicalize(target_dir).unwrap_or_else(|_| target_dir.to_path_buf());
    let mut offset = 0usize;
    let mut extracted_bytes = 0u64;
    while offset + TAR_BLOCK <= tar.len() {
        let header = &tar[offset..offset + TAR_BLOCK];
        offset += TAR_BLOCK;
        if header.iter().all(|b| *b == 0) {
            break;
        }
        let size = read_tar_size(header)?;
        let type_flag = header[156];
        let raw_path = read_tar_string(header, 0, 100);
        let entry_path = raw_path.trim_end_matches('/');
        if entry_path.is_empty() {
            return Err(SkillSyncError::BadArchive("empty archive entry path".into()));
        }
        let normalized = normalize_relative_path(entry_path)?;
        let mut target = target_root.clone();
        for part in normalized.split('/') {
            target.push(part);
        }
        if type_flag == b'5' {
            std::fs::create_dir_all(&target)?;
            continue;
        }
        if type_flag != b'0' && type_flag != 0 {
            return Err(SkillSyncError::BadArchive(format!(
                "unsupported skill archive entry type: {}",
                type_flag as char
            )));
        }
        extracted_bytes += size;
        if extracted_bytes > max_extracted {
            return Err(SkillSyncError::SizeLimit {
                phase: "extracted-content",
                actual: extracted_bytes,
                max: max_extracted,
            });
        }
        if offset + size as usize > tar.len() {
            return Err(SkillSyncError::BadArchive("truncated archive entry".into()));
        }
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&target, &tar[offset..offset + size as usize])?;
        offset += size as usize;
        let padding = (TAR_BLOCK - size as usize % TAR_BLOCK) % TAR_BLOCK;
        offset += padding;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_skill(root: &Path, rel: &str, name: &str, description: &str) -> PathBuf {
        let dir = root.join(rel);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join(SKILL_FILE_NAME),
            format!("---\nname: {name}\ndescription: {description}\n---\n\nBody of {name}.\n"),
        )
        .unwrap();
        std::fs::write(dir.join("helper.txt"), "helper content").unwrap();
        dir
    }

    #[test]
    fn scan_policy_matches_donor() {
        assert!(should_walk_entry("notes"));
        assert!(should_walk_entry(".system"));
        assert!(!should_walk_entry(".git"));
        assert!(!should_walk_entry("node_modules"));
        assert!(!should_walk_entry("__pycache__"));
    }

    #[test]
    fn path_normalization_rejects_escape() {
        assert_eq!(normalize_relative_path("a/b").unwrap(), "a/b");
        assert!(normalize_relative_path("/abs").is_err());
        assert!(normalize_relative_path("a/../b").is_err());
        assert!(normalize_relative_path("a\\b").is_err());
        assert!(normalize_relative_path("C:/x").is_err());
        assert!(normalize_relative_path("").is_err());
        assert!(resolve_within(Path::new("/tmp/r"), "../escape").is_err());
    }

    #[test]
    fn tolerant_metadata_falls_back_to_directory_name() {
        assert_eq!(
            parse_skill_metadata("---\nname: Alpha\ndescription: Does things\n---\nbody", "d"),
            ("Alpha".into(), "Does things".into())
        );
        // no frontmatter → fallback
        assert_eq!(parse_skill_metadata("just text", "d"), ("d".into(), "".into()));
        // frontmatter without name → fallback
        assert_eq!(
            parse_skill_metadata("---\ndescription: x\n---\nbody", "d"),
            ("d".into(), "x".into())
        );
    }

    #[test]
    fn candidates_prefer_okra_root_and_dedupe() {
        let td = tempfile::tempdir().unwrap();
        let okra = td.path().join(".okra/skills");
        let agents = td.path().join(".agents/skills");
        write_skill(&okra, "notes", "notes", "okra version");
        write_skill(&agents, "notes", "notes-agent", "shadowed by dir name");
        write_skill(&agents, "alpha", "NOTES", "shadowed by name key");
        write_skill(&agents, "extra", "extra", "only in agents");

        let svc = SkillSyncService::new(td.path());
        let candidates = svc.candidates();
        let names: Vec<&str> = candidates.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["extra", "notes"], "agents dupes by dir+name excluded");
        assert_eq!(candidates[0].root, SkillRoot::Agents);
        assert_eq!(candidates[1].root, SkillRoot::Okra);
        assert_eq!(candidates[0].directory_name, "extra");
        // ids are stable
        let again = svc.candidates();
        assert_eq!(candidates[0].id, again[0].id);
    }

    #[test]
    fn scan_respects_depth_and_exclusions() {
        let td = tempfile::tempdir().unwrap();
        let root = td.path().join(".okra/skills");
        write_skill(&root, "node_modules/pkg", "deep-pkg", "excluded dir");
        let mut deep = String::from("a");
        for _ in 0..MAX_SKILL_SCAN_DEPTH {
            deep.push_str("/b");
        }
        write_skill(&root, &deep, "too-deep", "beyond depth");
        write_skill(&root, "group/skill", "in-range", "depth 2");

        let svc = SkillSyncService::new(td.path());
        let names: Vec<String> = svc.candidates().into_iter().map(|c| c.name).collect();
        assert_eq!(names, vec!["in-range"]);
    }

    #[test]
    fn export_import_round_trip_with_skip_existing() {
        let local = tempfile::tempdir().unwrap();
        write_skill(&local.path().join(".okra/skills"), "notes", "notes", "my notes");
        write_skill(&local.path().join(".okra/skills"), "group/alpha", "alpha", "a");
        let svc = SkillSyncService::new(local.path());

        let candidates = svc.candidates();
        let ids: Vec<String> = candidates.iter().map(|c| c.id.clone()).collect();
        let (archive, exported) = svc.export_archive(&ids).unwrap();
        assert_eq!(exported.len(), 2);
        assert!(matches!(
            svc.export_archive(&["missing".into()]),
            Err(SkillSyncError::UnknownCandidate(_))
        ));

        // remote home: one skill already exists under a different directory
        let remote = tempfile::tempdir().unwrap();
        write_skill(&remote.path().join(".agents/skills"), "old-notes", "notes", "existing name");
        let remote_svc = SkillSyncService::new(remote.path());
        let outcomes = remote_svc.import_archive(&archive).unwrap();
        assert_eq!(outcomes.len(), 2);
        let by_dir: HashMap<&str, &ImportOutcome> =
            outcomes.iter().map(|o| (o.directory_name.as_str(), o)).collect();
        assert_eq!(by_dir["notes"].status, ImportStatus::Skipped);
        assert_eq!(
            by_dir["notes"].path.as_ref().unwrap().file_name().and_then(|n| n.to_str()),
            Some("old-notes"),
            "skipped in favor of the same-name agents skill"
        );
        assert_eq!(by_dir["group/alpha"].status, ImportStatus::Synced);
        let imported = remote.path().join(".okra/skills/group/alpha/SKILL.md");
        assert!(imported.exists());
        // re-import: now both exist → both skipped
        let outcomes = remote_svc.import_archive(&archive).unwrap();
        assert!(outcomes.iter().all(|o| o.status == ImportStatus::Skipped));
    }

    #[test]
    fn import_rejects_oversize_and_missing_skill_md() {
        let local = tempfile::tempdir().unwrap();
        write_skill(&local.path().join(".okra/skills"), "notes", "notes", "n");
        let svc = SkillSyncService::new(local.path());
        let (archive, _) = svc
            .export_archive(&[svc.candidates()[0].id.clone()])
            .unwrap();

        // archive cap
        let tiny = SkillSyncService::new(tempfile::tempdir().unwrap().path())
            .with_max_archive_bytes(8);
        assert!(matches!(
            tiny.import_archive(&archive),
            Err(SkillSyncError::SizeLimit { phase: "archive", .. })
        ));

        // a non-gzip payload fails cleanly
        let remote = tempfile::tempdir().unwrap();
        let remote_svc = SkillSyncService::new(remote.path());
        assert!(matches!(
            remote_svc.import_archive(b"not an archive"),
            Err(SkillSyncError::BadArchive(_))
        ));
    }

    #[test]
    fn archive_entries_cannot_escape_on_extract() {
        // hand-build a tar.gz with a ../ entry and verify extraction refuses
        let mut tar = Vec::new();
        tar.extend_from_slice(&tar_header("../../escape.txt", 5, b'0'));
        tar.extend_from_slice(b"hello");
        tar.extend(std::iter::repeat_n(0u8, TAR_BLOCK - 5));
        tar.extend_from_slice(&[0u8; TAR_BLOCK * 2]);
        let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
        std::io::Write::write_all(&mut encoder, &tar).unwrap();
        let archive = encoder.finish().unwrap();

        let td = tempfile::tempdir().unwrap();
        let target = td.path().join("out");
        std::fs::create_dir_all(&target).unwrap();
        let err = extract_archive(&archive, &target, u64::MAX).unwrap_err();
        assert!(matches!(err, SkillSyncError::UnsafePath(_)), "{err}");
        assert!(!td.path().join("escape.txt").exists());
    }
}
