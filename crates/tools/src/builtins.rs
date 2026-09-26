//! Builtin tools for the M0 gate: `read_file` (the one tool driven
//! end-to-end through the daemon per MASTER-PLAN M0) plus `list_dir`.
//! Read-only, idempotent — exactly the tools auto-retry may re-issue.

use serde::Deserialize;
use serde_json::json;
use std::path::PathBuf;

use crate::pipeline::SpillStore;
use crate::scheduler::ResourceAccess;
use crate::spec::{RiskLevel, SideEffectScope, ToolEntry, ToolMetadata, ToolSpec};
use crate::stream::{ToolError, ToolOutput, ToolStream};

#[derive(Debug, Deserialize)]
pub struct ReadFileArgs {
    pub path: String,
    /// 1-based; matches ZCode/grok read-state tooling conventions.
    #[serde(default)]
    pub offset_line: Option<u64>,
    #[serde(default = "default_max_bytes")]
    pub max_bytes: usize,
}

fn default_max_bytes() -> usize {
    256 * 1024
}

/// Build the `read_file` tool rooted at `workspace_root`. Declares a READ
/// access for the scheduler, idempotent=true (safe auto-retry), and spills
/// over-budget output to the spill store when one is configured.
pub fn read_file_tool(workspace_root: PathBuf) -> ErasedReadFile {
    ErasedReadFile { workspace_root }
}

pub struct ErasedReadFile {
    workspace_root: PathBuf,
}

impl ErasedReadFile {
    pub fn entry(&self) -> ToolEntry {
        ToolEntry {
            spec: ToolSpec {
                name: "read_file".into(),
                namespace: None,
                title: Some("Read file".into()),
                description: "Read a text file inside the workspace.".into(),
                arguments_schema: Some(json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" },
                        "offsetLine": { "type": "integer", "minimum": 1 },
                        "maxBytes": { "type": "integer", "minimum": 1 }
                    },
                    "required": ["path"]
                })),
                kind: Some("fs".into()),
                behavior_version: Some("1".into()),
                idempotent: true,
                read_only: true,
                timeout_ms: Some(10_000),
                max_concurrency: None,
            },
            metadata: ToolMetadata {
                read_only: true,
                destructive: false,
                concurrent_safe: true,
                needs_approval: false,
                side_effect_scope: SideEffectScope::None,
                risk_level: RiskLevel::None,
                max_output_bytes: Some(512 * 1024),
                allowed_in_plan_mode: Some(true),
                stop_turn_on_success: None,
                provider_visible: true,
                timeout_ms: Some(10_000),
            },
        }
    }

    pub fn accesses(&self, args: &serde_json::Value) -> crate::scheduler::ToolAccesses {
        let path = args
            .get("path")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        vec![ResourceAccess::read_file(path)]
    }

    pub fn execute(
        &self,
        args: &serde_json::Value,
        spill: Option<&dyn SpillStore>,
    ) -> ToolStream {
        let parsed: Result<ReadFileArgs, _> = serde_json::from_value(args.clone());
        let args = match parsed {
            Ok(a) => a,
            Err(e) => {
                return ToolStream::terminal_only(Err(ToolError::invalid_input(format!(
                    "read_file arguments: {e}"
                ))))
            }
        };

        // confine inside the workspace: resolve, then verify containment
        let root = match crate::canonical::canonicalize(&self.workspace_root) {
            Ok(r) => r,
            Err(e) => {
                return ToolStream::terminal_only(Err(ToolError::tool_failed(format!(
                    "workspace root unavailable: {e}"
                ))))
            }
        };
        let candidate = {
            let p = PathBuf::from(&args.path);
            if p.is_absolute() { p } else { root.join(p) }
        };
        let resolved = match crate::canonical::canonicalize(&candidate) {
            Ok(r) => r,
            Err(e) => {
                return ToolStream::terminal_only(Err(ToolError::tool_failed(format!(
                    "cannot read {}: {e}"
                , args.path))))
            }
        };
        if !resolved.starts_with(&root) {
            return ToolStream::terminal_only(Err(ToolError::invalid_input(
                "path escapes the workspace",
            )));
        }
        // regular files only (no dirs, no fifos)
        let meta = match std::fs::metadata(&resolved) {
            Ok(m) => m,
            Err(e) => {
                return ToolStream::terminal_only(Err(ToolError::tool_failed(format!(
                    "cannot stat {}: {e}"
                , args.path))))
            }
        };
        if !meta.is_file() {
            return ToolStream::terminal_only(Err(ToolError::invalid_input(format!(
                "{} is not a regular file",
                args.path
            ))));
        }

        let bytes = match std::fs::read(&resolved) {
            Ok(b) => b,
            Err(e) => {
                return ToolStream::terminal_only(Err(ToolError::tool_failed(format!(
                    "cannot read {}: {e}"
                , args.path))))
            }
        };

        // offset_line support: skip to the 1-based line
        let start = match args.offset_line {
            Some(line) if line > 1 => {
                let mut pos = 0usize;
                let mut seen = 1u64;
                for (i, b) in bytes.iter().enumerate() {
                    if *b == b'\n' {
                        seen += 1;
                        if seen == line {
                            pos = i + 1;
                            break;
                        }
                    }
                }
                pos
            }
            _ => 0,
        };
        let end = (start + args.max_bytes).min(bytes.len());
        let text = String::from_utf8_lossy(&bytes[start..end]).into_owned();
        let truncated = end < bytes.len();

        let full = if truncated {
            format!("{text}\n[truncated at {} of {} bytes]", end, bytes.len())
        } else {
            text
        };

        // over-budget spill (deepseek spill-policy semantics)
        let (inline, spill_ref) = match spill {
            Some(store) => crate::pipeline::apply_output_budget(
                store,
                crate::pipeline::SpillSource::Tool {
                    tool_name: "read_file".into(),
                    call_id: "builtin".into(),
                    label: args.path.clone(),
                },
                &full,
                64 * 1024,
            ),
            None => (full, None),
        };

        let value = json!({
            "path": args.path,
            "bytes": bytes.len(),
            "content": inline,
            "spill": spill_ref,
        });
        ToolStream::terminal_only(Ok(ToolOutput::from_value(value)))
    }
}

