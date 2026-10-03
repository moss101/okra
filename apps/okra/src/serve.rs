//! `okra serve --stdio` — the G0 daemon (MASTER-PLAN day 21–25, gate G0).
//!
//! Speaks line-delimited JSON-RPC over stdin/stdout with the ZCode host
//! bridge (`packages/desktop/src/host/okraBridge.ts`):
//!
//! - in:  `hello`, `v4/conversation/subscribe {topic, sessionId}`,
//!   `v4/command {envelope:{commandId, type, payload}}`
//! - out: results + notifications
//!   `v4/projection {topic, rows, control, seq, revision}`
//!
//! `rows` are ZCode `conversationRowSchema`-shaped JSON values
//! (turnHeader / userInput / assistantText / toolCall — field-for-field per
//! zcode-protocol-v4/rows.ts) so the bridge can wrap them straight into
//! `ConversationDelta` ops. The kernel event log stays the durable truth on
//! disk; this projection is the live view over it.

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use okra_agent_core::loop_::{Agent, LoopEvent};
use okra_agent_core::turn::{CancellationCategory, TurnOutcome};
use okra_kernel as kernel;
use okra_policy::ToolApprovalCeiling;
use okra_policy::approval::{
    ApprovalAnswer, ApprovalChannel, ApprovalOutcome, ApprovalPolicy, ApprovalRequest, ApprovalScope, ApprovalService,
};
use okra_host::notifications::{classify, NotificationClass};
use okra_providers::Sampler;
use okra_tools::Registry;

use crate::demo_sampler::DemoPlanner;

/// Builds a fresh sampler per turn (samplers may carry per-turn state —
/// e.g. the demo planner's step counter — so they are never shared).
pub type SamplerFactory = Arc<dyn Fn() -> Arc<dyn Sampler> + Send + Sync>;

/// Frame emitter shared with the approval watchdog (plain closure handle).
pub type BroadcastFn = Arc<dyn Fn(&str, serde_json::Value) + Send + Sync>;

/// The offline demo planner behind the factory seam (default when no
/// `--provider` is given; usable with no network).
pub fn demo_sampler_factory(cwd: PathBuf) -> SamplerFactory {
    Arc::new(move || Arc::new(DemoPlanner::new(cwd.clone())))
}

/// The real network provider behind the factory seam (`--provider openai`).
pub fn openai_sampler_factory(model: String) -> Result<SamplerFactory, String> {
    // fail fast at startup when no credential is present
    okra_providers::OpenAiProvider::from_env(model.clone())
        .ok_or_else(|| "set OKRA_API_KEY (or OPENAI_API_KEY) to use --provider openai".to_string())?;
    Ok(Arc::new(move || {
        Arc::new(
            okra_providers::OpenAiProvider::from_env(model.clone())
                .expect("key existed at startup"),
        )
    }))
}

/// The serve tool plane: the same four-tool registry the CLI runs
/// (read_file/list_dir/write_file/edit_file) — the web surface drives the
/// real toolchain, not a read-only subset.
///
/// `checkpoints` (N0029): when present, write_file/edit_file record
/// before/after bytes into the rewind checkpoint for `prompt_index` —
/// the G3 "rewind restores a scratched refactor" seam.
pub fn build_registry(cwd: &Path) -> Registry {
    build_registry_with_checkpoints(cwd, None, 0)
}

pub fn build_registry_with_checkpoints(
    cwd: &Path,
    checkpoints: Option<std::sync::Arc<Mutex<okra_host::checkpoints::CheckpointManager>>>,
    prompt_index: usize,
) -> Registry {
    let mut registry = Registry::new();
    let rf = okra_tools::builtins::read_file_tool(cwd.to_path_buf());
    let entry = rf.entry();
    registry
        .register(okra_tools::ErasedTool::simple(
            entry,
            vec![okra_tools::ResourceAccess::read_file("*")],
            move |args| rf.execute(args, None),
        ))
        .expect("read_file registers once");
    let ld = okra_tools::builtins::list_dir_tool(cwd.to_path_buf());
    let entry = ld.entry();
    registry
        .register(okra_tools::ErasedTool::simple(
            entry,
            vec![okra_tools::ResourceAccess::tree(
                okra_tools::FileAccessOperation::Read,
                "*",
            )],
            move |args| ld.execute(args),
        ))
        .expect("list_dir registers once");

    // checkpoint-capturing wrappers: read the bytes before, run the real
    // tool, read after — recording only files inside the workspace

    let wf = okra_tools::builtins::ErasedWriteFile::new(cwd.to_path_buf());
    let entry = wf.entry();
    let wf_ck = checkpoints.clone();
    let wf_cwd = cwd.to_path_buf();
    registry
        .register(okra_tools::ErasedTool::simple(
            entry,
            vec![okra_tools::ResourceAccess::write_file("*")],
            move |args| {
                let rel = rel_under(&wf_cwd, args["path"].as_str().unwrap_or_default());
                let before = rel.as_ref().and_then(|r| std::fs::read(wf_cwd.join(r)).ok());
                let result = wf.execute(args);
                let after = rel.as_ref().and_then(|r| std::fs::read(wf_cwd.join(r)).ok());
                if let Some(rel) = rel
                    && let Some(mgr) = wf_ck.as_ref()
                    && let Ok(mut mgr) = mgr.lock()
                {
                    let _ = mgr.record_operation(
                        prompt_index,
                        &rel,
                        before.as_deref(),
                        after.as_deref(),
                    );
                }
                result
            },
        ))
        .expect("write_file registers once");
    let ef = okra_tools::builtins::ErasedEditFile::new(cwd.to_path_buf());
    let entry = ef.entry();
    let ef_ck = checkpoints.clone();
    let ef_cwd = cwd.to_path_buf();
    registry
        .register(okra_tools::ErasedTool::simple(
            entry,
            vec![okra_tools::ResourceAccess::file(
                okra_tools::FileAccessOperation::Readwrite,
                "*",
            )],
            move |args| {
                let rel = rel_under(&ef_cwd, args["path"].as_str().unwrap_or_default());
                let before = rel.as_ref().and_then(|r| std::fs::read(ef_cwd.join(r)).ok());
                let result = ef.execute(args);
                let after = rel.as_ref().and_then(|r| std::fs::read(ef_cwd.join(r)).ok());
                if let Some(rel) = rel
                    && let Some(mgr) = ef_ck.as_ref()
                    && let Ok(mut mgr) = mgr.lock()
                {
                    let _ = mgr.record_operation(
                        prompt_index,
                        &rel,
                        before.as_deref(),
                        after.as_deref(),
                    );
                }
                result
            },
        ))
        .expect("edit_file registers once");
    registry
}

/// Workspace-relative form of a tool path (None = outside/empty — those
/// are the tool's own refusal cases, nothing to checkpoint).
fn rel_under(cwd: &Path, path: &str) -> Option<String> {
    if path.is_empty() {
        return None;
    }
    let p = Path::new(path);
    let joined = if p.is_absolute() { p.to_path_buf() } else { cwd.join(p) };
    joined
        .strip_prefix(cwd)
        .ok()
        .map(|r| r.to_string_lossy().into_owned())
}

/// The non-essential ToolSpec fields for ask_user (the meaningful ones
/// are set inline at the registration site).
fn ask_user_spec_rest() -> okra_tools::ToolSpec {
    okra_tools::ToolSpec {
        name: String::new(),
        namespace: None,
        title: None,
        description: String::new(),
        arguments_schema: None,
        kind: None,
        behavior_version: None,
        idempotent: false,
        read_only: false,
        timeout_ms: None,
        max_concurrency: None,
    }
}

/// The identity ToolSpec fields the computer-tool registrations set
/// inline (keeps the sites terse; the meaningful fields differ per tool).
fn computer_spec_rest() -> okra_tools::ToolSpec {
    okra_tools::ToolSpec {
        name: String::new(),
        namespace: None,
        title: None,
        description: String::new(),
        arguments_schema: None,
        kind: None,
        behavior_version: None,
        idempotent: false,
        read_only: false,
        timeout_ms: None,
        max_concurrency: None,
    }
}

/// Per-daemon computer-use consent (N0025): the Claude Desktop model —
/// `request_access` grants an app set for the session (ONE card), display-
/// scope tools need `request_full_control` (ONE card), both releasable.
#[derive(Default)]
pub struct ComputerConsent {
    /// Granted app names (System Events process names, e.g. "Finder").
    pub apps: std::collections::BTreeSet<String>,
    /// Screen-takeover consent held for this session.
    pub takeover: bool,
    /// clipboard grants (separate checkboxes on request_access).
    pub clipboard_read: bool,
    pub clipboard_write: bool,
    /// The session currently DRIVING the computer (session lock): only
    /// one at a time — Claude Desktop's rule, verbatim error and all.
    /// Acquired by the first computer tool of a turn, released at turn
    /// end (see run_turn_streaming finalize).
    pub driving: Option<String>,
}

impl ComputerConsent {
    /// Acquire the driving lock for `session`. Errors with Claude's
    /// vocabulary when another session holds it.
    pub fn acquire_driving(&mut self, session: &str) -> Result<(), String> {
        match &self.driving {
            Some(h) if h != session => Err(
                "Another okra session is currently using the computer. Press stop in that \
                 session or wait for its turn to end before driving the computer from here."
                    .to_string(),
            ),
            _ => {
                self.driving = Some(session.to_string());
                Ok(())
            }
        }
    }

    /// Release the lock when `session`'s turn ends (no-op if re-holder).
    pub fn release_driving(&mut self, session: &str) {
        if self.driving.as_deref() == Some(session) {
            self.driving = None;
        }
    }
}

pub type ComputerConsentHandle = Arc<Mutex<ComputerConsent>>;

/// Display-scope tool body: args → result text.
type DisplayFn = Box<dyn Fn(&serde_json::Value) -> Result<String, String> + Send + Sync>;
/// app_* tool body: (app, args) → result text.
type AppFn = Box<dyn Fn(&str, &serde_json::Value) -> Result<String, String> + Send + Sync>;

fn no_grant_error(app: &str) -> String {
    format!(
        "no app capability grant for {app} — call computer_request_access with this app and a reason first"
    )
}

fn no_takeover_error() -> String {
    "full-screen control not granted for this session — call computer_request_full_control first      (the user approves the screen takeover once; computer_release_full_control clears it)"
        .to_string()
}

/// N0034: the in-turn `subagent` tool — the model can delegate work to a
/// kernel-isolated child. Each call: a REAL git worktree is created
/// (branch `<name>`, shared object store), the child runs a full okra
/// turn confined to it (`--sandbox workspace-write`, fresh session =
/// inherit-nothing context, empty grants = nothing inherited), its work
/// is committed on the branch, and the worktree is cleaned up. The
/// parent checkout is never touched.
///
/// This is the G5 surface as a TOOL: "a subagent run in an isolated
/// worktree cannot touch paths outside its grant — enforced by sandbox,
/// not policy" (the child's nono confinement is the enforcement).
fn register_subagent_tool(
    registry: &mut okra_tools::Registry,
    cwd: &Path,
    session_id: &str,
    provider: Option<(String, String)>,
) {
    let spec = okra_tools::ToolSpec {
        name: "subagent".into(),
        description: "Delegate a self-contained task to an isolated subagent: it runs in its own git worktree (own branch, own session, nothing inherited from this conversation) and returns a summary plus the branch its work is committed on. The worktree is removed afterwards; merge or cherry-pick the branch to take the work.".into(),
        arguments_schema: Some(serde_json::json!({
            "type": "object",
            "properties": {
                "name": { "type": "string", "description": "short branch-safe task name (letters/digits/-/_)" },
                "task": { "type": "string", "description": "the complete task for the subagent — it sees ONLY this" }
            },
            "required": ["name", "task"],
        })),
        read_only: false,
        idempotent: false,
        kind: Some("delegation".into()),
        ..computer_spec_rest()
    };
    let metadata = okra_tools::ToolMetadata::default();
    let entry = okra_tools::ToolEntry::new(spec, metadata);
    let cwd = cwd.to_path_buf();
    let parent_session = session_id.to_string();
    let _ = registry.register(okra_tools::ErasedTool::simple(
        entry,
        vec![okra_tools::ResourceAccess::All],
        move |args: &serde_json::Value| {
            let name = args["name"].as_str().unwrap_or_default().to_string();
            let task = args["task"].as_str().unwrap_or_default().to_string();
            let outcome = run_isolated_subagent(&cwd, &name, &task, &parent_session, provider.as_ref());
            okra_tools::ToolStream::terminal_only(
                match outcome {
                    Ok(text) => Ok(okra_tools::ToolOutput::text(text)),
                    Err(e) => Err(okra_tools::ToolError::tool_failed(e)),
                },
            )
        },
    ));
}

/// One isolated subagent run (launch → confined child turn → collect →
/// cleanup). Sanctioned spawn site: the child IS a full okra binary with
/// its own kernel session and sandbox; arguments are host-built paths,
/// never raw model text.
fn run_isolated_subagent(
    cwd: &Path,
    name: &str,
    task: &str,
    parent_session: &str,
    provider: Option<&(String, String)>,
) -> Result<String, String> {
    use okra_host::subagent::{RoleScope, SubagentLauncher};
    if name.trim().is_empty() || task.trim().is_empty() {
        return Err("subagent needs a non-empty name and task".into());
    }
    let repo = okra_host::git::GitRepository::open(cwd)
        .map_err(|_| "subagent requires the workspace to be a git repository (worktree isolation)".to_string())?;
    let launcher = SubagentLauncher::new(
        repo,
        // the parent's own grants live HERE and nowhere else — the
        // launcher never copies them into the child grant
        vec![format!("parent:{parent_session}")],
    );
    let role = RoleScope {
        readable: vec![".".into()],
        writable: vec![".".into()],
    };
    let parent_scope = RoleScope {
        readable: vec![".".into()],
        writable: vec![".".into()],
    };
    let unique = format!("{name}-{}", uuid_v4());
    let worktree = std::env::temp_dir().join("okra-worktrees").join(&unique);
    let grant = launcher
        .launch(&unique, &worktree, &role, &parent_scope, task)
        .map_err(|e| format!("launch subagent: {e}"))?;

    // the child turn: full okra binary confined to the worktree
    let bin = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("okra")))
        .unwrap_or_else(|| std::path::PathBuf::from("okra"));
    let mut cmd = std::process::Command::new(&bin);
    cmd.args([
        "--cwd",
        &worktree.to_string_lossy(),
        "--sandbox",
        "workspace-write",
        "--json",
        task,
    ]);
    if let Some((provider_name, model)) = provider {
        cmd.arg("--provider").arg(provider_name);
        cmd.arg("--model").arg(model);
    }
    cmd.env("OKRA_SUBAGENT_PARENT", format!("session-{parent_session}"));
    #[allow(clippy::disallowed_methods)] // sanctioned site: the confined child runner
    let output = cmd.output().map_err(|e| format!("spawn subagent turn: {e}"))?;
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    let mut text = String::new();
    for line in stdout.lines() {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(line)
            && v["event"] == "text_delta"
            && let Some(delta) = v["text"].as_str()
        {
            text.push_str(delta);
        }
    }
    if !output.status.success() && text.trim().is_empty() {
        let _ = launcher.cleanup(&grant);
        return Err(format!(
            "subagent turn failed (exit {:?}): {}",
            output.status.code(),
            stderr.trim()
        ));
    }

    // collect: commit whatever the child produced on its branch, then
    // remove the worktree (the branch stays in the shared object store)
    let branch = grant.branch.clone();
    let commit = okra_host::subagent::SubagentLauncher::collect_work(&grant, &format!("subagent {name}: task output"))
        .ok();
    let _ = std::fs::remove_dir_all(worktree.join(".okra-sessions"));
    let _ = launcher.cleanup(&grant);
    let summary = text.trim().chars().take(2048).collect::<String>();
    Ok(format!(
        "subagent `{name}` completed (branch {branch}, commit {}). Summary:\n{summary}",
        commit.as_deref().unwrap_or("none — no changes produced"),
    ))
}

