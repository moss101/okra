//! okra-host — host services (MASTER-PLAN §3 #48/#51/#52). The ~50 ZCode
//! domains migrate strangler-style behind the protocol from M3; M1 lands
//! the three safety-bearing pieces:
//! - notifications policy: exactly 3 native classes + body redaction +
//!   focus suppression (§3 #51, ChatGPT2 study — fixes their leak)
//! - file-management safe-read: O_NOFOLLOW|O_NONBLOCK + regular-file +
//!   ownership checks (§3 #52, ChatGPT2 docs/03)
//! - fsutil: the single sanctioned canonicalize/home_dir call sites

pub mod files;
pub mod fsutil;
pub mod git;
pub mod mcp_sync;
pub mod oauth;
pub mod notifications;
pub mod plugins;
pub mod safe_fs;
pub mod terminal;

pub use notifications::{
    classify, redact_body, Notification, NotificationClass, NotificationsPolicy,
};
pub use git::{GitError, GitHead, GitRepository, GitStatusEntry};
pub use oauth::{DeviceAuthorization, OAuthClient, OAuthDomain, OAuthError, TokenSet};
pub use plugins::{PluginStore, SignedPluginEnvelope, SignatureVerdict, TrustStore};
pub use files::{
    Attachment, AttachmentKind, AttachmentOrigin, AttachmentStore, ChangeKind, DownloadEntry,
    DownloadError, DownloadState, DownloadStore, FileChange, FileWatchService, LifecycleEvent,
    LifecycleKind,
};
pub use mcp_sync::{
    ExportedServer, ImportOutcome, ImportStatus, McpServerRecord, McpSyncCandidate,
    McpSyncDescriptor, McpSyncError, McpSyncService, McpSyncSource, PathRewrite,
};
pub use terminal::{TerminalHost, TerminalSession, TerminalSize};
