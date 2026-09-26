//! Port of `core.ts` — protocol version, payload caps, delivery profiles.
//!
//! Clock rule (`core.ts:18`): Unix ms, CLI clock only; clients must never
//! subtract their local clock from a protocol timestamp.
//!
//! Delivery-profile rule (`core.ts:22`): profiles exist only as a parameter
//! table for the flush pipeline; profile *variables* never appear in client
//! code paths.

use serde::{Deserialize, Serialize};

/// V4 physical wire protocol version (`core.ts:7`).
/// The projection snapshot keeps its own independent `protocolVersion = 1`.
pub const V4_WIRE_PROTOCOL_VERSION: u32 = 3;

/// Unix-milliseconds CLI clock (`core.ts:19`).
pub type Timestamp = f64;

/// V3 row target (`core.ts:10`): a display row and its stable entity are
/// committed as a pair and validated by the same authoritative projection.
/// Part of the ported contract; consumed with the M3 snapshot crate.
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConversationRowTarget {
    pub row_id: u64,
    #[serde(rename = "entityId")]
    pub entity_id: String,
}

/// Streamable projection paths (`core.ts:23`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum StreamablePath {
    Text,
    InputText,
    #[serde(rename = "output.text")]
    OutputText,
    SummaryText,
}

impl StreamablePath {
    pub const ALL: [StreamablePath; 4] = [
        StreamablePath::Text,
        StreamablePath::InputText,
        StreamablePath::OutputText,
        StreamablePath::SummaryText,
    ];

    /// The exact wire spelling used by `streamablePathSchema` (`core.ts:23`).
    pub fn wire_name(self) -> &'static str {
        match self {
            StreamablePath::Text => "text",
            StreamablePath::InputText => "inputText",
            StreamablePath::OutputText => "output.text",
            StreamablePath::SummaryText => "summaryText",
        }
    }
}

/// Per-path stream switches, keyed exactly like `DeliveryProfile.streamPaths`
/// (`core.ts:29`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamablePathSet {
    pub text: bool,
    #[serde(rename = "inputText")]
    pub input_text: bool,
    #[serde(rename = "output.text")]
    pub output_text: bool,
    #[serde(rename = "summaryText")]
    pub summary_text: bool,
}

impl StreamablePathSet {
    pub fn get(self, path: StreamablePath) -> bool {
        match path {
            StreamablePath::Text => self.text,
            StreamablePath::InputText => self.input_text,
            StreamablePath::OutputText => self.output_text,
            StreamablePath::SummaryText => self.summary_text,
        }
    }
}

/// `DeliveryProfile` (`core.ts:26-32`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeliveryProfile {
    #[serde(rename = "desktopOnlyRows")]
    pub desktop_only_rows: bool,
    #[serde(rename = "flushWindowMs")]
    pub flush_window_ms: u64,
    #[serde(rename = "streamPaths")]
    pub stream_paths: StreamablePathSet,
    #[serde(rename = "streamOutputCapBytes")]
    pub stream_output_cap_bytes: u64,
    #[serde(rename = "toolProgress")]
    pub tool_progress: bool,
}

/// The two named profiles (`core.ts:34-59`) — values transcribed verbatim.
pub const DELIVERY_PROFILES: [(&str, DeliveryProfile); 2] = [
    (
        "continuous",
        DeliveryProfile {
            desktop_only_rows: true,
            flush_window_ms: 30,
            stream_paths: StreamablePathSet {
                text: true,
                input_text: true,
                output_text: true,
                summary_text: true,
            },
            stream_output_cap_bytes: 262_144,
            tool_progress: false,
        },
    ),
    (
        "replayable",
        DeliveryProfile {
            desktop_only_rows: false,
            flush_window_ms: 150,
            stream_paths: StreamablePathSet {
                text: true,
                input_text: false,
                output_text: false,
                summary_text: false,
            },
            stream_output_cap_bytes: 0,
            tool_progress: true,
        },
    ),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryProfileName {
    Continuous,
    Replayable,
}

/// Profile lookup — the only sanctioned way profile state enters a pipeline
/// (`core.ts:22` comment: profiles live in the flush pipeline's parameter
/// table).
pub fn delivery_profile(name: DeliveryProfileName) -> &'static DeliveryProfile {
    let idx = match name {
        DeliveryProfileName::Continuous => 0,
        DeliveryProfileName::Replayable => 1,
    };
    &DELIVERY_PROFILES[idx].1
}

