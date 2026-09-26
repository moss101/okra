//! Plugin manifest — kimi's manifest shape (MASTER-PLAN §3 #46), data-only
//! by default. A manifest declares DATA the host loads (skills, prompts,
//! command markdown, MCP server descriptors, hook definitions); fields that
//! would carry executable runtime code are recorded as unsupported info
//! diagnostics and never interpreted. Parsing is contained like kimi's:
//! malformed optional fields degrade to diagnostics, never abort the parse;
//! only a missing/invalid `name` fails the whole manifest.
//!
//! Source study: kimi `agent-core-v2/src/app/plugin/manifest.ts` +
//! `types.ts` (name regex, 32 KiB system-prompt cap, `./`-prefixed
//! path fields confined to the plugin root, severity model).

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// kimi `PLUGIN_NAME_REGEX = /^[a-z0-9][a-z0-9_-]{0,63}$/` (types.ts:196).
pub fn plugin_name_valid(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_ascii_lowercase() || c.is_ascii_digit() => {}
        _ => return false,
    }
    let rest = chars.count();
    rest <= 63
        && name.chars().skip(1).all(|c| {
            c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-'
        })
}

/// kimi `PLUGIN_SYSTEM_PROMPT_MAX_BYTES` (manifest.ts:19).
pub const PLUGIN_SYSTEM_PROMPT_MAX_BYTES: usize = 32 * 1024;

