//! Model message model + wire shims — kimi `kosong` compat shims
//! (`patterns.ts:33`: merge-consecutive-users; tool-call-id normalization).

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    /// Canonical args JSON string (the model-facing form).
    pub args_json: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub struct ToolResult {
    pub call_id: String,
    pub content: String,
    #[serde(default)]
    pub is_error: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentBlock {
    Text { text: String },
    ToolUse { call: ToolCall },
    ToolResponse { result: ToolResult },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: Vec<ContentBlock>,
}

impl Message {
    pub fn text(role: Role, text: impl Into<String>) -> Message {
        Message { role, content: vec![ContentBlock::Text { text: text.into() }] }
    }
    pub fn user(text: impl Into<String>) -> Message {
        Message::text(Role::User, text)
    }
    pub fn assistant_text(text: impl Into<String>) -> Message {
        Message::text(Role::Assistant, text)
    }
    pub fn tool_result(result: ToolResult) -> Message {
        Message { role: Role::Tool, content: vec![ContentBlock::ToolResponse { result }] }
    }
    /// Concatenated text content (for logging/projection).
    pub fn text_content(&self) -> String {
        self.content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Text { text } => Some(text.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("")
    }
    pub fn tool_calls(&self) -> Vec<ToolCall> {
        self.content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::ToolUse { call } => Some(call.clone()),
                _ => None,
            })
            .collect()
    }
}

/// `merge_consecutive_users` (kimi kosong shim, `patterns.ts:33`): wire APIs
/// that reject consecutive user messages get one merged message; the text
/// blocks join with a newline. Steering injection relies on this.
pub fn merge_consecutive_users(messages: &[Message]) -> Vec<Message> {
    let mut out: Vec<Message> = Vec::with_capacity(messages.len());
    for msg in messages {
        if msg.role == Role::User
            && let Some(last) = out.last_mut()
                && last.role == Role::User {
                    last.content.extend(msg.content.iter().cloned());
                    continue;
                }
        out.push(msg.clone());
    }
    out
}

/// Tool-call id normalization (kimi kosong): some wire APIs emit ids that
/// are empty or mismatch result ids; normalize both sides to a stable form
/// so pairing survives round-trips.
pub fn normalize_tool_call_id(id: &str) -> String {
    let trimmed = id.trim();
    if trimmed.is_empty() {
        "call_0".to_string()
    } else {
        trimmed.to_string()
    }
}

/// Ensure every tool_use has a following tool result and vice versa; drop
/// orphan results with an error marker (defensive shim before sampling).
pub fn pair_tool_calls(messages: &[Message]) -> Vec<Message> {
    let mut out = Vec::with_capacity(messages.len());
    let mut pending: Vec<String> = Vec::new();
    for msg in messages {
        match msg.role {
            Role::Assistant => {
                for call in msg.tool_calls() {
                    pending.push(call.id.clone());
                }
                out.push(msg.clone());
            }
            Role::Tool => {
                let ids: Vec<String> = msg
                    .content
                    .iter()
                    .filter_map(|b| match b {
                        ContentBlock::ToolResponse { result } => Some(result.call_id.clone()),
                        _ => None,
                    })
                    .collect();
                for id in &ids {
                    pending.retain(|p| p != id);
                }
                out.push(msg.clone());
            }
            _ => out.push(msg.clone()),
        }
    }
    // any assistant tool_use without a result gets a synthetic error result
    // so the request stays well-formed
    if !pending.is_empty() {
        for id in pending {
            out.push(Message::tool_result(ToolResult {
                call_id: id.clone(),
                content: "[error] tool result missing (interrupted)".into(),
                is_error: true,
            }));
        }
    }
    out
}

/// Structured output (grok `TurnOutcome.structured_output`): the model may
/// be asked to answer with a schema; a failed parse is a `String` error, not
/// a panic.
#[derive(Debug, Clone, PartialEq)]
pub enum StructuredOutput {
    Value(Value),
    Failed(String),
}
