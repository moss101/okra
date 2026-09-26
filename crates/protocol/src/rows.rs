//! Port of `rows.ts` — conversation rows.
//!
//! `rows.ts` defines ~20 display-row kinds for ZCode's full UI catalog; the
//! complete catalog is re-homed with the UI package (MASTER-PLAN §3 #49,
//! M3). M0 ports `RowBase` invariants and the three kinds the delta/coalesce
//! machinery needs, keeping the set closed and serde-tagged so the M3 port is
//! additive:
//!
//! - `rowId`: session-scoped monotonic, never reused; a deterministic pure
//!   function of the event log (`rows.ts:9`).
//! - `entityId` locates the persisted entity; `productTurnId` is the product
//!   turn and must not be re-guessed by the UI (`rows.ts:14-15`).

use serde::{Deserialize, Serialize};

pub type RowId = u64;

/// `RowBase` (`rows.ts:9-27`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RowBase {
    #[serde(rename = "rowId")]
    pub row_id: RowId,
    #[serde(rename = "entityId", skip_serializing_if = "Option::is_none")]
    pub entity_id: Option<String>,
    #[serde(
        rename = "productTurnId",
        skip_serializing_if = "Option::is_none"
    )]
    pub product_turn_id: Option<String>,
    #[serde(
        rename = "editDisposition",
        skip_serializing_if = "Option::is_none"
    )]
    pub edit_disposition: Option<EditDisposition>,
}

/// `rows.ts:27` — `rewind | fork`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum EditDisposition {
    Rewind,
    Fork,
}

/// The M0 row kinds. Field-for-field these mirror the donor's
/// userMessage / response / toolInvocation summaries (`rows.ts:63-174`);
/// names follow the donor's `kind` tags so the M3 full catalog extends
/// without a wire break.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum ConversationRow {
    /// User turn input.
    UserMessage {
        #[serde(flatten)]
        base: RowBase,
        text: String,
    },
    /// Assistant response. Invariant (`rows.ts:153`): appends may only target
    /// rows in `streaming` state; an ordinary new response always gets a new
    /// rowId.
    Response {
        #[serde(flatten)]
        base: RowBase,
        text: String,
        state: ResponseState,
    },
    /// Client-safe tool invocation summary (the donor collapsed
    /// HookInvocationRow to a shared desktop/mobile summary — `profiles.ts:17`).
    ToolInvocation {
        #[serde(flatten)]
        base: RowBase,
        #[serde(rename = "toolName")]
        tool_name: String,
        status: ToolStatus,
    },
}

/// `rows.ts:162`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ResponseState {
    Streaming,
    Complete,
    Interrupted,
    Failed,
}

/// `rows.ts:198`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ToolStatus {
    InputStreaming,
    PendingApproval,
    Running,
    Success,
    Error,
    Cancelled,
}

impl ConversationRow {
    pub fn row_id(&self) -> RowId {
        match self {
            ConversationRow::UserMessage { base, .. }
            | ConversationRow::Response { base, .. }
            | ConversationRow::ToolInvocation { base, .. } => base.row_id,
        }
    }
}
