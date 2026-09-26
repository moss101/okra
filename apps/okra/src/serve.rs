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
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use okra_agent_core::loop_::{Agent, LoopEvent};
use okra_agent_core::turn::TurnOutcome;
use okra_kernel as kernel;
use okra_policy::approval::{ApprovalPolicy, ApprovalService};
use okra_tools::Registry;

use crate::demo_sampler::DemoPlanner;

/// One live session projection.
pub struct SessionProjection {
    session_id: String,
    rows: Vec<serde_json::Value>,
    control: serde_json::Value,
    seq: u64,
    revision: u64,
    next_row_id: u64,
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
        }
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

/// Run one full turn against the demo planner, streaming row updates into
/// the projection and notifying the attached surface after each change.
/// `steering` (G4): when present, drained at every projection update —
/// a steered message becomes a user row on the SAME live session, from
/// whichever surface submitted it.
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
) -> Result<TurnOutcome, String> {
    let steering_rx = steering;
    // 1. turnHeader (running) + userInput rows
    let (assistant_row_slot, turn_id) = {
        let mut p = turn_row_lock.lock().unwrap();
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

    // 2. build the agent (demo planner + read_file/list_dir through policy)
    let mut registry = Registry::new();
    let rf = okra_tools::builtins::read_file_tool(cwd.clone());
    let entry = rf.entry();
    registry
        .register(okra_tools::ErasedTool::simple(
            entry,
            vec![okra_tools::ResourceAccess::read_file("*")],
            move |args| rf.execute(args, None),
        ))
        .map_err(|e| format!("register read_file: {e}"))?;
    let ld = okra_tools::builtins::list_dir_tool(cwd.clone());
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
        .map_err(|e| format!("register list_dir: {e}"))?;
    let approvals = ApprovalService::new(ApprovalPolicy::Never);
    let executor = okra_agent_core::loop_::PolicyToolExecutor::new(registry, approvals);

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

    let config = okra_agent_core::loop_::AgentConfig { max_steps: 8, unattended: true, ..Default::default() };
    let sampler = DemoPlanner::new(cwd.clone());
    let mut agent = Agent::new(config, Arc::new(sampler), Box::new(executor), kernel_session);

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

    // 4. finalize rows + control
    {
        let mut p = turn_row_lock.lock().unwrap();
        let assistant_state = match &outcome {
            Ok(TurnOutcome::Completed { stop: okra_agent_core::turn::CompletedStop::MaxTokens, .. }) => "interrupted",
            Ok(TurnOutcome::Completed { .. }) => "complete",
            _ => "failed",
        };
        if let Some(row_id) = *assistant_slot.lock().unwrap()
            && let Some(row) = p.rows.iter_mut().find(|r| r["rowId"].as_u64() == Some(row_id)) {
                row["state"] = serde_json::json!(assistant_state);
            }
        // finalize the running turnHeader (first turnHeader row still running)
        if let Some(row) = p.rows.iter_mut().find(|r| {
            r["kind"] == "turnHeader" && r["state"] == "running"
        }) {
            row["state"] = serde_json::json!(match &outcome {
                Ok(TurnOutcome::Completed { .. }) => "completedSuccess",
                Ok(TurnOutcome::StationarityEnded) => "completedInterrupted",
                _ => "failed",
            });
            row["endedAt"] = serde_json::json!(now_ms());
        }
        let (phase, ended) = match &outcome {
            Ok(TurnOutcome::Completed { .. }) | Ok(TurnOutcome::StationarityEnded) => {
                ("completedSuccess", true)
            }
            Ok(TurnOutcome::MaxTurnsReached { .. }) => ("completedInterrupted", true),
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
    outcome
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
                        if let Err(e) = run_turn_streaming(
                            &mut notify,
                            topic,
                            session_id,
                            cwd2,
                            sessions_dir2,
                            text,
                            projection,
                            None,
                        ) {
                            outbound.notification(
                                "v4/error",
                                serde_json::json!({ "topic": err_topic, "message": e }),
                            );
                        }
                    }
                    "stop" => {
                        // G0: turns are short demo turns; stop is accepted as a no-op
                        outbound.result(
                            id,
                            serde_json::json!({
                                "commandId": command_id,
                                "status": "accepted",
                                "revisionAtDecision": 0,
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