/// The Claude Desktop parity tool families (N0025): consent tools +
/// display-scope coordinate family + background app_* family. The N0023
/// trio (computer_observe/act/screenshot) stays as-is.
#[allow(clippy::too_many_lines)]
fn register_computer_parity_tools(
    registry: &mut okra_tools::Registry,
    consent: &ComputerConsentHandle,
    session_id: &str,
    bridge: &Arc<SurfaceApprovalChannel>,
) {
    use okra_tools::{ErasedTool, ResourceAccess, ToolEntry, ToolMetadata, ToolSpec};

    let mut mk = |spec: ToolSpec, metadata: ToolMetadata, f: Box<dyn Fn(&serde_json::Value) -> okra_tools::ToolStream + Send + Sync>| {
        let _ = registry.register(ErasedTool::simple(
            ToolEntry::new(spec, metadata),
            vec![ResourceAccess::All],
            f,
        ));
    };
    let rest = computer_spec_rest;
    let _ = rest;

    fn ok_text(t: String) -> okra_tools::ToolStream {
        okra_tools::ToolStream::terminal_only(Ok(okra_tools::ToolOutput::text(t)))
    }
    fn err(m: String) -> okra_tools::ToolStream {
        okra_tools::ToolStream::terminal_only(Err(okra_tools::ToolError::tool_failed(m)))
    }

    // ---- consent tools (the only ones that carry per-call approval cards) ----
    let session = session_id.to_string();
    let c = Arc::clone(consent);
    let ra_bridge = Arc::clone(bridge);
    mk(
        ToolSpec {
            name: "computer_request_access".into(),
            description: "Request per-application automation capability for this session. Each requested application gets its OWN approval card — the user decides per app; only allowed apps are granted. Grants persist until released. Required before any app_* tool. Explain the task, not the mechanism, in the reason.".into(),
            arguments_schema: Some(serde_json::json!({
                "type": "object",
                "properties": {
                    "apps": { "type": "array", "items": { "type": "string" },
                        "description": "Application names, e.g. [\"Finder\",\"TextEdit\"]" },
                    "reason": { "type": "string", "description": "One sentence shown to the user in the approval dialog" },
                    "clipboardRead": { "type": "boolean", "description": "Also grant clipboard reading" },
                    "clipboardWrite": { "type": "boolean", "description": "Also grant clipboard writing (fast path for multi-line type)" }
                },
                "required": ["apps", "reason"],
            })),
            kind: Some("computer".into()),
            ..computer_spec_rest()
        },
        // read_only=true: the executor-level card is skipped — the tool
        // raises ONE CARD PER APP itself (per-app choice: the day-4 dogfood
        // critique of the bundled whole-set dialog)
        ToolMetadata { read_only: true, ..Default::default() },
        Box::new(move |args| {
            let reason = args["reason"].as_str().unwrap_or_default().to_string();
            let apps: Vec<String> = args["apps"]
                .as_array()
                .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
                .unwrap_or_default();
            if apps.is_empty() {
                return err("apps[] required".into());
            }
            {
                let mut g = c.lock().unwrap();
                if let Err(lock_err) = g.acquire_driving(&session) {
                    return err(lock_err);
                }
            }
            // register ALL cards first: the user sees the whole request at
            // once and decides per app (allow/deny each)
            let card_args = |app: &str| {
                serde_json::json!({ "app": app, "reason": reason }).to_string()
            };
            for app in &apps {
                ra_bridge.register(
                    &format!("apr-app-{app}"),
                    "computer_app_grant",
                    &format!("grant-{app}"),
                    &card_args(app),
                );
            }
            let mut granted: Vec<String> = Vec::new();
            let mut denied: Vec<String> = Vec::new();
            for app in &apps {
                let outcome = ra_bridge.wait_for(&format!("apr-app-{app}"));
                if outcome.grants() {
                    granted.push(app.clone());
                } else {
                    denied.push(app.clone());
                }
            }
            {
                let mut g = c.lock().unwrap();
                for a in &granted {
                    g.apps.insert(a.clone());
                }
                if args["clipboardRead"].as_bool() == Some(true) && !granted.is_empty() {
                    g.clipboard_read = true;
                }
                if args["clipboardWrite"].as_bool() == Some(true) && !granted.is_empty() {
                    g.clipboard_write = true;
                }
            }
            ok_text(format!(
                "granted: [{}] denied: [{}] (reason: {reason}). Display-scope tools still need computer_request_full_control.",
                granted.join(", "),
                denied.join(", "),
            ))
        }),
    );

    let c = Arc::clone(consent);
    mk(
        ToolSpec {
            name: "computer_list_granted_applications".into(),
            description: "List the applications currently granted for this session and whether full-screen control is held.".into(),
            arguments_schema: Some(serde_json::json!({ "type": "object", "properties": {} })),
            read_only: true,
            kind: Some("computer".into()),
            ..computer_spec_rest()
        },
        ToolMetadata { read_only: true, ..Default::default() },
        Box::new(move |_| {
            let g = c.lock().unwrap();
            ok_text(serde_json::to_string(&serde_json::json!({
                "granted": g.apps.iter().collect::<Vec<_>>(),
                "fullControl": g.takeover,
            })).unwrap_or_default())
        }),
    );

    let c = Arc::clone(consent);
    mk(
        ToolSpec {
            name: "computer_release_access".into(),
            description: "Release per-app grants (all, or the listed apps). Always safe.".into(),
            arguments_schema: Some(serde_json::json!({
                "type": "object",
                "properties": { "apps": { "type": "array", "items": { "type": "string" } } }
            })),
            read_only: true,
            kind: Some("computer".into()),
            ..computer_spec_rest()
        },
        ToolMetadata { read_only: true, ..Default::default() },
        Box::new(move |args| {
            let mut g = c.lock().unwrap();
            let listed: Vec<String> = args["apps"]
                .as_array()
                .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
                .unwrap_or_default();
            if listed.is_empty() {
                g.apps.clear();
            } else {
                for a in listed {
                    g.apps.remove(&a);
                }
            }
            ok_text("released".into())
        }),
    );

    let c = Arc::clone(consent);
    mk(
        ToolSpec {
            name: "computer_request_full_control".into(),
            description: "Ask the user to approve full-screen control (screenshot, coordinate clicks, typing) for THIS SESSION. Used before display-scope tools. If declined, continue with background app_* tools only.".into(),
            arguments_schema: Some(serde_json::json!({ "type": "object", "properties": {} })),
            kind: Some("computer".into()),
            ..computer_spec_rest()
        },
        ToolMetadata::default(), // approval card = takeover consent
        Box::new(move |_| {
            c.lock().unwrap().takeover = true;
            ok_text("full-screen control granted for this session (computer_release_full_control clears it)".into())
        }),
    );

    let c = Arc::clone(consent);
    mk(
        ToolSpec {
            name: "computer_release_full_control".into(),
            description: "Drop back to BACKGROUND control: clears the full-screen approval so the NEXT display-scope action asks again. Always safe.".into(),
            arguments_schema: Some(serde_json::json!({ "type": "object", "properties": {} })),
            read_only: true,
            kind: Some("computer".into()),
            ..computer_spec_rest()
        },
        ToolMetadata { read_only: true, ..Default::default() },
        Box::new(move |_| {
            c.lock().unwrap().takeover = false;
            ok_text("full-screen control released".into())
        }),
    );

    // ---- display-scope coordinate family (takeover-gated) ----
    let mut display = |name: &str, desc: &str, schema: serde_json::Value, f: DisplayFn| {
        let c = Arc::clone(consent);
        let name = name.to_string();
        let session = session_id.to_string();
        mk(
            ToolSpec {
                name: name.clone(),
                description: desc.to_string(),
                arguments_schema: Some(schema),
                read_only: true,
                kind: Some("computer".into()),
                ..computer_spec_rest()
            },
            ToolMetadata { read_only: true, ..Default::default() },
            Box::new(move |args| {
                {
                    let mut g = c.lock().unwrap();
                    if let Err(lock_err) = g.acquire_driving(&session) {
                        return err(lock_err);
                    }
                    if !g.takeover {
                        return err(no_takeover_error());
                    }
                }
                match f(args) {
                    Ok(text) => ok_text(text),
                    Err(e) => err(e),
                }
            }),
        );
    };

    display(
        "computer_shot",
        "Full-screen screenshot (no sound). Returns a PNG data URL. Coordinates for other display tools refer to this frame.",
        serde_json::json!({ "type": "object", "properties": { "save_to_disk": { "type": "boolean" } } }),
        Box::new(|args| {
            let png = okra_computer::backend::screenshot()?;
            if args["save_to_disk"].as_bool() == Some(true) {
                let path = std::env::temp_dir().join(format!(
                    "okra-cu-share-{}.png",
                    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_millis()).unwrap_or_default()
                ));
                std::fs::write(&path, &png).map_err(|e| format!("save: {e}"))?;
                return Ok(format!("saved: {}", path.display()));
            }
            Ok(format!("data:image/png;base64,{}", b64_png(&png)))
        }),
    );
    display(
        "computer_zoom",
        "Re-capture a REGION of the screen at full density to inspect small text. Region is in the last full-screenshot frame.",
        serde_json::json!({ "type": "object", "properties": {
            "x": {"type": "integer"}, "y": {"type": "integer"},
            "w": {"type": "integer"}, "h": {"type": "integer"} },
            "required": ["x", "y", "w", "h"] }),
        Box::new(|args| {
            let g = |k: &str| args[k].as_i64().unwrap_or(0);
            let png = okra_computer::backend::screenshot_region(g("x"), g("y"), g("w"), g("h"))?;
            Ok(format!("data:image/png;base64,{}", b64_png(&png)))
        }),
    );
    display("computer_left_click", "Left-click at a coordinate in the last full-screenshot frame.",
        serde_json::json!({ "type": "object", "properties": { "x": {"type": "integer"}, "y": {"type": "integer"} }, "required": ["x", "y"] }),
        Box::new(|args| { okra_computer::backend::click_point(args["x"].as_i64().unwrap_or(0), args["y"].as_i64().unwrap_or(0)).map(|_| "clicked".into()) }));
    display("computer_double_click", "Double-click (selects a word in most text editors).",
        serde_json::json!({ "type": "object", "properties": { "x": {"type": "integer"}, "y": {"type": "integer"} }, "required": ["x", "y"] }),
        Box::new(|args| { okra_computer::backend::double_click_point(args["x"].as_i64().unwrap_or(0), args["y"].as_i64().unwrap_or(0)).map(|_| "double-clicked".into()) }));
    display("computer_right_click", "Right-click (opens a context menu).",
        serde_json::json!({ "type": "object", "properties": { "x": {"type": "integer"}, "y": {"type": "integer"} }, "required": ["x", "y"] }),
        Box::new(|args| { okra_computer::backend::right_click_point(args["x"].as_i64().unwrap_or(0), args["y"].as_i64().unwrap_or(0)).map(|_| "right-clicked".into()) }));
    display("computer_type", "Type text into whatever has keyboard focus. Newlines supported.",
        serde_json::json!({ "type": "object", "properties": { "text": {"type": "string"} }, "required": ["text"] }),
        Box::new(|args| { okra_computer::backend::type_text(args["text"].as_str().unwrap_or_default()).map(|_| "typed".into()) }));
    display("computer_key", "Press a key or combo, e.g. `Return`, `cmd+a`, `ctrl+shift+t`.",
        serde_json::json!({ "type": "object", "properties": { "key": {"type": "string"} }, "required": ["key"] }),
        Box::new(|args| { okra_computer::backend::press_combo(args["key"].as_str().unwrap_or_default()).map(|_| "pressed".into()) }));
    display("computer_scroll", "Scroll at a coordinate; dy>0 scrolls down, dy<0 up.",
        serde_json::json!({ "type": "object", "properties": { "x": {"type": "integer"}, "y": {"type": "integer"}, "dy": {"type": "integer"} }, "required": ["x", "y", "dy"] }),
        Box::new(|args| { okra_computer::backend::scroll_at(args["x"].as_i64().unwrap_or(0), args["y"].as_i64().unwrap_or(0), args["dy"].as_i64().unwrap_or(0) as i32).map(|_| "scrolled".into()) }));
    display("computer_mouse_move", "Move the cursor without clicking.",
        serde_json::json!({ "type": "object", "properties": { "x": {"type": "integer"}, "y": {"type": "integer"} }, "required": ["x", "y"] }),
        Box::new(|args| { okra_computer::backend::mouse_move(args["x"].as_i64().unwrap_or(0), args["y"].as_i64().unwrap_or(0)).map(|_| "moved".into()) }));
    display("computer_drag", "Press-drag-release from one coordinate to another.",
        serde_json::json!({ "type": "object", "properties": {
            "from_x": {"type": "integer"}, "from_y": {"type": "integer"},
            "to_x": {"type": "integer"}, "to_y": {"type": "integer"} },
            "required": ["from_x", "from_y", "to_x", "to_y"] }),
        Box::new(|args| {
            let g = |k: &str| args[k].as_i64().unwrap_or(0);
            okra_computer::backend::drag((g("from_x"), g("from_y")), (g("to_x"), g("to_y"))).map(|_| "dragged".into())
        }));
    display("computer_cursor_position", "Current cursor position (logical points).",
        serde_json::json!({ "type": "object", "properties": {} }),
        Box::new(|_| { let (x, y) = okra_computer::backend::cursor_position()?; Ok(format!("{x},{y}")) }));

    // non-consent helpers
    mk(
        ToolSpec {
            name: "computer_open_application".into(),
            description: "Launch/ensure an application is running. Does not force it to the front.".into(),
            arguments_schema: Some(serde_json::json!({
                "type": "object", "properties": { "app": { "type": "string" } }, "required": ["app"] })),
            read_only: true,
            kind: Some("computer".into()),
            ..computer_spec_rest()
        },
        ToolMetadata { read_only: true, ..Default::default() },
        Box::new(|args| {
            match okra_computer::backend::open_application(args["app"].as_str().unwrap_or_default()) {
                Ok(()) => ok_text("launched".into()),
                Err(e) => err(e),
            }
        }),
    );
    mk(
        ToolSpec {
            name: "computer_list_apps".into(),
            description: "Running applications first, then installed ones — pick names for computer_request_access.".into(),
            arguments_schema: Some(serde_json::json!({ "type": "object", "properties": {} })),
            read_only: true,
            kind: Some("computer".into()),
            ..computer_spec_rest()
        },
        ToolMetadata { read_only: true, ..Default::default() },
        Box::new(|_| {
            let running = okra_computer::backend::list_running_apps().unwrap_or_default();
            let installed = okra_computer::backend::list_installed_apps();
            ok_text(serde_json::to_string(&serde_json::json!({
                "running": running, "installed": installed,
            })).unwrap_or_default())
        }),
    );

    // ---- clipboard tools (grant checkboxes from request_access) ----
    let session = session_id.to_string();
    let cr = Arc::clone(consent);
    mk(
        ToolSpec {
            name: "computer_read_clipboard".into(),
            description: "Read the system clipboard. Requires the clipboardRead grant on computer_request_access.".into(),
            arguments_schema: Some(serde_json::json!({ "type": "object", "properties": {} })),
            read_only: true,
            kind: Some("computer".into()),
            ..computer_spec_rest()
        },
        ToolMetadata { read_only: true, ..Default::default() },
        {
            let session = session.clone();
            Box::new(move |_| {
            let mut g = cr.lock().unwrap();
            if let Err(lock_err) = g.acquire_driving(&session) {
                return err(lock_err);
            }
            if !g.clipboard_read {
                return err("clipboard reading not granted — request it via computer_request_access {clipboardRead: true}".into());
            }
            match okra_computer::backend::read_clipboard() {
                Ok(t) => ok_text(t),
                Err(e) => err(e),
            }
        })
        },
    );
    let cw = Arc::clone(consent);
    mk(
        ToolSpec {
            name: "computer_write_clipboard".into(),
            description: "Write text to the system clipboard. Requires the clipboardWrite grant.".into(),
            arguments_schema: Some(serde_json::json!({
                "type": "object", "properties": { "text": {"type": "string"} }, "required": ["text"] })),
            read_only: true,
            kind: Some("computer".into()),
            ..computer_spec_rest()
        },
        ToolMetadata { read_only: true, ..Default::default() },
        {
            let session = session.clone();
            Box::new(move |args| {
            let mut g = cw.lock().unwrap();
            if let Err(lock_err) = g.acquire_driving(&session) {
                return err(lock_err);
            }
            if !g.clipboard_write {
                return err("clipboard writing not granted — request it via computer_request_access {clipboardWrite: true}".into());
            }
            match okra_computer::backend::write_clipboard(args["text"].as_str().unwrap_or_default()) {
                Ok(()) => ok_text("written".into()),
                Err(e) => err(e),
            }
        })
        },
    );

    // ---- computer_batch: display-scope actions, sequential, stop on first error ----
    let session = session_id.to_string();
    let cb = Arc::clone(consent);
    mk(
        ToolSpec {
            name: "computer_batch".into(),
            description: "Run display-scope actions sequentially, stopping at the first error. Each individual tool call requires a model round trip (seconds) — batch instead. Actions refer to the full screenshot taken before the batch.".into(),
            arguments_schema: Some(serde_json::json!({
                "type": "object",
                "properties": {
                    "actions": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "kind": { "type": "string", "enum": ["left_click", "double_click", "right_click", "type", "key", "scroll", "mouse_move", "drag", "shot"] },
                                "x": {"type": "integer"}, "y": {"type": "integer"},
                                "to_x": {"type": "integer"}, "to_y": {"type": "integer"},
                                "text": {"type": "string"}, "key": {"type": "string"},
                                "dy": {"type": "integer"}
                            },
                            "required": ["kind"]
                        }
                    }
                },
                "required": ["actions"],
            })),
            read_only: true,
            kind: Some("computer".into()),
            ..computer_spec_rest()
        },
        ToolMetadata { read_only: true, ..Default::default() },
        Box::new(move |args| {
            {
                let mut g = cb.lock().unwrap();
                if let Err(lock_err) = g.acquire_driving(&session) {
                    return err(lock_err);
                }
                if !g.takeover {
                    return err(no_takeover_error());
                }
            }
            let actions = args["actions"].as_array().cloned().unwrap_or_default();
            let mut out_lines: Vec<String> = Vec::new();
            for (i, a) in actions.iter().enumerate() {
                let g = |k: &str| a[k].as_i64().unwrap_or(0);
                let r: Result<String, String> = match a["kind"].as_str().unwrap_or_default() {
                    "left_click" => okra_computer::backend::click_point(g("x"), g("y")).map(|_| "clicked".into()),
                    "double_click" => okra_computer::backend::double_click_point(g("x"), g("y")).map(|_| "double-clicked".into()),
                    "right_click" => okra_computer::backend::right_click_point(g("x"), g("y")).map(|_| "right-clicked".into()),
                    "type" => okra_computer::backend::type_text(a["text"].as_str().unwrap_or_default()).map(|_| "typed".into()),
                    "key" => okra_computer::backend::press_combo(a["key"].as_str().unwrap_or_default()).map(|_| "pressed".into()),
                    "scroll" => okra_computer::backend::scroll_at(g("x"), g("y"), a["dy"].as_i64().unwrap_or(0) as i32).map(|_| "scrolled".into()),
                    "mouse_move" => okra_computer::backend::mouse_move(g("x"), g("y")).map(|_| "moved".into()),
                    "drag" => okra_computer::backend::drag((g("x"), g("y")), (g("to_x"), g("to_y"))).map(|_| "dragged".into()),
                    "shot" => okra_computer::backend::screenshot()
                        .map(|p| format!("data:image/png;base64,{}", b64_png(&p))),
                    other => Err(format!("unknown action kind {other}")),
                };
                match r {
                    Ok(t) => out_lines.push(format!("[{i}] ok: {t}")),
                    Err(e) => {
                        out_lines.push(format!("[{i}] ERROR: {e}"));
                        out_lines.push("batch stopped at first error".into());
                        break;
                    }
                }
            }
            ok_text(out_lines.join("\n"))
        }),
    );

    // ---- app_batch: app actions on ONE granted app, sequential, stop on first error ----
    let session = session_id.to_string();
    let ab = Arc::clone(consent);
    mk(
        ToolSpec {
            name: "computer_app_batch".into(),
            description: "Run app_* actions on ONE granted application sequentially (observe/click/type/focus/ax_find), stopping at the first error. One observe is shared by the whole batch.".into(),
            arguments_schema: Some(serde_json::json!({
                "type": "object",
                "properties": {
                    "app": { "type": "string" },
                    "actions": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "kind": { "type": "string", "enum": ["click", "type", "focus", "key", "ax_find"] },
                                "element": {"type": "string"}, "text": {"type": "string"},
                                "key": {"type": "string"}, "role": {"type": "string"}, "title": {"type": "string"}
                            },
                            "required": ["kind"]
                        }
                    }
                },
                "required": ["app", "actions"],
            })),
            read_only: true,
            kind: Some("computer".into()),
            ..computer_spec_rest()
        },
        ToolMetadata { read_only: true, ..Default::default() },
        Box::new(move |args| {
            let app = args["app"].as_str().unwrap_or_default().to_string();
            {
                let mut g = ab.lock().unwrap();
                if let Err(lock_err) = g.acquire_driving(&session) {
                    return err(lock_err);
                }
                if !g.apps.contains(&app) {
                    return err(no_grant_error(&app));
                }
            }
            let actions = args["actions"].as_array().cloned().unwrap_or_default();
            let mut out_lines: Vec<String> = Vec::new();
            for (i, a) in actions.iter().enumerate() {
                // one observe per action keeps element ids fresh (Claude's
                // re-observe rule); v1 simplicity over round-trip savings
                let tree = match okra_computer::backend::observe(&app) {
                    Ok(t) => t,
                    Err(e) => {
                        out_lines.push(format!("[{i}] ERROR observe: {e}"));
                        out_lines.push("batch stopped at first error".into());
                        break;
                    }
                };
                let r: Result<String, String> = match a["kind"].as_str().unwrap_or_default() {
                    "click" => okra_computer::backend::click_element(
                        &app, &tree, a["element"].as_str().unwrap_or_default(),
                    )
                    .map(|_| "clicked".into()),
                    "type" => okra_computer::backend::app_type_into(
                        &app, &tree, a["element"].as_str(), a["text"].as_str().unwrap_or_default(),
                    )
                    .map(|_| "typed".into()),
                    "focus" => okra_computer::backend::app_focus(
                        &app, &tree, a["element"].as_str().unwrap_or_default(),
                    )
                    .map(|_| "focused".into()),
                    "key" => okra_computer::backend::press_combo(a["key"].as_str().unwrap_or_default())
                        .map(|_| "pressed".into()),
                    "ax_find" => {
                        let found = okra_computer::backend::app_ax_find(
                            &tree, a["role"].as_str(), a["title"].as_str(),
                        );
                        Ok(serde_json::to_string(
                            &found.iter().map(|e| &e.id).collect::<Vec<_>>(),
                        )
                        .unwrap_or_default())
                    }
                    other => Err(format!("unknown action kind {other}")),
                };
                match r {
                    Ok(t) => out_lines.push(format!("[{i}] ok: {t}")),
                    Err(e) => {
                        out_lines.push(format!("[{i}] ERROR: {e}"));
                        out_lines.push("batch stopped at first error".into());
                        break;
                    }
                }
            }
            ok_text(out_lines.join("\n"))
        }),
    );

    // ---- background app_* family (per-app grant gated, no per-call cards) ----
    let mut app_tool = |name: &str, desc: &str, schema: serde_json::Value, f: AppFn| {
        let c = Arc::clone(consent);
        let session = session_id.to_string();
        mk(
            ToolSpec {
                name: name.to_string(),
                description: desc.to_string(),
                arguments_schema: Some(schema),
                read_only: true,
                kind: Some("computer".into()),
                ..computer_spec_rest()
            },
            ToolMetadata { read_only: true, ..Default::default() },
            Box::new(move |args| {
                let app = args["app"].as_str().unwrap_or_default().to_string();
                if app.is_empty() {
                    return err("app required".into());
                }
                {
                    let mut g = c.lock().unwrap();
                    if let Err(lock_err) = g.acquire_driving(&session) {
                        return err(lock_err);
                    }
                    if !g.apps.contains(&app) {
                        return err(no_grant_error(&app));
                    }
                }
                match f(&app, args) {
                    Ok(t) => ok_text(t),
                    Err(e) => err(e),
                }
            }),
        );
    };

    app_tool("computer_app_list_windows",
        "List an app's windows (id, title, bounds) — from a fresh AX observe. Background: never raises windows.",
        serde_json::json!({ "type": "object", "properties": { "app": {"type": "string"} }, "required": ["app"] }),
        Box::new(|app, _| {
            let tree = okra_computer::backend::observe(app)?;
            let wins = okra_computer::backend::app_list_windows(&tree);
            Ok(serde_json::to_string(&wins).unwrap_or_default())
        }));
    app_tool("computer_app_screenshot",
        "Capture one window of a granted app (fresh region capture) + a digest of its interactive elements (indices for app_click). Background.",
        serde_json::json!({ "type": "object", "properties": { "app": {"type": "string"}, "window": { "type": "string", "description": "window id from app_list_windows, e.g. w0" } }, "required": ["app", "window"] }),
        Box::new(|app, args| {
            let win = args["window"].as_str().unwrap_or("w0");
            let tree = okra_computer::backend::observe(app)?;
            let (png, digest) = okra_computer::backend::app_screenshot(&tree, win)?;
            let digest: Vec<serde_json::Value> = digest
                .iter()
                .map(|e| serde_json::json!({ "id": e.id, "role": e.role, "label": e.label, "actions": e.actions }))
                .collect();
            Ok(format!(
                "data:image/png;base64,{}\n\nelements: {}",
                b64_png(&png),
                serde_json::to_string(&digest).unwrap_or_default()
            ))
        }));
    app_tool("computer_app_ax_find",
        "Search the fresh AX tree of a granted app by role and/or title substring → element ids.",
        serde_json::json!({ "type": "object", "properties": {
            "app": {"type": "string"}, "role": {"type": "string"}, "title": {"type": "string"} },
            "required": ["app"] }),
        Box::new(|app, args| {
            let tree = okra_computer::backend::observe(app)?;
            let found = okra_computer::backend::app_ax_find(
                &tree,
                args["role"].as_str(),
                args["title"].as_str(),
            );
            let out: Vec<serde_json::Value> = found
                .iter()
                .map(|e| serde_json::json!({ "id": e.id, "role": e.role, "label": e.label }))
                .collect();
            Ok(serde_json::to_string(&out).unwrap_or_default())
        }));
    app_tool("computer_app_click",
        "Click an element of a granted app by element id (AXPress when it reports the action, else coordinate at its center). Background: no window raising.",
        serde_json::json!({ "type": "object", "properties": {
            "app": {"type": "string"}, "element": {"type": "string", "description": "element id from observe/screenshot digest, e.g. w0/e1" } },
            "required": ["app", "element"] }),
        Box::new(|app, args| {
            let tree = okra_computer::backend::observe(app)?;
            okra_computer::backend::click_element(app, &tree, args["element"].as_str().unwrap_or_default())
                .map(|_| "clicked".to_string())
        }));
    app_tool("computer_app_focus",
        "Set AX focus on an element WITHOUT clicking or bringing the app front — prepares coordinate-less typing.",
        serde_json::json!({ "type": "object", "properties": {
            "app": {"type": "string"}, "element": {"type": "string"} }, "required": ["app", "element"] }),
        Box::new(|app, args| {
            let tree = okra_computer::backend::observe(app)?;
            okra_computer::backend::app_focus(app, &tree, args["element"].as_str().unwrap_or_default())
                .map(|_| "focused".to_string())
        }));
    app_tool("computer_app_type",
        "Type text into a granted app: focus the element first (or `focused` for the app current focus), then type.",
        serde_json::json!({ "type": "object", "properties": {
            "app": {"type": "string"}, "element": {"type": "string"}, "target": {"type": "string", "enum": ["focused"]},
            "text": {"type": "string"} }, "required": ["app", "text"] }),
        Box::new(|app, args| {
            let tree = okra_computer::backend::observe(app)?;
            let el = args["element"].as_str();
            let text = args["text"].as_str().unwrap_or_default();
            okra_computer::backend::app_type_into(app, &tree, el, text).map(|_| "typed".to_string())
        }));
    app_tool("computer_app_key",
        "Send a key/combo to a granted app (focus an element first for reliable delivery).",
        serde_json::json!({ "type": "object", "properties": {
            "app": {"type": "string"}, "element": {"type": "string"}, "key": {"type": "string"} },
            "required": ["app", "key"] }),
        Box::new(|app, args| {
            let tree = okra_computer::backend::observe(app)?;
            if let Some(el) = args["element"].as_str() {
                okra_computer::backend::app_focus(app, &tree, el)?;
            }
            okra_computer::backend::press_combo(args["key"].as_str().unwrap_or_default())
                .map(|_| "pressed".to_string())
        }));
}

