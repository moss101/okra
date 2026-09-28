//! Host domain: bots — the DATA-ONLY core of ZCode `packages/services/src/bots/`
//! (commandParser.ts, commandOrder.ts, config.ts, botConfigHelpers.ts,
//! workspaceHelpers.ts). The 13k-line network channel runtimes (telegram /
//! weixin / feishu transports) do NOT transfer — they are product surface,
//! not daemon contracts; what transfers is how a bot is CONFIGURED, how an
//! inbound text becomes a command, and who is authorized:
//!
//! - **command parse**: `/cmd [rest]` with zh aliases (`帮助`/`取消`/`状态`
//!   /`新建`/`项目`/`模型`/`模式`/`思考`/`回复`/`停止`/`回答`), plain `0`
//!   cancels a selection, `model provider x` / `model model y` sub-prefixes,
//!   `approve` demands request+option ids, unknown commands carry name+raw.
//! - **command policy**: defaults all-true; the removed `/cli` command is
//!   dropped on normalization (never written back); help/message/approve/
//!   task/stop are always allowed, `reconnect` rides the workspace policy.
//! - **workspace allowlist**: empty or containing `*` collapses to `[*]`
//!   (the only workspace permission boundary), otherwise dedupe; resolution
//!   accepts 1-based index into the ALLOWED list, or id/label/path.
//! - **authorization**: weixin bots bind by bot id; every other provider
//!   authorizes by (provider, providerUserId) pair.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const ALL_BOT_WORKSPACES: &str = "*";

// config.ts file names
pub const BOTS_CONFIG_FILE: &str = "bot-config.v3.json";
pub const BOTS_LEGACY_CONFIG_FILE: &str = "bot-config.json";
pub const BOTS_STATE_FILE: &str = "bot-state.v3.json";
pub const BOTS_MODEL_CACHE_FILE: &str = "bots-model-cache.v2.json";
const BOT_CREDENTIAL_PREFIX: &str = "bot";

pub const DEFAULT_BOT_REPLY_GRANULARITY: &str = "assistant_changes";

// ---------------------------------------------------------------------------
// command parsing (commandParser.ts + commandOrder.ts)
// ---------------------------------------------------------------------------

pub const BOT_POLICY_COMMAND_ORDER: [&str; 7] = [
    "status",
    "new",
    "workspace",
    "model",
    "mode",
    "thoughtLevel",
    "reply",
];

pub const BOT_MENU_COMMAND_ORDER: [&str; 9] = [
    "help",
    "status",
    "new",
    "workspace",
    "model",
    "mode",
    "thoughtLevel",
    "reply",
    "bind",
];

/// The tagged command union (serde: externally tagged snake_case, matching
/// the TS discriminated union's `type` field).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum BotCommand {
    Message { text: String },
    SelectionCancel,
    Bind { code: String },
    Help,
    Status,
    New,
    Reconnect,
    Stop,
    WorkspaceList,
    WorkspaceSet { value: String },
    ModelList,
    ModelSet { value: String },
    ModelProviderSet { value: String },
    ModeList,
    ModeSet { value: String },
    ThoughtLevelList,
    ThoughtLevelSet { value: String },
    TaskList,
    TaskSet { value: String },
    ReplyList,
    ReplySet { value: String },
    PermissionRespond { value: String },
    ElicitationSubmit,
    ElicitationRespond { value: String },
    Approve { request_id: String, option_id: String },
    Deny { request_id: String },
    Unknown { name: String, raw: String },
}

fn split_command(text: &str) -> Option<(String, String)> {
    let trimmed = text.trim();
    let body = trimmed.strip_prefix('/')?;
    let (name, rest) = match body.find(char::is_whitespace) {
        Some(first_space) => (
            &body[..first_space],
            body[first_space + 1..].trim(),
        ),
        None => (body, ""),
    };
    Some((name.to_ascii_lowercase(), rest.to_string()))
}

