//! okra-providers — model plane (MASTER-PLAN §3 #35-#36).
//!
//! - grok sampler shape: one trait over wire APIs, closed error taxonomy
//!   (401 uncharged park / 429 rate-limit park / context-length → compaction
//!   / transient bounded retry / permanent) (`sampler.rs`)
//! - kimi kosong shims: merge-consecutive-users, tool-call-id
//!   normalization, tool pairing (`messages.rs`)
//! - scripted model stub with fault injection (delay, truncate) for the
//!   killAtPhase harness (`sampler.rs`, §3 #63)

pub mod embeddings;
pub mod messages;
pub mod jev;
pub mod openai;
pub mod sampler;

pub use messages::{
    merge_consecutive_users, normalize_tool_call_id, pair_tool_calls, ContentBlock, Message,
    Role, StructuredOutput, ToolCall, ToolResult,
};
pub use openai::{OpenAiConfig, OpenAiProvider, DEFAULT_BASE_URL};
pub use sampler::{
    DoomLoopGuard, RetryBudget, SampleRequest, SampleResponse, Sampler, SamplerError,
    ScriptedModel, ScriptedStep, StopReason, TextStream, ToolView, Usage,
};
