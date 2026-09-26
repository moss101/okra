//! Normalize-before-hooks + spill store.
//!
//! Pipeline invariant (ZCode `call-runner.ts:~216,233,270`, MASTER-PLAN
//! §3 #14): **approved bytes = executed bytes.** Arguments are normalized
//! deliberately BEFORE PreToolUse hooks run; hooks approve/reject the exact
//! bytes that will be executed, and the executor consumes the frozen bytes
//! unchanged (deepseek's "args deep-frozen before policy" realization).
//!
//! Spill store (deepseek `packages/spill`): one method, `save_text`,
//! persists the FULL content verbatim or rejects on real storage failure —
//! rejection is best-effort upstream (keep the inline result).

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use crate::spec::ToolEntry;

// ---- invocation pipeline ----

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum PipelineError {
    #[error("normalizer failed for `{tool}`: {message}")]
    NormalizeFailed { tool: String, message: String },
    #[error("hook `{hook}` denied tool `{tool}`: {reason}")]
    HookDenied { hook: String, tool: String, reason: String },
    #[error("invalid arguments for `{tool}`: {message}")]
    InvalidArguments { tool: String, message: String },
}

/// Verdict of one PreToolUse hook.
#[derive(Debug, Clone, PartialEq)]
pub enum HookVerdict {
    Allow,
    Deny { reason: String },
}

/// PreToolUse hook: sees NORMALIZED arguments only.
pub trait PreToolUseHook: Send + Sync {
    fn name(&self) -> &str;
    fn on_tool_use(&self, tool: &str, normalized_args: &Value) -> HookVerdict;
}

/// Argument normalizer: deterministic, idempotent rewriting of raw model
/// arguments into the exact bytes the tool will execute.
pub trait ArgumentNormalizer: Send + Sync {
    fn normalize(&self, tool: &str, args: &Value) -> Result<Value, PipelineError>;
}

/// Pass-through normalizer (tools whose schema is already exact).
pub struct IdentityNormalizer;

impl ArgumentNormalizer for IdentityNormalizer {
    fn normalize(&self, _tool: &str, args: &Value) -> Result<Value, PipelineError> {
        Ok(args.clone())
    }
}

/// An approved invocation: normalized args, accepted by every hook, frozen
/// as bytes. The executor MUST execute these bytes and nothing else.
#[derive(Debug, Clone, PartialEq)]
pub struct ApprovedInvocation {
    pub tool: String,
    /// Canonical serialization of the normalized args — the approved bytes.
    pub args_json: String,
    /// Which hooks approved (for the audit trail / reverse-RPC UX).
    pub approved_by: Vec<String>,
}

impl ApprovedInvocation {
    pub fn args(&self) -> Value {
        serde_json::from_str(&self.args_json).expect("approved bytes are valid JSON")
    }
}

/// `normalize before hooks` (ZCode call-runner ordering): raw args →
/// normalizers (in order) → freeze bytes → hooks (in order) → approved
/// invocation. Hooks never see pre-normalization bytes; the executor never
/// sees post-approval mutations.
pub fn normalize_before_hooks(
    entry: &ToolEntry,
    raw_args: &Value,
    normalizers: &[&dyn ArgumentNormalizer],
    hooks: &[&dyn PreToolUseHook],
) -> Result<ApprovedInvocation, PipelineError> {
    let tool = &entry.spec.name;
    let mut args = raw_args.clone();
    for n in normalizers {
        args = n.normalize(tool, &args)?;
    }
    if !args.is_object() && entry.spec.arguments_schema.is_some() {
        return Err(PipelineError::InvalidArguments {
            tool: tool.clone(),
            message: "normalized arguments must be an object".into(),
        });
    }
    let args_json = serde_json::to_string(&args)
        .map_err(|e| PipelineError::InvalidArguments { tool: tool.clone(), message: e.to_string() })?;

    let mut approved_by = Vec::new();
    for hook in hooks {
        match hook.on_tool_use(tool, &args) {
            HookVerdict::Allow => approved_by.push(hook.name().to_string()),
            HookVerdict::Deny { reason } => {
                return Err(PipelineError::HookDenied {
                    hook: hook.name().to_string(),
                    tool: tool.clone(),
                    reason,
                });
            }
        }
    }
    Ok(ApprovedInvocation { tool: tool.clone(), args_json, approved_by })
}

