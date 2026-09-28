//! Builtin tools for the M0 gate: `read_file` (the one tool driven
//! end-to-end through the daemon per MASTER-PLAN M0) plus `list_dir`.
//! Read-only, idempotent — exactly the tools auto-retry may re-issue.

use serde::Deserialize;
use serde_json::json;
use std::path::PathBuf;

use crate::pipeline::SpillStore;
use crate::scheduler::{FileAccessOperation, ResourceAccess};
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

// ---- write_file / edit_file (M1: real multi-file coding tasks, G1) ----
//
// Durability contract (G1 "no torn state"): content is written to a temp
// file in the SAME directory, fsynced, then renamed over the target —
// a crash at any instant leaves either the old or the new file, never a
// partial one.

/// Atomic POSIX write: temp sibling + fsync + rename.
pub fn atomic_write(path: &std::path::Path, bytes: &[u8]) -> Result<(), ToolError> {
    use std::io::Write;
    let dir = path
        .parent()
        .ok_or_else(|| ToolError::invalid_input("path has no parent directory"))?;
    std::fs::create_dir_all(dir)
        .map_err(|e| ToolError::tool_failed(format!("cannot create directory {}: {e}", dir.display())))?;
    let tmp = dir.join(format!(
        ".{}.okra-tmp-{}",
        path.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "file".into()),
        std::process::id(),
    ));
    {
        let mut f = std::fs::File::create(&tmp)
            .map_err(|e| ToolError::tool_failed(format!("cannot create temp file: {e}")))?;
        f.write_all(bytes)
            .and_then(|_| f.flush())
            .and_then(|_| f.sync_all())
            .map_err(|e| ToolError::tool_failed(format!("cannot write temp file: {e}")))?;
    }
    // An existing target's mode (exec bit on scripts, restrictive modes)
    // belongs to the file, not to this write — a fresh temp file would
    // otherwise reset it on rename (dogfood day-1 finding: overwriting a
    // script silently dropped +x). Carry the mode across the rename.
    #[cfg(unix)]
    if let Ok(meta) = std::fs::metadata(path) {
        use std::os::unix::fs::PermissionsExt;
        let mode = meta.permissions().mode();
        let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode));
    }
    match std::fs::rename(&tmp, path) {
        Ok(()) => {
            // fsync the directory so the rename itself is durable
            if let Ok(d) = std::fs::File::open(dir) {
                let _ = d.sync_all();
            }
            Ok(())
        }
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(ToolError::tool_failed(format!("cannot rename into place: {e}")))
        }
    }
}