/// Constants & limits (`core.ts:64-100`). Initial values, tuned by
/// measurement — do not "fix" without a benchmark. The attachment-stat
/// comment is donor-authored context preserved because it explains why two
/// similar-looking limits differ by 100x.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProtocolV4Limits;

impl ProtocolV4Limits {
    pub const MAX_FRAME_BYTES: usize = 1024 * 1024;
    pub const LOGICAL_FRAME_ASSEMBLY_MAX_BYTES: usize = 16 * 1024 * 1024;
    pub const LOGICAL_FRAME_ASSEMBLY_MAX_FRAGMENTS: usize = 1024;
    pub const LOGICAL_FRAME_ASSEMBLY_MAX_CONCURRENT: usize = 32;
    pub const LOGICAL_FRAME_ASSEMBLY_MAX_STAGED_BYTES: usize = 32 * 1024 * 1024;
    pub const LOGICAL_FRAME_ASSEMBLY_TIMEOUT_MS: u64 = 30_000;
    pub const TRANSPORT_ENVELOPE_ID_MAX_CHARS: usize = 256;
    pub const SUBSCRIBER_BUFFER_MAX_OPS: usize = 500;
    pub const SUBSCRIBER_BUFFER_MAX_BYTES: usize = 1024 * 1024;
    pub const EVENT_RETENTION_PER_SESSION: usize = 2000;
    pub const SNAPSHOT_TAIL_WINDOW_ROWS: usize = 60;
    pub const ROWS_RANGE_MAX_LIMIT: usize = 200;
    pub const TOOL_OUTPUT_FINAL_HEAD_BYTES: usize = 32 * 1024;
    pub const TOOL_OUTPUT_FINAL_TAIL_BYTES: usize = 32 * 1024;
    pub const GOAL_VERIFICATIONS_RETAINED: usize = 20;
    pub const PENDING_COMMANDS_DISPLAY_MAX: usize = 32;
    pub const COMMAND_PENDING_TTL_MS: u64 = 24 * 60 * 60 * 1000;
    pub const IDEMPOTENCY_TABLE_PER_SESSION: usize = 512;
    pub const CONVERSATION_QUERY_TIMEOUT_MS: u64 = 10_000;
    pub const ATTACHMENT_MAX_BYTES: usize = 20 * 1024 * 1024;
    pub const ATTACHMENT_CHUNK_MAX_BYTES: usize = 512 * 1024;
    /// Donor note (`core.ts:87-91`): the share-selection metadata-only stat
    /// once reused ATTACHMENT_PREVIEW_MAX_BYTES (30MiB) as its totalBytes cap,
    /// so oversized attachments were mis-classified as deferred and silently
    /// dropped at schema validation. A stat moves no bytes; it needs only a
    /// bound expressive of real file sizes.
    pub const ATTACHMENT_STAT_MAX_BYTES: usize = 2 * 1024 * 1024 * 1024;
    pub const ATTACHMENT_PREVIEW_MAX_CHUNKS: usize = 60;
    pub const ATTACHMENT_READ_CACHE_MAX_BYTES: usize = 30 * 1024 * 1024;
    pub const ATTACHMENT_READ_CACHE_TTL_MS: u64 = 30_000;
    pub const ATTACHMENT_UPLOAD_MAX_CHUNKS: usize = 64;
    pub const ATTACHMENT_UPLOAD_MAX_CONCURRENT: usize = 16;
    pub const ATTACHMENT_UPLOAD_MAX_STAGED_BYTES: usize = 64 * 1024 * 1024;
    pub const ATTACHMENT_UPLOAD_TTL_MS: u64 = 5 * 60_000;
    pub const ATTACHMENT_UNREFERENCED_TTL_MS: u64 = 24 * 60 * 60 * 1000;
}