/// Minimal base64 for PNG data URLs (no dependency added).
pub(crate) fn b64_png(png: &[u8]) -> String {
    const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(png.len().div_ceil(3) * 4);
    for chunk in png.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        out.push(T[(b[0] >> 2) as usize] as char);
        out.push(T[(((b[0] & 0x03) << 4) | (b[1] >> 4)) as usize] as char);
        out.push(if chunk.len() > 1 { T[(((b[1] & 0x0f) << 2) | (b[2] >> 6)) as usize] as char } else { '=' });
        out.push(if chunk.len() > 2 { T[(b[2] & 0x3f) as usize] as char } else { '=' });
    }
    out
}

/// Register configured MCP servers' tools into the turn registry
/// (N0019 + N0022): each enabled stdio server connects ONCE as a
/// persistent session (initialize + tools_list over a live child), and
/// every tool registers under `mcp__<server>__<tool>` sharing that
/// session — stateful servers keep state, stateless ones pay startup
/// once. MCP tools are unknown side-effectors: `ResourceAccess::All` +
/// read_only=false, so the approval card fires before any call executes.
/// Failed/timeout servers degrade to absent (fail-open, N0018 contract).
pub fn register_mcp_tools(
    registry: &mut okra_tools::Registry,
    cwd: &Path,
    sessions: &Mutex<std::collections::BTreeMap<String, Arc<Mutex<okra_tools::McpClient>>>>,
) -> usize {
    use std::sync::mpsc;
    let home = okra_host::fsutil::home_dir().unwrap_or_else(|| cwd.to_path_buf());
    let svc = okra_host::mcp_sync::McpSyncService::new(home);
    let Ok(servers) = svc.load(Some(cwd)) else { return 0 };
    let mut registered = 0usize;
    for r in servers {
        if !r.enabled {
            continue;
        }
        let Some(command) = r.config["command"].as_str().map(str::to_string) else {
            continue;
        };
        let args: Vec<String> = r.config["args"]
            .as_array()
            .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
            .unwrap_or_default();
        // ONE persistent session per server PER DAEMON: cached across
        // turns; a dead cached session respawns once
        let cached = sessions.lock().unwrap().get(&r.name).cloned();
        let (session, tools) = if let Some(shared) = &cached {
            // reuse: refresh the tool list over the live session
            let (tx, rx) = mpsc::channel();
            let shared = Arc::clone(shared);
            std::thread::spawn(move || {
                let tools = shared.lock().unwrap().tools_list().unwrap_or_default();
                let _ = tx.send(tools);
            });
            match rx.recv_timeout(std::time::Duration::from_secs(20)) {
                Ok(tools) if !tools.is_empty() => (cached.clone(), tools),
                _ => {
                    // dead session: drop and respawn below
                    sessions.lock().unwrap().remove(&r.name);
                    (None, Vec::new())
                }
            }
        } else {
            (None, Vec::new())
        };
        let (session, tools) = if session.is_some() {
            (session, tools)
        } else {
            let (tx, rx) = mpsc::channel();
            let srv = r.name.clone();
            let cmd = command.clone();
            let argv = args.clone();
            std::thread::spawn(move || {
                let outcome = match okra_tools::McpClient::stdio_persistent(&srv, &cmd, &argv) {
                    Ok(mut c) => {
                        if c.initialize().is_err() {
                            None
                        } else {
                            let tools = c.tools_list().unwrap_or_default();
                            Some((c, tools))
                        }
                    }
                    Err(_) => None,
                };
                let _ = tx.send(outcome);
            });
            match rx.recv_timeout(std::time::Duration::from_secs(20)) {
                Ok(Some((c, tools))) => {
                    let shared = Arc::new(Mutex::new(c));
                    sessions
                        .lock()
                        .unwrap()
                        .insert(r.name.clone(), Arc::clone(&shared));
                    (Some(shared), tools)
                }
                _ => (None, Vec::new()),
            }
        };
        let Some(session) = session else { continue };
        for t in tools {
            let srv = r.name.clone();
            let tool = t.name.clone();
            let session = Arc::clone(&session);
            let spec = okra_tools::ToolSpec {
                name: format!("mcp__{srv}_{tool}"),
                namespace: Some("mcp".into()),
                title: Some(tool.clone()),
                description: format!("[{srv}] {}", t.description),
                arguments_schema: Some(t.input_schema.clone()),
                kind: Some("mcp".into()),
                behavior_version: None,
                idempotent: false,
                read_only: false,
                timeout_ms: Some(30_000),
                max_concurrency: Some(1),
            };
            let metadata = okra_tools::ToolMetadata {
                read_only: false,
                ..Default::default()
            };
            let entry = okra_tools::ToolEntry::new(spec, metadata);
            let _ = registry.register(okra_tools::ErasedTool::simple(
                entry,
                vec![okra_tools::ResourceAccess::All],
                move |args: &serde_json::Value| {
                    // the shared persistent session: no per-call spawn
                    let mut c = session.lock().unwrap();
                    match c.tools_call(&tool, args.clone()) {
                        Ok((text, is_error)) => {
                            if is_error {
                                okra_tools::ToolStream::terminal_only(Err(
                                    okra_tools::ToolError::tool_failed(text),
                                ))
                            } else {
                                okra_tools::ToolStream::terminal_only(Ok(
                                    okra_tools::ToolOutput::text(text),
                                ))
                            }
                        }
                        Err(e) => okra_tools::ToolStream::terminal_only(Err(
                            okra_tools::ToolError::tool_failed(e),
                        )),
                    }
                },
            ));
            registered += 1;
        }
    }
    registered
}

/// One live question from the task to the user (N0020).
#[derive(Debug, Clone)]
pub struct PendingQuestion {
    pub id: String,
    pub question: String,
    pub asked_at: f64,
}

/// The surface-side ask/answer channel (mirrors SurfaceApprovalChannel):
/// the `ask_user` tool blocks on `ask` (bounded waits) until
/// `answerQuestion` resolves it or the stop flag flips (→ a cancelled
/// marker returns to the model as the tool result).
pub struct SurfaceQuestionChannel {
    pending: Mutex<Option<PendingQuestion>>,
    answers: Mutex<std::collections::HashMap<String, String>>,
    wake: std::sync::Condvar,
    stop: Arc<AtomicBool>,
}

impl SurfaceQuestionChannel {
    pub fn new(stop: Arc<AtomicBool>) -> Self {
        SurfaceQuestionChannel {
            pending: Mutex::new(None),
            answers: Mutex::new(std::collections::HashMap::new()),
            wake: std::sync::Condvar::new(),
            stop,
        }
    }

    /// Tool-side ask: block for the answer; stop-flag aware.
    pub fn ask(&self, question: String) -> String {
        let id = format!("q-{}", uuid_v4());
        *self.pending.lock().unwrap() = Some(PendingQuestion {
            id: id.clone(),
            question,
            asked_at: now_ms(),
        });
        loop {
            let answers = self.answers.lock().unwrap();
            let (mut answers, _) = self
                .wake
                .wait_timeout(answers, std::time::Duration::from_millis(250))
                .unwrap();
            if let Some(a) = answers.remove(&id) {
                drop(answers);
                *self.pending.lock().unwrap() = None;
                self.wake.notify_all();
                return a;
            }
            drop(answers);
            if self.stop.load(Ordering::Relaxed) {
                *self.pending.lock().unwrap() = None;
                self.wake.notify_all();
                return "(question cancelled — the turn was stopped)".to_string();
            }
        }
    }

    /// A surface answered. Returns whether the id matched a live ask.
    pub fn resolve(&self, question_id: &str, answer: String) -> bool {
        let known = matches!(self.pending.lock().unwrap().as_ref(), Some(p) if p.id == question_id);
        if known {
            self.answers
                .lock()
                .unwrap()
                .insert(question_id.to_string(), answer);
            self.wake.notify_all();
        }
        known
    }

    /// The `control.awaitingQuestion` payload, when a live ask exists.
    pub fn pending_snapshot(&self) -> Option<serde_json::Value> {
        self.pending.lock().unwrap().as_ref().map(|p| {
            serde_json::json!({
                "questionId": p.id,
                "question": p.question,
                "askedAt": p.asked_at,
            })
        })
    }
}

/// One steered input waiting in the serve-level queue: text plus its
/// attachment paths (N0017 follow-up — steering no longer drops files).
#[derive(Debug, Clone)]
pub struct SteeredInput {
    pub text: String,
    pub attachments: Vec<String>,
}

/// One live session projection.
pub struct SessionProjection {
    session_id: String,
    rows: Vec<serde_json::Value>,
    control: serde_json::Value,
    seq: u64,
    revision: u64,
    next_row_id: u64,
    /// First user text of the session (the task title the UI shows).
    title: Option<String>,
}

impl SessionProjection {
    pub fn new(session_id: String, _cwd: std::path::PathBuf) -> Self {
        SessionProjection {
            session_id,
            rows: Vec::new(),
            control: serde_json::json!({
                "phase": "draft",
                "sessionEnded": false,
                "canStop": false,
                "stopState": "idle",
                "stopTargetKind": "unknown",
                "activeWorks": [],
                "lastError": null,
                "apiRetry": null,
            }),
            seq: 0,
            revision: 0,
            next_row_id: 1,
            title: None,
        }
    }

    /// The session title (first user text, truncated) — set once.
    pub fn set_title_if_empty(&mut self, text: &str) {
        if self.title.is_none() {
            let t: String = text.chars().take(80).collect();
            self.title = Some(t);
        }
    }

    pub fn title(&self) -> Option<&str> {
        self.title.as_deref()
    }

    pub fn session_id(&self) -> Option<&str> {
        Some(&self.session_id)
    }

    fn alloc_row_id(&mut self) -> u64 {
        let id = self.next_row_id;
        self.next_row_id += 1;
        id
    }

    fn base_row(&mut self, turn_id: &str) -> (u64, serde_json::Value, f64) {
        let row_id = self.alloc_row_id();
        self.seq += 1;
        let created_seq = self.seq;
        let now = now_ms();
        (
            row_id,
            serde_json::json!({
                "rowId": row_id,
                "turnId": turn_id,
                "createdAt": now,
                "createdAtSeq": created_seq,
            }),
            now,
        )
    }

    fn upsert_row(&mut self, row: serde_json::Value) {
        let id = row["rowId"].as_u64().unwrap_or(0);
        if let Some(slot) = self
            .rows
            .iter_mut()
            .find(|r| r["rowId"].as_u64() == Some(id))
        {
            *slot = row;
        } else {
            self.rows.push(row);
        }
        self.revision += 1;
    }
}

fn now_ms() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as f64)
        .unwrap_or(0.0)
}

pub fn uuid_v4() -> String {
    // random enough for a demo turn id; no uuid dependency needed
    let mut bytes = [0u8; 16];
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64 ^ d.as_secs())
        .unwrap_or(0);
    let pid = std::process::id() as u64;
    for (i, b) in bytes.iter_mut().enumerate() {
        let mix = nanos.rotate_right((i as u32) * 3) ^ pid.rotate_left((i as u32) * 5) ^ (i as u64);
        *b = (mix ^ (mix >> 17) ^ (i as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15)) as u8;
    }
    bytes[6] = (bytes[6] & 0x0f) | 0x40; // v4
    bytes[8] = (bytes[8] & 0x3f) | 0x80; // rfc
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

struct Outbound {
    out: Mutex<std::io::Stdout>,
    static_id: AtomicU64,
}

impl Outbound {
    fn send(&self, value: serde_json::Value) {
        let mut out = self.out.lock().unwrap();
        let mut line = serde_json::to_vec(&value).unwrap_or_default();
        line.push(b'\n');
        let _ = out.write_all(&line);
        let _ = out.flush();
    }

    fn result(&self, id: u64, result: serde_json::Value) {
        self.send(serde_json::json!({ "id": id, "result": result }));
    }

    fn notification(&self, method: &str, params: serde_json::Value) {
        self.send(serde_json::json!({ "method": method, "params": params }));
    }

    fn next_static(&self) -> u64 {
        self.static_id.fetch_add(1, Ordering::SeqCst)
    }
}

pub fn projection_notification(
    topic: &str,
    p: &SessionProjection,
) -> serde_json::Value {
    serde_json::json!({
        "method": "v4/projection",
        "params": {
            "topic": topic,
            "sessionId": p.session_id,
            "rows": p.rows,
            "control": p.control,
            "seq": p.seq,
            "revision": p.revision,
        }
    })
}


/// One ask sitting in the workbench UI (the proposed action is inline).
#[derive(Debug, Clone)]
pub struct PendingApproval {
    pub id: String,
    pub tool_name: String,
    pub call_id: String,
    pub args_json: String,
    pub asked_at: f64,
}

