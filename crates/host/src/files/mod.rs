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
pub mod trace;
pub mod watcher;

pub use attachments::{Attachment, AttachmentKind, AttachmentOrigin, AttachmentStore};
pub use downloads::{
    DownloadEntry, DownloadError, DownloadState, DownloadStore, LifecycleEvent, LifecycleKind,
};
pub use trace::{
    gunzip_equals, package_trace, upload_trace, TraceError, TracePackage, TraceRecording,
    TraceUpload, DEFAULT_MAX_PACKAGED_BYTES, DEFAULT_MAX_TRACE_BYTES, TRACE_CLASSIFICATION,
    TRACE_SIDECAR_VERSION,
};
pub use watcher::{ChangeKind, FileChange, FileWatchService};