/// Inbound text → bot command. zh aliases resolve to the same variants.
pub fn parse_bot_command(text: &str) -> BotCommand {
    let Some((name, rest)) = split_command(text) else {
        return if text.trim() == "0" {
            BotCommand::SelectionCancel
        } else {
            BotCommand::Message { text: text.to_string() }
        };
    };
    let has_rest = !rest.is_empty();
    match name.as_str() {
        "bind" if has_rest => BotCommand::Bind { code: rest },
        "help" | "帮助" => BotCommand::Help,
        "cancel" | "取消" => BotCommand::SelectionCancel,
        "status" | "状态" => BotCommand::Status,
        "new" | "clear" | "新建" => BotCommand::New,
        "reconnect" | "重连" => BotCommand::Reconnect,
        "workspace" | "project" | "项目" if has_rest => BotCommand::WorkspaceSet { value: rest },
        "workspace" | "project" | "项目" => BotCommand::WorkspaceList,
        "model" | "模型" if !has_rest => BotCommand::ModelList,
        "model" | "模型" if let Some(v) = rest.strip_prefix("provider ") => {
            BotCommand::ModelProviderSet { value: v.trim().to_string() }
        }
        "model" | "模型" if let Some(v) = rest.strip_prefix("model ") => {
            BotCommand::ModelSet { value: v.trim().to_string() }
        }
        "model" | "模型" => BotCommand::ModelSet { value: rest },
        "mode" | "模式" if has_rest => BotCommand::ModeSet { value: rest },
        "mode" | "模式" => BotCommand::ModeList,
        "thoughtlevel" | "thought_level" | "thought-level" | "think" | "思考" if has_rest => {
            BotCommand::ThoughtLevelSet { value: rest }
        }
        "thoughtlevel" | "thought_level" | "thought-level" | "think" | "思考" => {
            BotCommand::ThoughtLevelList
        }
        "task" if has_rest => BotCommand::TaskSet { value: rest },
        "task" => BotCommand::TaskList,
        "reply" | "回复" if has_rest => BotCommand::ReplySet { value: rest },
        "reply" | "回复" => BotCommand::ReplyList,
        "stop" | "停止" => BotCommand::Stop,
        "permission" if has_rest => BotCommand::PermissionRespond { value: rest },
        "elicitation" | "answer" | "回答" if ["submit", "done", "完成", "提交"].contains(&rest.to_lowercase().as_str()) => {
            BotCommand::ElicitationSubmit
        }
        "elicitation" | "answer" | "回答" if has_rest => {
            BotCommand::ElicitationRespond { value: rest }
        }
        "approve" => match rest.split_whitespace().collect::<Vec<_>>()[..] {
            [request_id, option_id] => BotCommand::Approve {
                request_id: request_id.to_string(),
                option_id: option_id.to_string(),
            },
            _ => BotCommand::Unknown { name, raw: text.to_string() },
        },
        "deny" if has_rest => BotCommand::Deny { request_id: rest },
        _ => BotCommand::Unknown { name, raw: text.to_string() },
    }
}

// ---------------------------------------------------------------------------
// config plane (config.ts + botConfigHelpers.ts)
// ---------------------------------------------------------------------------

