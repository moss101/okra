//! okra-providers — model plane (MASTER-PLAN §3 #35-#37).
//!
//! - grok sampler shape: one trait over wire APIs, closed error taxonomy
//!   (401 uncharged park / 429 rate-limit park / context-length → compaction
//!   / transient bounded retry / permanent) (`sampler.rs`)
//! - kimi kosong shims: merge-consecutive-users, tool-call-id
//!   normalization, tool pairing (`messages.rs`)
//! - scripted model stub with fault injection (delay, truncate) for the
//!   killAtPhase harness (`sampler.rs`, §3 #63)
//! - qwen XML tool-call recovery: text-embedded calls become REAL calls
//!   through the same approved-bytes pipeline (`recovery.rs`, §3 #37)
//! - qwen model fallback: candidate chain with recorded switch events,
//!   never silent (`fallback.rs`, §3 #37)

pub mod embeddings;
pub mod fallback;
pub mod messages;
pub mod jev;
pub mod openai;
pub mod recovery;
pub mod sampler;

pub use fallback::{FallbackEvent, FallbackSampler, SamplerFactory};
pub use messages::{
    merge_consecutive_users, normalize_tool_call_id, pair_tool_calls, ContentBlock, Message,
    Role, StructuredOutput, ToolCall, ToolResult,
};
pub use openai::{OpenAiConfig, OpenAiProvider, DEFAULT_BASE_URL};
pub use recovery::recover_xml_tool_calls;
pub use sampler::{
    DoomLoopGuard, RetryBudget, SampleRequest, SampleResponse, Sampler, SamplerError,
    ScriptedModel, ScriptedStep, StopReason, TextStream, ToolView, Usage,
};