/// The surface-side `ApprovalChannel`: `answer` registers the ask, then
/// blocks until a surface resolves it (`resolveApproval` command) or the
/// turn's stop flag flips (→ Cancelled — stop-interruptible by contract).
/// Fail-closed: the outcome union is unchanged; exactly AllowedOnce grants.
/// #53: the resolution may carry a SCOPE (once / conversation / always) —
/// the default is `once`, so every pre-existing caller is unchanged.
pub struct SurfaceApprovalChannel {
    pending: Mutex<Vec<PendingApproval>>,
    answers: Mutex<std::collections::HashMap<String, ApprovalAnswer>>,
    wake: std::sync::Condvar,
    stop: Arc<AtomicBool>,
}

impl SurfaceApprovalChannel {
    pub fn new(stop: Arc<AtomicBool>) -> Self {
        SurfaceApprovalChannel {
            pending: Mutex::new(Vec::new()),
            answers: Mutex::new(std::collections::HashMap::new()),
            wake: std::sync::Condvar::new(),
            stop,
        }
    }

    /// A surface answered: allow → AllowedOnce, deny → Rejected.
    /// (Default-scope form; the callers that don't model #53 scopes land here.)
    #[allow(dead_code)]
    pub fn resolve(&self, approval_id: &str, allow: bool) -> bool {
        self.resolve_scoped(approval_id, allow, ApprovalScope::Once)
    }

    /// A surface answered WITH a scope (#53). Denials always store the
    /// tightest scope — the scope only exists where the outcome grants.
    pub fn resolve_scoped(&self, approval_id: &str, allow: bool, scope: ApprovalScope) -> bool {
        let answer = if allow {
            ApprovalAnswer::scoped(ApprovalOutcome::AllowedOnce, scope)
        } else {
            ApprovalAnswer::new(ApprovalOutcome::Rejected)
        };
        let known = {
            let mut answers = self.answers.lock().unwrap();
            answers.insert(approval_id.to_string(), answer);
            self.pending
                .lock()
                .unwrap()
                .iter()
                .any(|p| p.id == approval_id)
        };
        self.wake.notify_all();
        known
    }

    /// Serialized snapshot for `control.awaitingApproval` (stable order).
    pub fn pending_snapshot(&self) -> Vec<serde_json::Value> {
        self.pending
            .lock()
            .unwrap()
            .iter()
            .map(|p| {
                serde_json::json!({
                    "approvalId": p.id,
                    "toolName": p.tool_name,
                    "callId": p.call_id,
                    "args": p.args_json,
                    "askedAt": p.asked_at,
                })
            })
            .collect()
    }
}

/// `Box<dyn ApprovalChannel>` view over the shared bridge — the service
/// owns the waterfall, the surface state owns the Arc; both point at the
/// same pending list.
struct SharedBridge(Arc<SurfaceApprovalChannel>);

impl ApprovalChannel for SharedBridge {
    fn answer(&self, request: &ApprovalRequest) -> Option<ApprovalOutcome> {
        self.0.answer(request)
    }

    fn answer_scoped(&self, request: &ApprovalRequest) -> Option<ApprovalAnswer> {
        self.0.answer_scoped(request)
    }
}

impl SurfaceApprovalChannel {
    /// Register a pending ask WITHOUT waiting (batch consent: register all,
    /// then wait for each — every card is on screen at once).
    pub fn register(
        &self,
        id: &str,
        tool_name: &str,
        call_id: &str,
        args_json: &str,
    ) {
        self.pending.lock().unwrap().push(PendingApproval {
            id: id.to_string(),
            tool_name: tool_name.to_string(),
            call_id: call_id.to_string(),
            args_json: args_json.to_string(),
            asked_at: now_ms(),
        });
        self.wake.notify_all();
    }

    /// Block until `id` is answered (stop-flag aware). Fails closed.
    pub fn wait_for(&self, id: &str) -> ApprovalOutcome {
        self.wait_for_scoped(id).outcome
    }

    /// The scoped wait (#53): same stop-aware loop, the answer carries
    /// the user's scope.
    pub fn wait_for_scoped(&self, id: &str) -> ApprovalAnswer {
        loop {
            // bounded waits so a stop flip is honoured mid-approval
            let answers = self.answers.lock().unwrap();
            let (mut answers, timeout) = self
                .wake
                .wait_timeout(answers, std::time::Duration::from_millis(250))
                .unwrap();
            if let Some(answer) = answers.remove(id) {
                drop(answers);
                self.pending.lock().unwrap().retain(|p| p.id != id);
                self.wake.notify_all();
                return answer;
            }
            drop(answers);
            if self.stop.load(Ordering::Relaxed) {
                self.pending.lock().unwrap().retain(|p| p.id != id);
                self.wake.notify_all();
                return ApprovalAnswer::new(ApprovalOutcome::Cancelled);
            }
            let _ = timeout;
        }
    }
}

impl ApprovalChannel for SurfaceApprovalChannel {
    fn answer(&self, request: &ApprovalRequest) -> Option<ApprovalOutcome> {
        self.register(
            &request.id,
            &request.tool_name,
            &request.call_id,
            &request.args_json,
        );
        Some(self.wait_for(&request.id))
    }

    fn answer_scoped(&self, request: &ApprovalRequest) -> Option<ApprovalAnswer> {
        self.register(
            &request.id,
            &request.tool_name,
            &request.call_id,
            &request.args_json,
        );
        Some(self.wait_for_scoped(&request.id))
    }
}

/// The projection control patch the workbench reads: pending approvals fold
/// into `control.awaitingApproval` and flip the phase while the turn is
/// paused on the bridge.
/// The session id of a projection (the notification envelope needs it).
fn proj_session_of(p: &Mutex<SessionProjection>) -> String {
    p.lock()
        .unwrap()
        .session_id()
        .unwrap_or_default()
        .to_string()
}

fn emit_approval_state(
    notify: &dyn Fn(&str, serde_json::Value),
    topic: &str,
    p: &Mutex<SessionProjection>,
    approvals: &SurfaceApprovalChannel,
    turn_running: bool,
    last_len: &mut usize,
) {
    let snapshot = approvals.pending_snapshot();
    if snapshot.len() == *last_len {
        // unchanged — but a PENDING ask re-emits periodically so late
        // subscribers (page reload, a new surface attaching mid-pause)
        // learn the card exists instead of wedging the turn (dogfood
        // day-4 finding: change-only emission loses the card on reload)
        if snapshot.is_empty() {
            return;
        }
        *last_len = snapshot.len().wrapping_sub(1);
        return;
    }
    // new asks surface the permission-request class (redacted label: the
    // tool name is metadata, the args are never sent to a lock screen)
    for ask in snapshot.iter().skip(*last_len) {
        let n = classify(
            NotificationClass::PermissionRequest,
            &format!("Approval needed: {}", ask["toolName"].as_str().unwrap_or("tool")),
            &proj_session_of(p),
        );
        notify(
            "v4/notification",
            serde_json::json!({
                "class": n.class,
                "label": n.label,
                "sessionId": n.session_id,
            }),
        );
    }
    *last_len = snapshot.len();
    let mut proj = p.lock().unwrap();
    proj.control["awaitingApproval"] = serde_json::json!(snapshot);
    if !snapshot.is_empty() {
        proj.control["phase"] = serde_json::json!("awaitingApproval");
    } else if turn_running {
        proj.control["phase"] = serde_json::json!("running");
    }
    notify(
        "v4/projection",
        projection_notification(topic, &proj)["params"].clone(),
    );
}

/// Run one full turn against the state's sampler factory, streaming row
/// updates into the projection and notifying the attached surface after
/// each change. `steering` (G4): when present, drained at every projection
/// update — a steered message becomes a user row on the SAME live session,
/// from whichever surface submitted it. `stop` (G4): a surface can flip
/// this flag mid-turn; the turn cancels at the next step boundary
/// (Cancelled(UserRequested)) and recovers through the standard path.
/// The network embedding tier, only when the user opted in
/// (`OKRA_EMBEDDINGS=on`) AND a credential exists. Any failure at use
/// time falls back offline inside `rank_relevant`.
fn embeddings_network_tier() -> Option<Box<okra_memory::retrieval::NetworkEmbed>> {
    let on = std::env::var("OKRA_EMBEDDINGS")
        .map(|v| {
            let v = v.to_lowercase();
            v == "on" || v == "1" || v == "true"
        })
        .unwrap_or(false);
    if !on {
        return None;
    }
    let client = okra_providers::embeddings::EmbeddingClient::from_env(
        std::env::var("OKRA_EMBEDDINGS_MODEL").unwrap_or_else(|_| "text-embedding-3-small".into()),
    )?;
    Some(Box::new(move |texts: &[&str]| {
        client
            .embed(texts)
            .map_err(|e| format!("embeddings tier: {e:?}"))
    }))
}

/// #24 permission learning, shared across every turn of the daemon:
/// granted approvals feed the learner; CONFIRMED suggestions land in the
/// shared lattice AND persist to workspace settings (`permissions.rules`).
/// Nothing here auto-applies — the surface shows the suggestion and a
/// human confirms over POST /api/rules.
pub struct PermissionLearning {
    pub learner: Mutex<okra_policy::RulesetLearner>,
    pub lattice: Mutex<okra_policy::PermissionLattice>,
    /// Explicitly dismissed suggestions (tool, path prefix) — never
    /// re-suggested this daemon lifetime.
    pub dismissed: Mutex<Vec<(String, Option<String>)>>,
}

pub type PermissionLearningHandle = Arc<PermissionLearning>;

impl PermissionLearning {
    /// Load persisted rules from the workspace settings scope.
    pub fn load(home: &Path, cwd: &Path) -> PermissionLearningHandle {
        let lattice = Mutex::new(load_persisted_rules(home, cwd));
        Arc::new(PermissionLearning {
            learner: Mutex::new(okra_policy::RulesetLearner::new()),
            lattice,
            dismissed: Mutex::new(Vec::new()),
        })
    }

    /// Current suggestions (lattice-aware, dismissed-filtered).
    pub fn suggestions(&self, min_occurrences: u32, max: usize) -> Vec<okra_policy::SuggestedPermissionUpdate> {
        let learner = self.learner.lock().unwrap();
        let lattice = self.lattice.lock().unwrap();
        let dismissed = self.dismissed.lock().unwrap();
        learner
            .suggest(&lattice, min_occurrences, max + dismissed.len())
            .into_iter()
            .filter(|s| {
                let prefix = s.rule.path_prefix.as_deref();
                !dismissed
                    .iter()
                    .any(|(t, p)| &s.rule.tool == t && p.as_deref() == prefix)
            })
            .take(max)
            .collect()
    }
}

/// Parse `permissions.rules` out of the workspace settings scope.
fn load_persisted_rules(home: &Path, cwd: &Path) -> okra_policy::PermissionLattice {
    let mut lattice = okra_policy::PermissionLattice::new();
    let store = okra_host::settings::SettingsStore::new(home).with_workspace(cwd);
    if let Ok((serde_json::Value::Array(rules), _)) = store.get("permissions.rules") {
        for r in rules {
            if let Ok(rule) = serde_json::from_value::<okra_policy::PermissionRule>(r) {
                lattice.add_rule(rule);
            }
        }
    }
    lattice
}

/// Persist one rule into the workspace settings scope (`permissions.rules`
/// array), deduped.
pub fn persist_rule(home: &Path, cwd: &Path, rule: &okra_policy::PermissionRule) -> Result<usize, String> {
    let store = okra_host::settings::SettingsStore::new(home).with_workspace(cwd);
    let mut rules: Vec<serde_json::Value> = match store.get("permissions.rules") {
        Ok((serde_json::Value::Array(a), _)) => a,
        _ => Vec::new(),
    };
    let encoded = serde_json::to_value(rule).map_err(|e| e.to_string())?;
    if rules.contains(&encoded) {
        return Ok(rules.len());
    }
    rules.push(encoded);
    store
        .set("permissions.rules", serde_json::Value::Array(rules.clone()), okra_host::settings::SettingsScope::Workspace)
        .map_err(|e| e.to_string())?;
    Ok(rules.len())
}

/// #25: the workspace-provided content whose activation the trust gate
/// controls — skills, the workspace MCP/daemon config, workspace settings
/// overrides, and workspace slash commands. An empty set needs no trust.
pub fn gated_workspace_files(cwd: &Path) -> Vec<PathBuf> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, out);
            } else if p.is_file() {
                out.push(p);
            }
        }
    }
    let mut files = Vec::new();
    for dir in [".okra/skills", ".zcode/commands"] {
        let root = cwd.join(dir);
        if root.is_dir() {
            walk(&root, &mut files);
        }
    }
    for f in [".okra/config.json", ".okra/settings.json"] {
        let p = cwd.join(f);
        if p.is_file() {
            files.push(p);
        }
    }
    files.sort();
    files.dedup();
    files
}

/// M3 watchers domain (#52 per-conversation watchers): what files THIS
/// conversation touched, and their LIVE dirty state. The durable source is
/// the checkpoint manager's per-prompt write records (n0029) — no extra
/// watcher state exists to drift; a file is `changed` when its on-disk
/// sha256 differs from the conversation's last recorded `after` snapshot,
/// `deleted` when it no longer exists, `clean` otherwise.
pub fn watchers_state(
    mgr: &okra_host::checkpoints::CheckpointManager,
    cwd: &Path,
    turns_seen: usize,
) -> serde_json::Value {
    // union of every path this session ever recorded, with the LAST
    // after-snapshot per path (turns are 0..turns_seen for the session)
    let mut last_after: std::collections::BTreeMap<String, okra_host::checkpoints::FileSnapshot> =
        Default::default();
    let mut first_before: std::collections::BTreeMap<String, String> = Default::default();
    for turn in 0..turns_seen {
        if let Some(cp) = mgr.get_checkpoint(turn) {
            for (path, snap) in &cp.fs.before {
                first_before.entry(path.clone()).or_insert_with(|| snap.sha256.clone());
            }
            for (path, snap) in &cp.fs.after {
                last_after.insert(path.clone(), snap.clone());
            }
        }
    }
    let files: Vec<serde_json::Value> = last_after
        .iter()
        .map(|(path, after)| {
            let abs = cwd.join(path);
            let live = std::fs::read(&abs).ok();
            let state = match live {
                None => "deleted",
                Some(bytes) => {
                    let live_hash = okra_host::plugins::store::sha256_hex(&bytes);
                    if live_hash == after.sha256 { "clean" } else { "changed" }
                }
            };
            serde_json::json!({
                "path": path,
                "state": state,
                "createdByConversation": !first_before.contains_key(path) && !after.exists,
                "sizeBytes": after.size_bytes,
            })
        })
        .collect();
    serde_json::json!({ "turns": turns_seen, "files": files })
}

/// #25: the current trust verdict for the workspace, as the API renders it.
pub fn trust_state(store: &okra_policy::ProjectTrustStore, cwd: &Path) -> serde_json::Value {    let gated = gated_workspace_files(cwd);
    if gated.is_empty() {
        return serde_json::json!({
            "verdict": "none",
            "message": "no workspace-provided content to activate",
            "gatedFiles": [],
        });
    }
    let digest = okra_policy::content_digest(cwd, &gated);
    match okra_policy::ensure_trusted(store, cwd, &gated) {
        okra_policy::TrustVerdict::Trusted { .. } => serde_json::json!({
            "verdict": "trusted", "digest": digest,
            "gatedFiles": gated.iter().filter_map(|f| f.strip_prefix(cwd).ok()).filter_map(|r| r.to_str()).collect::<Vec<_>>(),
        }),
        okra_policy::TrustVerdict::Changed { stored, current } => serde_json::json!({
            "verdict": "changed", "storedDigest": stored, "digest": current,
            "message": "the project's activatable content changed since you trusted it — it stays inert until you re-trust",
            "gatedFiles": gated.iter().filter_map(|f| f.strip_prefix(cwd).ok()).filter_map(|r| r.to_str()).collect::<Vec<_>>(),
        }),
        okra_policy::TrustVerdict::Untrusted => serde_json::json!({
            "verdict": "untrusted", "digest": digest,
            "message": "this project provides skills, commands, MCP servers or settings that stay INERT until you trust it",
            "gatedFiles": gated.iter().filter_map(|f| f.strip_prefix(cwd).ok()).filter_map(|r| r.to_str()).collect::<Vec<_>>(),
        }),
    }
}

