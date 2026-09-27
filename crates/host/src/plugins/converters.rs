//! Cross-ecosystem plugin converters — MASTER-PLAN §3 #46 remainder (design
//! donor: qwen-code `packages/core/src/extension/{claude,gemini}-converter.ts`).
//!
//! Both converters produce okra's **data-only** `PluginManifest` (kimi shape)
//! plus diagnostics; neither executes anything from the plugin. Contracts:
//!
//! - **Claude Code plugin** (`.claude-plugin/plugin.json`): metadata maps
//!   1:1; path fields (`commands`/`agents`/`skills`: string | string[]) are
//!   normalized to `./`-relative, lexically confined entries (absolute and
//!   escaping paths are dropped with an error diagnostic); `mcpServers` may
//!   be an inline record OR a `./`-relative path to a JSON file whose
//!   `mcpServers` key (or body) holds the record. Claude's `hooks` shape is
//!   a DIFFERENT data language from okra's hook definitions — it is dropped
//!   with a warning rather than guessed at (re-declare in okra format).
//! - **Gemini CLI extension** (`gemini-extension.json`): metadata + inline
//!   `mcpServers`; `commands/*.toml` files (`prompt` + optional
//!   `description`) convert to markdown command files (frontmatter when a
//!   description exists — the qwen TOML→markdown rule). Unparseable TOML is
//!   an error diagnostic for that file, never a blanket failure.

use std::path::Path;

use serde_json::{json, Value};

use super::manifest::{
    check_plugin_path, parse_manifest, PluginAuthor, PluginDiagnostic, PluginManifest,
};
use crate::fsutil::normalize_lexical;

/// Result of one conversion: the manifest (ready for
/// `plugins::manifest::parse_manifest`-consumers), any files the conversion
/// GENERATES (Gemini TOML→markdown), and honest diagnostics.
#[derive(Debug, Clone, PartialEq)]
pub struct ConversionOutcome {
    pub manifest: PluginManifest,
    /// (plugin-relative path, content) pairs to materialize alongside the
    /// manifest — currently Gemini TOML→markdown commands.
    pub generated_files: Vec<(String, String)>,
    pub diagnostics: Vec<PluginDiagnostic>,
}

impl ConversionOutcome {
    fn warn(&mut self, message: impl Into<String>) {
        self.diagnostics.push(PluginDiagnostic {
            severity: super::manifest::DiagnosticSeverity::Warn,
            message: message.into(),
        });
    }

    fn error(&mut self, message: impl Into<String>) {
        self.diagnostics.push(PluginDiagnostic {
            severity: super::manifest::DiagnosticSeverity::Error,
            message: message.into(),
        });
    }

    /// The manifest re-parsed through okra's own gate — the converted output
    /// must satisfy the same data-only rules as a hand-written plugin.
    pub fn reparsed(&self) -> super::manifest::ParsedManifest {
        let text = serde_json::to_string(&json!({
            "name": self.manifest.name,
            "version": self.manifest.version,
            "description": self.manifest.description,
            "keywords": self.manifest.keywords,
            "homepage": self.manifest.homepage,
            "license": self.manifest.license,
            "author": self.manifest.author,
            "skills": self.manifest.skills,
            "agents": self.manifest.agents,
            "mcpServers": self.manifest.mcp_servers,
            "commands": self.manifest.commands,
        }))
        .unwrap_or_default();
        parse_manifest(&text)
    }
}

/// `commands` | ["commands", "more"] → each entry `./`-prefixed, lexically
/// inside the plugin, or dropped with a diagnostic.
fn confined_path_list(value: &Value, field: &str, out: &mut ConversionOutcome) -> Vec<String> {
    let items: Vec<&str> = match value {
        Value::Null => Vec::new(),
        Value::String(s) => vec![s.as_str()],
        Value::Array(a) => a.iter().filter_map(Value::as_str).collect(),
        _ => {
            out.error(format!("{field}: expected a string or string array"));
            return Vec::new();
        }
    };
    let mut kept = Vec::new();
    for item in items {
        if Path::new(item).is_absolute() {
            out.error(format!("{field}: `{item}` dropped (absolute path)"));
            continue;
        }
        let prefixed = if item.starts_with("./") {
            item.to_string()
        } else {
            format!("./{item}")
        };
        match check_plugin_path(&prefixed) {
            Ok(()) => kept.push(prefixed),
            Err(reason) => out.error(format!("{field}: `{item}` dropped ({reason})")),
        }
    }
    kept
}