/// Per-command allow switches; `None` = default (enabled). The removed
/// `/cli` key never survives normalization.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct BotCommandPolicy {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<bool>,
    #[serde(rename = "thoughtLevel", default, skip_serializing_if = "Option::is_none")]
    pub thought_level: Option<bool>,
    #[serde(rename = "sandboxMode", default, skip_serializing_if = "Option::is_none")]
    pub sandbox_mode: Option<bool>,
    #[serde(rename = "approvalPolicy", default, skip_serializing_if = "Option::is_none")]
    pub approval_policy: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply: Option<bool>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BotCurrentOptions {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_selection: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    #[serde(rename = "sandboxMode", default, skip_serializing_if = "Option::is_none")]
    pub sandbox_mode: Option<String>,
    #[serde(rename = "approvalPolicy", default, skip_serializing_if = "Option::is_none")]
    pub approval_policy: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BotWorkspaceRef {
    pub id: String,
    pub label: String,
    #[serde(rename = "workspacePath")]
    pub workspace_path: String,
    #[serde(rename = "workspaceIdentity", default, skip_serializing_if = "Option::is_none")]
    pub workspace_identity: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BotConfig {
    pub id: String,
    pub provider: String,
    #[serde(default)]
    pub enabled: bool,
    #[serde(rename = "providerUserId", default, skip_serializing_if = "Option::is_none")]
    pub provider_user_id: Option<String>,
    #[serde(rename = "allowedWorkspaces", default)]
    pub allowed_workspaces: Vec<String>,
    #[serde(rename = "allowedCommands", default)]
    pub allowed_commands: BotCommandPolicy,
    #[serde(rename = "currentOptions", default)]
    pub current_options: BotCurrentOptions,
    #[serde(rename = "replyMode", default, skip_serializing_if = "Option::is_none")]
    pub reply_mode: Option<String>,
    #[serde(rename = "webhookUrl", default, skip_serializing_if = "Option::is_none")]
    pub webhook_url: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct BotsConfigFile {
    #[serde(default = "default_config_version")]
    pub version: u32,
    #[serde(default)]
    pub bots: Vec<BotConfig>,
}

fn default_config_version() -> u32 {
    3
}

pub fn create_default_bots_config() -> BotsConfigFile {
    BotsConfigFile { version: 3, bots: Vec::new() }
}

/// Fill defaults; the removed `cli` key is read past and never written.
pub fn normalize_bot_command_policy(raw: &Value) -> BotCommandPolicy {
    let map = raw.as_object();
    let pick = |key: &str| -> Option<bool> {
        map.and_then(|m| m.get(key))
            .and_then(Value::as_bool)
    };
    BotCommandPolicy {
        status: pick("status"),
        new: pick("new"),
        workspace: pick("workspace"),
        model: pick("model"),
        mode: pick("mode"),
        thought_level: pick("thoughtLevel"),
        sandbox_mode: pick("sandboxMode"),
        approval_policy: pick("approvalPolicy"),
        reply: pick("reply"),
    }
}

/// Only the known current keys survive (a stray `model`/`thoughtLevel`/
/// `cli` is compatibility-read upstream and never saved); modelSelection
/// must be an object or it is dropped.
pub fn normalize_bot_current_options(raw: &Value) -> BotCurrentOptions {
    let Some(map) = raw.as_object() else {
        return BotCurrentOptions::default();
    };
    let model_selection = match map.get("modelSelection") {
        Some(v) if v.is_object() => Some(v.clone()),
        _ => None,
    };
    let opt_str = |key: &str| {
        map.get(key)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    BotCurrentOptions {
        model_selection,
        mode: opt_str("mode"),
        sandbox_mode: opt_str("sandboxMode"),
        approval_policy: opt_str("approvalPolicy"),
    }
}

/// empty or containing `*` → `[*]` (the only boundary is "everywhere");
/// otherwise dedupe keeping order.
pub fn normalize_allowed_workspaces(allowed: &[String]) -> Vec<String> {
    let ids: Vec<String> = allowed.iter().map(|s| s.trim()).filter(|s| !s.is_empty()).map(str::to_string).collect();
    if ids.is_empty() || ids.iter().any(|i| i == ALL_BOT_WORKSPACES) {
        return vec![ALL_BOT_WORKSPACES.to_string()];
    }
    let mut out: Vec<String> = Vec::new();
    for id in ids {
        if !out.contains(&id) {
            out.push(id);
        }
    }
    out
}

pub fn is_workspace_allowed(workspace_id: &str, allowed: &[String]) -> bool {
    allowed.is_empty()
        || allowed.iter().any(|a| a == ALL_BOT_WORKSPACES)
        || allowed.iter().any(|a| a == workspace_id)
}

/// Feishu/Lark can render streaming cards; no other channel can.
pub fn normalize_bot_reply_granularity(provider: &str, reply_mode: Option<&str>) -> String {
    let supported: &[&str] = if provider == "feishu" {
        &["streaming_card"]
    } else {
        &[
            "assistant_changes",
            "assistant_final",
            DEFAULT_BOT_REPLY_GRANULARITY,
        ]
    };
    let candidate = reply_mode.unwrap_or(DEFAULT_BOT_REPLY_GRANULARITY);
    if supported.contains(&candidate) {
        candidate.to_string()
    } else {
        supported[0].to_string()
    }
}

/// Save-time normalization: workspace boundary collapses, weixin loses any
/// foreign webhookUrl (weixin uses the built-in iLink address).
pub fn normalize_bot_config(bot: &BotConfig) -> BotConfig {
    let mut normalized = BotConfig {
        id: bot.id.clone(),
        provider: bot.provider.clone(),
        enabled: bot.enabled,
        provider_user_id: bot.provider_user_id.clone(),
        allowed_workspaces: normalize_allowed_workspaces(&bot.allowed_workspaces),
        allowed_commands: bot.allowed_commands.clone(),
        current_options: bot.current_options.clone(),
        reply_mode: Some(normalize_bot_reply_granularity(
            &bot.provider,
            bot.reply_mode.as_deref(),
        )),
        webhook_url: bot.webhook_url.clone(),
    };
    if normalized.provider != "weixin" {
        return normalized;
    }
    normalized.webhook_url = None;
    normalized
}

pub fn find_bot<'a>(config: &'a BotsConfigFile, bot_id: &str) -> Option<&'a BotConfig> {
    config.bots.iter().find(|b| b.id == bot_id)
}

/// Callback routing: an explicit payload botId wins; otherwise the FIRST
/// enabled bot of the provider.
pub fn find_callback_bot<'a>(
    config: &'a BotsConfigFile,
    provider: &str,
    payload: &Value,
) -> Option<&'a BotConfig> {
    if let Some(bot_id) = payload.get("botId").and_then(Value::as_str)
        && !bot_id.is_empty()
    {
        return find_bot(config, bot_id);
    }
    config
        .bots
        .iter()
        .find(|b| b.provider == provider && b.enabled)
}

