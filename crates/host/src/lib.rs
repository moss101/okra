//! okra-host — host services (MASTER-PLAN §3 #48/#51/#52). The ~50 ZCode
//! domains migrate strangler-style behind the protocol from M3; M1 lands
//! the three safety-bearing pieces:
//! - notifications policy: exactly 3 native classes + body redaction +
//!   focus suppression (§3 #51, ChatGPT2 study — fixes their leak)
//! - file-management safe-read: O_NOFOLLOW|O_NONBLOCK + regular-file +
//!   ownership checks (§3 #52, ChatGPT2 docs/03)
//! - fsutil: the single sanctioned canonicalize/home_dir call sites

pub mod checkpoints;
pub mod conversation_share;
pub mod credential;
pub mod files;
pub mod fsutil;
pub mod git;
pub mod mcp_sync;
pub mod oauth;
pub mod notifications;
pub mod plugin_sync;
pub mod plugins;
pub mod safe_fs;
pub mod settings;
pub mod settings_sync;
pub mod storage;
pub mod subagent;
pub mod usage;
pub mod skill_sync;
pub mod terminal;

pub use notifications::{
    classify, redact_body, Notification, NotificationClass, NotificationsPolicy,
};
pub use git::{
    FileSource, GhRunner, GitError, GitHead, GitRepository, GitStatusEntry, RealGh,
    MAX_GIT_FILE_BYTES,
};
pub use oauth::{DeviceAuthorization, OAuthClient, OAuthDomain, OAuthError, TokenSet};
pub use plugins::{PluginStore, SignedPluginEnvelope, SignatureVerdict, TrustStore};
pub use plugin_sync::{
    ComponentType, ImportOutcome as PluginImportOutcome, PluginSyncCandidate, PluginSyncError,
    PluginSyncService, RemoteSkipReason, RemoteStatus, SyncStatus as PluginSyncStatus,
    DEFAULT_MAX_ARCHIVE_BYTES as PLUGIN_SYNC_MAX_ARCHIVE_BYTES, INLINE_PLUGIN_MARKETPLACE,
    METADATA_ARCHIVE_PATH,
};
pub use subagent::{
    ProjectedContext, RoleScope, SubagentGrant, SubagentLaunchError, SubagentLauncher,
};
pub use settings::{SettingsError as SettingsStoreError, SettingsScope, SettingsStore};
pub use usage::{GroupedUsage, UsageError, UsageLedger, UsageRecord, UsageSnapshot, UsageTotals};
pub use storage::{CatalogEntry, StorageError as StorageDomainError, StorageService};
pub use credential::{CredentialError, CredentialStore};
pub use checkpoints::{
    CheckpointError, CheckpointManager, FileSnapshot, GitState, RestoreReport, RewindCheckpoint,
    RewindPoint,
};
pub use conversation_share::{
    build_integrity, build_public_projection, canonical_json, verify_integrity, PublicProjection,
    ProjectionErrorKind, ShareError, ShareIntegrity,
};
pub use files::{
    Attachment, AttachmentKind, AttachmentOrigin, AttachmentStore, ChangeKind, DownloadEntry,
    DownloadError, DownloadState, DownloadStore, FileChange, FileWatchService, LifecycleEvent,
    LifecycleKind,
};
pub use mcp_sync::{
    ExportedServer, ImportOutcome, ImportStatus, McpServerRecord, McpSyncCandidate,
    McpSyncDescriptor, McpSyncError, McpSyncService, McpSyncSource, PathRewrite,
};
pub use skill_sync::{
    SkillCandidate, SkillRoot, SkillSyncError, SkillSyncService, DEFAULT_MAX_ARCHIVE_BYTES,
    MAX_SKILL_SCAN_DEPTH, SKILL_FILE_NAME, SKILL_SCAN_EXCLUDED_DIRECTORY_NAMES,
};
pub use settings_sync::{
    AgentDiscovery, DiscoveryResult, ImportMode, ImportResult, McpCandidate, SettingsSyncService,
    SkipReason, SourceScope, SyncAgent, SyncCandidate, SyncImportStatus,
};
pub use terminal::{TerminalHost, TerminalSession, TerminalSize};