/// kimi `UNSUPPORTED_RUNTIME_FIELDS` (manifest.ts:21-28): a plugin carrying
/// these stays data-only — the fields are reported, never executed.
pub const UNSUPPORTED_RUNTIME_FIELDS: [&str; 6] =
    ["tools", "apps", "inject", "configFile", "config_file", "bootstrap"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticSeverity {
    Error,
    Warn,
    Info,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginDiagnostic {
    pub severity: DiagnosticSeverity,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginAuthor {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionStart {
    pub skill: String,
}

/// The parsed data-only manifest (kimi `PluginManifest` shape).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginManifest {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub keywords: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub homepage: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub license: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub author: Option<PluginAuthor>,
    /// `./`-relative skill directories, lexically confined to the plugin.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skills: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub agents: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_start: Option<SessionStart>,
    /// MCP server descriptors, kept as raw JSON values — the tools crate's
    /// MCP client owns their schema (okra-tools `mcp.rs`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mcp_servers: Option<serde_json::Map<String, Value>>,
    /// Hook definitions (kimi `HookDefConfig[]`) — data consumed by the
    /// policy/hooks plane, never code this crate executes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hooks: Option<Vec<Value>>,
    /// `./`-relative command markdown files/directories.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub commands: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skill_instructions: Option<String>,
    /// Inline system prompt (≤32 KiB; over-budget → warn + ignored).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ParsedManifest {
    pub manifest: Option<PluginManifest>,
    pub diagnostics: Vec<PluginDiagnostic>,
}

impl ParsedManifest {
    fn error(message: impl Into<String>) -> Self {
        ParsedManifest {
            manifest: None,
            diagnostics: vec![PluginDiagnostic {
                severity: DiagnosticSeverity::Error,
                message: message.into(),
            }],
        }
    }
}

/// Parse a manifest from JSON text (the decoded `kimi.plugin.json`).
pub fn parse_manifest(json: &str) -> ParsedManifest {
    let raw: Value = match serde_json::from_str(json) {
        Ok(v) => v,
        Err(e) => return ParsedManifest::error(format!("failed to parse manifest: {e}")),
    };
    let Some(map) = raw.as_object() else {
        return ParsedManifest::error("manifest must be a JSON object");
    };

    let name = match map.get("name").and_then(Value::as_str) {
        Some(n) => n.trim().to_string(),
        None => return ParsedManifest::error("\"name\" is required"),
    };
    if name.is_empty() {
        return ParsedManifest::error("\"name\" is required");
    }
    if !plugin_name_valid(&name) {
        return ParsedManifest::error(format!(
            "\"name\" must match ^[a-z0-9][a-z0-9_-]{{0,63}}$ (got \"{name}\")"
        ));
    }

    let mut diagnostics = Vec::new();
    for field in UNSUPPORTED_RUNTIME_FIELDS {
        if map.contains_key(field) {
            diagnostics.push(PluginDiagnostic {
                severity: DiagnosticSeverity::Info,
                message: format!("\"{field}\" is present but not supported: plugins are data-only"),
            });
        }
    }

    let skills = dir_list_field("skills", map.get("skills"), &mut diagnostics);
    let agents = dir_list_field("agents", map.get("agents"), &mut diagnostics);
    let commands = dir_list_field("commands", map.get("commands"), &mut diagnostics);

    let session_start = match map.get("sessionStart") {
        None | Some(Value::Null) => None,
        Some(v) => match v.get("skill").and_then(Value::as_str).map(str::trim) {
            Some(skill) if !skill.is_empty() => Some(SessionStart {
                skill: skill.to_string(),
            }),
            _ => {
                diagnostics.push(PluginDiagnostic {
                    severity: DiagnosticSeverity::Warn,
                    message: "\"sessionStart.skill\" is required when sessionStart is present"
                        .into(),
                });
                None
            }
        },
    };

    let system_prompt = match map.get("systemPrompt").and_then(Value::as_str) {
        None => None,
        Some(p) if p.trim().is_empty() => None,
        Some(p) => {
            if p.len() > PLUGIN_SYSTEM_PROMPT_MAX_BYTES {
                diagnostics.push(PluginDiagnostic {
                    severity: DiagnosticSeverity::Warn,
                    message: format!(
                        "\"systemPrompt\" is {} bytes, exceeding the 32 KB limit; the field is ignored",
                        p.len()
                    ),
                });
                None
            } else {
                Some(p.to_string())
            }
        }
    };

    let mcp_servers = match map.get("mcpServers") {
        None | Some(Value::Null) => None,
        Some(Value::Object(m)) => Some(m.clone()),
        Some(_) => {
            diagnostics.push(PluginDiagnostic {
                severity: DiagnosticSeverity::Warn,
                message: "\"mcpServers\" must be an object".into(),
            });
            None
        }
    };

    let hooks = match map.get("hooks") {
        None | Some(Value::Null) => None,
        Some(Value::Array(a)) => Some(a.clone()),
        Some(_) => {
            diagnostics.push(PluginDiagnostic {
                severity: DiagnosticSeverity::Warn,
                message: "\"hooks\" must be an array".into(),
            });
            None
        }
    };

    ParsedManifest {
        manifest: Some(PluginManifest {
            name,
            version: string_field(map, "version"),
            description: string_field(map, "description"),
            keywords: string_array_field(map, "keywords"),
            homepage: string_field(map, "homepage"),
            license: string_field(map, "license"),
            author: read_author(map.get("author")),
            skills,
            agents,
            session_start,
            mcp_servers,
            hooks,
            commands,
            skill_instructions: string_field(map, "skillInstructions"),
            system_prompt,
        }),
        diagnostics,
    }
}

/// `./`-relative path list fields, lexically confined to the plugin root —
/// the offline half of kimi's realpath containment (its fs realpath check
/// happens again at install time against the store layout).
fn dir_list_field(
    field: &str,
    raw: Option<&Value>,
    diagnostics: &mut Vec<PluginDiagnostic>,
) -> Vec<String> {
    let Some(raw) = raw else {
        return Vec::new();
    };
    let entries: Vec<String> = match raw {
        Value::String(s) => vec![s.clone()],
        Value::Array(a) if a.iter().all(Value::is_string) => a
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect(),
        _ => {
            diagnostics.push(PluginDiagnostic {
                severity: DiagnosticSeverity::Error,
                message: format!("\"{field}\" must be a string or string[]"),
            });
            return Vec::new();
        }
    };
    let mut resolved = Vec::new();
    for entry in entries {
        match check_plugin_path(&entry) {
            Ok(()) => resolved.push(entry),
            Err(reason) => diagnostics.push(PluginDiagnostic {
                severity: DiagnosticSeverity::Error,
                message: format!("\"{field}\" {reason} (got \"{entry}\")"),
            }),
        }
    }
    resolved
}

/// Validate one `./`-relative path field: must start with `./` and stay
/// inside the plugin (no `..` components, no absolute paths).
pub fn check_plugin_path(entry: &str) -> Result<(), &'static str> {
    if !entry.starts_with("./") {
        return Err("path must start with \"./\"");
    }
    let mut depth: usize = 1;
    for comp in entry.trim_start_matches("./").split('/') {
        match comp {
            "" | "." => {}
            ".." => {
                if depth == 1 {
                    return Err("path resolves outside the plugin");
                }
                depth -= 1;
            }
            _ => depth += 1,
        }
    }
    Ok(())
}

fn string_field(map: &serde_json::Map<String, Value>, key: &str) -> Option<String> {
    map.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn string_array_field(map: &serde_json::Map<String, Value>, key: &str) -> Vec<String> {
    map.get(key)
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn read_author(raw: Option<&Value>) -> Option<PluginAuthor> {
    match raw {
        Some(Value::String(s)) if !s.trim().is_empty() => Some(PluginAuthor {
            name: Some(s.clone()),
            email: None,
        }),
        Some(Value::Object(m)) => {
            let author = PluginAuthor {
                name: m.get("name").and_then(Value::as_str).map(str::to_string),
                email: m.get("email").and_then(Value::as_str).map(str::to_string),
            };
            (author.name.is_some() || author.email.is_some()).then_some(author)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_regex_matches_kimi() {
        assert!(plugin_name_valid("a"));
        assert!(plugin_name_valid("ok-ra_1"));
        assert!(plugin_name_valid("0edge"));
        assert!(!plugin_name_valid(""));
        assert!(!plugin_name_valid("-lead"));
        assert!(!plugin_name_valid("_lead"));
        assert!(!plugin_name_valid("Upper"));
        assert!(!plugin_name_valid("has space"));
        assert!(!plugin_name_valid(&"a".repeat(65)));
        assert!(plugin_name_valid(&"a".repeat(64)));
    }

    #[test]
    fn parses_full_data_only_manifest() {
        let parsed = parse_manifest(
            r#"{
                "name": "okra-notes",
                "version": "1.2.0",
                "description": "note-taking skills",
                "keywords": ["notes"],
                "author": { "name": "Ada", "email": "ada@example.com" },
                "skills": ["./skills", "./more-skills"],
                "commands": "./commands",
                "sessionStart": { "skill": "notes-welcome" },
                "systemPrompt": "You take notes.",
                "hooks": [{ "event": "preToolUse" }],
                "mcpServers": { "notes": { "transport": "stdio", "command": "./bin/notes" } }
            }"#,
        );
        assert!(parsed.diagnostics.is_empty(), "{:?}", parsed.diagnostics);
        let m = parsed.manifest.unwrap();
        assert_eq!(m.name, "okra-notes");
        assert_eq!(m.author.as_ref().unwrap().email.as_deref(), Some("ada@example.com"));
        assert_eq!(m.skills, ["./skills", "./more-skills"]);
        assert_eq!(m.session_start.as_ref().unwrap().skill, "notes-welcome");
    }

    #[test]
    fn runtime_fields_are_recorded_not_executed() {
        let parsed = parse_manifest(
            r#"{ "name": "p", "tools": [{"exec": "sh"}], "bootstrap": "x" }"#,
        );
        let m = parsed.manifest.expect("data-only fields must not fail the parse");
        assert_eq!(m.name, "p");
        let infos: Vec<&str> = parsed
            .diagnostics
            .iter()
            .filter(|d| d.severity == DiagnosticSeverity::Info)
            .map(|d| d.message.as_str())
            .collect();
        assert_eq!(infos.len(), 2, "{infos:?}");
        assert!(infos[0].contains("data-only"));
    }

    #[test]
    fn name_errors_abort_parse() {
        for bad in [
            r#"{}"#,
            r#"{ "name": "" }"#,
            r#"{ "name": "Nope" }"#,
            r#"{ "name": 7 }"#,
        ] {
            let parsed = parse_manifest(bad);
            assert!(parsed.manifest.is_none(), "{bad}");
            assert_eq!(parsed.diagnostics[0].severity, DiagnosticSeverity::Error);
        }
    }

    #[test]
    fn path_fields_confined_and_diagnostics_contained() {
        let parsed = parse_manifest(
            r#"{ "name": "p", "skills": ["./ok", "./../escape", "relative"] }"#,
        );
        let m = parsed.manifest.unwrap();
        assert_eq!(m.skills, ["./ok"]);
        let msgs: Vec<&str> = parsed
            .diagnostics
            .iter()
            .map(|d| d.message.as_str())
            .collect();
        assert!(msgs.iter().any(|m| m.contains("outside the plugin")), "{msgs:?}");
        assert!(msgs.iter().any(|m| m.contains("must start with")), "{msgs:?}");

        // kimi's whole-field rule: one non-string entry invalidates the field
        let parsed = parse_manifest(r#"{ "name": "p", "skills": ["./ok", 3] }"#);
        assert!(parsed.manifest.unwrap().skills.is_empty());
        assert_eq!(
            parsed.diagnostics[0].message,
            "\"skills\" must be a string or string[]"
        );
    }

    #[test]
    fn system_prompt_cap_degrades_to_warn() {
        let big = format!(r#"{{ "name": "p", "systemPrompt": "{}" }}"#, "x".repeat(33 * 1024));
        let parsed = parse_manifest(&big);
        assert!(parsed.manifest.unwrap().system_prompt.is_none());
        assert_eq!(parsed.diagnostics[0].severity, DiagnosticSeverity::Warn);
    }

    #[test]
    fn path_checker_lexical_cases() {
        assert!(check_plugin_path("./a/b").is_ok());
        assert!(check_plugin_path("./a/../b").is_ok());
        assert!(check_plugin_path("../a").is_err());
        assert!(check_plugin_path("./a/../../b").is_err());
        assert!(check_plugin_path("a/b").is_err());
        assert!(check_plugin_path("/abs").is_err());
    }
}
