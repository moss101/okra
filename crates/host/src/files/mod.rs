//! File-management domain (MASTER-PLAN §3 #52) — the three donor pieces
//! beyond safe-read (already in [`crate::safe_fs`]):
//! - [`downloads`]: DownloadStore — monotonic lifecycle-sequence audit,
//!   acknowledge flow, per-conversation binding, persisted JSONL history;
//! - [`attachments`]: conversation-scoped attachment state (context files,
//!   pasted text, goals, images) owned by the daemon, shared by surfaces;
//! - [`watcher`]: per-conversation open-file watches producing
//!   `open-file-changed`-style records, removed on conversation discard.

pub mod attachments;
pub mod downloads;
pub mod watcher;

pub use attachments::{Attachment, AttachmentKind, AttachmentOrigin, AttachmentStore};
pub use downloads::{
    DownloadEntry, DownloadError, DownloadState, DownloadStore, LifecycleEvent, LifecycleKind,
};
pub use watcher::{ChangeKind, FileChange, FileWatchService};
