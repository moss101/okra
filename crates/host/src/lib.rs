//! okra-host — host services (MASTER-PLAN §3 #48/#51/#52). The ~50 ZCode
//! domains migrate strangler-style behind the protocol from M3; M1 lands
//! the three safety-bearing pieces:
//! - notifications policy: exactly 3 native classes + body redaction +
//!   focus suppression (§3 #51, ChatGPT2 study — fixes their leak)
//! - file-management safe-read: O_NOFOLLOW|O_NONBLOCK + regular-file +
//!   ownership checks (§3 #52, ChatGPT2 docs/03)
//! - fsutil: the single sanctioned canonicalize/home_dir call sites

pub mod automation;
pub mod bigmodel;
pub mod bots;
pub mod broadcast;
pub mod checkpoints;
pub mod client_info;
pub mod client_scenes;
pub mod coding_plan;
pub mod commands;
pub mod conversation_share;
pub mod credential;
pub mod files;
pub mod official_mcp;
pub mod feedback;
pub mod feedback_logs;
pub mod fsutil;
pub mod i18n;
pub mod git;
pub mod mcp_sync;
pub mod media;
pub mod model_provider;
pub mod oauth;
pub mod onboarding;
pub mod notifications;
pub mod plugin_sync;
pub mod plugins;
pub mod process_tree;
pub mod remote_access;
pub mod replay_export;
pub mod runtime_env;
pub mod runtime_tools;
pub mod log_archive;
pub mod managed_policy;
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
pub use automation::{AutomationError, AutomationSpec, AutomationStore};
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
pub use commands::{Command, CommandScope, CommandSource, CommandsError, CommandsList, CommandsService, ParsedCommandFile, WriteCommandParams};
pub use bigmodel::{ApiKeyEnsureResult, ApiKeyEnsureStatus, BizClient, BizEnvelopeDiagnostics, CodingPlanEntitlement, TeamPlanBizContext, UnavailableReason, UreqBizClient, classify_personal_entitlement, classify_team_entitlement, ensure_team_plan_project_api_key, is_active_personal_coding_plan};
pub use i18n::{negotiate_locale, parse_catalog_document, Catalog, LocaleTag};
pub use runtime_env::{build_agent_runtime_env, capture_login_shell_env_snapshot, extract_captured_env_snapshot, format_log_prefix_at, format_timestamp_utc_ms, parse_null_separated_env_snapshot, resolve_shell_path, CaptureError, CaptureOptions, LoginShellExecutor, RealLoginShellExecutor, ServiceLogger, StderrSink, DEFAULT_MAX_BUFFER, DEFAULT_TIMEOUT, LOGIN_ENV_CAPTURE_PREFIX, LOGIN_ENV_CAPTURE_SUFFIX};
pub use bots::{parse_bot_command, normalize_bot_command_policy, normalize_bot_current_options, normalize_bot_config, normalize_allowed_workspaces, is_user_command_allowed, is_workspace_allowed, resolve_workspace_by_value, create_workspace_ref, find_bot, find_callback_bot, find_authorized_bot, build_bot_credential_key, build_bot_webhook_secret_key, BotCommand, BotCommandPolicy, BotConfig, BotCurrentOptions, BotWorkspaceRef, BotsConfigFile};
pub use managed_policy::{clamp_sandbox, load_managed_pin, load_managed_pin_verified, load_trusted_signers, resolve_approval, ManagedPin, ManagedPolicy, PinProvenance, PinState, SandboxCeiling, MANAGED_POLICY_SCHEMA_VERSION};
pub use replay_export::{export_session_replay, render_events_html, render_rows, ReplayError, ReplayRow};
pub use runtime_tools::{app_ca_cert_paths, app_ca_pair_status, append_path_entries, build_runtime_tool_env_patch, ensure_app_ca_pair, generate_self_signed_ca_pem, prepend_path_entries, resolve_command_on_path, resolve_runtime_tool_binary, AppCaPairStatus, GeneratedCaPem, RuntimeToolId, APP_CA_CERT_FILE, APP_CA_KEY_FILE};