#[allow(clippy::too_many_arguments)]
pub fn run_turn_streaming(
    broadcast: BroadcastFn,
    topic: String,
    session_id: String,
    cwd: std::path::PathBuf,
    sessions_dir: std::path::PathBuf,
    input_text: String,
    turn_row_lock: Arc<Mutex<SessionProjection>>,
    steering: Option<Arc<Mutex<std::collections::VecDeque<SteeredInput>>>>,
    stop: Arc<AtomicBool>,
    sampler_factory: &SamplerFactory,
    approvals: Arc<SurfaceApprovalChannel>,
    unattended: bool,
    attachments: Vec<String>,
    questions: Option<Arc<SurfaceQuestionChannel>>,
    mcp_sessions: &Mutex<std::collections::BTreeMap<String, Arc<Mutex<okra_tools::McpClient>>>>,
    computer_consent: &ComputerConsentHandle,
    session_context: Option<Arc<Mutex<okra_compaction::SessionContext>>>,
    checkpoints: Option<Arc<Mutex<okra_host::checkpoints::CheckpointManager>>>,
    prompt_index: usize,
    mediation: (okra_policy::lattice::MediationPolicy, Option<String>),
    mediation_probe: Option<okra_policy::mediation::AttachedProbe>,
    subagent_provider: Option<(String, String)>,
    permissions: Option<PermissionLearningHandle>,
    // #25: when present, workspace-provided content activates only if the
    // project is trusted at its current content digest.
    trust: Option<okra_policy::ProjectTrustStore>,
    // #39/#40: when present, the turn's executor runs under this dispatch
    // class (a cron-fired turn gets CronScheduled — the automation
    // self-mutation guard then denies cron_* inside it), and the cron_*
    // tools are registered against the store.
    automation_dispatch: Option<okra_agent_core::tasks::TurnDispatch>,
    automation_store: Option<Arc<okra_host::automation::AutomationStore>>,
) -> Result<TurnOutcome, String> {
    let steering_rx = steering;
    // attachments fold BEFORE anything surfaces: model-visible means
    // logged, so the folded text is what the row and the kernel event
    // carry (the raw attachment list rides the row for the UI chips).
    let input_text_orig = input_text.clone();
    let (input_text, attachments) = fold_attachments(&cwd, &input_text, &attachments);
    // 1. turnHeader (running) + userInput rows
    let (assistant_row_slot, turn_id) = {
        let mut p = turn_row_lock.lock().unwrap();
        p.set_title_if_empty(&input_text_orig);
        let turn_id = uuid_v4();
        let (header_id, header_row, _) = p.base_row(&turn_id);
        let header = {
            let mut r = header_row;
            r["kind"] = serde_json::json!("turnHeader");
            r["origin"] = serde_json::json!("userInput");
            r["state"] = serde_json::json!("running");
            r["startedAt"] = serde_json::json!(now_ms());
            r
        };
        p.upsert_row(header);
        let _ = header_id;
        let (_user_id, user_row, _) = p.base_row(&turn_id);
        let user = {
            let mut r = user_row;
            r["kind"] = serde_json::json!("userInput");
            r["text"] = serde_json::json!(input_text);
            r["origin"] = serde_json::json!("realUser");
            if !attachments.is_empty() {
                r["attachments"] = serde_json::json!(attachments);
            }
            r
        };
        p.upsert_row(user);
        p.control["phase"] = serde_json::json!("running");
        p.control["canStop"] = serde_json::json!(true);
        p.control["activeWorks"] = serde_json::json!([{ "kind": "primaryTurn", "startedAt": now_ms() }]);
        (broadcast)(
            "v4/projection",
            projection_notification(&topic, &p)["params"].clone(),
        );
        (Arc::new(Mutex::new(None::<u64>)), turn_id)
    };

    // 2. build the agent (full CLI tool plane + policy ceiling, sampler
    // from the state's factory — real provider when `--provider` was given)
    // N0029: writes capture into this prompt's rewind checkpoint
    if let Some(mgr) = checkpoints.as_ref()
        && let Ok(mut mgr) = mgr.lock()
    {
        mgr.begin_prompt(prompt_index);
    }
    let mut registry = build_registry_with_checkpoints(&cwd, checkpoints.clone(), prompt_index);
    // N0019: probed MCP tools join the turn (approval-gated: unknown
    // side-effectors); N0020: ask_user joins ATTENDED surfaces only (an
    // unattended bridge would block the turn forever).
    let _mcp_tools = register_mcp_tools(&mut registry, &cwd, mcp_sessions);
    if let Some(bridge) = questions.as_ref() {
        let bridge = Arc::clone(bridge);
        let spec = okra_tools::ToolSpec {
            name: "ask_user".into(),
            description: "Ask the user a clarifying question mid-turn; the tool returns their answer.".into(),
            arguments_schema: Some(serde_json::json!({
                "type": "object",
                "properties": { "question": { "type": "string" } },
                "required": ["question"],
            })),
            read_only: true,
            idempotent: false,
            kind: Some("interaction".into()),
            ..ask_user_spec_rest()
        };
        let metadata = okra_tools::ToolMetadata { read_only: true, ..Default::default() };
        let entry = okra_tools::ToolEntry::new(spec, metadata);
        let _ = registry.register(okra_tools::ErasedTool::simple(
            entry,
            vec![okra_tools::ResourceAccess::All],
            move |args: &serde_json::Value| {
                let q = args["question"].as_str().unwrap_or_default().to_string();
                let answer = bridge.ask(q);
                okra_tools::ToolStream::terminal_only(Ok(okra_tools::ToolOutput::text(answer)))
            },
        ));
    }
    // N0025: the Claude Desktop parity families (consent + display-scope
    // + app_*), sharing the daemon-level consent ledger
    register_computer_parity_tools(&mut registry, computer_consent, &session_id, &approvals);
    // N0034: delegation — the model can spawn a kernel-isolated subagent
    // (real worktree, confined child turn, work committed to a branch)
    register_subagent_tool(&mut registry, &cwd, &session_id, subagent_provider);

    // #16 deferred tool discovery: the compact directory indexes every
    // REGISTERED tool (native + MCP + interaction + subagent); the model
    // searches it on demand and receives full schemas for the matches.
    {
        let index = Arc::new(okra_tools::DiscoveryIndex::new());
        let entries: Vec<okra_tools::DiscoveryEntry> = registry
            .entries()
            .into_iter()
            .map(|e| okra_tools::DiscoveryEntry {
                name: e.spec.name.clone(),
                description: e.spec.description.clone(),
                source: if e.spec.kind.as_deref() == Some("mcp") {
                    format!("mcp:{}", e.spec.namespace.clone().unwrap_or_else(|| "server".into()))
                } else {
                    "native".into()
                },
                schema: e.spec.arguments_schema.clone(),
            })
            .collect();
        index.refresh(entries);
        let search = okra_tools::ToolSearch { index };
        let entry = search.entry();
        let _ = registry.register(okra_tools::ErasedTool::simple(
            entry,
            vec![],
            move |args: &serde_json::Value| search.execute(args),
        ));
    }
    // #39/#40: the automation domain's tools. Side-effecting (they mutate
    // the durable schedule table); the tool-plane guard denies cron_* on
    // cron-fired and idle turns, so automation can never reschedule itself.
    if let Some(store) = &automation_store {
        let mk = |name: &str, desc: &str| okra_tools::ToolSpec {
            name: name.into(),
            namespace: None,
            title: Some(name.into()),
            description: desc.into(),
            arguments_schema: Some(serde_json::json!({"type":"object"})),
            kind: Some("automation".into()),
            behavior_version: Some("1".into()),
            idempotent: true,
            read_only: false,
            timeout_ms: Some(5_000),
            max_concurrency: None,
        };
        let md = okra_tools::ToolMetadata { needs_approval: true, ..Default::default() };

        let st = Arc::clone(store);
        let mut spec = mk(
            "cron_create",
            "Create a scheduled automation: fires `prompt` into its own session either every `every_secs` seconds or daily at `at_hhmm` (UTC, \"HH:MM\").",
        );
        spec.arguments_schema = Some(serde_json::json!({
            "type": "object",
            "properties": {
                "name": { "type": "string" },
                "prompt": { "type": "string" },
                "every_secs": { "type": "integer", "minimum": 10 },
                "at_hhmm": { "type": "string", "description": "UTC, \"HH:MM\"" }
            },
            "required": ["name", "prompt"]
        }));
        let _ = registry.register(okra_tools::ErasedTool::simple(
            okra_tools::ToolEntry::new(spec, md.clone()),
            vec![okra_tools::ResourceAccess::All],
            move |args: &serde_json::Value| {
                // stable id from the spec content: same schedule → same id
                let digest = okra_host::plugins::store::sha256_hex(args.to_string().as_bytes());
                let id = format!("auto-{}", &digest[..10]);
                let at = args["at_hhmm"].as_str().and_then(|s| {
                    let mut it = s.split(':');
                    Some((
                        it.next()?.trim().parse::<u8>().ok()?,
                        it.next()?.trim().parse::<u8>().ok()?,
                    ))
                });
                let created = okra_host::automation::AutomationSpec {
                    session_id: format!("auto-{id}"),
                    id,
                    name: args["name"].as_str().unwrap_or_default().into(),
                    prompt: args["prompt"].as_str().unwrap_or_default().into(),
                    every_secs: args["every_secs"].as_u64(),
                    at_hhmm: at,
                    created_at_epoch_ms: std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_millis() as u64)
                        .unwrap_or(0),
                    last_fired_epoch_ms: None,
                    fire_count: 0,
                    enabled: true,
                };
                match st.create(created) {
                    Ok(spec) => okra_tools::ToolStream::terminal_only(Ok(
                        okra_tools::ToolOutput::from_value(serde_json::json!({
                            "created": true, "id": spec.id, "session": spec.session_id,
                        })),
                    )),
                    Err(e) => okra_tools::ToolStream::terminal_only(Err(
                        okra_tools::ToolError::tool_failed(e.to_string()),
                    )),
                }
            },
        ));

        let st = Arc::clone(store);
        let mut spec = mk("cron_list", "List the scheduled automations (id, schedule, fire count, enabled).");
        spec.arguments_schema = Some(serde_json::json!({"type":"object"}));
        let md_ro = okra_tools::ToolMetadata { read_only: true, needs_approval: false, concurrent_safe: true, allowed_in_plan_mode: Some(true), ..Default::default() };
        let _ = registry.register(okra_tools::ErasedTool::simple(
            okra_tools::ToolEntry::new(spec, md_ro),
            vec![],
            move |_args: &serde_json::Value| {
                let items: Vec<serde_json::Value> = st
                    .list()
                    .into_iter()
                    .map(|s| serde_json::json!({
                        "id": s.id, "name": s.name, "prompt": s.prompt,
                        "session": s.session_id,
                        "everySecs": s.every_secs, "atHHMM": s.at_hhmm.map(|(h, m)| format!("{h:02}:{m:02}")),
                        "fireCount": s.fire_count, "enabled": s.enabled,
                    }))
                    .collect();
                okra_tools::ToolStream::terminal_only(Ok(okra_tools::ToolOutput::from_value(
                    serde_json::json!({ "automations": items }),
                )))
            },
        ));

        let st = Arc::clone(store);
        let mut spec = mk("cron_delete", "Delete a scheduled automation by id.");
        spec.arguments_schema = Some(serde_json::json!({
            "type": "object",
            "properties": { "id": { "type": "string" } },
            "required": ["id"]
        }));
        let _ = registry.register(okra_tools::ErasedTool::simple(
            okra_tools::ToolEntry::new(spec, md.clone()),
            vec![okra_tools::ResourceAccess::All],
            move |args: &serde_json::Value| {
                let id = args["id"].as_str().unwrap_or_default();
                match st.delete(id) {
                    Ok(true) => okra_tools::ToolStream::terminal_only(Ok(
                        okra_tools::ToolOutput::from_value(serde_json::json!({ "deleted": true, "id": id })),
                    )),
                    Ok(false) => okra_tools::ToolStream::terminal_only(Err(
                        okra_tools::ToolError::tool_failed(format!("no such automation: {id}")),
                    )),
                    Err(e) => okra_tools::ToolStream::terminal_only(Err(
                        okra_tools::ToolError::tool_failed(e.to_string()),
                    )),
                }
            },
        ));
    }

    // N0023: computer control tools (Claude Desktop parity, AX-first).
    // The approval card IS the split consent: computer_act's card grants
    // the named app for that batch; computer_screenshot's card is the
    // screen-takeover consent. Separate tools, separate consents.
    {
        let spec_observe = okra_tools::ToolSpec {
            name: "computer_observe".into(),
            description: "Observe an application's accessibility tree (element ids, roles, labels, positions). Call this before computer_act.".into(),
            arguments_schema: Some(serde_json::json!({
                "type": "object",
                "properties": { "app": { "type": "string" } },
                "required": ["app"],
            })),
            kind: Some("computer".into()),
            ..computer_spec_rest()
        };
        // read_only stays FALSE: observing the user's screen is a privacy
        // side-effect — the approval card is the app-capability consent
        let entry = okra_tools::ToolEntry::new(spec_observe, okra_tools::ToolMetadata::default());
        let _ = registry.register(okra_tools::ErasedTool::simple(
            entry,
            vec![okra_tools::ResourceAccess::All],
            move |args: &serde_json::Value| {
                let app = args["app"].as_str().unwrap_or_default().to_string();
                match okra_computer::backend::observe(&app) {
                    Ok(tree) => okra_tools::ToolStream::terminal_only(Ok(
                        okra_tools::ToolOutput::text(
                            serde_json::to_string_pretty(&tree).unwrap_or_default(),
                        ),
                    )),
                    Err(e) => okra_tools::ToolStream::terminal_only(Err(
                        okra_tools::ToolError::tool_failed(e),
                    )),
                }
            },
        ));

        let spec_act = okra_tools::ToolSpec {
            name: "computer_act".into(),
            description: "Execute a batch of element-targeted actions (click/type/press_key) against one app, in order, stopping at the first error; each executed action is followed by a re-observe.".into(),
            arguments_schema: Some(serde_json::json!({
                "type": "object",
                "properties": {
                    "app": { "type": "string" },
                    "typing": { "type": "boolean", "description": "true when the user is actively typing — input injection pauses" },
                    "actions": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "kind": { "type": "string", "enum": ["click", "type", "press_key"] },
                                "element_id": { "type": "string" },
                                "text": { "type": "string" },
                                "key": { "type": "string" }
                            },
                            "required": ["kind"]
                        }
                    }
                },
                "required": ["app", "actions"],
            })),
            kind: Some("computer".into()),
            ..computer_spec_rest()
        };
        let entry = okra_tools::ToolEntry::new(
            spec_act,
            okra_tools::ToolMetadata::default(),
        );
        let _ = registry.register(okra_tools::ErasedTool::simple(
            entry,
            vec![okra_tools::ResourceAccess::All],
            move |args: &serde_json::Value| {
                let app = args["app"].as_str().unwrap_or_default().to_string();
                let typing = args["typing"].as_bool().unwrap_or(false);
                let actions: Vec<okra_computer::AxAction> = args["actions"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| serde_json::from_value(v.clone()).ok())
                            .collect()
                    })
                    .unwrap_or_default();
                let results = okra_computer::backend::execute_real(&app, &actions, typing);
                okra_tools::ToolStream::terminal_only(Ok(okra_tools::ToolOutput::text(
                    serde_json::to_string_pretty(&results).unwrap_or_default(),
                )))
            },
        ));

        let spec_shot = okra_tools::ToolSpec {
            name: "computer_screenshot".into(),
            description: "Capture the screen (no sound); returns a PNG data URL the workbench renders.".into(),
            arguments_schema: Some(serde_json::json!({ "type": "object", "properties": {} })),
            kind: Some("computer".into()),
            ..computer_spec_rest()
        };
        // screen-takeover consent: the screenshot approval card IS it
        let entry = okra_tools::ToolEntry::new(spec_shot, okra_tools::ToolMetadata::default());
        let _ = registry.register(okra_tools::ErasedTool::simple(
            entry,
            vec![okra_tools::ResourceAccess::All],
            move |_args: &serde_json::Value| {
                match okra_computer::backend::screenshot() {
                    Ok(png) => {
                        let b64 = {
                            // minimal base64 (no dependency in this crate's path)
                            const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
                            let mut out = String::with_capacity(png.len().div_ceil(3) * 4);
                            for chunk in png.chunks(3) {
                                let b = [chunk[0],
                                    *chunk.get(1).unwrap_or(&0),
                                    *chunk.get(2).unwrap_or(&0)];
                                out.push(T[(b[0] >> 2) as usize] as char);
                                out.push(T[(((b[0] & 0x03) << 4) | (b[1] >> 4)) as usize] as char);
                                out.push(if chunk.len() > 1 {
                                    T[(((b[1] & 0x0f) << 2) | (b[2] >> 6)) as usize] as char
                                } else { '=' });
                                out.push(if chunk.len() > 2 { T[(b[2] & 0x3f) as usize] as char } else { '=' });
                            }
                            out
                        };
                        okra_tools::ToolStream::terminal_only(Ok(okra_tools::ToolOutput::text(
                            format!("data:image/png;base64,{b64}"),
                        )))
                    }
                    Err(e) => okra_tools::ToolStream::terminal_only(Err(
                        okra_tools::ToolError::tool_failed(e),
                    )),
                }
            },
        ));
    }

    let mut approval_service = ApprovalService::new(ApprovalPolicy::Ask);
    if !unattended {
        // attended surfaces: the ask crosses the MEDIATOR (n0033) — the
        // workbench bridge is the one attached client today (local,
        // loopback); policy decides who answers when more clients attach.
        // first-responder with one client is byte-identical to the old
        // direct bridge.
        let mut mediator = okra_policy::mediation::Mediator::new(mediation.0);
        if let Some(d) = &mediation.1 {
            mediator = mediator.with_designated(d);
        }
        if let Some(probe) = mediation_probe {
            mediator = mediator.with_attached_probe(probe);
        }
        mediator.add_client(
            "workbench",
            true,
            Box::new(SharedBridge(Arc::clone(&approvals))),
        );
        approval_service.add_channel(Box::new(mediator));
    }
    let mut executor = okra_agent_core::loop_::PolicyToolExecutor::new(registry, approval_service);
    // #39/#40: a cron-fired turn runs under CronScheduled — the automation
    // self-mutation guard then denies cron_* inside it (automation may not
    // reschedule itself); ordinary turns may create schedules.
    if let Some(d) = automation_dispatch {
        executor.turn_dispatch = d;
    }
    // #24: persisted project rules ride every turn's lattice (loaded at
    // daemon start; /api/rules keeps both in step)
    if let Some(perms) = &permissions {
        for rule in perms.lattice.lock().unwrap().rules().to_vec() {
            executor.lattice.add_rule(rule);
        }
    }
    // Attended surfaces (the workbench) ASK through the bridge: the turn
    // pauses on a non-read-only tool until a surface resolves it. Headless
    // stdio bridges run UnattendedAllowed (no approver exists there);
    // arg-hash grants record every decision either way.
    executor.ceiling = if unattended {
        ToolApprovalCeiling::UnattendedAllowed
    } else {
        ToolApprovalCeiling::GrantsAllowed
    };

    let header = kernel::SessionHeader {
        version: kernel::SESSION_FORMAT_VERSION,
        id: format!("session-{session_id}"),
        created_at: now_ms(),
        cwd: cwd.to_string_lossy().into_owned(),
        parent_session: None,
        is_seeded: false,
    };
    let kernel_session = match kernel::SessionHandle::open(&sessions_dir, &format!("session-{session_id}"), kernel::SessionAccess::Write) {
        Ok(h) => h,
        Err(kernel::HandleError::NotFound(_)) => kernel::SessionHandle::create(&sessions_dir, &header)
            .map_err(|e| format!("create kernel session: {e}"))?,
        Err(e) => return Err(format!("open kernel session: {e}")),
    };

    // managed pin ceiling clamps every serve surface's turn budget (the
    // CLI clamps its own --max-turns flag); re-read per turn so a pin
    // deployed mid-session is honored by the next turn
    let (max_steps, clamped) = okra_host::managed_policy::runtime_pin().clamp_max_turns(32);
    let max_steps = max_steps as usize;
    if clamped {
        eprintln!("[pin] max-turns clamped to {max_steps}");
    }
    let config = okra_agent_core::loop_::AgentConfig {
        max_steps,
        unattended: true,
        semantic_judge: okra_agent_core::semantics::from_env_config(),
        ..Default::default()
    };
    let sampler = (sampler_factory)();
    let mut agent = Agent::new(config, sampler, Box::new(executor), kernel_session);
    agent.set_stop_flag(stop);
    // one Agent per turn on a shared kernel session: seed the counter so
    // turn/start (and replay's row numbering) stays monotonic per session
    agent.set_turn_counter(prompt_index as u64);

    // 3. drive the turn, streaming LoopEvents into rows
    let topic_for_events = topic.clone();
    let row_lock = turn_row_lock.clone();
    let assistant_slot = assistant_row_slot.clone();
    let mut tool_row_by_call: std::collections::HashMap<String, u64> = Default::default();
    // approval watchdog: while the turn is paused on the bridge, surfaces
    // still receive frames (control.awaitingApproval / phase flips)
    let wd_bridge = Arc::clone(&approvals);
    let wd_proj = Arc::clone(&turn_row_lock);
    let wd_topic = topic.clone();
    let wd_broadcast = Arc::clone(&broadcast);
    let wd_done = Arc::new(AtomicBool::new(false));
    let wd_done_inner = Arc::clone(&wd_done);
    let wd_handle = std::thread::spawn(move || {
        let mut last_len = 0usize;
        let emit = |m: &str, p: serde_json::Value| wd_broadcast(m, p);
        while !wd_done_inner.load(Ordering::Relaxed) {
            emit_approval_state(&emit, &wd_topic, &wd_proj, &wd_bridge, true, &mut last_len);
            std::thread::sleep(std::time::Duration::from_millis(250));
        }
        // final state: pending list emptied (or never populated)
        emit_approval_state(&emit, &wd_topic, &wd_proj, &wd_bridge, true, &mut last_len);
    });

    // surface-queue → agent steering inbox FORWARDER: a dedicated thread
    // (the event sink is buffered until turn end, so it cannot forward
    // mid-turn). Mid-turn arrivals reach the MODEL at the next step
    // boundary — logged user/message origin=steering — instead of
    // rendering cosmetic rows; each entry's attachments fold into the
    // forwarded text.
    let fwd_queue = steering_rx.clone();
    let fwd_cwd = cwd.clone();
    let fwd_inbox = agent.steering_sender();
    let forwarded = std::sync::Arc::new(std::sync::Mutex::new(std::collections::VecDeque::new()));
    let fwd_done = Arc::new(AtomicBool::new(false));
    {
        let forwarded = Arc::clone(&forwarded);
        let fwd_done = Arc::clone(&fwd_done);
        std::thread::spawn(move || {
            while !fwd_done.load(Ordering::Relaxed) {
                let entries: Vec<crate::serve::SteeredInput> = match fwd_queue {
                    Some(ref q) => {
                        let mut q = q.lock().unwrap();
                        q.drain(..).collect()
                    }
                    None => Vec::new(),
                };
                for entry in entries {
                    let (folded, _) = fold_attachments(&fwd_cwd, &entry.text, &entry.attachments);
                    forwarded.lock().unwrap().push_back(crate::serve::SteeredInput {
                        text: folded.clone(),
                        attachments: entry.attachments,
                    });
                    let _ = fwd_inbox.send(okra_agent_core::steering::Tagged {
                        interjection: okra_agent_core::steering::PendingInterjection {
                            text: folded,
                        },
                        submitted_while_running: true,
                    });
                }
                std::thread::sleep(std::time::Duration::from_millis(80));
            }
        });
    }

    let mut q_teardown_flag: Option<Arc<AtomicBool>> = None;
    // question watchdog (N0020): a live ask surfaces as
    // control.awaitingQuestion + the question-class notification; clearing
    // restores the running phase
    if let Some(qb) = questions.as_ref() {
        let qb = Arc::clone(qb);
        let q_proj = Arc::clone(&turn_row_lock);
        let q_topic = topic.clone();
        let q_session = session_id.clone();
        let q_broadcast = Arc::clone(&broadcast);
        let q_done = Arc::new(AtomicBool::new(false));
        let q_done_inner = Arc::clone(&q_done);
        std::thread::spawn(move || {
            let emit = |m: &str, p: serde_json::Value| q_broadcast(m, p);
            let mut last_asked = false;
            let mut ticks_since_emit = 0u32;
            while !q_done_inner.load(Ordering::Relaxed) {
                let snap = qb.pending_snapshot();
                let is_asked = snap.is_some();
                if is_asked != last_asked || (is_asked && ticks_since_emit >= 4) {
                    ticks_since_emit = 0;
                    last_asked = is_asked;
                    let mut proj = q_proj.lock().unwrap();
                    if let Some(q) = &snap {
                        let n = classify(
                            NotificationClass::Question,
                            &format!("Question: {}", q["question"].as_str().unwrap_or_default()),
                            &q_session,
                        );
                        emit(
                            "v4/notification",
                            serde_json::json!({
                                "class": n.class,
                                "label": n.label,
                                "sessionId": n.session_id,
                            }),
                        );
                        proj.control["awaitingQuestion"] = q.clone();
                        proj.control["phase"] = serde_json::json!("awaitingQuestion");
                    } else {
                        proj.control["awaitingQuestion"] = serde_json::Value::Null;
                        proj.control["phase"] = serde_json::json!("running");
                    }
                    emit(
                        "v4/projection",
                        projection_notification(&q_topic, &proj)["params"].clone(),
                    );
                }
                std::thread::sleep(std::time::Duration::from_millis(150));
                ticks_since_emit += 1;
            }
        });
        q_teardown_flag = Some(q_done);
    }

    let outcome = {
        // M2 wiring (n0028): daemon turns are CONTINUATIONS — the
        // per-session SessionContext chains turns (compaction, world
        // state), tiered memory recall folds into the head, and the
        // project skill catalog activates path-conditionally with
        // progressive disclosure. Both load per turn (fail-open: absent
        // dirs are empty), so Tools-tab installs take effect on the next
        // send without daemon state.
        let memory_reader = okra_memory::TieredReader::new(
            okra_host::fsutil::home_dir().unwrap_or_else(|| cwd.clone()),
            cwd.clone(),
        );
        // #25 project trust: workspace-provided skills stay INERT until the
        // project is trusted (or re-trusted after its content changed).
        // User-scope content (~/.okra/skills) is unaffected — trust gates
        // what the PROJECT brought in, not what the user installed.
        let gated = gated_workspace_files(&cwd);
        let workspace_content_trusted = gated.is_empty()
            || trust.as_ref().map(|t| {
                matches!(
                    okra_policy::ensure_trusted(t, &cwd, &gated),
                    okra_policy::TrustVerdict::Trusted { .. }
                )
            }).unwrap_or(false);
        let skills_root = if workspace_content_trusted {
            cwd.join(".okra").join("skills")
        } else {
            if !gated.is_empty() {
                eprintln!(
                    "[trust] workspace content held INERT: {} gated file(s) not trusted (trust over POST /api/trust)",
                    gated.len()
                );
            }
            // load_dir fails open on absent dirs — a nonexistent path IS
            // the empty catalog; the PROJECT skills alone are gated here
            cwd.join(".okra").join("skills").join(".untrusted-gate")
        };
        let skill_catalog = okra_memory::SkillCatalog::load_dir(&skills_root);
        // n0035: prompt-relevant skill retrieval (embedding tier). Offline
        // hashed embeddings by default — deterministic, keyless; the
        // network tier engages with OKRA_EMBEDDINGS=on + a credential and
        // falls back offline on any error (retrieval must never fail a
        // turn). Suggestions are distinct from path-conditional
        // activations (RELEVANT vs ACTIVE in the head).
        let relevant_skills: Vec<(String, f32)> = {
            let items: Vec<(String, String)> = skill_catalog
                .skills
                .iter()
                .map(|s| {
                    (
                        s.name.clone(),
                        format!("{} {}", s.description, s.match_patterns.join(" ")),
                    )
                })
                .collect();
            let network = embeddings_network_tier();
            okra_memory::retrieval::rank_relevant(items, &input_text, network.as_deref(), 3)
        };
        let mut handle_ev = |ev: LoopEvent| {
            let mut p = row_lock.lock().unwrap();
        match ev {
            LoopEvent::TextDelta { text } => {
                let mut slot = assistant_slot.lock().unwrap();
                let row_id = match *slot {
                    Some(id) => id,
                    None => {
                        let (row_id, base, _) = p.base_row(&turn_id);
                        let mut r = base;
                        r["kind"] = serde_json::json!("assistantText");
                        r["text"] = serde_json::json!("");
                        r["state"] = serde_json::json!("streaming");
                        p.upsert_row(r);
                        *slot = Some(row_id);
                        row_id
                    }
                };
                if let Some(row) = p.rows.iter_mut().find(|r| r["rowId"].as_u64() == Some(row_id)) {
                    let cur = row["text"].as_str().unwrap_or_default().to_string();
                    row["text"] = serde_json::json!(format!("{cur}{text}"));
                }
                p.revision += 1;
            }
            LoopEvent::SteeringInjected { text } => {
                // the loop drained the inbox INTO the model — the row is the
                // honest receipt (the text is the folded, logged form)
                let (_row_id, base, _) = p.base_row(&turn_id);
                let mut r = base;
                r["kind"] = serde_json::json!("userInput");
                r["text"] = serde_json::json!(format!("[steered] {text}"));
                r["origin"] = serde_json::json!("realUser");
                let entry = forwarded.lock().unwrap().pop_front();
                if let Some(e) = entry && !e.attachments.is_empty() {
                    r["attachments"] = serde_json::json!(e.attachments);
                }
                p.upsert_row(r);
                p.revision += 1;
            }
            LoopEvent::ToolCallStarted { id, name, args_json } => {
                let (row_id, base, _) = p.base_row(&turn_id);
                let mut r = base;
                r["kind"] = serde_json::json!("toolCall");
                r["toolCallId"] = serde_json::json!(id);
                r["toolName"] = serde_json::json!(name);
                r["status"] = serde_json::json!("running");
                r["inputText"] = serde_json::json!("");
                // raw sampled args — tool cards show the target (path) and
                // the preview drawer can open it
                if let Ok(args) = serde_json::from_str::<serde_json::Value>(&args_json) {
                    r["input"] = args;
                }
                r["startedAt"] = serde_json::json!(now_ms());
                p.upsert_row(r);
                tool_row_by_call.insert(id, row_id);
            }
            LoopEvent::ApprovalGranted { tool, path, scope } => {
                // #24: evidence for the ruleset learner. A human still
                // confirms any suggested rule over POST /api/rules —
                // nothing lands in settings from this arm.
                if let Some(perms) = &permissions {
                    let outcome = okra_policy::ApprovalOutcome::AllowedOnce;
                    let scope = match scope.as_str() {
                        "conversation" => okra_policy::ApprovalScope::Conversation,
                        "always" => okra_policy::ApprovalScope::Always,
                        _ => okra_policy::ApprovalScope::Once,
                    };
                    perms.learner.lock().unwrap().observe(&tool, path.as_deref(), outcome, scope);
                }
            }
            LoopEvent::ToolCallFinished { id, name, is_error, output } => {
                let row_id = tool_row_by_call.get(&id).copied();
                if let Some(row_id) = row_id {
                    if let Some(row) = p.rows.iter_mut().find(|r| r["rowId"].as_u64() == Some(row_id)) {
                        row["status"] = serde_json::json!(if is_error { "error" } else { "success" });
                        row["endedAt"] = serde_json::json!(now_ms());
                        row["output"] = serde_json::json!({ "text": output });
                        if is_error {
                            row["error"] = serde_json::json!({ "code": "tool_failed", "message": output });
                        }
                    }
                } else {
                    let _ = name;
                }
                p.revision += 1;
            }
            _ => {}
        }
            (broadcast)(
                "v4/projection",
                projection_notification(&topic_for_events, &p)["params"].clone(),
            );
        };
        match session_context {
            Some(ctx_lock) => {
                let mut ctx = ctx_lock.lock().unwrap();
                for (name, score) in &relevant_skills {
                    let digest = skill_catalog
                        .full_body(name)
                        .and_then(|body| body.lines().next().map(str::to_string))
                        .unwrap_or_default();
                    ctx.suggest_skill(name, &format!("({:.2}) {}", score, digest));
                }
                agent.run_turn_continuation(
                    &mut ctx,
                    &okra_compaction::ScriptedCompactor,
                    Some(&memory_reader),
                    Some(&skill_catalog),
                    &input_text,
                    &mut handle_ev,
                )
            }
            None => agent.run_turn(&input_text, &mut handle_ev),
        }
    };

    wd_done.store(true, Ordering::Relaxed);
    let _ = wd_handle.join();
    fwd_done.store(true, Ordering::Relaxed);
    // N0029: finalize this prompt's rewind checkpoint — the git HEAD at
    // turn end is what restore_to resets to; outside a repo it is FS-only
    if let Some(mgr) = checkpoints.as_ref()
        && let Ok(mut mgr) = mgr.lock()
    {
        let repo = okra_host::git::GitRepository::open(&cwd).ok();
        let _ = mgr.finalize_prompt(prompt_index, None, repo.as_ref());
    }
    // session lock: this session's turn is over — the computer is free
    computer_consent
        .lock()
        .unwrap()
        .release_driving(&session_id);
    if let Some(f) = q_teardown_flag.take() {
        f.store(true, Ordering::Relaxed);
    }

    // 4. finalize rows + control (honest states: a user stop is an
    // interrupted turn, not an error)
    {
        let mut p = turn_row_lock.lock().unwrap();
        p.control["awaitingApproval"] = serde_json::json!([]);
        p.control["awaitingQuestion"] = serde_json::Value::Null;
        let (assistant_state, header_state) = match &outcome {
            Ok(TurnOutcome::Completed { stop: okra_agent_core::turn::CompletedStop::MaxTokens, .. })
                | Ok(TurnOutcome::Cancelled { category: Some(CancellationCategory::UserRequested) }) => {
                    ("interrupted", "completedInterrupted")
                }
            Ok(TurnOutcome::Completed { .. }) => ("complete", "completedSuccess"),
            Ok(TurnOutcome::Cancelled { .. }) | Err(_) => ("failed", "failed"),
            Ok(TurnOutcome::MaxTurnsReached { .. }) | Ok(TurnOutcome::StationarityEnded) => {
                ("interrupted", "completedInterrupted")
            }
        };
        if let Some(row_id) = *assistant_slot.lock().unwrap()
            && let Some(row) = p.rows.iter_mut().find(|r| r["rowId"].as_u64() == Some(row_id)) {
                row["state"] = serde_json::json!(assistant_state);
            }
        // finalize the running turnHeader (first turnHeader row still running)
        if let Some(row) = p.rows.iter_mut().find(|r| {
            r["kind"] == "turnHeader" && r["state"] == "running"
        }) {
            row["state"] = serde_json::json!(header_state);
            row["endedAt"] = serde_json::json!(now_ms());
        }
        let (phase, ended) = match &outcome {
            Ok(TurnOutcome::Completed { stop: okra_agent_core::turn::CompletedStop::MaxTokens, .. })
                | Ok(TurnOutcome::MaxTurnsReached { .. })
                | Ok(TurnOutcome::StationarityEnded)
                | Ok(TurnOutcome::Cancelled { category: Some(CancellationCategory::UserRequested) }) => {
                    ("completedInterrupted", true)
                }
            Ok(TurnOutcome::Completed { .. }) => ("completedSuccess", true),
            _ => ("error", false),
        };
        p.control["phase"] = serde_json::json!(phase);
        p.control["sessionEnded"] = serde_json::json!(ended);
        p.control["canStop"] = serde_json::json!(false);
        p.control["activeWorks"] = serde_json::json!([]);
        p.revision += 1;
        (broadcast)(
            "v4/projection",
            projection_notification(&topic, &p)["params"].clone(),
        );
    }

    // 6. native-notification boundary (3 classes, redacted — ChatGPT2
    // docs/02): the surface decides native vs in-app from ITS focus; the
    // daemon only guarantees the label carries no content.
    {
        let title = turn_row_lock
            .lock()
            .ok()
            .and_then(|p| p.title().map(str::to_string))
            .unwrap_or_else(|| session_id.clone());
        let (class, label) = match &outcome {
            Ok(TurnOutcome::Cancelled { category: Some(CancellationCategory::UserRequested) }) => {
                (NotificationClass::TurnComplete, format!("Task stopped: {title}"))
            }
            Ok(TurnOutcome::Completed { .. }) => {
                (NotificationClass::TurnComplete, format!("Task completed: {title}"))
            }
            _ => (NotificationClass::TurnComplete, format!("Task failed: {title}")),
        };
        let n = classify(class, &label, &session_id);
        (broadcast)(
            "v4/notification",
            serde_json::json!({
                "class": n.class,
                "label": n.label,
                "sessionId": n.session_id,
            }),
        );
    }

    // 5. M3 strangler: fold this session into the SQLite task/session index.
    // Per-session replace — other sessions' indexed rows survive.
    {
        let title = turn_row_lock
            .lock()
            .ok()
            .and_then(|p| p.title().map(str::to_string));
        if let Ok(db) = kernel::ProjectionDb::open(&sessions_dir.join("index.db"))
            && let Ok(reader) = kernel::SessionHandle::open(
                &sessions_dir,
                &format!("session-{session_id}"),
                kernel::SessionAccess::Read,
            )
            && let Ok(events) = reader.read_all()
        {
            let workspace = cwd.to_string_lossy().into_owned();
            let _ = db.replace_session(&events, &session_id, &workspace);
            if let Some(t) = title {
                let created = events.first().map(|e| e.time).unwrap_or_else(now_ms);
                let _ = db.upsert_session(
                    &session_id,
                    &workspace,
                    &t,
                    "active",
                    created,
                    events.len() as u64,
                );
            }
        }
    }
    outcome
}

