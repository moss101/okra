//! Feedback domain (MASTER-PLAN §3 #48, from ZCode
//! `packages/services/src/feedback/`): local-first feedback tickets with
//! a device-scoped ticket store, comment threads, and attachment records
//! (log / image / other — a gzipped trace package from
//! [`crate::files::trace`] is just an attachment whose kind is `log`).
//!
//! Donor contracts kept:
//! - **tickets are device-scoped**: the local store filters by the
//!   device id (ZCode `deviceMid`), so one install never sees another
//!   device's tickets;
//! - **list queries filter by status and type**;
//! - **statuses** carry the donor workflow vocabulary (submitted →
//!   needs-info / in-development / resolved / shipped / rejected /
//!   adopted / closed-by-reply / archived);
//! - **attachments have kinds** (log/image/other) with filename + mime;
//! - the remote submission itself is an injected seam — okra's daemon is
//!   offline-first, the ticket store is the durable part.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::client_info;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TicketStatus {
    Submitted,
    NeedsInfo,
    InDevelopment,
    Resolved,
    Shipped,
    Rejected,
    Adopted,
    ClosedByReply,
    Archived,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TicketType {
    Bug,
    Feature,
    Question,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttachmentKind {
    Log,
    Image,
    Other,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FeedbackAttachment {
    pub id: String,
    pub kind: AttachmentKind,
    pub file_name: String,
    pub mime: String,
    pub size_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FeedbackComment {
    pub id: String,
    pub author: String,
    pub body: String,
    pub created_at_epoch_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FeedbackTicket {
    pub id: String,
    pub device_id: String,
    pub title: String,
    pub ticket_type: TicketType,
    pub severity: Option<String>,
    pub module: Option<String>,
    pub status: TicketStatus,
    pub created_at_epoch_ms: u64,
    pub updated_at_epoch_ms: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachments: Vec<FeedbackAttachment>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub comments: Vec<FeedbackComment>,
}

#[derive(Debug, Clone, Default)]
pub struct ListQuery {
    pub status: Option<TicketStatus>,
    pub ticket_type: Option<TicketType>,
}

#[derive(Debug, thiserror::Error)]
pub enum FeedbackError {
    #[error("feedback title must be a non-empty string")]
    EmptyTitle,
    #[error("feedback codec: {0}")]
    Codec(#[from] serde_json::Error),
    #[error("feedback io: {0}")]
    Io(#[from] std::io::Error),
}

/// The device-scoped local ticket store: `~/.okra/feedback/tickets.json`.
/// Corrupt stores fail loudly (same rule as credentials — never wipe a
/// user's ticket history on a bad read).
pub struct FeedbackTicketStore {
    path: PathBuf,
    device_id: String,
}

impl FeedbackTicketStore {
    pub fn open(home: &Path) -> Result<Self, FeedbackError> {
        Ok(FeedbackTicketStore {
            path: home.join(".okra").join("feedback").join("tickets.json"),
            device_id: client_info::load_or_create_device_identity(home)?.device_id,
        })
    }

    pub fn with_device(home: &Path, device_id: impl Into<String>) -> Self {
        FeedbackTicketStore {
            path: home.join(".okra").join("feedback").join("tickets.json"),
            device_id: device_id.into(),
        }
    }

    fn read_tickets(&self) -> Result<Vec<FeedbackTicket>, FeedbackError> {
        let raw = match std::fs::read_to_string(&self.path) {
            Ok(raw) => raw,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e.into()),
        };
        let parsed: Result<Value, _> = serde_json::from_str(&raw);
        match parsed {
            Ok(v) => {
                let tickets = v
                    .get("tickets")
                    .cloned()
                    .unwrap_or(Value::Array(Vec::new()));
                Ok(serde_json::from_value(tickets)?)
            }
            // corrupt: keep the evidence next to the store and fail loudly
            Err(e) => {
                let backup = self.path.with_extension(format!(
                    "json.corrupt-{}",
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_millis())
                        .unwrap_or(0)
                ));
                let _ = std::fs::rename(&self.path, &backup);
                Err(FeedbackError::Codec(e))
            }
        }
    }

    fn write_tickets(&self, tickets: &[FeedbackTicket]) -> Result<(), FeedbackError> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let payload = json!({ "deviceId": self.device_id, "tickets": tickets });
        std::fs::write(
            &self.path,
            serde_json::to_vec_pretty(&payload)?,
        )?;
        Ok(())
    }

    /// Create a ticket (status starts `submitted`). Empty titles fail.
    pub fn create(
        &self,
        title: &str,
        ticket_type: TicketType,
        severity: Option<&str>,
        module: Option<&str>,
        description: &str,
    ) -> Result<FeedbackTicket, FeedbackError> {
        if title.trim().is_empty() {
            return Err(FeedbackError::EmptyTitle);
        }
        let now = now_ms();
        let ticket = FeedbackTicket {
            id: format!("fb-{}", hash_now()),
            device_id: self.device_id.clone(),
            title: title.trim().to_string(),
            ticket_type,
            severity: severity.map(str::to_string),
            module: module.map(str::to_string),
            status: TicketStatus::Submitted,
            created_at_epoch_ms: now,
            updated_at_epoch_ms: now,
            attachments: Vec::new(),
            comments: if description.trim().is_empty() {
                Vec::new()
            } else {
                vec![FeedbackComment {
                    id: "c1".into(),
                    author: "reporter".into(),
                    body: description.trim().to_string(),
                    created_at_epoch_ms: now,
                }]
            },
        };
        let mut tickets = self.read_tickets()?;
        tickets.push(ticket.clone());
        self.write_tickets(&tickets)?;
        Ok(ticket)
    }

    /// List, device-scoped, filtered by status and/or type.
    pub fn list(&self, query: &ListQuery) -> Result<Vec<FeedbackTicket>, FeedbackError> {
        Ok(self
            .read_tickets()?
            .into_iter()
            .filter(|t| t.device_id == self.device_id)
            .filter(|t| query.status.map(|s| t.status == s).unwrap_or(true))
            .filter(|t| query.ticket_type.map(|ty| t.ticket_type == ty).unwrap_or(true))
            .collect())
    }

    pub fn get(&self, id: &str) -> Result<Option<FeedbackTicket>, FeedbackError> {
        Ok(self
            .read_tickets()?
            .into_iter()
            .find(|t| t.id == id))
    }

    /// Append a comment and bump the ticket's update time.
    pub fn comment(&self, id: &str, author: &str, body: &str) -> Result<Option<FeedbackComment>, FeedbackError> {
        let mut tickets = self.read_tickets()?;
        let now = now_ms();
        for ticket in &mut tickets {
            if ticket.id != id {
                continue;
            }
            let comment = FeedbackComment {
                id: format!("c{}", ticket.comments.len() + 1),
                author: author.to_string(),
                body: body.trim().to_string(),
                created_at_epoch_ms: now,
            };
            ticket.comments.push(comment.clone());
            ticket.updated_at_epoch_ms = now;
            self.write_tickets(&tickets)?;
            return Ok(Some(comment));
        }
        Ok(None)
    }

    /// Record an attachment (log/image/other) against a ticket. The bytes
    /// live where they already are (e.g. a trace package in the staging
    /// dir); only the metadata is stored.
    pub fn attach(
        &self,
        id: &str,
        kind: AttachmentKind,
        file_name: &str,
        mime: &str,
        size_bytes: u64,
    ) -> Result<Option<FeedbackAttachment>, FeedbackError> {
        let mut tickets = self.read_tickets()?;
        for ticket in &mut tickets {
            if ticket.id != id {
                continue;
            }
            let attachment = FeedbackAttachment {
                id: format!("att-{}", ticket.attachments.len() + 1),
                kind,
                file_name: file_name.to_string(),
                mime: mime.to_string(),
                size_bytes,
            };
            ticket.attachments.push(attachment.clone());
            ticket.updated_at_epoch_ms = now_ms();
            self.write_tickets(&tickets)?;
            return Ok(Some(attachment));
        }
        Ok(None)
    }

    /// The device snapshot the remote submission would carry.
    pub fn device_snapshot(&self) -> Value {
        json!({ "deviceId": self.device_id })
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn hash_now() -> u64 {
    now_ms() ^ ((std::process::id() as u64) << 32)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(home: &Path, device: &str) -> FeedbackTicketStore {
        FeedbackTicketStore::with_device(home, device)
    }

    #[test]
    fn create_list_filter_comment_round_trip() {
        let td = tempfile::tempdir().unwrap();
        let s = store(td.path(), "device-A");
        let bug = s
            .create("crash on start", TicketType::Bug, Some("high"), Some("Plugin / MCP"), "steps: 1) run")
            .unwrap();
        s.create("dark mode", TicketType::Feature, None, None, "").unwrap();

        assert_eq!(bug.status, TicketStatus::Submitted);
        assert_eq!(bug.comments.len(), 1, "description becomes first comment");

        // filters
        let bugs = s
            .list(&ListQuery {
                status: None,
                ticket_type: Some(TicketType::Bug),
            })
            .unwrap();
        assert_eq!(bugs.len(), 1);
        let features = s
            .list(&ListQuery {
                status: Some(TicketStatus::Submitted),
                ticket_type: Some(TicketType::Feature),
            })
            .unwrap();
        assert_eq!(features.len(), 1);
        assert_eq!(features[0].title, "dark mode");

        // get + comment
        let _ = s.get(&bug.id).unwrap().unwrap();
        let comment = s.comment(&bug.id, "dev", "looking into it").unwrap().unwrap();
        assert_eq!(comment.author, "dev");
        let full = s.get(&bug.id).unwrap().unwrap();
        assert_eq!(full.comments.len(), 2);
        assert_eq!(full.comments.last().unwrap().id, "c2");
        assert!(s.get("fb-missing").unwrap().is_none());
        let _ = full;
    }

    #[test]
    fn empty_title_fails() {
        let td = tempfile::tempdir().unwrap();
        assert!(matches!(
            store(td.path(), "d").create("   ", TicketType::Bug, None, None, ""),
            Err(FeedbackError::EmptyTitle)
        ));
    }

    #[test]
    fn tickets_are_device_scoped() {
        let td = tempfile::tempdir().unwrap();
        store(td.path(), "device-A")
            .create("mine", TicketType::Bug, None, None, "")
            .unwrap();
        // another device sharing the same home sees nothing
        assert!(store(td.path(), "device-B")
            .list(&ListQuery::default())
            .unwrap()
            .is_empty());
        // ...but device A still sees its own
        assert_eq!(
            store(td.path(), "device-A").list(&ListQuery::default()).unwrap().len(),
            1
        );
    }

    #[test]
    fn attachments_record_metadata_for_trace_packages() {
        let td = tempfile::tempdir().unwrap();
        let s = store(td.path(), "d");
        let ticket = s.create("slow", TicketType::Bug, None, None, "").unwrap();
        let attachment = s
            .attach(
                &ticket.id,
                AttachmentKind::Log,
                "trace-conv-17.json.gz",
                "application/gzip",
                1234,
            )
            .unwrap()
            .unwrap();
        assert_eq!(attachment.kind, AttachmentKind::Log);
        assert_eq!(attachment.size_bytes, 1234);
        assert_eq!(s.get(&ticket.id).unwrap().unwrap().attachments.len(), 1);
        assert!(s.attach("fb-missing", AttachmentKind::Log, "x", "", 0).unwrap().is_none());
    }

    #[test]
    fn corrupt_store_backed_up_not_overwritten() {
        let td = tempfile::tempdir().unwrap();
        let path = td.path().join(".okra/feedback/tickets.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "{ broken").unwrap();
        let s = FeedbackTicketStore::with_device(td.path(), "d");
        assert!(s.list(&ListQuery::default()).is_err(), "corrupt store fails loudly");
        // the corrupt file was moved aside, so create starts a fresh store
        s.create("after crash", TicketType::Bug, None, None, "").unwrap();
        assert_eq!(s.list(&ListQuery::default()).unwrap().len(), 1);
        let _ = path;
    }
}