// ---- spill store (deepseek packages/spill) ----

/// `SpillSource` (`types.ts:63-73`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SpillSource {
    Tool { tool_name: String, call_id: String, label: String },
    SessionReference { session_id: String, label: String },
}

/// `SpillRef` (`types.ts:76-80`): consumers never parse the locator.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SpillRef {
    pub locator: String,
    pub bytes: u64,
    pub retrieval_hint: String,
}

/// `SpillStore` (`index.ts:45-56`): persists FULL content verbatim; rejects
/// on real storage failure. No retrieval API here by design.
pub trait SpillStore: Send + Sync {
    fn save_text(&self, source: SpillSource, suggested_name: &str, content: &str) -> std::io::Result<SpillRef>;
}

/// Filesystem-backed spill store: sanitized one-segment name derived FROM
/// (never equal to) the suggestion + monotonic counter for collision
/// freedom.
pub struct FsSpillStore {
    root: Mutex<PathBuf>,
    counter: AtomicU64,
}

impl FsSpillStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        FsSpillStore { root: Mutex::new(root.into()), counter: AtomicU64::new(0) }
    }

    fn sanitize(name: &str) -> String {
        let cleaned: String = name
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_' { c } else { '_' })
            .collect();
        let trimmed = cleaned.trim_matches('_');
        if trimmed.is_empty() { "spill".to_string() } else { trimmed.to_string() }
    }
}

impl SpillStore for FsSpillStore {
    fn save_text(&self, source: SpillSource, suggested_name: &str, content: &str) -> std::io::Result<SpillRef> {
        let root = self.root.lock().unwrap().clone();
        std::fs::create_dir_all(&root)?;
        let n = self.counter.fetch_add(1, Ordering::SeqCst);
        let label = match &source {
            SpillSource::Tool { tool_name, call_id, .. } => format!("{tool_name}-{call_id}"),
            SpillSource::SessionReference { session_id, .. } => session_id.clone(),
        };
        let name = format!("{}-{}-{}.txt", Self::sanitize(suggested_name), Self::sanitize(&label), n);
        let path = root.join(&name);
        std::fs::write(&path, content)?; // FULL content verbatim
        Ok(SpillRef {
            locator: path.to_string_lossy().into_owned(),
            bytes: content.len() as u64,
            retrieval_hint: format!(
                "Full output spilled to {name}; read the file head for the retained excerpt."
            ),
        })
    }
}

/// Retention strategy (deepseek `util/output-retention`, `index.ts:93-110`):
/// head | tail | head+tail. Cuts preserve UTF-8 boundaries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TextRetentionStrategy {
    Head { max_bytes: usize },
    Tail { max_bytes: usize },
    HeadTail { head_bytes: usize, tail_bytes: usize },
}

/// `RetainedText` (`index.ts:80-84`).
#[derive(Debug, Clone, PartialEq)]
pub struct RetainedText {
    pub text: String,
    pub truncated: bool,
    pub omitted_bytes: u64,
}

fn floor_char_boundary(bytes: &[u8], mut idx: usize) -> usize {
    while idx > 0 && (bytes[idx] & 0xC0) == 0x80 {
        idx -= 1;
    }
    idx
}

fn ceil_char_boundary(bytes: &[u8], mut idx: usize) -> usize {
    while idx < bytes.len() && (bytes[idx] & 0xC0) == 0x80 {
        idx += 1;
    }
    idx
}