/// Fold the kernel log of one (non-live) session into projection rows the
/// UI can render — the replay path that makes sessions survive page
/// reloads and daemon restarts. Event vocabulary (loop_.rs): turn/start,
/// user/message, assistant/message, tool/call, tool/result, turn/end.
/// `tool/result` carries the full output text (loop_.rs logs it because
/// it is model-visible); rows without it (logs from before the field
/// landed) fall back to status-only tool cards.
pub fn rows_from_kernel_events(
    events: &[kernel::SessionEvent],
) -> Result<(Vec<serde_json::Value>, serde_json::Value), String> {
    let mut rows: Vec<serde_json::Value> = Vec::new();
    let mut next_row_id: u64 = 1;
    // kernel turn number -> open turnHeader rowId
    let mut header_by_turn: std::collections::HashMap<u64, u64> = Default::default();
    // last seen turn/start number (rows carry it as their turnId)
    let mut current_turn: u64 = 0;
    // tool callId -> toolCall rowId
    let mut tool_row_by_call: std::collections::HashMap<String, u64> = Default::default();
    // approvalId -> approval rowId
    let mut approval_row_by_id: std::collections::HashMap<String, u64> = Default::default();

    let header_state_of = |kind: &str| match kind {
        "completed" => "completedSuccess",
        "cancelled" | "max_turns" | "stationarity" => "completedInterrupted",
        _ => "failed",
    };

    for ev in events {
        let data = &ev.data;
        match ev.event_type.as_str() {
            "turn/start" => {
                let turn = data["turn"].as_u64().unwrap_or(0);
                current_turn = turn;
                let row_id = next_row_id;
                next_row_id += 1;
                rows.push(serde_json::json!({
                    "rowId": row_id,
                    "turnId": format!("t{turn}"),
                    "createdAt": ev.time,
                    "createdAtSeq": ev.seq,
                    "kind": "turnHeader",
                    "origin": "userInput",
                    "state": "running",
                    "startedAt": ev.time,
                }));
                header_by_turn.insert(turn, row_id);
            }
            "user/message" => {
                let text = data["text"].as_str().unwrap_or_default();
                let steered = data["origin"].as_str() == Some("steering");
                let row_id = next_row_id;
                next_row_id += 1;
                let mut row = serde_json::json!({
                    "rowId": row_id,
                    "turnId": format!("t{current_turn}"),
                    "createdAt": ev.time,
                    "createdAtSeq": ev.seq,
                    "kind": "userInput",
                    "text": if steered { format!("[steered] {text}") } else { text.to_string() },
                    "origin": "realUser",
                });
                // attachments are replayed from the fold markers in the
                // logged text (the transcript of record)
                let attached: Vec<&str> = text
                    .match_indices("[Attached file: ")
                    .filter_map(|(i, _)| {
                        let rest = &text[i + "[Attached file: ".len()..];
                        let end = rest.find(']')?;
                        rest[..end].split(" — ").next()
                    })
                    .collect();
                if !attached.is_empty() {
                    row["attachments"] = serde_json::json!(attached);
                }
                rows.push(row);
            }
            "assistant/message" => {
                // pure tool-call responses log an assistant/message with
                // empty text (n0028 fix); live turns render no assistant
                // row for those — replay must match
                let text = data["text"].as_str().unwrap_or_default();
                if text.is_empty() {
                    continue;
                }
                let row_id = next_row_id;
                next_row_id += 1;
                rows.push(serde_json::json!({
                    "rowId": row_id,
                    "turnId": format!("t{current_turn}"),
                    "createdAt": ev.time,
                    "createdAtSeq": ev.seq,
                    "kind": "assistantText",
                    "text": text,
                    "state": "complete",
                }));
            }
            "tool/call" => {
                let call_id = data["callId"].as_str().unwrap_or_default().to_string();
                let row_id = next_row_id;
                next_row_id += 1;
                rows.push(serde_json::json!({
                    "rowId": row_id,
                    "turnId": format!("t{current_turn}"),
                    "createdAt": ev.time,
                    "createdAtSeq": ev.seq,
                    "kind": "toolCall",
                    "toolCallId": call_id,
                    "toolName": data["tool"].as_str().unwrap_or_default(),
                    "status": "success",
                    "inputText": "",
                    "startedAt": ev.time,
                    "endedAt": ev.time,
                }));
                tool_row_by_call.insert(call_id, row_id);
            }
            "tool/result" => {
                let call_id = data["callId"].as_str().unwrap_or_default();
                if let Some(row_id) = tool_row_by_call.get(call_id)
                    && let Some(row) = rows.iter_mut().find(|r| r["rowId"].as_u64() == Some(*row_id))
                {
                    // latest result wins, mirroring the live path: status
                    // and error flip per result event
                    let is_error = data["isError"] == serde_json::Value::Bool(true);
                    row["status"] = serde_json::json!(if is_error { "error" } else { "success" });
                    // `output` landed after the first release (older logs
                    // carry callId+isError only) — attach it when present
                    if let Some(text) = data["output"].as_str() {
                        row["output"] = serde_json::json!({ "text": text });
                    }
                    if is_error {
                        let message = data["output"]
                            .as_str()
                            .unwrap_or("tool result logged as error");
                        row["error"] = serde_json::json!({"code": "tool_failed", "message": message});
                    } else {
                        if let Some(obj) = row.as_object_mut() {
                            obj.remove("error");
                        }
                    }
                }
            }
            "approval/asked" => {
                let row_id = next_row_id;
                next_row_id += 1;
                rows.push(serde_json::json!({
                    "rowId": row_id,
                    "turnId": format!("t{current_turn}"),
                    "createdAt": ev.time,
                    "createdAtSeq": ev.seq,
                    "kind": "approval",
                    "approvalId": data["approvalId"].as_str().unwrap_or_default(),
                    "toolName": data["toolName"].as_str().unwrap_or_default(),
                    "args": data["args"].as_str().unwrap_or_default(),
                    "state": "pending",
                }));
                approval_row_by_id.insert(
                    data["approvalId"].as_str().unwrap_or_default().to_string(),
                    row_id,
                );
            }
            "approval/decided" => {
                let approval_id = data["approvalId"].as_str().unwrap_or_default();
                if let Some(row_id) = approval_row_by_id.get(approval_id)
                    && let Some(row) = rows.iter_mut().find(|r| r["rowId"].as_u64() == Some(*row_id))
                {
                    let outcome = data["outcome"].as_str().unwrap_or_default();
                    row["state"] = serde_json::json!(match outcome {
                        "allowed-once" => "allowed",
                        "rejected" => "denied",
                        "cancelled" => "cancelled",
                        _ => "denied",
                    });
                }
            }
            "turn/end" => {
                let turn = data["turn"].as_u64().unwrap_or(0);
                let kind = data["kind"].as_str().unwrap_or_default().to_string();
                if let Some(row_id) = header_by_turn.get(&turn)
                    && let Some(row) = rows.iter_mut().find(|r| r["rowId"].as_u64() == Some(*row_id))
                {
                    row["state"] = serde_json::json!(header_state_of(&kind));
                    row["endedAt"] = serde_json::json!(ev.time);
                }
            }
            other => {
                // vocabulary-growth rule: unknown events are skippable only
                // when marked ignorable; otherwise refuse the replay
                if ev.ignorable != Some(true) {
                    return Err(format!(
                        "cannot replay: unrecognized non-ignorable event `{other}` at seq {}",
                        ev.seq
                    ));
                }
            }
        }
    }

    let control = serde_json::json!({
        "phase": "replayed",
        "sessionEnded": true,
        "canStop": false,
        "stopState": "idle",
        "stopTargetKind": "unknown",
        "activeWorks": [],
        "lastError": null,
        "apiRetry": null,
    });
    Ok((rows, control))
}