/// Resolve a workspace-relative path; the shared confinement check used by
/// every filesystem builtin.
pub fn resolve_in_workspace(
    workspace_root: &std::path::Path,
    path: &str,
) -> Result<std::path::PathBuf, ToolError> {
    let root = crate::canonical::canonicalize(workspace_root)
        .map_err(|e| ToolError::tool_failed(format!("workspace root unavailable: {e}")))?;
    let p = std::path::PathBuf::from(path);
    let candidate = if p.is_absolute() {
        p.clone()
    } else {
        root.join(&p)
    };
    if let Ok(resolved) = crate::canonical::canonicalize(&candidate) {
        if !resolved.starts_with(&root) {
            return Err(ToolError::invalid_input("path escapes the workspace"));
        }
        return Ok(resolved);
    }
    // Target does not exist yet (write path): confine lexically. Always
    // resolve against the ORIGINAL `path` spelling — joining onto the
    // canonical root first would double-apply the root for absolute paths.
    let normalized = if p.is_absolute() {
        let lex = crate::pipeline::confine_lexical(std::path::Path::new("/"), &p)
            .ok_or(ToolError::invalid_input("path escapes the workspace"))?;
        if !lex.starts_with(&root) {
            return Err(ToolError::invalid_input("path escapes the workspace"));
        }
        lex
    } else {
        let lex = crate::pipeline::confine_lexical(&root, &p)
            .ok_or(ToolError::invalid_input("path escapes the workspace"))?;
        if !lex.starts_with(&root) {
            return Err(ToolError::invalid_input("path escapes the workspace"));
        }
        lex
    };
    Ok(normalized)
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WriteFileArgs {
    pub path: String,
    pub content: String,
}

pub struct ErasedWriteFile {
    workspace_root: PathBuf,
}

impl ErasedWriteFile {
    pub fn new(workspace_root: PathBuf) -> Self {
        ErasedWriteFile { workspace_root }
    }

    pub fn entry(&self) -> ToolEntry {
        ToolEntry {
            spec: ToolSpec {
                name: "write_file".into(),
                namespace: None,
                title: Some("Write file".into()),
                description: "Create or overwrite a file with full content (atomic).".into(),
                arguments_schema: Some(json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" },
                        "content": { "type": "string" }
                    },
                    "required": ["path", "content"]
                })),
                kind: Some("fs".into()),
                behavior_version: Some("1".into()),
                // same args → same final file state: safe to auto-retry
                idempotent: true,
                read_only: false,
                timeout_ms: Some(10_000),
                max_concurrency: None,
            },
            metadata: ToolMetadata {
                read_only: false,
                destructive: false, // overwrite is recoverable via checkpoints
                concurrent_safe: false,
                needs_approval: true,
                side_effect_scope: SideEffectScope::Workspace,
                risk_level: RiskLevel::Medium,
                max_output_bytes: None,
                allowed_in_plan_mode: Some(false),
                stop_turn_on_success: None,
                provider_visible: true,
                timeout_ms: Some(10_000),
            },
        }
    }

    pub fn accesses(&self, args: &serde_json::Value) -> crate::scheduler::ToolAccesses {
        let path = args.get("path").and_then(|v| v.as_str()).unwrap_or_default().to_string();
        vec![ResourceAccess::write_file(path)]
    }

    pub fn execute(&self, args: &serde_json::Value) -> ToolStream {
        let parsed: Result<WriteFileArgs, _> = serde_json::from_value(args.clone());
        let args = match parsed {
            Ok(a) => a,
            Err(e) => return ToolStream::terminal_only(Err(ToolError::invalid_input(format!("write_file arguments: {e}")))),
        };
        let resolved = match resolve_in_workspace(&self.workspace_root, &args.path) {
            Ok(r) => r,
            Err(e) => return ToolStream::terminal_only(Err(e)),
        };
        match atomic_write(&resolved, args.content.as_bytes()) {
            Ok(()) => ToolStream::terminal_only(Ok(ToolOutput::from_value(json!({
                "path": args.path,
                "bytes": args.content.len(),
                "written": true,
            })))),
            Err(e) => ToolStream::terminal_only(Err(e)),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EditFileArgs {
    pub path: String,
    pub old_text: String,
    pub new_text: String,
    #[serde(default)]
    pub replace_all: bool,
}

pub struct ErasedEditFile {
    workspace_root: PathBuf,
}

impl ErasedEditFile {
    pub fn new(workspace_root: PathBuf) -> Self {
        ErasedEditFile { workspace_root }
    }

    pub fn entry(&self) -> ToolEntry {
        ToolEntry {
            spec: ToolSpec {
                name: "edit_file".into(),
                namespace: None,
                title: Some("Edit file".into()),
                description: "Replace an exact substring in a file (atomic write).".into(),
                arguments_schema: Some(json!({
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" },
                        "oldText": { "type": "string" },
                        "newText": { "type": "string" },
                        "replaceAll": { "type": "boolean" }
                    },
                    "required": ["path", "oldText", "newText"]
                })),
                kind: Some("fs".into()),
                behavior_version: Some("1".into()),
                // same old_text → same final state; a second application fails
                // exact-match without corrupting anything
                idempotent: true,
                read_only: false,
                timeout_ms: Some(10_000),
                max_concurrency: None,
            },
            metadata: ToolMetadata {
                read_only: false,
                destructive: false,
                concurrent_safe: false,
                needs_approval: true,
                side_effect_scope: SideEffectScope::Workspace,
                risk_level: RiskLevel::Medium,
                max_output_bytes: None,
                allowed_in_plan_mode: Some(false),
                stop_turn_on_success: None,
                provider_visible: true,
                timeout_ms: Some(10_000),
            },
        }
    }

    pub fn accesses(&self, args: &serde_json::Value) -> crate::scheduler::ToolAccesses {
        let path = args.get("path").and_then(|v| v.as_str()).unwrap_or_default().to_string();
        vec![ResourceAccess::file(FileAccessOperation::Readwrite, path)]
    }

    pub fn execute(&self, args: &serde_json::Value) -> ToolStream {
        let parsed: Result<EditFileArgs, _> = serde_json::from_value(args.clone());
        let args = match parsed {
            Ok(a) => a,
            Err(e) => return ToolStream::terminal_only(Err(ToolError::invalid_input(format!("edit_file arguments: {e}")))),
        };
        if args.old_text.is_empty() {
            return ToolStream::terminal_only(Err(ToolError::invalid_input("oldText must not be empty")));
        }
        let resolved = match resolve_in_workspace(&self.workspace_root, &args.path) {
            Ok(r) => r,
            Err(e) => return ToolStream::terminal_only(Err(e)),
        };
        let content = match std::fs::read_to_string(&resolved) {
            Ok(c) => c,
            Err(e) => return ToolStream::terminal_only(Err(ToolError::tool_failed(format!("cannot read {}: {e}", args.path)))),
        };
        let occurrences = content.matches(&args.old_text).count();
        if occurrences == 0 {
            return ToolStream::terminal_only(Err(ToolError::tool_failed(format!(
                "oldText not found in {} (file unchanged)",
                args.path
            ))));
        }
        let updated = if args.replace_all {
            content.replace(&args.old_text, &args.new_text)
        } else {
            content.replacen(&args.old_text, &args.new_text, 1)
        };
        match atomic_write(&resolved, updated.as_bytes()) {
            Ok(()) => ToolStream::terminal_only(Ok(ToolOutput::from_value(json!({
                "path": args.path,
                "replacements": if args.replace_all { occurrences } else { 1 },
                "bytes": updated.len(),
            })))),
            Err(e) => ToolStream::terminal_only(Err(e)),
        }
    }
}