fn read_json_file(path: &Path) -> Result<Value, String> {
    let raw = std::fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    serde_json::from_str(&raw).map_err(|e| format!("parse {}: {e}", path.display()))
}

/// Author mapping: Claude `{name?, email?, url?}` → okra `{name?, email?}`
/// (url has no okra field; kept out, not invented).
fn convert_author(value: &Value) -> Option<PluginAuthor> {
    let map = value.as_object()?;
    Some(PluginAuthor {
        name: map.get("name").and_then(Value::as_str).map(str::to_string),
        email: map.get("email").and_then(Value::as_str).map(str::to_string),
    })
}

/// Claude Code plugin (`.claude-plugin/plugin.json`) → okra data-only shape.
pub fn convert_claude_plugin(root: &Path) -> Result<ConversionOutcome, String> {
    let manifest_path = root.join(".claude-plugin").join("plugin.json");
    if !manifest_path.exists() {
        return Err(format!(
            "not a Claude Code plugin: {} missing",
            manifest_path.display()
        ));
    }
    let raw = read_json_file(&manifest_path)?;
    let Some(map) = raw.as_object() else {
        return Err("plugin.json is not an object".into());
    };

    let mut out = ConversionOutcome {
        manifest: PluginManifest {
            name: map.get("name").and_then(Value::as_str).unwrap_or_default().to_string(),
            version: map.get("version").and_then(Value::as_str).map(str::to_string),
            description: map.get("description").and_then(Value::as_str).map(str::to_string),
            keywords: map
                .get("keywords")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect())
                .unwrap_or_default(),
            homepage: map.get("homepage").and_then(Value::as_str).map(str::to_string),
            license: map.get("license").and_then(Value::as_str).map(str::to_string),
            author: map.get("author").and_then(convert_author),
            skills: Vec::new(),
            agents: Vec::new(),
            session_start: None,
            mcp_servers: None,
            hooks: None,
            commands: Vec::new(),
            skill_instructions: None,
            system_prompt: None,
        },
        generated_files: Vec::new(),
        diagnostics: Vec::new(),
    };

    // path fields: normalize + confine
    out.manifest.commands = confined_path_list(map.get("commands").unwrap_or(&Value::Null), "commands", &mut out);
    out.manifest.agents = confined_path_list(map.get("agents").unwrap_or(&Value::Null), "agents", &mut out);
    out.manifest.skills = confined_path_list(map.get("skills").unwrap_or(&Value::Null), "skills", &mut out);

    // mcpServers: inline record, or "./"-relative path to a JSON file
    match map.get("mcpServers") {
        None | Some(Value::Null) => {}
        Some(Value::Object(m)) => {
            out.manifest.mcp_servers = Some(m.clone());
        }
        Some(Value::String(path)) => {
            let prefixed = if path.starts_with("./") {
                path.clone()
            } else {
                format!("./{path}")
            };
            match check_plugin_path(&prefixed) {
                Err(reason) => {
                    out.error(format!("mcpServers: `{path}` dropped ({reason})"));
                }
                Ok(()) => {
                    let file = root.join(prefixed.trim_start_matches("./"));
                    match read_json_file(&file) {
                        Err(e) => out.error(format!("mcpServers: {e}")),
                        Ok(parsed) => {
                            // either {"mcpServers": {...}} or a bare record
                            let record = parsed
                                .get("mcpServers")
                                .and_then(Value::as_object)
                                .cloned()
                                .or_else(|| parsed.as_object().cloned());
                            out.manifest.mcp_servers = record;
                        }
                    }
                }
            }
        }
        Some(_) => out.error("mcpServers: expected a record or a path string"),
    }

    // foreign hook data language: dropped honestly, never guessed
    if map.contains_key("hooks") {
        out.warn(
            "hooks: Claude hook definitions dropped — re-declare them in okra's data-only format",
        );
    }
    for dropped in ["workflows", "outputStyles", "lspServers"] {
        if map.contains_key(dropped) {
            out.warn(format!("{dropped}: no okra equivalent — dropped"));
        }
    }

    Ok(out)
}