/// First user text of a session log (the UI title).
pub fn session_title_from_log(sessions_dir: &Path, session_id: &str) -> Option<String> {
    let reader = kernel::SessionHandle::open(
        sessions_dir,
        &format!("session-{session_id}"),
        kernel::SessionAccess::Read,
    )
    .ok()?;
    let events = reader.read_all().ok()?;
    events
        .iter()
        .find(|e| e.event_type == "user/message" && e.data["origin"].as_str() != Some("steering"))
        .map(|e| {
            e.data["text"]
                .as_str()
                .unwrap_or_default()
                .chars()
                .take(80)
                .collect::<String>()
        })
}

/// Percent-decode a query parameter (the UI sends encodeURIComponent).
fn percent_decode(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() + 1 && i + 2 <= bytes.len() - 1 + 1 => {
                let hex = |b: u8| -> Option<u8> {
                    match b {
                        b'0'..=b'9' => Some(b - b'0'),
                        b'a'..=b'f' => Some(b - b'a' + 10),
                        b'A'..=b'F' => Some(b - b'A' + 10),
                        _ => None,
                    }
                };
                if i + 2 < bytes.len()
                    && let (Some(h), Some(l)) = (hex(bytes[i + 1]), hex(bytes[i + 2]))
                {
                    out.push(h * 16 + l);
                    i += 3;
                    continue;
                }
                out.push(bytes[i]);
                i += 1;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Confine a client-supplied relative path to the workspace root: no `..`
/// escapes, no absolute re-anchoring, symlinks resolved and checked.
fn confine_to_workspace(
    root: &Path,
    rel_raw: &str,
) -> Result<PathBuf, (u16, String)> {
    let rel = percent_decode(rel_raw);
    let rel = rel.trim_start_matches('/');
    if rel.is_empty() {
        return Ok(root.to_path_buf());
    }
    let candidate = root.join(rel);
    // lexical check first (clear error), then canonical reality
    for component in candidate.components() {
        if component == std::path::Component::ParentDir {
            return Err((400, "`..` is not allowed".to_string()));
        }
    }
    let canon_root = okra_host::fsutil::canonicalize(root)
        .map_err(|e| (500, format!("workspace root: {e}")))?;
    let canon = okra_host::fsutil::canonicalize(&candidate).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            (404, "path not found".to_string())
        } else {
            (400, format!("path: {e}"))
        }
    })?;
    if !canon.starts_with(&canon_root) {
        return Err((400, "path escapes the workspace".to_string()));
    }
    Ok(canon)
}

/// GET /api/files?path=rel — workspace-confined directory listing.
/// Dot entries (including .okra-sessions) are never listed; symlinks are
/// reported with `link: true` and never followed.
pub fn files_listing(cwd: &Path, rel_raw: &str) -> Result<serde_json::Value, (u16, String)> {
    let dir = confine_to_workspace(cwd, rel_raw)?;
    if !dir.is_dir() {
        return Err((400, "not a directory".to_string()));
    }
    let mut entries: Vec<serde_json::Value> = Vec::new();
    let read = std::fs::read_dir(&dir).map_err(|e| (500, format!("read_dir: {e}")))?;
    for entry in read.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') {
            continue; // dot entries never surface (incl. .okra-sessions)
        }
        let ft = match entry.file_type() {
            Ok(ft) => ft,
            Err(_) => continue,
        };
        let is_symlink = ft.is_symlink();
        let is_dir = if is_symlink {
            // report the link's own kind; never follow it
            false
        } else {
            ft.is_dir()
        };
        let mut item = serde_json::json!({
            "name": name,
            "dir": is_dir,
            "link": is_symlink,
        });
        if !is_dir
            && !is_symlink
            && let Ok(md) = entry.metadata()
        {
            item["size"] = serde_json::json!(md.len());
        }
        entries.push(item);
    }
    entries.sort_by(|a, b| {
        b["dir"]
            .as_bool()
            .unwrap_or(false)
            .cmp(&a["dir"].as_bool().unwrap_or(false))
            .then_with(|| a["name"].as_str().cmp(&b["name"].as_str()))
    });
    Ok(serde_json::json!({ "entries": entries }))
}

/// Preview cap: previews are for reading, not for shipping the whole file.
const FILE_PREVIEW_MAX_BYTES: u64 = 256 * 1024;

/// GET /api/file?path=rel — safe-read preview (host safe_fs: O_NOFOLLOW,
/// O_NONBLOCK, regular-file verification) confined to the workspace.
pub fn file_preview(cwd: &Path, rel_raw: &str) -> Result<serde_json::Value, (u16, String)> {
    let file = confine_to_workspace(cwd, rel_raw)?;
    let bytes = okra_host::safe_fs::safe_read(&file).map_err(|e| match e {
        okra_host::safe_fs::SafeReadError::Io(io)
            if io.kind() == std::io::ErrorKind::NotFound =>
        {
            (404, "file not found".to_string())
        }
        other => (400, other.to_string()),
    })?;
    let size = bytes.len() as u64;
    let truncated = size > FILE_PREVIEW_MAX_BYTES;
    let shown = if truncated {
        &bytes[..FILE_PREVIEW_MAX_BYTES as usize]
    } else {
        &bytes[..]
    };
    Ok(serde_json::json!({
        "path": percent_decode(rel_raw),
        "size": size,
        "truncated": truncated,
        "binary": shown.contains(&0u8),
        "content": String::from_utf8_lossy(shown),
        // the safe-read honesty contract, visible to surfaces
        "enforcement": okra_host::safe_fs::enforcement_level(),
    }))
}

/// GET /api/git — branch + working-tree changes (the Changes tab source).
/// Outside a repository this is honest: `repository: false`.
pub fn git_overview(cwd: &Path) -> Result<serde_json::Value, (u16, String)> {
    let repo = match okra_host::git::GitRepository::open(cwd) {
        Ok(r) => r,
        Err(_) => return Ok(serde_json::json!({ "repository": false })),
    };
    let head = repo.head().map_err(|e| (500, e.to_string()))?;
    let changes: Vec<serde_json::Value> = repo
        .status()
        .map_err(|e| (500, e.to_string()))?
        .into_iter()
        // the daemon's own bookkeeping is never a user-visible change
        .filter(|c| !c.path.starts_with(".okra-sessions"))
        .map(|c| serde_json::json!({ "code": c.code, "path": c.path }))
        .collect();
    Ok(serde_json::json!({
        "repository": true,
        "branch": head.branch,
        "hash": head.hash,
        "changes": changes,
    }))
}

/// GET /api/git/diff?path=rel — unified working-tree diff for one file
/// (repo-root-relative; `..` and option-looking paths refused).
pub fn git_diff(cwd: &Path, rel_raw: &str) -> Result<serde_json::Value, (u16, String)> {
    let repo = okra_host::git::GitRepository::open(cwd)
        .map_err(|_| (400, "not a git repository".to_string()))?;
    let path = percent_decode(rel_raw);
    if path.split(['/', '\\']).any(|seg| seg == "..") {
        return Err((400, "`..` is not allowed".to_string()));
    }
    let diff = repo.diff_file(&path).map_err(|e| (400, e.to_string()))?;
    Ok(serde_json::json!({ "path": path, "diff": diff }))
}

/// POST /api/git/stage|unstage {paths:[...]} — index operations.
pub fn git_stage(cwd: &Path, raw_paths: &[String], unstage: bool) -> Result<serde_json::Value, (u16, String)> {
    if raw_paths.is_empty() {
        return Err((400, "paths required".to_string()));
    }
    let repo = okra_host::git::GitRepository::open(cwd)
        .map_err(|_| (400, "not a git repository".to_string()))?;
    let paths: Vec<&str> = raw_paths.iter().map(String::as_str).collect();
    let r = if unstage {
        repo.unstage(&paths)
    } else {
        repo.stage(&paths)
    };
    r.map_err(|e| (400, e.to_string()))?;
    Ok(serde_json::json!({ "staged": !unstage, "count": paths.len() }))
}

/// POST /api/git/commit {message} — commit the staged index.
pub fn git_commit(cwd: &Path, message: &str) -> Result<serde_json::Value, (u16, String)> {
    let repo = okra_host::git::GitRepository::open(cwd)
        .map_err(|_| (400, "not a git repository".to_string()))?;
    let hash = repo.commit(message).map_err(|e| (400, e.to_string()))?;
    Ok(serde_json::json!({ "hash": hash, "branch": repo.head().map(|h| h.branch).unwrap_or_default() }))
}

/// Per-turn attachment budget: content is folded into the logged,
/// model-visible user message, so it is bounded like any other prompt.
const ATTACH_FILE_CAP: usize = 16 * 1024;
const ATTACH_TOTAL_CAP: usize = 48 * 1024;

/// Fold attachment files into the turn input: each existing, confined file
/// inlines as a fenced block after the prompt; missing/unreadable ones are
/// listed as such. Model-visible means logged — the fold happens BEFORE
/// the user message row/event, so the log stays the transcript of record.
fn fold_attachments(
    cwd: &Path,
    input_text: &str,
    attachments: &[String],
) -> (String, Vec<String>) {
    if attachments.is_empty() {
        return (input_text.to_string(), Vec::new());
    }
    let mut folded = String::new();
    let mut missing = Vec::new();
    let mut total = 0usize;
    for rel in attachments {
        let Ok(file) = confine_to_workspace(cwd, rel) else {
            missing.push(rel.clone());
            continue;
        };
        let bytes = match okra_host::safe_fs::safe_read(&file) {
            Ok(b) => b,
            Err(_) => {
                missing.push(rel.clone());
                continue;
            }
        };
        if total + bytes.len().min(ATTACH_FILE_CAP) > ATTACH_TOTAL_CAP {
            missing.push(format!("{rel} (attachment budget exhausted)"));
            continue;
        }
        let capped = bytes.len() > ATTACH_FILE_CAP;
        let shown = &bytes[..bytes.len().min(ATTACH_FILE_CAP)];
        let text = String::from_utf8_lossy(shown);
        folded.push_str(&format!(
            "\n\n[Attached file: {rel}{}]\n```\n{}\n```",
            if capped { " — truncated" } else { "" },
            text.trim_end(),
        ));
        total += shown.len();
    }
    let mut out = input_text.to_string();
    if !folded.is_empty() {
        out.push_str("\n\n--- attachments ---");
        out.push_str(&folded);
    }
    if !missing.is_empty() {
        out.push_str(&format!("\n\n[attachments not loaded: {}]", missing.join(", ")));
    }
    (out, attachments.to_vec())
}

/// Recursive workspace file search for @-mentions: same confinement and
/// dot-entry rules as the tree, substring match on the relative path,
/// results capped.
pub fn file_search(cwd: &Path, query_raw: &str) -> Result<serde_json::Value, (u16, String)> {
    let query = percent_decode(query_raw).to_lowercase();
    let root = okra_host::fsutil::canonicalize(cwd).map_err(|e| (500, format!("root: {e}")))?;
    let mut results = Vec::new();
    let mut stack = vec![root.clone()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                continue;
            }
            let Ok(ft) = entry.file_type() else { continue };
            let path = entry.path();
            let rel = path
                .strip_prefix(&root)
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default();
            if ft.is_dir() {
                // symlinks are never followed
                if !ft.is_symlink() && stack.len() < 64 {
                    stack.push(path);
                }
                continue;
            }
            if query.is_empty() || rel.to_lowercase().contains(&query) {
                results.push(rel);
                if results.len() >= 50 {
                    return Ok(serde_json::json!({ "matches": results, "capped": true }));
                }
            }
        }
    }
    Ok(serde_json::json!({ "matches": results, "capped": false }))
}

fn skills_dir(cwd: &Path) -> PathBuf {
    cwd.join(".okra").join("skills")
}

/// The skill FILE name for a skill name (sanitized): `SKILL-<name>.md`.
fn skill_file_name(name: &str) -> Result<String, (u16, String)> {
    let clean: String = name
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .collect();
    if clean.is_empty() {
        return Err((400, "skill name has no usable characters".to_string()));
    }
    Ok(format!("SKILL-{clean}.md"))
}

/// GET /api/skills — installed (`.okra/skills/*.md`) + disabled
/// (`*.md.disabled`) skills with their path-conditional patterns
/// (disclosure layer 1).
pub fn skills_listing(cwd: &Path) -> serde_json::Value {
    let catalog = okra_memory::SkillCatalog::load_dir(&skills_dir(cwd));
    let mut skills: Vec<serde_json::Value> = catalog
        .skills
        .iter()
        .map(|s| {
            serde_json::json!({
                "name": s.name,
                "description": s.description,
                "patterns": s.match_patterns,
                "disabled": false,
            })
        })
        .collect();
    // disabled skills: `*.md.disabled` — name recovered from the frontmatter
    let dir = skills_dir(cwd);
    if let Ok(entries) = std::fs::read_dir(&dir) {
        let mut files: Vec<_> = entries.flatten().map(|e| e.path()).collect();
        files.sort();
        for path in files {
            let Some(name) = path.file_name().map(|n| n.to_string_lossy().into_owned()) else {
                continue;
            };
            if !name.ends_with(".md.disabled") {
                continue;
            }
            if let Ok(src) = std::fs::read_to_string(&path)
                && let Ok(skill) = okra_memory::SkillDef::parse(&src)
            {
                skills.push(serde_json::json!({
                    "name": skill.name,
                    "description": skill.description,
                    "patterns": skill.match_patterns,
                    "disabled": true,
                }));
            }
        }
    }
    serde_json::json!({ "dir": ".okra/skills", "skills": skills })
}

/// POST /api/skills/install — write a new skill file (fails on duplicate).
#[allow(clippy::too_many_arguments)]
pub fn skills_install(
    cwd: &Path,
    name: &str,
    description: &str,
    patterns: &[String],
    body: &str,
) -> Result<serde_json::Value, (u16, String)> {
    let file = skill_file_name(name)?;
    let dir = skills_dir(cwd);
    std::fs::create_dir_all(&dir).map_err(|e| (500, format!("skills dir: {e}")))?;
    let path = dir.join(&file);
    if path.exists() {
        return Err((409, format!("skill {name} already installed")));
    }
    let mut src = String::from("---\n");
    src.push_str(&format!("name: {name}\n"));
    src.push_str(&format!("description: {description}\n"));
    if !patterns.is_empty() {
        src.push_str(&format!("match: {}\n", patterns.join(" ")));
    }
    src.push_str("---\n");
    src.push_str(body);
    src.push('\n');
    std::fs::write(&path, src).map_err(|e| (500, format!("write: {e}")))?;
    Ok(serde_json::json!({ "installed": name, "file": format!(".okra/skills/{file}") }))
}

