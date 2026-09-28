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
use okra_policy::approval::{ApprovalPolicy, ApprovalService};
use okra_providers::Sampler;
use okra_tools::Registry;

use crate::demo_sampler::DemoPlanner;

/// Builds a fresh sampler per turn (samplers may carry per-turn state —
/// e.g. the demo planner's step counter — so they are never shared).
pub type SamplerFactory = Arc<dyn Fn() -> Arc<dyn Sampler> + Send + Sync>;

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

/// Run one full turn against the state's sampler factory, streaming row
/// updates into the projection and notifying the attached surface after
/// each change. `steering` (G4): when present, drained at every projection
/// update — a steered message becomes a user row on the SAME live session,
/// from whichever surface submitted it. `stop` (G4): a surface can flip
/// this flag mid-turn; the turn cancels at the next step boundary
/// (Cancelled(UserRequested)) and recovers through the standard path.
#[allow(clippy::too_many_arguments)]
pub fn run_turn_streaming(
    notify: &mut dyn FnMut(&str, serde_json::Value),
    topic: String,
    session_id: String,
    cwd: std::path::PathBuf,
    sessions_dir: std::path::PathBuf,
    input_text: String,
    turn_row_lock: Arc<Mutex<SessionProjection>>,
    steering: Option<Arc<Mutex<std::collections::VecDeque<String>>>>,
    stop: Arc<AtomicBool>,
    sampler_factory: &SamplerFactory,
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
        notify(
            "v4/projection",
            projection_notification(&topic, &p)["params"].clone(),
        );
        (Arc::new(Mutex::new(None::<u64>)), turn_id)
    };

    // 2. build the agent (full CLI tool plane + policy ceiling, sampler
    // from the state's factory — real provider when `--provider` was given)
    let registry = build_registry(&cwd);
    let approvals = ApprovalService::new(ApprovalPolicy::Ask);
    let mut executor = okra_agent_core::loop_::PolicyToolExecutor::new(registry, approvals);
    // Serve turns are unattended web turns, same ceiling as headless CLI
    // runs (arg-hash grants still record every approval).
    executor.ceiling = ToolApprovalCeiling::UnattendedAllowed;

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

    let config = okra_agent_core::loop_::AgentConfig { max_steps: 32, unattended: true, ..Default::default() };
    let sampler = (sampler_factory)();
    let mut agent = Agent::new(config, sampler, Box::new(executor), kernel_session);
    agent.set_stop_flag(stop);

    // 3. drive the turn, streaming LoopEvents into rows
    let topic_for_events = topic.clone();
    let row_lock = turn_row_lock.clone();
    let assistant_slot = assistant_row_slot.clone();
    let mut tool_row_by_call: std::collections::HashMap<String, u64> = Default::default();
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
            LoopEvent::ToolCallStarted { id, name } => {
                let (row_id, base, _) = p.base_row(&turn_id);
                let mut r = base;
                r["kind"] = serde_json::json!("toolCall");
                r["toolCallId"] = serde_json::json!(id);
                r["toolName"] = serde_json::json!(name);
                r["status"] = serde_json::json!("running");
                r["inputText"] = serde_json::json!("");
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
        notify(
            "v4/projection",
            projection_notification(&topic_for_events, &p)["params"].clone(),
        );
    });

    // 4. finalize rows + control (honest states: a user stop is an
    // interrupted turn, not an error)
    {
        let mut p = turn_row_lock.lock().unwrap();
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
        notify(
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
                        let mut notify = |m: &str, p: serde_json::Value| {
                            outbound.notification(m, p)
                        };
                        let err_topic = topic.clone();
                        let factory = demo_sampler_factory(cwd.clone());
                        let stop = Arc::new(AtomicBool::new(false));
                        if let Err(e) = run_turn_streaming(
                            &mut notify,
                            topic,
                            session_id,
                            cwd2,
                            sessions_dir2,
                            text,
                            projection,
                            None,
                            stop,
                            &factory,
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