/// weixin binds by bot id (no stable provider user id); everything else
/// authorizes by the (provider, providerUserId) pair. Unauthorized actors
/// see nothing — `None`.
pub fn find_authorized_bot<'a>(
    config: &'a BotsConfigFile,
    provider: &str,
    provider_user_id: &str,
    bot_id: &str,
) -> Option<&'a BotConfig> {
    if provider == "weixin" {
        return config
            .bots
            .iter()
            .find(|b| b.enabled && b.provider == "weixin" && b.id == bot_id);
    }
    config.bots.iter().find(|b| {
        b.enabled
            && b.provider == provider
            && b.provider_user_id.as_deref() == Some(provider_user_id)
    })
}

/// A bot is "bound" to the actor when the actor's identity matches its
/// own (weixin: same bot id).
pub fn is_bound_user(bot: &BotConfig, provider: &str, provider_user_id: &str) -> bool {
    provider == "weixin" || bot.provider_user_id.as_deref() == Some(provider_user_id)
}

/// Always-allowed set: help/message/approve/task/stop; `reconnect` rides
/// the workspace policy like the workspace command itself.
pub fn is_user_command_allowed(bot: &BotConfig, requested: &str) -> bool {
    let policy = &bot.allowed_commands;
    match requested {
        "help" | "message" | "approve" | "task" | "stop" => true,
        "reconnect" => policy.workspace.unwrap_or(true),
        "status" => policy.status.unwrap_or(true),
        "new" => policy.new.unwrap_or(true),
        "workspace" => policy.workspace.unwrap_or(true),
        "model" => policy.model.unwrap_or(true),
        "mode" => policy.mode.unwrap_or(true),
        "thoughtLevel" => policy.thought_level.unwrap_or(true),
        "reply" => policy.reply.unwrap_or(true),
        _ => false,
    }
}

