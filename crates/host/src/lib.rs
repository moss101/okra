//! okra-host — host services (MASTER-PLAN §3 #48/#51/#52). The ~50 ZCode
//! domains migrate strangler-style behind the protocol from M3; M1 lands
//! the three safety-bearing pieces:
//! - notifications policy: exactly 3 native classes + body redaction +
//!   focus suppression (§3 #51, ChatGPT2 study — fixes their leak)
//! - file-management safe-read: O_NOFOLLOW|O_NONBLOCK + regular-file +
//!   ownership checks (§3 #52, ChatGPT2 docs/03)
//! - fsutil: the single sanctioned canonicalize/home_dir call sites

pub mod broadcast;
pub mod checkpoints;
pub mod client_info;
pub mod client_scenes;
pub mod conversation_share;
pub mod credential;
pub mod files;
pub mod feedback;
pub mod feedback_logs;
pub mod fsutil;
pub mod git;
pub mod mcp_sync;
pub mod media;
pub mod model_provider;
pub mod oauth;
pub mod onboarding;
pub mod notifications;
pub mod plugin_sync;
pub mod plugins;
pub mod remote_access;
pub mod log_archive;
pub mod safe_fs;
pub mod settings;
pub mod settings_sync;
pub mod prompt_transfer;
pub mod storage;
pub mod subagent;
pub mod usage;
pub mod skill_sync;
pub mod surfaces;
pub mod system_info;
pub mod telemetry;
pub mod terminal;

pub use notifications::{
    classify, redact_body, Notification, NotificationClass, NotificationsPolicy,
};
pub use git::{
    FileSource, GhRunner, GitError, GitHead, GitRepository, GitStatusEntry, RealGh,
    MAX_GIT_FILE_BYTES,
};
pub use oauth::{DeviceAuthorization, OAuthClient, OAuthDomain, OAuthError, TokenSet};
pub use media::{image_dimensions, sniff_mime, MediaError, MediaInfo};
pub use onboarding::{
    DecisionReason, DecisionStatus, OnboardingDecision, OnboardingEntry, OnboardingError,
    OnboardingRecordFile, OnboardingService, UploadState, RECORD_VERSION_CURRENT,
};
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
pub use prompt_transfer::{
    adopt as adopt_prompt_attachment, cancel as cancel_prompt_transfer,
    validate_ref as validate_prompt_ref, LocalTransfer, StageParams, StageResult,
    StagingTransfer, TransferError, TransferPhase, TransferProgress,
};
pub use system_info::{
    integrated_terminal_shells, probe_intranet, system_info, IntranetProbeResult, IsExecutable,
    ProbeOutcome, ProbeTarget, SystemInfo, TcpConnect, TcpProbe,
};
pub use telemetry::{TelemetryEvent, TelemetryLog};
pub use settings::{SettingsError as SettingsStoreError, SettingsScope, SettingsStore};
pub use surfaces::{AttachDecision, SurfaceError, SurfaceInfo, SurfaceKind, SurfaceRegistry};
pub use usage::{GroupedUsage, UsageError, UsageLedger, UsageRecord, UsageSnapshot, UsageTotals};
pub use storage::{CatalogEntry, StorageError as StorageDomainError, StorageService};
pub use credential::{CredentialError, CredentialStore};
pub use client_info::{
    client_config, load_or_create_device_identity, ClientConfig, DeviceIdentity, DAEMON_NAME,
    PROTOCOL_VERSION,
};
pub use broadcast::{Broadcast, BroadcastBus, BroadcastError};
pub use feedback_logs::{attach_logs_to_ticket, DiagnosticAttachment, FeedbackArchiveError};
pub use client_scenes::{cascaded_items, localized, parse_response_body, ClientSceneCatalog, SceneConfig, SceneItem, SceneOption};
pub use checkpoints::{
    CheckpointError, CheckpointManager, FileSnapshot, GitState, RestoreReport, RewindCheckpoint,
    RewindPoint,
};
pub use conversation_share::{
    build_integrity, build_public_projection, canonical_json, verify_integrity, PublicProjection,
    ProjectionErrorKind, ShareError, ShareIntegrity,
};
pub use log_archive::{
    create_diagnostic_archive, archive_checksum, ArchiveReport, MAX_FILE_BYTES as ARCHIVE_MAX_FILE_BYTES,
    MAX_TOTAL_BYTES as ARCHIVE_MAX_TOTAL_BYTES,
};
pub use feedback::{
    FeedbackAttachment, FeedbackComment, FeedbackError, FeedbackTicket, FeedbackTicketStore,
    ListQuery, TicketStatus, TicketType,
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
pub use remote_access::{
    check_directory_write_access, check_directories_write_access, WriteAccessResult,
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
