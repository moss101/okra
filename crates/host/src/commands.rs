//! Host domain: user/slash commands — row-48 re-homed from ZCode
//! `packages/services/src/commands/` (commandsService.ts + commandFileParser.ts).
//!
//! Markdown command files with an optional frontmatter (`description`,
//! `argument-hint`; unknown keys are preserved on rewrite), named by their
//! path relative to a commands root (`deploy/staging.md` → `/deploy/staging`).
//! The load-bearing semantics that transfer verbatim:
//!
//! - **Discovery order** (list): ALL workspace directory sources merge before
//!   ALL user directory sources, and within a scope the `.zcode/commands`
//!   source is strong-priority — if it yields any command, the `.agents`
//!   source is skipped for that scope. Names dedupe first-wins.
//! - **Enable overrides** live in `~/.zcode/cli/config.json` under
//!   `command: { <file path>: { enable: false } }`; default enabled; write
//!   clears the override, rename MIGRATES it (a renamed command must not
//!   resurrect), delete clears it, and the empty `command` key is removed.
//! - **Plugin commands** are read-only sources: manifest `commands` paths
//!   resolve strictly inside the plugin root (absolute/traversal rejected),
//!   a present-but-unresolvable `commands` key never falls back to the
//!   default `commands/` dir, suppressed builtins are skipped, and per-file
//!   enable composes with plugin enable.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde_json::{json, Map, Value};

use crate::fsutil::normalize_lexical;

pub const FILE_EXTENSION: &str = ".md";
pub const NAMESPACE_SEPARATOR: char = '/';
pub const AGENT_SOURCE: &str = "zcodeAgent";
/// Strong-priority directory source: wins its scope when non-empty.
pub const DIR_SOURCE_ZCODE: &str = "zcode";
pub const DIR_SOURCE_AGENTS: &str = "agents";
const USER_SEGMENTS: [&str; 2] = [".zcode", "commands"];
const AGENTS_SEGMENTS: [&str; 2] = [".agents", "commands"];
const OFFICIAL_MARKETPLACE: &str = "zcode-plugins-official";
const MANIFEST_PATHS: [&str; 3] = [
    ".zcode-plugin/plugin.json",
    ".claude-plugin/plugin.json",
    ".codex-plugin/plugin.json",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedCommandFile {
    /// `/name` or `/namespace/name` (basename or path relative to the root).
    pub name: String,
    pub prompt: String,
    /// Body exactly as split (no trim) — what a rewrite of the prompt keeps.
    pub content: String,
    pub description: Option<String>,
    pub argument_hint: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandScope {
    Global,
    Project,
}

impl CommandScope {
    fn as_str(self) -> &'static str {
        match self {
            CommandScope::Global => "global",
            CommandScope::Project => "project",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandSource {
    User,
    Plugin,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Command {
    pub id: String,
    pub name: String,
    pub prompt: String,
    pub description: Option<String>,
    pub argument_hint: Option<String>,
    pub file_path: PathBuf,
    pub enabled: bool,
    pub scope: CommandScope,
    pub source: CommandSource,
    pub directory_source: &'static str,
    pub project_path: Option<PathBuf>,
    pub plugin_name: Option<String>,
    pub plugin_marketplace: Option<String>,
    pub plugin_enabled: Option<bool>,
}

#[derive(Debug, Clone, Default)]
pub struct CommandsList {
    pub user_commands: Vec<Command>,
    pub plugin_commands: Vec<Command>,
}

#[derive(Debug)]
pub enum CommandsError {
    Io(std::io::Error),
    AlreadyExists(String),
    MissingWorkspace,
}

impl std::fmt::Display for CommandsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CommandsError::Io(e) => write!(f, "command file io: {e}"),
            CommandsError::AlreadyExists(name) => write!(f, "command file already exists: {name}"),
            CommandsError::MissingWorkspace => {
                write!(f, "missing workspace path for project command")
            }
        }
    }
}

impl std::error::Error for CommandsError {}

impl From<std::io::Error> for CommandsError {
    fn from(e: std::io::Error) -> Self {
        CommandsError::Io(e)
    }
}

// ---------------------------------------------------------------------------
// Markdown command file parser (commandFileParser.ts)
// ---------------------------------------------------------------------------

fn split_markdown(content: &str) -> (Vec<&str>, Vec<&str>) {
    let lines: Vec<&str> = content.split('\n').collect();
    let start = lines.iter().position(|l| l.trim() == "---");
    let Some(start) = start else { return (Vec::new(), lines) };
    let end = lines
        .iter()
        .enumerate()
        .skip_while(|(i, _)| *i <= start)
        .find(|(_, l)| l.trim() == "---")
        .map(|(i, _)| i);
    let Some(end) = end else { return (Vec::new(), lines) };
    (lines[start + 1..end].to_vec(), lines[end + 1..].to_vec())
}

/// Frontmatter key of a line: indented lines are continuations (no key);
/// keys match `[A-Za-z0-9_-]+ :` and compare case-insensitively.
fn read_frontmatter_key(line: &str) -> Option<String> {
    if line.starts_with(' ') || line.starts_with('\t') {
        return None;
    }
    let trimmed = line.trim_start();
    let key_len = trimmed
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '-'))
        .unwrap_or(trimmed.len());
    let (key, rest) = trimmed.split_at(key_len);
    if key.is_empty() || !rest.trim_start().starts_with(':') {
        return None;
    }
    Some(key.to_ascii_lowercase())
}

