//! Tool stream shape — port of grok `xai-tool-runtime/src/tool.rs`.
//!
//! The invariant (`tool.rs:12-14`): a tool call's stream carries **arbitrarily
//! many `Progress` items ending in exactly one `Terminal`**. A stream that
//! ends without a Terminal is a protocol violation (`dispatch.rs:46-67`,
//! `stream_no_terminal`).

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ToolProgress {
    Text { text: String },
    Content { blocks: Vec<ContentBlock> },
    /// Tool-private payload; `subkind` is the tool's own stable snake_case
    /// discriminator, kept one level below the serde tag (grok `tool.rs:151`).
    Custom { subkind: String, payload: Value },
}

/// `ContentBlock` (`tool.rs:167-201`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text { text: String },
    Image {
        mime_type: String,
        /// base64
        data: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        media_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        filename: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        path: Option<String>,
    },
    Resource {
        uri: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        mime_type: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        text: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ToolError {
    /// The tool failed but the model may adapt from the message.
    ToolFailed { message: String },
    /// The call itself was malformed (bad arguments, unknown tool).
    InvalidInput { message: String },
    Custom { code: String, message: String },
}

impl ToolError {
    pub fn tool_failed(message: impl Into<String>) -> Self {
        ToolError::ToolFailed { message: message.into() }
    }
    pub fn invalid_input(message: impl Into<String>) -> Self {
        ToolError::InvalidInput { message: message.into() }
    }
}

/// Final tool result. `model_output` is what the model sees (MCP invariant,
/// grok `tool.rs:262-278`: always non-empty — falls back to extraction from
/// the serialized value).
#[derive(Debug, Clone, PartialEq)]
pub struct ToolOutput {
    pub value: Value,
    pub model_output: Vec<ContentBlock>,
}

impl ToolOutput {
    pub fn from_value(v: Value) -> Self {
        let model_output = extract_content_blocks(&v);
        ToolOutput { value: v, model_output }
    }
    pub fn text(text: impl Into<String>) -> Self {
        let text = text.into();
        ToolOutput {
            model_output: vec![ContentBlock::Text { text: text.clone() }],
            value: Value::String(text),
        }
    }
}

/// `extract_content_blocks` (grok `render.rs`): pull Text blocks out of an
/// arbitrary value's string forms so `model_output` is never empty.
pub fn extract_content_blocks(v: &Value) -> Vec<ContentBlock> {
    match v {
        Value::String(s) => vec![ContentBlock::Text { text: s.clone() }],
        Value::Object(o) => {
            if let Some(Value::String(text)) =
                o.get("text").or_else(|| o.get("output")).or_else(|| o.get("content"))
            {
                return vec![ContentBlock::Text { text: text.clone() }];
            }
            vec![ContentBlock::Text { text: v.to_string() }]
        }
        other => vec![ContentBlock::Text { text: other.to_string() }],
    }
}

/// `ToolStreamItem` (`tool.rs:121-126`).
#[derive(Debug, Clone, PartialEq)]
pub enum ToolStreamItem {
    Progress(ToolProgress),
    Terminal(Result<ToolOutput, ToolError>),
}

/// A validated tool stream. Constructed only through builders that enforce
/// [Progress*, exactly one Terminal].
#[derive(Debug, Clone, PartialEq)]
pub struct ToolStream {
    items: Vec<ToolStreamItem>,
}

impl ToolStream {
    /// `terminal_only` (`tool.rs:207`).
    pub fn terminal_only(result: Result<ToolOutput, ToolError>) -> Self {
        ToolStream { items: vec![ToolStreamItem::Terminal(result)] }
    }

    /// `with_progress(progress, terminal)` (`tool.rs:234`).
    pub fn with_progress(progress: Vec<ToolProgress>, terminal: Result<ToolOutput, ToolError>) -> Self {
        let mut items: Vec<ToolStreamItem> = progress.into_iter().map(ToolStreamItem::Progress).collect();
        items.push(ToolStreamItem::Terminal(terminal));
        ToolStream { items }
    }

    /// Raw adapter for wrappers that collect items from arbitrary sources
    /// (e.g. streaming runtimes). Prefer the builders; anything you build
    /// here must still pass `validate`.
    pub fn from_items_unchecked(items: Vec<ToolStreamItem>) -> Self {
        ToolStream { items }
    }

    /// Validate the invariant. Returns `stream_no_terminal` when violated
    /// (grok dispatch treats it as a protocol violation).
    pub fn validate(&self) -> Result<(), ToolError> {
        let mut saw_terminal = false;
        for item in &self.items {
            if saw_terminal {
                return Err(ToolError::Custom {
                    code: "stream_items_after_terminal".into(),
                    message: "progress item after terminal".into(),
                });
            }
            if matches!(item, ToolStreamItem::Terminal(_)) {
                saw_terminal = true;
            }
        }
        if !saw_terminal {
            return Err(ToolError::Custom {
                code: "stream_no_terminal".into(),
                message: "tool stream ended without a terminal item".into(),
            });
        }
        Ok(())
    }

    pub fn items(&self) -> &[ToolStreamItem] {
        &self.items
    }

    pub fn into_items(self) -> Vec<ToolStreamItem> {
        self.items
    }

    /// The terminal item, if this stream is well-formed.
    pub fn terminal(&self) -> Option<&Result<ToolOutput, ToolError>> {
        self.items.last().and_then(|i| match i {
            ToolStreamItem::Terminal(t) => Some(t),
            _ => None,
        })
    }
}

/// Partial-result delta contract (grok `streaming.rs:29-47`): deltas are
/// append-only and lossless; `gap` marks a lost oversized tick, `truncated`
/// is the caller's cumulative upstream loss flag.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "snake_case")]
pub struct PartialResultPayload {
    pub delta: String,
    pub total_bytes: u64,
    pub truncated: bool,
    pub gap: bool,
}

/// Default per-delta cap (grok `streaming.rs:18`).
pub const DEFAULT_MAX_DELTA_BYTES: usize = 16 * 1024;

/// Emit a lossless UTF-8 delta from `tail` (bytes added since last call),
/// holding back an incomplete UTF-8 suffix rather than splitting a char.
pub fn stream_chunk(
    tail: &[u8],
    total_bytes: u64,
    truncated: bool,
    max_delta_bytes: usize,
) -> (Option<PartialResultPayload>, usize) {
    let max = max_delta_bytes.min(DEFAULT_MAX_DELTA_BYTES.max(1));
    let mut take = tail.len().min(max);
    while take < tail.len() && tail[take] & 0xC0 == 0x80 {
        take -= 1; // hold back incomplete suffix
    }
    if take == 0 {
        return (
            Some(PartialResultPayload {
                delta: String::new(),
                total_bytes,
                truncated,
                gap: true,
            }),
            0,
        );
    }
    let delta = String::from_utf8_lossy(&tail[..take]).into_owned();
    (
        Some(PartialResultPayload { delta, total_bytes, truncated, gap: false }),
        take,
    )
}
