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
    ApprovalChannel, ApprovalOutcome, ApprovalPolicy, ApprovalRequest, ApprovalService,
};
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
pub fn build_registry(cwd: &Path) -> Registry {
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
    let wf = okra_tools::builtins::ErasedWriteFile::new(cwd.to_path_buf());
    let entry = wf.entry();
    registry
        .register(okra_tools::ErasedTool::simple(
            entry,
            vec![okra_tools::ResourceAccess::write_file("*")],
            move |args| wf.execute(args),
        ))
        .expect("write_file registers once");
    let ef = okra_tools::builtins::ErasedEditFile::new(cwd.to_path_buf());
    let entry = ef.entry();
    registry
        .register(okra_tools::ErasedTool::simple(
            entry,
            vec![okra_tools::ResourceAccess::file(
                okra_tools::FileAccessOperation::Readwrite,
                "*",
            )],
            move |args| ef.execute(args),
        ))
        .expect("edit_file registers once");
    registry
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
pub struct SurfaceApprovalChannel {
    pending: Mutex<Vec<PendingApproval>>,
    answers: Mutex<std::collections::HashMap<String, ApprovalOutcome>>,
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
    pub fn resolve(&self, approval_id: &str, allow: bool) -> bool {
        let outcome = if allow {
            ApprovalOutcome::AllowedOnce
        } else {
            ApprovalOutcome::Rejected
        };
        let known = {
            let mut answers = self.answers.lock().unwrap();
            answers.insert(approval_id.to_string(), outcome);
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
}

impl ApprovalChannel for SurfaceApprovalChannel {
    fn answer(&self, request: &ApprovalRequest) -> Option<ApprovalOutcome> {
        self.pending.lock().unwrap().push(PendingApproval {
            id: request.id.clone(),
            tool_name: request.tool_name.clone(),
            call_id: request.call_id.clone(),
            args_json: request.args_json.clone(),
            asked_at: now_ms(),
        });
        loop {
            // bounded waits so a stop flip is honoured mid-approval
            let answers = self.answers.lock().unwrap();
            let (mut answers, timeout) = self
                .wake
                .wait_timeout(answers, std::time::Duration::from_millis(250))
                .unwrap();
            if let Some(outcome) = answers.remove(&request.id) {
                drop(answers);
                self.pending
                    .lock()
                    .unwrap()
                    .retain(|p| p.id != request.id);
                self.wake.notify_all();
                return Some(outcome);
            }
            drop(answers);
            if self.stop.load(Ordering::Relaxed) {
                self.pending
                    .lock()
                    .unwrap()
                    .retain(|p| p.id != request.id);
                self.wake.notify_all();
                return Some(ApprovalOutcome::Cancelled);
            }
            let _ = timeout;
        }
    }
}

/// The projection control patch the workbench reads: pending approvals fold
/// into `control.awaitingApproval` and flip the phase while the turn is
/// paused on the bridge.
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
        return; // unchanged — no frame churn
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
#[allow(clippy::too_many_arguments)]
pub fn run_turn_streaming(
    broadcast: BroadcastFn,
    topic: String,
    session_id: String,
    cwd: std::path::PathBuf,
    sessions_dir: std::path::PathBuf,
    input_text: String,
    turn_row_lock: Arc<Mutex<SessionProjection>>,
    steering: Option<Arc<Mutex<std::collections::VecDeque<String>>>>,
    stop: Arc<AtomicBool>,
    sampler_factory: &SamplerFactory,
    approvals: Arc<SurfaceApprovalChannel>,
    unattended: bool,
) -> Result<TurnOutcome, String> {
    let steering_rx = steering;
    // 1. turnHeader (running) + userInput rows
    let (assistant_row_slot, turn_id) = {
        let mut p = turn_row_lock.lock().unwrap();
        p.set_title_if_empty(&input_text);
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
    let registry = build_registry(&cwd);
    let mut approval_service = ApprovalService::new(ApprovalPolicy::Ask);
    if !unattended {
        // attended surfaces: the bridge IS the approval waterfall — the
        // turn pauses on it until the workbench resolves (or stops)
        approval_service.add_channel(Box::new(SharedBridge(Arc::clone(&approvals))));
    }
    let mut executor = okra_agent_core::loop_::PolicyToolExecutor::new(registry, approval_service);
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
    let config = okra_agent_core::loop_::AgentConfig { max_steps, unattended: true, ..Default::default() };
    let sampler = (sampler_factory)();
    let mut agent = Agent::new(config, sampler, Box::new(executor), kernel_session);
    agent.set_stop_flag(stop);

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

    let outcome = agent.run_turn(&input_text, &mut |ev: LoopEvent| {
        let mut p = row_lock.lock().unwrap();
        // G4 steering drain: steered text becomes a user row on the live
        // session (from whichever surface submitted it)
        while let Some(steer_text) = steering_rx
            .as_ref()
            .and_then(|q| q.lock().ok().and_then(|mut q| q.pop_front()))
        {
            let (_row_id, base, _) = p.base_row(&turn_id);
            let mut r = base;
            r["kind"] = serde_json::json!("userInput");
            r["text"] = serde_json::json!(format!("[steered] {steer_text}"));
            r["origin"] = serde_json::json!("realUser");
            p.upsert_row(r);
            p.revision += 1;
        }
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
    });

    wd_done.store(true, Ordering::Relaxed);
    let _ = wd_handle.join();

    // 4. finalize rows + control (honest states: a user stop is an
    // interrupted turn, not an error)
    {
        let mut p = turn_row_lock.lock().unwrap();
        p.control["awaitingApproval"] = serde_json::json!([]);
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
                rows.push(serde_json::json!({
                    "rowId": row_id,
                    "turnId": format!("t{current_turn}"),
                    "createdAt": ev.time,
                    "createdAtSeq": ev.seq,
                    "kind": "userInput",
                    "text": if steered { format!("[steered] {text}") } else { text.to_string() },
                    "origin": "realUser",
                }));
            }
            "assistant/message" => {
                let row_id = next_row_id;
                next_row_id += 1;
                rows.push(serde_json::json!({
                    "rowId": row_id,
                    "turnId": format!("t{current_turn}"),
                    "createdAt": ev.time,
                    "createdAtSeq": ev.seq,
                    "kind": "assistantText",
                    "text": data["text"].as_str().unwrap_or_default(),
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
    }))
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