fn read_frontmatter_multiline_value(lines: &[&str], target_key: &str) -> Option<String> {
    let mut collected: Vec<&str> = Vec::new();
    let mut collecting = false;
    for line in lines {
        if let Some(key) = read_frontmatter_key(line) {
            if collecting {
                break;
            }
            if key == target_key {
                collecting = true;
                // value = everything after the first colon (colons in the
                // value survive — TS splits on ':' and re-joins the tail)
                if let Some((_, value)) = line.split_once(':') {
                    let value = value.trim();
                    if !value.is_empty() {
                        collected.push(value);
                    }
                }
            }
            continue;
        }
        if collecting && (line.starts_with(' ') || line.starts_with('\t')) {
            collected.push(line.trim());
        }
    }
    let value = collected.join(" ").trim().to_string();
    if value.is_empty() { None } else { Some(value) }
}

fn preserve_frontmatter_lines(existing: Option<&str>, replaced_keys: &[&str]) -> Vec<String> {
    let mut preserved = Vec::new();
    let mut skip_until_next_key = false;
    for line in split_markdown(existing.unwrap_or_default()).0 {
        if let Some(key) = read_frontmatter_key(line) {
            // a replaced key swallows its whole block, continuation lines
            // included (they carry no key)
            skip_until_next_key = replaced_keys.contains(&key.as_str());
        }
        if !skip_until_next_key {
            preserved.push(line.to_string());
        }
    }
    preserved
}

fn basename_cross_platform(file_path: &str) -> &str {
    file_path.rsplit(['/', '\\']).next().unwrap_or("")
}

pub fn parse_command_file(content: &str, file_path: &str) -> Option<ParsedCommandFile> {
    let (frontmatter, body) = split_markdown(content);
    let file_name = basename_cross_platform(file_path);
    let stem = file_name.strip_suffix(".md").or_else(|| file_name.strip_suffix(".MD"))?;
    let name = format!("/{stem}");
    let prompt = body.join("\n").trim().to_string();
    Some(ParsedCommandFile {
        name,
        prompt,
        content: body.join("\n"),
        description: read_frontmatter_multiline_value(&frontmatter, "description"),
        argument_hint: read_frontmatter_multiline_value(&frontmatter, "argument-hint"),
    })
}

pub fn generate_command_file_content(
    prompt: &str,
    description: Option<&str>,
    argument_hint: Option<&str>,
    existing_content: Option<&str>,
) -> String {
    let replaced = ["description", "argument-hint"];
    let mut frontmatter = preserve_frontmatter_lines(existing_content, &replaced);
    if let Some(d) = description.map(str::trim).filter(|d| !d.is_empty()) {
        frontmatter.push(format!("description: {d}"));
    }
    if let Some(h) = argument_hint.map(str::trim).filter(|h| !h.is_empty()) {
        frontmatter.push(format!("argument-hint: {h}"));
    }
    if frontmatter.is_empty() {
        return prompt.to_string();
    }
    format!("---\n{}\n---\n\n{}", frontmatter.join("\n"), prompt)
}