pub fn build_bot_credential_key(bot_id: &str) -> String {
    format!("{BOT_CREDENTIAL_PREFIX}:{bot_id}:credential")
}

pub fn build_bot_webhook_secret_key(bot_id: &str) -> String {
    format!("{BOT_CREDENTIAL_PREFIX}:{bot_id}:webhook-secret")
}

// ---------------------------------------------------------------------------
// workspace helpers (workspaceHelpers.ts)
// ---------------------------------------------------------------------------

pub fn get_workspace_key(workspace_path: &str, workspace_identity: Option<&str>) -> String {
    match workspace_identity.map(str::trim).filter(|s| !s.is_empty()) {
        Some(identity) => identity.to_string(),
        None => workspace_path.to_string(),
    }
}

pub fn get_workspace_label(workspace_path: &str) -> String {
    workspace_path
        .split(['/', '\\'])
        .rfind(|s: &&str| !s.is_empty())
        .unwrap_or(workspace_path)
        .to_string()
}

pub fn create_workspace_ref(workspace_path: &str, workspace_identity: Option<&str>) -> BotWorkspaceRef {
    BotWorkspaceRef {
        id: get_workspace_key(workspace_path, workspace_identity),
        label: get_workspace_label(workspace_path),
        workspace_path: workspace_path.to_string(),
        workspace_identity: workspace_identity.map(str::to_string),
    }
}

fn is_all_workspaces_allowed(allowed: &[String]) -> bool {
    allowed.is_empty() || allowed.iter().any(|a| a == ALL_BOT_WORKSPACES)
}

pub fn filter_allowed_workspaces<'a>(
    workspaces: &'a [BotWorkspaceRef],
    allowed: &[String],
) -> Vec<&'a BotWorkspaceRef> {
    if is_all_workspaces_allowed(allowed) {
        workspaces.iter().collect()
    } else {
        workspaces
            .iter()
            .filter(|w| allowed.iter().any(|a| a == &w.id))
            .collect()
    }
}

/// `/workspace <value>` resolution: a 1-based index into the ALLOWED list
/// wins, then case-insensitive id/label and exact path/key matches.
pub fn resolve_workspace_by_value(
    workspaces: &[BotWorkspaceRef],
    value: &str,
    allowed: &[String],
) -> Option<BotWorkspaceRef> {
    let trimmed = value.trim();
    let filtered = filter_allowed_workspaces(workspaces, allowed);
    if let Ok(index) = trimmed.parse::<usize>()
        && index > 0
    {
        return filtered.get(index - 1).map(|w| (*w).clone());
    }
    let normalized = trimmed.to_lowercase();
    filtered
        .into_iter()
        .find(|w| {
            w.id.to_lowercase() == normalized
                || w.label.to_lowercase() == normalized
                || w.workspace_path == trimmed
                || get_workspace_key(&w.workspace_path, w.workspace_identity.as_deref()) == trimmed
        })
        .cloned()
}

