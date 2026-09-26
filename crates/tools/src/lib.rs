//! okra-tools — the tool plane (MASTER-PLAN §3 #13-#18, M1).
//!
//! Fused blocks:
//! - grok `Tool` trait streaming shape: `[Progress*, exactly one Terminal]`
//!   (`stream.rs`, xai-tool-runtime/src/tool.rs)
//! - ZCode `ToolMetadata` policy metadata + normalize-before-hooks
//!   ordering, approved bytes = executed bytes (`spec.rs`, `pipeline.rs`)
//! - kimi `ToolAccesses` conflict-based parallel scheduling (`scheduler.rs`,
//!   agent-core-v2/src/tool/toolContract.ts + toolScheduler.ts)
//! - deepseek spill store + output retention (`pipeline.rs`,
//!   packages/spill)
//! - clean-room `ToolSpec.idempotent` for safe auto-retry (§3 #18)

pub mod builtins;
pub mod canonical;
pub mod pipeline;
pub mod registry;
pub mod scheduler;
pub mod spec;
pub mod stream;

pub use pipeline::{
    apply_output_budget, normalize_before_hooks, retain_text, ApprovedInvocation, ArgumentNormalizer,
    FsSpillStore, HookVerdict, IdentityNormalizer, PipelineError, PreToolUseHook, RetainedText,
    SpillRef, SpillSource, SpillStore, TextRetentionStrategy,
};
pub use registry::{ErasedTool, Registry, RegistryError};
pub use scheduler::{
    accesses_conflict, normalize_path, resource_accesses_conflict, FileAccessOperation,
    ResourceAccess, ToolAccesses, ToolScheduler,
};
pub use spec::{RiskLevel, SideEffectScope, ToolEntry, ToolMetadata, ToolSpec};
pub use stream::{
    extract_content_blocks, stream_chunk, ContentBlock, PartialResultPayload, ToolError,
    ToolOutput, ToolProgress, ToolStream, ToolStreamItem, DEFAULT_MAX_DELTA_BYTES,
};