/// Gemini CLI extension (`gemini-extension.json`) → okra data-only shape.
/// TOML command files under `commands/` convert to markdown (generated_files).
pub fn convert_gemini_extension(root: &Path) -> Result<ConversionOutcome, String> {
    let manifest_path = root.join("gemini-extension.json");
    if !manifest_path.exists() {
        return Err(format!(
            "not a Gemini CLI extension: {} missing",
            manifest_path.display()
        ));
    }
    let raw = read_json_file(&manifest_path)?;
    let Some(map) = raw.as_object() else {
        return Err("gemini-extension.json is not an object".into());
    };

    let mut out = ConversionOutcome {
        manifest: PluginManifest {
            name: map.get("name").and_then(Value::as_str).unwrap_or_default().to_string(),
            version: map.get("version").and_then(Value::as_str).map(str::to_string),
            description: map.get("description").and_then(Value::as_str).map(str::to_string),
            keywords: Vec::new(),
            homepage: None,
            license: None,
            author: None,
            skills: Vec::new(),
            agents: Vec::new(),
            session_start: None,
            mcp_servers: map
                .get("mcpServers")
                .and_then(Value::as_object)
                .cloned(),
            hooks: None,
            commands: Vec::new(),
            skill_instructions: None,
            system_prompt: None,
        },
        generated_files: Vec::new(),
        diagnostics: Vec::new(),
    };

    // commands dir: .toml files convert to markdown; .md files pass through
    let commands_dir = root.join("commands");
    if commands_dir.is_dir() {
        out.manifest.commands = vec!["./commands".to_string()];
        let mut toml_paths: Vec<std::path::PathBuf> = Vec::new();
        collect_toml_files(&commands_dir, &mut toml_paths);
        toml_paths.sort();
        for path in toml_paths {
            let rel = path
                .strip_prefix(&commands_dir)
                .unwrap_or(&path)
                .to_string_lossy()
                .into_owned();
            match std::fs::read_to_string(&path)
                .map_err(|e| format!("read {}: {e}", path.display()))
                .and_then(|c| convert_toml_command(&c))
            {
                Ok(markdown) => {
                    let md_rel = format!(
                        "./commands/{}",
                        normalize_lexical(Path::new(&rel))
                            .with_extension("md")
                            .to_string_lossy()
                    );
                    out.generated_files.push((md_rel, markdown));
                }
                Err(e) => out.error(format!("commands/{rel}: {e}")),
            }
        }
    }

    Ok(out)
}

fn collect_toml_files(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_toml_files(&path, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("toml") {
            out.push(path);
        }
    }
}

/// TOML command → markdown (qwen `toml-to-markdown-converter.ts` rule):
/// `prompt` required; `description` becomes frontmatter.
pub fn convert_toml_command(content: &str) -> Result<String, String> {
    let parsed: Value = toml::from_str(content).map_err(|e| format!("parse TOML: {e}"))?;
    let Some(prompt) = parsed.get("prompt").and_then(Value::as_str) else {
        return Err("TOML must contain a \"prompt\" field".into());
    };
    let description = parsed.get("description").and_then(Value::as_str);
    match description {
        Some(d) => Ok(format!("---\ndescription: {d}\n---\n\n{prompt}\n")),
        None => Ok(format!("{prompt}\n")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn toml_command_converts_with_and_without_description() {
        let with = convert_toml_command("prompt = \"deploy it\"\ndescription = \"deploy cmd\"\n").unwrap();
        assert_eq!(with, "---\ndescription: deploy cmd\n---\n\ndeploy it\n");
        let without = convert_toml_command("prompt = \"just run\"\n").unwrap();
        assert_eq!(without, "just run\n");
        assert!(convert_toml_command("description = \"no prompt\"").is_err());
    }

    #[test]
    fn path_lists_confine_escapes() {
        let mut out = ConversionOutcome {
            manifest: PluginManifest {
                name: "t".into(),
                version: None,
                description: None,
                keywords: vec![],
                homepage: None,
                license: None,
                author: None,
                skills: vec![],
                agents: vec![],
                session_start: None,
                mcp_servers: None,
                hooks: None,
                commands: vec![],
                skill_instructions: None,
                system_prompt: None,
            },
            generated_files: vec![],
            diagnostics: vec![],
        };
        let kept = confined_path_list(
            &json!(["commands", "./skills", "../escape", "/abs"]),
            "commands",
            &mut out,
        );
        assert_eq!(kept, vec!["./commands", "./skills"]);
        assert_eq!(out.diagnostics.len(), 2, "escape + absolute dropped");
    }
}