/// The env snapshot shape the runtime hands the parser (helper for
/// callers building probe contexts in tests).
pub type BotEnv = BTreeMap<String, String>;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parser_routes_commands_aliases_and_rest() {
        assert_eq!(parse_bot_command("hello"), BotCommand::Message { text: "hello".into() });
        assert_eq!(parse_bot_command("0"), BotCommand::SelectionCancel);
        assert_eq!(parse_bot_command("/help"), BotCommand::Help);
        assert_eq!(parse_bot_command("/帮助"), BotCommand::Help);
        assert_eq!(parse_bot_command("  /新建  "), BotCommand::New);
        assert_eq!(parse_bot_command("/bind"), BotCommand::Unknown { name: "bind".into(), raw: "/bind".into() });
        assert_eq!(parse_bot_command("/bind CODE1"), BotCommand::Bind { code: "CODE1".into() });

        // model sub-prefixes and bare fallback
        assert_eq!(
            parse_bot_command("/model provider glm"),
            BotCommand::ModelProviderSet { value: "glm".into() }
        );
        assert_eq!(
            parse_bot_command("/model model glm-5.3-flash"),
            BotCommand::ModelSet { value: "glm-5.3-flash".into() }
        );
        assert_eq!(
            parse_bot_command("/model glm-5.3-flash"),
            BotCommand::ModelSet { value: "glm-5.3-flash".into() }
        );
        assert_eq!(parse_bot_command("/模型"), BotCommand::ModelList);

        // approve needs BOTH ids
        assert_eq!(
            parse_bot_command("/approve req-1 opt-2"),
            BotCommand::Approve { request_id: "req-1".into(), option_id: "opt-2".into() }
        );
        assert!(matches!(
            parse_bot_command("/approve req-1"),
            BotCommand::Unknown { .. }
        ));
        // elicitation submit keywords
        assert_eq!(parse_bot_command("/answer done"), BotCommand::ElicitationSubmit);
        assert_eq!(parse_bot_command("/回答 提交"), BotCommand::ElicitationSubmit);
        assert_eq!(
            parse_bot_command("/answer blue"),
            BotCommand::ElicitationRespond { value: "blue".into() }
        );
        // rest keeps inner spacing, loses outer
        assert_eq!(
            parse_bot_command("/workspace   /a/b/c  "),
            BotCommand::WorkspaceSet { value: "/a/b/c".into() }
        );
    }

    #[test]
    fn command_policy_drops_removed_cli_and_fills_defaults() {
        let raw = json!({ "status": false, "cli": true, "model": null });
        let policy = normalize_bot_command_policy(&raw);
        assert_eq!(policy.status, Some(false));
        assert_eq!(policy.model, None, "explicit null reads as default");
        assert_eq!(policy.workspace, None);
        assert!(!serde_json::to_string(&policy).unwrap().contains("cli"));

        let user_only = json!({ "status": false, "new": false });
        let bot = BotConfig {
            id: "b1".into(),
            provider: "telegram".into(),
            enabled: true,
            provider_user_id: Some("u1".into()),
            allowed_commands: normalize_bot_command_policy(&user_only),
            ..Default::default()
        };
        assert!(!is_user_command_allowed(&bot, "status"));
        assert!(!is_user_command_allowed(&bot, "new"));
        assert!(is_user_command_allowed(&bot, "help"));
        assert!(is_user_command_allowed(&bot, "message"));
        assert!(is_user_command_allowed(&bot, "approve"));
        assert!(is_user_command_allowed(&bot, "task"));
        assert!(is_user_command_allowed(&bot, "stop"));
        assert!(is_user_command_allowed(&bot, "model"), "unset policy key defaults to allowed");
        assert!(is_user_command_allowed(&bot, "reconnect"), "reconnect rides workspace, still default-allowed");
    }

    #[test]
    fn workspace_allowlist_collapses_to_star_and_resolves_by_value() {
        assert_eq!(normalize_allowed_workspaces(&[]), vec!["*".to_string()]);
        assert_eq!(
            normalize_allowed_workspaces(&["*".to_string(), "/a".to_string()]),
            vec!["*".to_string()],
            "an explicit star wins over enumerations"
        );
        assert_eq!(
            normalize_allowed_workspaces(&["/a".into(), "/b".into(), "/a ".into()]),
            vec!["/a".to_string(), "/b".to_string()],
            "trimmed + deduped, order kept"
        );

        let ws = vec![
            create_workspace_ref("/home/dev/alpha", None),
            create_workspace_ref("/home/dev/beta", Some("ws-identity-1")),
        ];
        assert_eq!(ws[0].label, "alpha");
        assert_eq!(ws[1].id, "ws-identity-1", "identity is the key when present");

        let allowed = vec!["/home/dev/alpha".to_string()];
        // 1-based index into the ALLOWED list
        assert_eq!(
            resolve_workspace_by_value(&ws, "1", &allowed).map(|w| w.id),
            Some("/home/dev/alpha".to_string())
        );
        assert_eq!(resolve_workspace_by_value(&ws, "2", &allowed), None, "index beyond the allowed set");
        assert_eq!(
            resolve_workspace_by_value(&ws, "0", &allowed),
            None,
            "0 is not a selection (that is cancel)"
        );
        // label / identity matching is case-insensitive
        assert_eq!(
            resolve_workspace_by_value(&ws, "BETA", &[]).map(|w| w.id),
            Some("ws-identity-1".to_string())
        );
        assert_eq!(resolve_workspace_by_value(&ws, "nope", &[]), None);
    }

    #[test]
    fn authorization_pairs_identities_and_weixin_binds_by_id() {
        let config = BotsConfigFile {
            version: 3,
            bots: vec![
                BotConfig {
                    id: "tg".into(),
                    provider: "telegram".into(),
                    enabled: true,
                    provider_user_id: Some("42".into()),
                    ..Default::default()
                },
                BotConfig {
                    id: "wx".into(),
                    provider: "weixin".into(),
                    enabled: true,
                    ..Default::default()
                },
                BotConfig {
                    id: "tg-disabled".into(),
                    provider: "telegram".into(),
                    enabled: false,
                    provider_user_id: Some("42".into()),
                    ..Default::default()
                },
            ],
        };
        assert_eq!(
            find_authorized_bot(&config, "telegram", "42", "").map(|b| b.id.as_str()),
            Some("tg"),
            "pair (provider, providerUserId); disabled bots invisible"
        );
        assert_eq!(find_authorized_bot(&config, "telegram", "43", ""), None);
        assert_eq!(
            find_authorized_bot(&config, "weixin", "", "wx").map(|b| b.id.as_str()),
            Some("wx"),
            "weixin binds by bot id"
        );
        assert_eq!(find_authorized_bot(&config, "weixin", "", "nope"), None);

        // callback: explicit botId wins over the provider's first enabled bot
        let cb = find_callback_bot(&config, "telegram", &json!({ "botId": "tg-disabled" }));
        assert_eq!(cb.map(|b| b.id.as_str()), Some("tg-disabled"), "routing is not an authorization check");
        assert_eq!(
            find_callback_bot(&config, "telegram", &json!({})).map(|b| b.id.as_str()),
            Some("tg"),
            "fallback = first ENABLED bot of the provider"
        );
        assert!(is_bound_user(&config.bots[0], "telegram", "42"));
        assert!(!is_bound_user(&config.bots[0], "telegram", "99"));
    }

    #[test]
    fn bot_config_normalization_strips_weixin_webhook_and_pins_reply_granularity() {
        let bot = BotConfig {
            id: "w1".into(),
            provider: "weixin".into(),
            enabled: true,
            allowed_workspaces: vec![],
            reply_mode: Some("streaming_card".into()),
            webhook_url: Some("https://elsewhere.example.com/hook".into()),
            ..Default::default()
        };
        let normalized = normalize_bot_config(&bot);
        assert_eq!(normalized.allowed_workspaces, vec!["*".to_string()]);
        assert_eq!(normalized.webhook_url, None, "weixin uses the built-in address; foreign webhook fields must not persist");
        assert_eq!(
            normalized.reply_mode.as_deref(),
            Some("assistant_changes"),
            "weixin cannot render streaming cards; granularity falls to a supported one"
        );

        // feishu KEEPS streaming_card (Card JSON 2.0 renders there)
        let feishu = normalize_bot_config(&BotConfig {
            provider: "feishu".into(),
            reply_mode: None,
            ..bot
        });
        assert_eq!(feishu.reply_mode.as_deref(), Some("streaming_card"));

        // credential + webhook-secret keys follow the credential-store shape
        assert_eq!(build_bot_credential_key("b1"), "bot:b1:credential");
        assert_eq!(build_bot_webhook_secret_key("b1"), "bot:b1:webhook-secret");
    }
}