/// `/deploy/staging.md` relative to its root → `/deploy/staging`.
fn command_name(root: &Path, file_path: &Path) -> String {
    let rel = file_path
        .strip_prefix(root)
        .unwrap_or(file_path)
        .to_string_lossy()
        .into_owned();
    let without_ext = rel
        .strip_suffix(FILE_EXTENSION)
        .or_else(|| {
            rel.strip_suffix(".MD")
        })
        .unwrap_or(&rel);
    let joined: Vec<&str> = without_ext
        .split(['/', '\\'])
        .filter(|s| !s.is_empty())
        .collect();
    format!("/{}", joined.join("/"))
}

// ---------------------------------------------------------------------------
// CLI config enable overrides (commandsService.ts)
// ---------------------------------------------------------------------------

fn is_record(value: &Value) -> bool {
    value.is_object()
}

fn read_string_array(value: &Value) -> Vec<String> {
    value
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

fn read_boolean_record(value: &Value) -> BTreeMap<String, bool> {
    let mut out = BTreeMap::new();
    if let Some(map) = value.as_object() {
        for (k, v) in map {
            if let Some(b) = v.as_bool() {
                out.insert(k.clone(), b);
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Service
// ---------------------------------------------------------------------------

pub struct CommandsService {
    home: PathBuf,
}

impl CommandsService {
    pub fn new(home: impl Into<PathBuf>) -> Self {
        CommandsService { home: home.into() }
    }

    fn cli_config_path(&self) -> PathBuf {
        self.home.join(".zcode").join("cli").join("config.json")
    }

    fn read_user_cli_config(&self) -> Map<String, Value> {
        let Ok(content) = std::fs::read_to_string(self.cli_config_path()) else {
            return Map::new();
        };
        match serde_json::from_str::<Value>(&content) {
            Ok(v) if is_record(&v) => v.as_object().cloned().unwrap_or_default(),
            // a corrupt config reads as empty — never blank the user's file
            // on the next write (we rewrite from the parsed object only when
            // an override changes)
            _ => Map::new(),
        }
    }

    fn write_user_cli_config(&self, config: &Map<String, Value>) -> Result<(), CommandsError> {
        let path = self.cli_config_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut body = serde_json::to_string_pretty(&Value::Object(config.clone()))
            .map_err(|e| CommandsError::Io(std::io::Error::other(e)))?;
        body.push('\n');
        std::fs::write(path, body)?;
        Ok(())
    }

    /// `command: { <file path>: { enable: false } }` → path → enabled.
    fn read_enabled_overrides(&self) -> BTreeMap<String, bool> {
        let mut out = BTreeMap::new();
        let Some(command) = self.read_user_cli_config().get("command").cloned() else {
            return out;
        };
        let Some(map) = command.as_object() else { return out };
        for (path, entry) in map {
            if let Some(enabled) = entry.get("enable").and_then(Value::as_bool) {
                out.insert(path.clone(), enabled);
            }
        }
        out
    }

    /// Enabled=true removes the override key (the default is enabled);
    /// enabled=false writes `{ enable: false }`; an empty `command` object
    /// is dropped from the config entirely.
    fn set_override_in_config(
        config: &Map<String, Value>,
        file_path: &str,
        enabled: bool,
    ) -> Map<String, Value> {
        let mut next = config.clone();
        let mut command: Map<String, Value> = next
            .get("command")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        if enabled {
            command.remove(file_path);
        } else {
            command.insert(file_path.to_string(), json!({ "enable": false }));
        }
        if command.is_empty() {
            next.remove("command");
        } else {
            next.insert("command".into(), Value::Object(command));
        }
        next
    }

    fn override_key(file_path: &Path) -> String {
        normalize_lexical(file_path).to_string_lossy().into_owned()
    }

    fn commands_root(&self, segments: &[&str; 2], workspace: Option<&Path>) -> PathBuf {
        match workspace {
            Some(ws) => ws.join(segments[0]).join(segments[1]),
            None => self.home.join(segments[0]).join(segments[1]),
        }
    }

    // -- discovery ----------------------------------------------------------

    fn discover_root(
        root: &Path,
        directory_source: &'static str,
        scope: CommandScope,
        project_path: Option<&Path>,
        overrides: &BTreeMap<String, bool>,
        commands: &mut Vec<Command>,
    ) {
        if !root.exists() {
            return;
        }
        Self::discover_recursive(
            root, root, directory_source, scope, project_path, overrides, commands,
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn discover_recursive(
        root: &Path,
        dir: &Path,
        directory_source: &'static str,
        scope: CommandScope,
        project_path: Option<&Path>,
        overrides: &BTreeMap<String, bool>,
        commands: &mut Vec<Command>,
    ) {
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                continue;
            }
            let path = entry.path();
            match entry.file_type() {
                Ok(t) if t.is_dir() => Self::discover_recursive(
                    root, &path, directory_source, scope, project_path, overrides, commands,
                ),
                Ok(_) if name.to_ascii_lowercase().ends_with(FILE_EXTENSION) => {
                    let Ok(content) = std::fs::read_to_string(&path) else { continue };
                    let Some(parsed) = parse_command_file(&content, &path.to_string_lossy())
                    else {
                        continue;
                    };
                    let cname = command_name(root, &path);
                    let enabled = overrides
                        .get(&Self::override_key(&path))
                        .copied()
                        .unwrap_or(true);
                    commands.push(Command {
                        id: format!(
                            "{AGENT_SOURCE}:{directory_source}:{scope}:{cname}",
                            scope = scope.as_str(),
                        ),
                        name: cname,
                        prompt: parsed.prompt,
                        description: parsed.description,
                        argument_hint: parsed.argument_hint,
                        file_path: path.clone(),
                        enabled,
                        scope,
                        source: CommandSource::User,
                        directory_source,
                        project_path: project_path.map(Path::to_path_buf),
                        plugin_name: None,
                        plugin_marketplace: None,
                        plugin_enabled: None,
                    });
                }
                _ => {}
            }
        }
    }

    /// One scope's directory sources in priority order: `.zcode/commands`
    /// is strong-priority — if it yields any command, `.agents/commands`
    /// does not participate in that scope.
    fn discover_scope(
        base: &Path,
        scope: CommandScope,
        project_path: Option<&Path>,
        overrides: &BTreeMap<String, bool>,
        commands: &mut Vec<Command>,
    ) {
        let zcode_root = base.join(USER_SEGMENTS[0]).join(USER_SEGMENTS[1]);
        let before = commands.len();
        Self::discover_root(&zcode_root, DIR_SOURCE_ZCODE, scope, project_path, overrides, commands);
        let found_zcode = commands.len() > before;
        if !found_zcode {
            let agents_root = base.join(AGENTS_SEGMENTS[0]).join(AGENTS_SEGMENTS[1]);
            Self::discover_root(&agents_root, DIR_SOURCE_AGENTS, scope, project_path, overrides, commands);
        }
    }

    pub fn list(&self, workspace: Option<&Path>) -> Result<CommandsList, CommandsError> {
        let overrides = self.read_enabled_overrides();
        let mut user_commands: Vec<Command> = Vec::new();

        // ALL workspace sources merge before ALL user sources, so a user
        // ~/.zcode never out-prioritizes a workspace .agents source.
        if let Some(ws) = workspace {
            Self::discover_scope(ws, CommandScope::Project, Some(ws), &overrides, &mut user_commands);
        }
        Self::discover_scope(&self.home, CommandScope::Global, None, &overrides, &mut user_commands);

        // first discovery of a name wins
        let mut seen = BTreeSet::new();
        user_commands.retain(|c| seen.insert(c.name.clone()));

        let plugin_commands = self.plugin_commands()?;
        Ok(CommandsList {
            user_commands,
            plugin_commands,
        })
    }

    // -- write / update / delete -------------------------------------------

    fn file_name_for(&self, name: &str) -> String {
        let raw = name.strip_prefix('/').unwrap_or(name);
        format!("{raw}{FILE_EXTENSION}")
    }

    fn storage_target(
        &self,
        project_level: bool,
        workspace: Option<&Path>,
    ) -> Result<(PathBuf, CommandScope, Option<PathBuf>), CommandsError> {
        if project_level {
            let Some(ws) = workspace else {
                return Err(CommandsError::MissingWorkspace);
            };
            return Ok((
                self.commands_root(&USER_SEGMENTS, Some(ws)),
                CommandScope::Project,
                Some(ws.to_path_buf()),
            ));
        }
        Ok((
            self.commands_root(&USER_SEGMENTS, None),
            CommandScope::Global,
            None,
        ))
    }

    fn build_user_command(
        &self,
        parsed: ParsedCommandFile,
        root: &Path,
        file_path: &Path,
        scope: CommandScope,
        project_path: Option<PathBuf>,
        overrides: &BTreeMap<String, bool>,
    ) -> Command {
        let name = command_name(root, file_path);
        let enabled = overrides
            .get(&Self::override_key(file_path))
            .copied()
            .unwrap_or(true);
        Command {
            id: format!("{AGENT_SOURCE}:{DIR_SOURCE_ZCODE}:{scope}:{name}", scope = scope.as_str()),
            name,
            prompt: parsed.prompt,
            description: parsed.description,
            argument_hint: parsed.argument_hint,
            file_path: file_path.to_path_buf(),
            enabled,
            scope,
            source: CommandSource::User,
            directory_source: DIR_SOURCE_ZCODE,
            project_path,
            plugin_name: None,
            plugin_marketplace: None,
            plugin_enabled: None,
        }
    }

    pub fn write_command_file(
        &self,
        params: &WriteCommandParams,
    ) -> Result<Command, CommandsError> {
        let (root, scope, project_path) = self.storage_target(params.project_level, params.workspace.as_deref())?;
        std::fs::create_dir_all(&root)?;
        let file_name = self.file_name_for(&params.name);
        let file_path = root.join(&file_name);
        if file_path.exists() {
            return Err(CommandsError::AlreadyExists(file_name));
        }
        let content = generate_command_file_content(
            &params.prompt,
            params.description.as_deref(),
            params.argument_hint.as_deref(),
            None,
        );
        std::fs::write(&file_path, &content)?;
        // a fresh command is enabled: drop any stale override for the path
        let config = Self::set_override_in_config(&self.read_user_cli_config(), &Self::override_key(&file_path), true);
        self.write_user_cli_config(&config)?;

        let overrides = self.read_enabled_overrides();
        let parsed = parse_command_file(&content, &file_path.to_string_lossy())
            .ok_or_else(|| CommandsError::Io(std::io::Error::other("written command failed to parse")))?;
        Ok(self.build_user_command(parsed, &root, &file_path, scope, project_path, &overrides))
    }

    pub fn update_command_file(
        &self,
        params: &WriteCommandParams,
        old_file_path: Option<&Path>,
    ) -> Result<Command, CommandsError> {
        let (root, scope, project_path) = self.storage_target(params.project_level, params.workspace.as_deref())?;
        let new_file_path = root.join(self.file_name_for(&params.name));
        let overrides_before = self.read_enabled_overrides();
        let existing_content = old_file_path
            .and_then(|p| std::fs::read_to_string(p).ok());

        // rename: the old file goes away once the new one is written
        if let Some(old) = old_file_path
            && old != new_file_path
        {
            let _ = std::fs::remove_file(old);
        }
        if old_file_path != Some(new_file_path.as_path()) && new_file_path.exists() {
            return Err(CommandsError::AlreadyExists(
                new_file_path.file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default(),
            ));
        }

        let content = generate_command_file_content(
            &params.prompt,
            params.description.as_deref(),
            params.argument_hint.as_deref(),
            existing_content.as_deref(),
        );
        if let Some(parent) = new_file_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&new_file_path, &content)?;

        // overrides are keyed by file path: a rename must MIGRATE the
        // override or a just-disabled command resurrects under its new name
        if let Some(old) = old_file_path {
            let old_key = Self::override_key(old);
            let new_key = Self::override_key(&new_file_path);
            if old != new_file_path && overrides_before.contains_key(&old_key) {
                let enabled = overrides_before.get(&old_key).copied().unwrap_or(true);
                let config = Self::set_override_in_config(
                    &Self::set_override_in_config(&self.read_user_cli_config(), &old_key, true),
                    &new_key,
                    enabled,
                );
                self.write_user_cli_config(&config)?;
            }
        }

        let overrides = self.read_enabled_overrides();
        let parsed = parse_command_file(&content, &new_file_path.to_string_lossy())
            .ok_or_else(|| CommandsError::Io(std::io::Error::other("written command failed to parse")))?;
        Ok(self.build_user_command(parsed, &root, &new_file_path, scope, project_path, &overrides))
    }

    /// Deleting a missing file is a success (idempotent), and the deleted
    /// path's override is cleared so the config never accretes ghosts.
    pub fn delete_command_file(&self, file_path: &Path) -> Result<(), CommandsError> {
        match std::fs::remove_file(file_path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
        let config = Self::set_override_in_config(
            &self.read_user_cli_config(),
            &Self::override_key(file_path),
            true,
        );
        self.write_user_cli_config(&config)
    }

    pub fn set_command_enabled(&self, file_path: &Path, enabled: bool) -> Result<(), CommandsError> {
        let config = Self::set_override_in_config(
            &self.read_user_cli_config(),
            &Self::override_key(file_path),
            enabled,
        );
        self.write_user_cli_config(&config)
    }

    /// The primary user commands root, created on demand (open-in-file-manager
    /// cannot open a path that does not exist).
    pub fn primary_user_commands_directory(&self) -> Result<PathBuf, CommandsError> {
        let path = self.commands_root(&USER_SEGMENTS, None);
        std::fs::create_dir_all(&path)?;
        Ok(path)
    }

    // -- plugin commands (read-only sources) --------------------------------

    fn plugin_config(&self) -> (bool, Vec<PathBuf>, BTreeMap<String, bool>, Vec<String>, PathBuf) {
        let config = self.read_user_cli_config();
        let plugins = config.get("plugins").cloned().unwrap_or(Value::Null);
        let empty = Map::new();
        let map = plugins.as_object().unwrap_or(&empty);
        let enabled = map.get("enabled").and_then(Value::as_bool).unwrap_or(true);
        let dirs = read_string_array(map.get("dirs").unwrap_or(&Value::Null))
            .into_iter()
            .map(|d| self.resolve_config_path(Path::new(&d)))
            .collect();
        let enabled_plugins = read_boolean_record(map.get("enabledPlugins").unwrap_or(&Value::Null));
        let suppressed = read_string_array(map.get("suppressedBuiltins").unwrap_or(&Value::Null));
        let storage_dir = map
            .get("storage")
            .and_then(Value::as_object)
            .and_then(|s| s.get("dir"))
            .and_then(Value::as_str)
            .filter(|d| !d.trim().is_empty())
            .unwrap_or("~/.zcode");
        let storage_root = self.resolve_cli_storage_root(storage_dir);
        (enabled, dirs, enabled_plugins, suppressed, storage_root)
    }

    fn resolve_config_path(&self, raw: &Path) -> PathBuf {
        let raw_str = raw.to_string_lossy();
        let expanded = if let Some(rest) = raw_str.strip_prefix("~/") {
            self.home.join(rest)
        } else {
            raw.to_path_buf()
        };
        if expanded.is_absolute() {
            expanded
        } else {
            std::env::current_dir()
                .unwrap_or_else(|_| PathBuf::from("."))
                .join(expanded)
        }
    }

    fn resolve_cli_storage_root(&self, storage_dir: &str) -> PathBuf {
        let root = self.resolve_config_path(Path::new(storage_dir));
        if root.file_name().and_then(|s| s.to_str()) == Some("cli") {
            root
        } else {
            root.join("cli")
        }
    }

    /// Plugin storage root + official marketplace cache roots, sorted.
    fn official_plugin_cache_roots(&self, plugin_storage_root: &Path) -> Vec<PathBuf> {
        let cache_root = plugin_storage_root
            .join("cache")
            .join(OFFICIAL_MARKETPLACE);
        let Ok(plugin_entries) = std::fs::read_dir(&cache_root) else {
            return Vec::new();
        };
        let mut roots = Vec::new();
        for plugin_dir in plugin_entries.flatten() {
            let Ok(version_entries) = std::fs::read_dir(plugin_dir.path()) else { continue };
            for version in version_entries.flatten() {
                if version.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                    roots.push(version.path());
                }
            }
        }
        roots.sort();
        roots
    }

    fn read_plugin_manifest(&self, root: &Path) -> Option<(String, Value)> {
        for rel in MANIFEST_PATHS {
            let path = root.join(rel);
            if !path.exists() {
                continue;
            }
            let Ok(raw) = std::fs::read_to_string(&path) else { return None };
            let Ok(parsed) = serde_json::from_str::<Value>(&raw) else { return None };
            if !is_record(&parsed) {
                return None;
            }
            let name = parsed
                .get("name")
                .and_then(Value::as_str)
                .map(str::trim)
                .unwrap_or_default();
            if !crate::plugins::manifest::plugin_name_valid(name) {
                return None;
            }
            return Some((name.to_string(), parsed.get("commands").cloned().unwrap_or(Value::Null)));
        }
        None
    }

    /// Manifest `commands` paths resolve strictly inside the plugin root.
    /// An absolute path or a `..` escape is rejected; `.` resolves to the
    /// root itself (mirrors resolveInside).
    fn resolve_inside(root: &Path, raw: &str) -> Option<PathBuf> {
        if Path::new(raw).is_absolute() {
            return None;
        }
        let resolved = normalize_lexical(&root.join(raw));
        let root_norm = normalize_lexical(root);
        if resolved.starts_with(&root_norm) {
            Some(resolved)
        } else {
            None
        }
    }

    pub fn plugin_commands(&self) -> Result<Vec<Command>, CommandsError> {
        let (plugins_enabled, dirs, enabled_plugins, suppressed, storage_root) =
            self.plugin_config();
        if !plugins_enabled {
            return Ok(Vec::new());
        }
        let plugin_storage_root = storage_root.join("plugins");

        struct Candidate {
            default_enabled: bool,
            marketplace: &'static str,
            root: PathBuf,
        }
        let mut candidates: Vec<Candidate> = dirs
            .into_iter()
            .map(|d| Candidate {
                default_enabled: true,
                marketplace: "inline",
                root: d,
            })
            .collect();
        candidates.extend(
            self.official_plugin_cache_roots(&plugin_storage_root)
                .into_iter()
                .map(|root| Candidate {
                    default_enabled: false,
                    marketplace: OFFICIAL_MARKETPLACE,
                    root,
                }),
        );

        let overrides = self.read_enabled_overrides();
        let mut commands: Vec<Command> = Vec::new();
        let mut seen_paths: BTreeSet<String> = BTreeSet::new();
        let mut seen_plugin_ids: BTreeSet<String> = BTreeSet::new();

        for candidate in candidates {
            let Some((name, commands_field)) = self.read_plugin_manifest(&candidate.root) else {
                continue;
            };
            let plugin_id = format!("{name}@{}", candidate.marketplace);
            // an uninstalled builtin stays uninstalled: the official cache
            // still holds its files, so suppress here like the CLI resolve
            if candidate.marketplace == OFFICIAL_MARKETPLACE
                && suppressed.iter().any(|s| s == &plugin_id)
            {
                continue;
            }
            if !seen_plugin_ids.insert(plugin_id.clone()) {
                continue;
            }
            let default_enabled =
                candidate.default_enabled || DEFAULT_ENABLED_OFFICIAL.contains(&plugin_id.as_str());
            let plugin_enabled = enabled_plugins.get(&plugin_id).copied().unwrap_or(default_enabled);
            if !plugin_enabled {
                continue;
            }

            // command roots: manifest paths (each must resolve inside), or
            // the default `commands/` dir ONLY when the key is absent
            let mut roots: Vec<PathBuf> = Vec::new();
            match &commands_field {
                Value::String(s) => {
                    if let Some(r) = Self::resolve_inside(&candidate.root, s) {
                        roots.push(r);
                    }
                }
                Value::Array(items) => {
                    for item in items {
                        if let Some(s) = item.as_str()
                            && let Some(r) = Self::resolve_inside(&candidate.root, s)
                        {
                            roots.push(r);
                        }
                    }
                }
                _ => {
                    let default_root = candidate.root.join("commands");
                    if default_root.exists() {
                        roots.push(default_root);
                    }
                }
            }

            for root in roots {
                Self::discover_plugin_root(
                    &root,
                    &root,
                    &name,
                    candidate.marketplace,
                    plugin_enabled,
                    &overrides,
                    &mut seen_paths,
                    &mut commands,
                );
            }
        }
        commands.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(commands)
    }

    #[allow(clippy::too_many_arguments)]
    fn discover_plugin_root(
        root: &Path,
        dir: &Path,
        plugin_name: &str,
        marketplace: &str,
        plugin_enabled: bool,
        overrides: &BTreeMap<String, bool>,
        seen_paths: &mut BTreeSet<String>,
        commands: &mut Vec<Command>,
    ) {
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                continue;
            }
            let path = entry.path();
            match entry.file_type() {
                Ok(t) if t.is_dir() => Self::discover_plugin_root(
                    root, &path, plugin_name, marketplace, plugin_enabled, overrides, seen_paths,
                    commands,
                ),
                Ok(_) if name.to_ascii_lowercase().ends_with(FILE_EXTENSION) => {
                    let key = path.to_string_lossy().replace('\\', "/").to_ascii_lowercase();
                    if !seen_paths.insert(key) {
                        continue;
                    }
                    let Ok(content) = std::fs::read_to_string(&path) else { continue };
                    let Some(parsed) = parse_command_file(&content, &path.to_string_lossy())
                    else {
                        continue;
                    };
                    let cname = command_name(root, &path);
                    let enabled = plugin_enabled
                        && overrides
                            .get(&Self::override_key(&path))
                            .copied()
                            .unwrap_or(true);
                    commands.push(Command {
                        id: format!(
                            "plugin:{marketplace}:{plugin_name}:{cname}:{path}",
                            path = path.to_string_lossy()
                        ),
                        name: cname,
                        prompt: parsed.prompt,
                        description: parsed.description,
                        argument_hint: parsed.argument_hint,
                        file_path: path,
                        enabled,
                        scope: CommandScope::Global,
                        source: CommandSource::Plugin,
                        directory_source: DIR_SOURCE_ZCODE,
                        project_path: None,
                        plugin_name: Some(plugin_name.to_string()),
                        plugin_marketplace: Some(marketplace.to_string()),
                        plugin_enabled: Some(plugin_enabled),
                    });
                }
                _ => {}
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct WriteCommandParams {
    pub name: String,
    pub prompt: String,
    pub description: Option<String>,
    pub argument_hint: Option<String>,
    pub project_level: bool,
    pub workspace: Option<PathBuf>,
}

/// Built-in official plugins enabled out of the box
/// (DEFAULT_ENABLED_OFFICIAL_PLUGIN_IDS — none today).
const DEFAULT_ENABLED_OFFICIAL: [&str; 0] = [];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frontmatter_parse_and_preserve_unknown_keys() {
        let content = "---\ntitle: keep me\ndescription: first line\n  second line\nargument-hint: file?\n---\n\nBody here\n";
        let parsed = parse_command_file(content, "review.md").unwrap();
        assert_eq!(parsed.name, "/review");
        assert_eq!(parsed.prompt, "Body here");
        assert_eq!(parsed.description.as_deref(), Some("first line second line"));
        assert_eq!(parsed.argument_hint.as_deref(), Some("file?"));

        // the unknown key survives a description rewrite; the replaced
        // description block (including its continuation line) does not
        let out = generate_command_file_content(
            "New body",
            Some("new desc"),
            parsed.argument_hint.as_deref(),
            Some(content),
        );
        assert!(out.contains("title: keep me"), "{out}");
        assert!(out.contains("description: new desc"), "{out}");
        assert!(!out.contains("first line"), "{out}");
        assert!(out.contains("argument-hint: file?"), "{out}");
        assert!(out.ends_with("New body"), "{out}");

        // no frontmatter at all → bare prompt body
        let bare = generate_command_file_content("Just prompt", None, None, None);
        assert_eq!(bare, "Just prompt");
    }

    #[test]
    fn names_are_paths_with_separator() {
        assert_eq!(command_name(Path::new("/r"), Path::new("/r/deploy/staging.md")), "/deploy/staging");
        assert_eq!(command_name(Path::new("/r"), Path::new("/r/single.md")), "/single");
    }
}