/// POST /api/skills/disable|enable — rename to/from the `.disabled` suffix.
pub fn skills_set_disabled(
    cwd: &Path,
    name: &str,
    disabled: bool,
) -> Result<serde_json::Value, (u16, String)> {
    let file = skill_file_name(name)?;
    let dir = skills_dir(cwd);
    let (from, to) = if disabled {
        (dir.join(&file), dir.join(format!("{file}.disabled")))
    } else {
        (dir.join(format!("{file}.disabled")), dir.join(&file))
    };
    if !from.exists() {
        return Err((404, format!("skill file not found: {file}")));
    }
    std::fs::rename(&from, &to).map_err(|e| (500, format!("rename: {e}")))?;
    Ok(serde_json::json!({ "name": name, "disabled": disabled }))
}

/// POST /api/skills/delete — remove the skill file (enabled or disabled).
pub fn skills_delete(cwd: &Path, name: &str) -> Result<serde_json::Value, (u16, String)> {
    let file = skill_file_name(name)?;
    let dir = skills_dir(cwd);
    for candidate in [dir.join(&file), dir.join(format!("{file}.disabled"))] {
        if candidate.exists() {
            std::fs::remove_file(&candidate)
                .map_err(|e| (500, format!("remove: {e}")))?;
            return Ok(serde_json::json!({ "deleted": name }));
        }
    }
    Err((404, format!("skill file not found: {file}")))
}

/// POST /api/mcp/probe — connect to one (or all) configured stdio MCP
/// servers, initialize + tools/list with a bounded wait, and cache the
/// runtime status the Tools tab displays. Probing is explicit (a POST),
/// never a side effect of listing.
pub fn mcp_probe(
    cwd: &Path,
    status_cache: &Mutex<std::collections::BTreeMap<String, serde_json::Value>>,
    name: Option<&str>,
) -> Result<serde_json::Value, (u16, String)> {
    use std::sync::mpsc;
    let home = okra_host::fsutil::home_dir().unwrap_or_else(|| cwd.to_path_buf());
    let svc = okra_host::mcp_sync::McpSyncService::new(home);
    let servers = svc.load(Some(cwd)).map_err(|e| (500, e.to_string()))?;
    let mut probed = Vec::new();
    for r in servers {
        if let Some(n) = name && r.name != n {
            continue;
        }
        let command = r.config["command"].as_str().map(str::to_string);
        let args: Vec<String> = r.config["args"]
            .as_array()
            .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
            .unwrap_or_default();
        let mut status = serde_json::json!({
            "name": r.name,
            "enabled": r.enabled,
            "status": "no-command",
            "probedAt": now_ms(),
        });
        if let Some(command) = command {
            let (tx, rx) = mpsc::channel();
            let srv = r.name.clone();
            std::thread::spawn(move || {
                let mut client = okra_tools::McpClient::stdio(&srv, &command, &args);
                if client.initialize().is_err() {
                    let _ = tx.send(serde_json::json!({ "status": "error" }));
                    return;
                }
                let tools = client.tools_list().unwrap_or_default();
                let _ = tx.send(serde_json::json!({
                    "status": "connected",
                    "protocolVersion": client.protocol_version,
                    "serverInfo": client.server_info,
                    "tools": tools.iter().map(|t| t.name.clone()).collect::<Vec<_>>(),
                    "toolCount": tools.len(),
                }));
            });
            match rx.recv_timeout(std::time::Duration::from_secs(20)) {
                Ok(mut result) => {
                    result["name"] = serde_json::json!(r.name);
                    result["enabled"] = serde_json::json!(r.enabled);
                    result["probedAt"] = serde_json::json!(now_ms());
                    status = result;
                }
                Err(_) => {
                    status["status"] = serde_json::json!("timeout");
                }
            }
        }
        status_cache
            .lock()
            .unwrap()
            .insert(r.name.clone(), status.clone());
        probed.push(status);
    }
    Ok(serde_json::json!({ "probed": probed }))
}

/// GET /api/mcp — configured MCP servers across scopes (workspace
/// `.okra/config.json` `mcp.servers` wins over user-level sources).
/// Status is the sync domain's truth (enabled + source + scope) plus the
/// cached RUNTIME status from the last explicit probe, when present.
pub fn mcp_listing(
    cwd: &Path,
    status_cache: &Mutex<std::collections::BTreeMap<String, serde_json::Value>>,
) -> serde_json::Value {
    let home = okra_host::fsutil::home_dir().unwrap_or_else(|| cwd.to_path_buf());
    let svc = okra_host::mcp_sync::McpSyncService::new(home);
    let servers: Vec<serde_json::Value> = svc
        .load(Some(cwd))
        .unwrap_or_default()
        .into_iter()
        .map(|r| {
            let summary = r.config["command"]
                .as_str()
                .map(|c| {
                    let args: Vec<&str> = r.config["args"]
                        .as_array()
                        .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
                        .unwrap_or_default();
                    if args.is_empty() {
                        c.to_string()
                    } else {
                        format!("{c} {}", args.join(" "))
                    }
                })
                .or_else(|| r.config["url"].as_str().map(str::to_string))
                .or_else(|| r.config["type"].as_str().map(str::to_string))
                .unwrap_or_default();
            let mut item = serde_json::json!({
                "name": r.name,
                "enabled": r.enabled,
                "source": r.source.as_str(),
                "scope": if r.workspace_path.is_some() { "workspace" } else { "user" },
                "summary": summary,
            });
            if let Some(st) = status_cache.lock().unwrap().get(&r.name) {
                item["status"] = st.clone();
            }
            item
        })
        .collect();
    serde_json::json!({ "servers": servers })
}

/// Read one session's durable log (the replay source of truth).
pub fn session_events(
    sessions_dir: &Path,
    kernel_name: &str,
) -> Result<Vec<kernel::SessionEvent>, String> {
    let reader = kernel::SessionHandle::open(sessions_dir, kernel_name, kernel::SessionAccess::Read)
        .map_err(|e| e.to_string())?;
    reader.read_all().map_err(|e| e.to_string())
}

/// GET /api/sessions: the index (SQLite projection) merged with live
/// in-memory sessions that have no indexed row yet.
pub fn list_session_summaries(
    sessions_dir: &Path,
    live: &std::collections::BTreeMap<String, Arc<Mutex<SessionProjection>>>,
) -> Vec<serde_json::Value> {
    let mut out: Vec<serde_json::Value> = Vec::new();
    let mut seen: std::collections::BTreeSet<String> = Default::default();
    if let Ok(db) = kernel::ProjectionDb::open(&sessions_dir.join("index.db"))
        && let Ok(rows) = db.list_sessions()
    {
        for r in rows {
            seen.insert(r.id.clone());
            out.push(serde_json::json!({
                "id": r.id,
                "title": if r.title.is_empty() {
                    session_title_from_log(sessions_dir, &r.id).unwrap_or_default()
                } else {
                    r.title
                },
                "status": r.status,
                "eventCount": r.event_count,
                "workspace": r.workspace,
                "live": false,
            }));
        }
    }
    for (id, proj) in live {
        if seen.contains(id) {
            continue;
        }
        let p = proj.lock().unwrap();
        out.push(serde_json::json!({
            "id": id,
            "title": p.title().unwrap_or(id),
            "status": p.control["phase"].as_str().unwrap_or("unknown"),
            "eventCount": p.rows.len(),
            "workspace": "",
            "live": true,
        }));
    }
    // newest first by id is meaningless; keep insertion order (index order)
    out
}

/// `okra serve --stdio --cwd <dir>`: JSON-RPC loop over stdin/stdout.
pub fn serve_stdio(cwd: std::path::PathBuf, sessions_dir: std::path::PathBuf) -> ! {
    let outbound = Arc::new(Outbound {
        out: Mutex::new(std::io::stdout()),
        static_id: AtomicU64::new(1000),
    });
    let sessions: Arc<Mutex<std::collections::HashMap<String, Arc<Mutex<SessionProjection>>>>> =
        Arc::new(Mutex::new(std::collections::HashMap::new()));
    // n0028: per-session continuation contexts (chained turns over stdio)
    let contexts: Arc<Mutex<std::collections::HashMap<String, Arc<Mutex<okra_compaction::SessionContext>>>>> =
        Arc::new(Mutex::new(std::collections::HashMap::new()));
    // n0040: stdio rewind checkpoints — the same capture/restore surface
    // the TCP daemon has (writes record per-prompt; a manager per daemon,
    // durable mirror under the workspace, loaded at startup)
    let checkpoint_mirror = cwd.join(".okra").join("checkpoints.jsonl");
    let mut checkpoint_mgr = okra_host::checkpoints::CheckpointManager::new(cwd.clone())
        .with_durable_mirror(checkpoint_mirror.clone());
    let _ = checkpoint_mgr.load_durable_mirror(checkpoint_mirror);
    let checkpoints: Arc<Mutex<okra_host::checkpoints::CheckpointManager>> =
        Arc::new(Mutex::new(checkpoint_mgr));
    let session_turn_ordinals: Arc<Mutex<std::collections::HashMap<String, usize>>> =
        Arc::new(Mutex::new(std::collections::HashMap::new()));
    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let Ok(msg) = serde_json::from_str::<serde_json::Value>(&line) else {
            outbound.result(0, serde_json::json!({ "error": "bad json" }));
            continue;
        };
        let id = msg["id"].as_u64().unwrap_or(0);
        let method = msg["method"].as_str().unwrap_or_default().to_string();
        let params = if msg["params"].is_null() { serde_json::Value::Null } else { msg["params"].clone() };
        match method.as_str() {
            "hello" => {
                outbound.result(
                    id,
                    serde_json::json!({
                        "daemon": "okra",
                        "protocolVersion": 3,
                        "cwd": cwd.to_string_lossy(),
                    }),
                );
            }
            "ping" => {
                outbound.result(id, serde_json::json!({ "pong": outbound.next_static() }));
            }
            "v4/conversation/subscribe" => {
                let session_id = params["sessionId"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string();
                let topic = format!("conversation/{session_id}");
                let sessions_guard = sessions.lock().unwrap();
                if let Some(p) = sessions_guard.get(&session_id) {
                    let p = p.lock().unwrap();
                    outbound.notification(
                        "v4/projection",
                        projection_notification(&topic, &p)["params"].clone(),
                    );
                }
                outbound.result(
                    id,
                    serde_json::json!({
                        "ack": {
                            "subscriptionId": format!("okra-sub-{}", outbound.next_static()),
                            "mode": "snapshot",
                            "logEpoch": "0",
                        }
                    }),
                );
            }
            "v4/command" => {
                let envelope = &params["envelope"];
                let command_id = envelope["commandId"]
                    .as_str()
                    .unwrap_or("cmd")
                    .to_string();
                let cmd_type = envelope["type"].as_str().unwrap_or_default().to_string();
                match cmd_type.as_str() {
                    "createSession" | "sendText" => {
                        let session_id = envelope["sessionId"]
                            .as_str()
                            .map(str::to_string)
                            .unwrap_or_else(|| format!("okra-{}", uuid_v4()));
                        let text = envelope["payload"]["text"]
                            .as_str()
                            .unwrap_or_default()
                            .to_string();
                        let is_new = cmd_type == "createSession";
                        {
                            let mut sessions_guard = sessions.lock().unwrap();
                            sessions_guard
                                .entry(session_id.clone())
                                .or_insert_with(|| {
                                    Arc::new(Mutex::new(SessionProjection::new(
                                        session_id.clone(),
                                        cwd.clone(),
                                    )))
                                });
                        }
                        let input_id = format!("in-{}", outbound.next_static());
                        let result = if is_new {
                            serde_json::json!({
                                "type": "createSession",
                                "sessionId": session_id,
                                "input": { "delivery": "startNow", "inputId": input_id },
                            })
                        } else {
                            serde_json::json!({
                                "type": "inputAccepted",
                                "delivery": "startNow",
                                "inputId": input_id,
                            })
                        };
                        outbound.result(
                            id,
                            serde_json::json!({
                                "commandId": command_id,
                                "status": "accepted",
                                "revisionAtDecision": 0,
                                "result": result,
                            }),
                        );

                        // run the turn on its own thread; notifications stream
                        let outbound = outbound.clone();
                        let topic = format!("conversation/{session_id}");
                        let sessions_guard = sessions.lock().unwrap();
                        let projection = sessions_guard.get(&session_id).cloned().expect("just inserted");
                        drop(sessions_guard);
                        let cwd2 = cwd.clone();
                        let sessions_dir2 = sessions_dir.clone();
                        let outbound2 = Arc::clone(&outbound);
                        let broadcast = Arc::new(
                            move |m: &str, p: serde_json::Value| outbound2.notification(m, p),
                        );
                        let err_topic = topic.clone();
                        let factory = demo_sampler_factory(cwd.clone());
                        let stop = Arc::new(AtomicBool::new(false));
                        let bridge = Arc::new(SurfaceApprovalChannel::new(Arc::clone(&stop)));
                        let session_ctx = {
                            let mut guard = contexts.lock().unwrap();
                            Arc::clone(guard.entry(session_id.clone()).or_insert_with(|| {
                                Arc::new(Mutex::new(okra_compaction::SessionContext::default()))
                            }))
                        };
                        let turn_ordinal = {
                            let mut ordinals = session_turn_ordinals.lock().unwrap();
                            let next = ordinals.get(&session_id).copied().unwrap_or(0);
                            ordinals.insert(session_id.clone(), next + 1);
                            next
                        };
                        let session_checkpoints = Arc::clone(&checkpoints);
                        if let Err(e) = run_turn_streaming(
                            broadcast,
                            topic,
                            session_id,
                            cwd2,
                            sessions_dir2,
                            text,
                            projection,
                            None,
                            stop,
                            &factory,
                            bridge,
                            true,
                            Vec::new(),
                            None,
                            &Mutex::new(std::collections::BTreeMap::new()),
                            &Arc::new(Mutex::new(ComputerConsent::default())),
                            Some(session_ctx),
                            Some(session_checkpoints),
                            turn_ordinal,
                            (okra_policy::lattice::MediationPolicy::FirstResponder, None),
                            None,
                            None,
                            None,
                            None,
                            None,
                            None,
                        ) {
                            outbound.notification(
                                "v4/error",
                                serde_json::json!({ "topic": err_topic, "message": e }),
                            );
                        }
                    }
                    "stop" => {
                        // stdio bridge turns run synchronously on this loop,
                        // so a stop can only ever arrive while idle — accept
                        // it honestly (no live turn to cancel)
                        outbound.result(
                            id,
                            serde_json::json!({
                                "commandId": command_id,
                                "status": "accepted",
                                "revisionAtDecision": 0,
                                "result": { "type": "stopIdle" },
                            }),
                        );
                    }
                    other => {
                        outbound.result(
                            id,
                            serde_json::json!({
                                "commandId": command_id,
                                "status": "rejected",
                                "reasonCode": "okra.g0.unsupportedCommand",
                                "message": format!("okra G0 daemon does not implement command `{other}`"),
                                "revisionAtDecision": 0,
                            }),
                        );
                    }
                }
            }
            _ => {
                outbound.result(id, serde_json::json!({ "error": format!("unknown method {method}") }));
            }
        }
    }
    std::process::exit(0);
}

/// Offline starter-scene catalog served at GET /scenes (client-scenes):
/// the prompt-starter cards a fresh surface shows before any server call.
pub fn starter_scene_catalog() -> okra_host::client_scenes::ClientSceneCatalog {
    use okra_host::client_scenes::{SceneConfig, SceneItem, SceneOption};
    
    use std::collections::BTreeMap;

    fn localized_text(en: &str) -> BTreeMap<String, String> {
        BTreeMap::from([("en".to_string(), en.to_string())])
    }

    let mut catalog = okra_host::client_scenes::ClientSceneCatalog::new();
    let mut options: BTreeMap<String, SceneOption> = BTreeMap::new();
    let mut contents: std::collections::BTreeMap<String, String> = std::collections::BTreeMap::new();
    contents.insert("en".into(), "Depth".into());
    let mut items = Vec::new();
    for (id, en) in [("overview", "Overview"), ("deep", "Deep dive")] {
        items.push(SceneItem {
            id: id.into(),
            item_type: "option".into(),
            contents: localized_text(en),
            descs: BTreeMap::new(),
            labels: BTreeMap::new(),
            on_finish: None,
            img: Some("layers".into()),
        });
    }
    options.insert(
        "depth".into(),
        SceneOption {
            id: "depth".into(),
            option_type: "select".into(),
            contents: localized_text("How deep should I go?"),
            prompts: BTreeMap::new(),
            items,
            refer: None,
            cascades: BTreeMap::new(),
            templates: BTreeMap::new(),
        },
    );
    catalog.register(SceneConfig {
        namespace: "okra".into(),
        scene: "repo-explain".into(),
        options,
        created_at: None,
        updated_at: None,
    });
    catalog
}

#[cfg(test)]
mod replay_fold_tests {
    use super::{rows_from_kernel_events, };
    use okra_kernel as kernel;

    fn ev(seq: u64, ty: &str, data: serde_json::Value) -> kernel::SessionEvent {
        let mut e = kernel::make_event(ty, data, || 1_789_510_400_123.0);
        e.seq = seq;
        e
    }

    #[test]
    fn tool_result_output_attaches_to_its_call_row() {
        let events = vec![
            ev(0, "tool/call", serde_json::json!({"callId": "c1", "tool": "read_file"})),
            ev(1, "tool/result", serde_json::json!({
                "callId": "c1", "isError": false,
                "output": "file contents here"
            })),
        ];
        let (rows, _) = rows_from_kernel_events(&events).unwrap();
        let row = rows.iter().find(|r| r["kind"] == "toolCall").unwrap();
        assert_eq!(row["output"]["text"], "file contents here");
        assert_eq!(row["status"], "success");
    }

    #[test]
    fn tool_result_error_message_comes_from_output() {
        let events = vec![
            ev(0, "tool/call", serde_json::json!({"callId": "c1", "tool": "write_file"})),
            ev(1, "tool/result", serde_json::json!({
                "callId": "c1", "isError": true,
                "output": "executor error: permission denied"
            })),
        ];
        let (rows, _) = rows_from_kernel_events(&events).unwrap();
        let row = rows.iter().find(|r| r["kind"] == "toolCall").unwrap();
        assert_eq!(row["status"], "error");
        assert_eq!(row["error"]["message"], "executor error: permission denied");
        assert_eq!(row["output"]["text"], "executor error: permission denied");
    }

    #[test]
    fn pre_output_logs_replay_as_status_only_cards() {
        // a log written before `output` landed: callId + isError only
        let events = vec![
            ev(0, "tool/call", serde_json::json!({"callId": "old", "tool": "list_dir"})),
            ev(1, "tool/result", serde_json::json!({"callId": "old", "isError": true})),
            ev(2, "tool/result", serde_json::json!({"callId": "old", "isError": false})),
        ];
        let (rows, _) = rows_from_kernel_events(&events).unwrap();
        let row = rows.iter().find(|r| r["kind"] == "toolCall").unwrap();
        assert!(row.get("output").is_none(), "no invented output: {row}");
        // last result wins: the successful one clears the error status
        assert_eq!(row["status"], "success");
        assert!(row.get("error").is_none());
    }
}