/// Apply retention; the omitted middle is marked with a mechanical gap
/// notice (the tool supplies recovery wording).
pub fn retain_text(text: &str, strategy: TextRetentionStrategy) -> RetainedText {
    let bytes = text.as_bytes();
    let total = bytes.len();
    let cut = |head: usize, tail: usize| -> RetainedText {
        if head + tail >= total {
            return RetainedText { text: text.to_string(), truncated: false, omitted_bytes: 0 };
        }
        let head_end = ceil_char_boundary(bytes, head.min(total));
        let tail_start = floor_char_boundary(bytes, total.saturating_sub(tail));
        let omitted = (tail_start - head_end) as u64;
        let gap = format!("\n[... {omitted} bytes omitted ...]\n");
        RetainedText {
            text: format!(
                "{}{}{}",
                String::from_utf8_lossy(&bytes[..head_end]),
                gap,
                String::from_utf8_lossy(&bytes[tail_start..])
            ),
            truncated: true,
            omitted_bytes: omitted,
        }
    };
    match strategy {
        TextRetentionStrategy::Head { max_bytes } => {
            if max_bytes >= total {
                RetainedText { text: text.to_string(), truncated: false, omitted_bytes: 0 }
            } else {
                let end = ceil_char_boundary(bytes, max_bytes);
                RetainedText {
                    text: format!(
                        "{}\n[... {} bytes omitted ...]",
                        String::from_utf8_lossy(&bytes[..end]),
                        (total - end) as u64
                    ),
                    truncated: true,
                    omitted_bytes: (total - end) as u64,
                }
            }
        }
        TextRetentionStrategy::Tail { max_bytes } => {
            if max_bytes >= total {
                RetainedText { text: text.to_string(), truncated: false, omitted_bytes: 0 }
            } else {
                let start = floor_char_boundary(bytes, total - max_bytes);
                RetainedText {
                    text: format!(
                        "[... {} bytes omitted ...]\n{}",
                        start as u64,
                        String::from_utf8_lossy(&bytes[start..])
                    ),
                    truncated: true,
                    omitted_bytes: start as u64,
                }
            }
        }
        TextRetentionStrategy::HeadTail { head_bytes, tail_bytes } => cut(head_bytes, tail_bytes),
    }
}

/// The combined budget policy: over-budget tool output spills the full text
/// to the store and retains head+tail inline (deepseek spill-policy,
/// `index.ts:91-131`); spill rejection degrades to inline retention
/// (best-effort).
pub fn apply_output_budget(
    store: &dyn SpillStore,
    source: SpillSource,
    text: &str,
    max_inline_bytes: usize,
) -> (String, Option<SpillRef>) {
    if text.len() <= max_inline_bytes {
        return (text.to_string(), None);
    }
    let spill = store
        .save_text(
            source,
            "tool-output",
            text,
        )
        .ok();
    let retained = retain_text(
        text,
        TextRetentionStrategy::HeadTail {
            head_bytes: max_inline_bytes / 2,
            tail_bytes: max_inline_bytes / 2,
        },
    );
    (retained.text, spill)
}

/// Lexically confine `candidate` under `root` without touching the
/// filesystem (write paths that do not exist yet).
/// Lexically resolve `candidate` under `root`. A `..` that would climb out
/// of the root REFUSES the path (None) — silently dropping it would turn
/// `../x` into `x` inside the workspace (a path-traversal hole).
pub fn confine_lexical(root: &Path, candidate: &Path) -> Option<PathBuf> {
    let mut out = root.to_path_buf();
    for component in candidate.components() {
        match component {
            std::path::Component::Normal(c) => out.push(c),
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                if !out.pop() {
                    return None; // escapes the root: refuse
                }
            }
            std::path::Component::RootDir | std::path::Component::Prefix(_) => {
                return None; // absolute component under a relative root: refuse
            }
        }
    }
    Some(out)
}

/// Ensure a path is inside the workspace root — used by builtins (M0) and
/// subagent FS isolation later (M5).
pub fn confine_path_inside(root: &Path, candidate: &Path) -> Option<PathBuf> {
    let root_clean = crate::canonical::canonicalize(root).ok()?;
    let cand = if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        root_clean.join(candidate)
    };
    let cand_clean = crate::canonical::canonicalize(&cand).ok()?;
    if cand_clean.starts_with(&root_clean) {
        Some(cand_clean)
    } else {
        None
    }
}