/// `list_dir` — read-only directory listing.
pub fn list_dir_tool(workspace_root: PathBuf) -> ErasedListDir {
    ErasedListDir { workspace_root }
}

pub struct ErasedListDir {
    workspace_root: PathBuf,
}

impl ErasedListDir {
    pub fn entry(&self) -> ToolEntry {
        ToolEntry {
            spec: ToolSpec {
                name: "list_dir".into(),
                namespace: None,
                title: Some("List directory".into()),
                description: "List entries of a directory inside the workspace.".into(),
                arguments_schema: Some(json!({
                    "type": "object",
                    "properties": { "path": { "type": "string" } },
                    "required": ["path"]
                })),
                kind: Some("fs".into()),
                behavior_version: Some("1".into()),
                idempotent: true,
                read_only: true,
                timeout_ms: Some(10_000),
                max_concurrency: None,
            },
            metadata: ToolMetadata {
                read_only: true,
                destructive: false,
                concurrent_safe: true,
                needs_approval: false,
                side_effect_scope: SideEffectScope::None,
                risk_level: RiskLevel::None,
                max_output_bytes: Some(256 * 1024),
                allowed_in_plan_mode: Some(true),
                stop_turn_on_success: None,
                provider_visible: true,
                timeout_ms: Some(10_000),
            },
        }
    }

    pub fn accesses(&self, args: &serde_json::Value) -> crate::scheduler::ToolAccesses {
        let path = args
            .get("path")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        vec![ResourceAccess::tree(crate::scheduler::FileAccessOperation::Read, path)]
    }

    pub fn execute(&self, args: &serde_json::Value) -> ToolStream {
        let Some(path) = args.get("path").and_then(|v| v.as_str()) else {
            return ToolStream::terminal_only(Err(ToolError::invalid_input("path required")));
        };
        let root = match crate::canonical::canonicalize(&self.workspace_root) {
            Ok(r) => r,
            Err(e) => {
                return ToolStream::terminal_only(Err(ToolError::tool_failed(format!(
                    "workspace root unavailable: {e}"
                ))))
            }
        };
        let p = PathBuf::from(path);
        let candidate = if p.is_absolute() { p } else { root.join(p) };
        let resolved = match crate::canonical::canonicalize(&candidate) {
            Ok(r) => r,
            Err(e) => {
                return ToolStream::terminal_only(Err(ToolError::tool_failed(format!(
                    "cannot list {path}: {e}"
                ))))
            }
        };
        if !resolved.starts_with(&root) {
            return ToolStream::terminal_only(Err(ToolError::invalid_input(
                "path escapes the workspace",
            )));
        }
        let mut entries: Vec<String> = Vec::new();
        let read_dir = match std::fs::read_dir(&resolved) {
            Ok(rd) => rd,
            Err(e) => {
                return ToolStream::terminal_only(Err(ToolError::tool_failed(format!(
                    "cannot list {path}: {e}"
                ))))
            }
        };
        for entry in read_dir.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
            entries.push(if dir { format!("{name}/") } else { name });
        }
        entries.sort();
        ToolStream::terminal_only(Ok(ToolOutput::from_value(
            json!({ "path": path, "entries": entries }),
        )))
    }
}
