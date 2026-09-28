//! `okra serve --acp` — the ACP gateway (MASTER-PLAN §3 #57, G4 remainder):
//! okra as an Agent Client Protocol agent over stdio, so an editor (Zed et
//! al.) drives the same daemon the browser/TUI/NDJSON surfaces drive.
//!
//! Wire: JSON-RPC 2.0, one message per line (newline-delimited) on stdin/
//! stdout. Implemented per agentclientprotocol.com v1:
//! - `initialize` → negotiate protocolVersion (we support 1), advertise
//!   agentCapabilities (loadSession=false v1) + agentInfo
//! - `session/new` → a kernel-backed session in the daemon cwd
//! - `session/prompt` → runs the turn on a worker thread (the reader keeps
//!   consuming stdin so mid-turn `session/cancel` arrives immediately);
//!   the reply carries the stopReason
//! - `session/cancel` → flips the session's stop flag; the turn aborts at
//!   the next step boundary as Cancelled(UserRequested) (the same seam the
//!   web `stop` command uses) and the prompt reply reports "cancelled" —
//!   honest, because the client asked for it
//! - session/update variants emitted: agent_message_chunk, tool_call,
//!   tool_call_update (status completed|failed)
//!
//! Editor parity note: this seam speaks ACP only; the workbench-internal
//! protocol remains zcode-v4 (day-1 decision, PORT-TO-RUST §7).

use std::io::{BufRead, Write};
use std::sync::{Arc, Mutex};

use okra_agent_core::loop_::{Agent, LoopEvent};
use okra_agent_core::turn::TurnOutcome;
use okra_kernel as kernel;
use okra_policy::approval::{ApprovalPolicy, ApprovalService};
use okra_tools::Registry;

use crate::demo_sampler::DemoPlanner;
use crate::serve::uuid_v4;

const ACP_PROTOCOL_VERSION: u64 = 1;

struct AcpOutbound {
    out: Mutex<std::io::Stdout>,
}

impl AcpOutbound {
    fn send(&self, value: serde_json::Value) {
        let mut out = self.out.lock().unwrap();
        let mut line = serde_json::to_vec(&value).unwrap_or_default();
        line.push(b'\n');
        let _ = out.write_all(&line);
        let _ = out.flush();
    }

    fn result(&self, id: &serde_json::Value, result: serde_json::Value) {
        self.send(serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": result }));
    }

    fn error(&self, id: &serde_json::Value, code: i64, message: &str) {
        self.send(serde_json::json!({
            "jsonrpc": "2.0", "id": id,
            "error": { "code": code, "message": message }
        }));
    }

    fn notification(&self, method: &str, params: serde_json::Value) {
        self.send(serde_json::json!({ "jsonrpc": "2.0", "method": method, "params": params }));
    }
}

#[derive(Default)]
struct AcpState {
    /// session/new allocations live for the daemon's lifetime; ACP session
    /// ids are opaque to the client
    sessions: Mutex<std::collections::HashMap<String, AcpSession>>,
    /// stop flag per RUNNING session — session/cancel flips it (the same
    /// external-stop seam the web surface's stop command uses)
    stop_flags: Mutex<std::collections::BTreeMap<String, Arc<std::sync::atomic::AtomicBool>>>,
    /// sessions with a turn in flight; a second prompt on one is a client
    /// bug and is refused honestly instead of interleaving turns
    running: Mutex<std::collections::BTreeSet<String>>,
}

struct AcpSession {
    kernel_id: String,
    /// The workspace this session operates on: session/new's `cwd` when the
    /// client provides a real directory (ACP: all paths absolute), else the
    /// daemon cwd.
    session_cwd: std::path::PathBuf,
}

/// `okra serve --acp`: JSON-RPC/ACP loop over stdin/stdout.
pub fn serve_acp(cwd: std::path::PathBuf, sessions_dir: std::path::PathBuf) -> ! {
    let outbound = Arc::new(AcpOutbound { out: Mutex::new(std::io::stdout()) });
    let state = Arc::new(AcpState::default());
    eprintln!("[serve-acp] ACP agent on stdio (protocol v{ACP_PROTOCOL_VERSION})");
    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let Ok(msg) = serde_json::from_str::<serde_json::Value>(&line) else {
            outbound.error(&serde_json::json!(-32700), -32700, "parse error: not valid JSON");
            continue;
        };
        let id = msg.get("id").cloned().unwrap_or(serde_json::Value::Null);
        let method = msg["method"].as_str().unwrap_or_default().to_string();
        let params = msg.get("params").cloned().unwrap_or(serde_json::Value::Null);
        if msg.get("method").is_some() && msg.get("id").is_none() {
            // notification (no id): session/cancel
            handle_notification(&state, &method, &params);
            continue;
        }
        match method.as_str() {
            "initialize" => {
                let requested = params["protocolVersion"].as_u64();
                // if we support the requested version, echo it; otherwise
                // respond with the latest WE support
                let agreed = match requested {
                    Some(v) if v == ACP_PROTOCOL_VERSION => v,
                    _ => ACP_PROTOCOL_VERSION,
                };
                outbound.result(
                    &id,
                    serde_json::json!({
                        "protocolVersion": agreed,
                        "agentCapabilities": {
                            "loadSession": false,
                            "promptCapabilities": {}
                        },
                        "agentInfo": {
                            "name": "okra",
                            "title": "okra",
                            "version": env!("CARGO_PKG_VERSION")
                        },
                        "authMethods": []
                    }),
                );
            }
            "authenticate" => {
                // no auth methods advertised → nothing to authenticate
                outbound.error(&id, -32601, "no auth methods available");
            }
            "session/new" => {
                let session_id = format!("acp-{}", uuid_v4());
                let kernel_id = format!("session-{session_id}");
                // the client's workspace wins when it is a real directory —
                // the editor's project, not the daemon's launch dir
                let session_cwd = match params["cwd"].as_str() {
                    Some(c) if std::path::Path::new(c).is_dir() => std::path::PathBuf::from(c),
                    _ => cwd.clone(),
                };
                let header = kernel::SessionHeader {
                    version: kernel::SESSION_FORMAT_VERSION,
                    id: kernel_id.clone(),
                    created_at: now_ms(),
                    cwd: session_cwd.to_string_lossy().into_owned(),
                    parent_session: None,
                    is_seeded: false,
                };
                if let Err(e) = kernel::SessionHandle::create(&sessions_dir, &header) {
                    outbound.error(&id, -32000, &format!("create session: {e}"));
                    continue;
                }
                state.sessions.lock().unwrap().insert(
                    session_id.clone(),
                    AcpSession { kernel_id, session_cwd },
                );
                outbound.result(&id, serde_json::json!({ "sessionId": session_id }));
            }
            "session/prompt" => {
                let session_id = params["sessionId"].as_str().unwrap_or_default().to_string();
                let session = state.sessions.lock().unwrap().get(&session_id)
                    .map(|s| (s.kernel_id.clone(), s.session_cwd.clone()));
                let Some((kernel_id, session_cwd)) = session else {
                    outbound.error(&id, -32002, &format!("unknown session: {session_id}"));
                    continue;
                };
                // text blocks only (promptCapabilities: text is mandatory;
                // we advertise nothing else)
                let mut text = String::new();
                if let Some(blocks) = params["prompt"].as_array() {
                    for block in blocks {
                        if block["type"] == "text" {
                            if !text.is_empty() {
                                text.push('\n');
                            }
                            text.push_str(block["text"].as_str().unwrap_or_default());
                        }
                    }
                }
                if text.is_empty() {
                    outbound.error(&id, -32602, "prompt carries no text content");
                    continue;
                }
                if state.running.lock().unwrap().contains(&session_id) {
                    outbound.error(&id, -32000, &format!("turn already running on session {session_id}"));
                    continue;
                }
                let stop_flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
                state
                    .stop_flags
                    .lock()
                    .unwrap()
                    .insert(session_id.clone(), Arc::clone(&stop_flag));
                state.running.lock().unwrap().insert(session_id.clone());
                // worker thread: the reader loop stays on stdin so a mid-turn
                // session/cancel notification is seen while the turn runs
                let oc = Arc::clone(&outbound);
                let st = Arc::clone(&state);
                let sid = session_id.clone();
                let session_cwd_t = session_cwd.clone();
                let sessions_dir_t = sessions_dir.clone();
                let kernel_id_t = kernel_id.clone();
                let text_t = text.clone();
                std::thread::spawn(move || {
                    let (oc_n, sid_n) = (Arc::clone(&oc), sid.clone());
                    let mut notify = move |update: serde_json::Value| {
                        oc_n.notification(
                            "session/update",
                            serde_json::json!({ "sessionId": sid_n, "update": update }),
                        );
                    };
                    match acp_turn(
                        &session_cwd_t,
                        &sessions_dir_t,
                        &kernel_id_t,
                        &text_t,
                        &stop_flag,
                        &mut notify,
                    ) {
                        Ok(stop_reason) => oc.result(
                            &id,
                            serde_json::json!({ "stopReason": stop_reason }),
                        ),
                        Err(e) => oc.error(&id, -32000, &format!("turn failed: {e}")),
                    }
                    st.stop_flags.lock().unwrap().remove(&sid);
                    st.running.lock().unwrap().remove(&sid);
                });
            }
            other => outbound.error(&id, -32601, &format!("method not found: {other}")),
        }
    }
    std::process::exit(0);
}

fn handle_notification(state: &AcpState, method: &str, params: &serde_json::Value) {
    if method == "session/cancel"
        && let Some(session_id) = params["sessionId"].as_str()
    {
        let flag = state.stop_flags.lock().unwrap().get(session_id).cloned();
        match flag {
            Some(flag) => {
                flag.store(true, std::sync::atomic::Ordering::Relaxed);
                eprintln!("[serve-acp] session/cancel delivered to running turn: {session_id}");
            }
            None => {
                // cancel with no turn in flight: nothing to abort; logged
                // so the wire shows it was seen, not dropped silently
                eprintln!("[serve-acp] session/cancel ignored (no running turn): {session_id}");
            }
        }
    }
}

fn now_ms() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as f64)
        .unwrap_or(0.0)
}

fn tool_kind(name: &str) -> &'static str {
    match name {
        "read_file" | "list_dir" => "read",
        "write_file" | "edit_file" => "edit",
        _ => "other",
    }
}

/// One ACP turn: the same setup as the v4 seam (read_file/list_dir through
/// the policy pipeline, kernel-backed session, demo planner) with LoopEvents
/// mapped to ACP `session/update` notifications. `stop` is the surface-held
/// flag session/cancel flips; the loop observes it at every step boundary.
fn acp_turn(
    cwd: &std::path::Path,
    sessions_dir: &std::path::Path,
    kernel_id: &str,
    input_text: &str,
    stop: &Arc<std::sync::atomic::AtomicBool>,
    notify: &mut dyn FnMut(serde_json::Value),
) -> Result<String, String> {
    let mut registry = Registry::new();
    let rf = okra_tools::builtins::read_file_tool(cwd.to_path_buf());
    let entry = rf.entry();
    registry
        .register(okra_tools::ErasedTool::simple(
            entry,
            vec![okra_tools::ResourceAccess::read_file("*")],
            move |args| rf.execute(args, None),
        ))
        .map_err(|e| format!("register read_file: {e}"))?;
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
        .map_err(|e| format!("register list_dir: {e}"))?;
    let approvals = ApprovalService::new(ApprovalPolicy::Never);
    let executor = okra_agent_core::loop_::PolicyToolExecutor::new(registry, approvals);

    let kernel_session =
        kernel::SessionHandle::open(sessions_dir, kernel_id, kernel::SessionAccess::Write)
            .map_err(|e| format!("open kernel session: {e}"))?;

    let config = okra_agent_core::loop_::AgentConfig {
        max_steps: 8,
        unattended: true,
        ..Default::default()
    };
    let sampler = DemoPlanner::new(cwd.to_path_buf());
    let mut agent = Agent::new(config, Arc::new(sampler), Box::new(executor), kernel_session);
    agent.set_stop_flag(Arc::clone(stop));

    let outcome = agent.run_turn(input_text, &mut |ev: LoopEvent| {
        match ev {
            LoopEvent::TextDelta { text } => notify(serde_json::json!({
                "sessionUpdate": "agent_message_chunk",
                "content": { "type": "text", "text": text }
            })),
            LoopEvent::ToolCallStarted { id, name } => notify(serde_json::json!({
                "sessionUpdate": "tool_call",
                "toolCallId": id,
                "title": name,
                "kind": tool_kind(&name),
                "status": "in_progress"
            })),
            LoopEvent::ToolCallFinished { id, name, is_error, output } => {
                let _ = name;
                let status = if is_error { "failed" } else { "completed" };
                notify(serde_json::json!({
                    "sessionUpdate": "tool_call_update",
                    "toolCallId": id,
                    "status": status,
                    "content": [
                        { "type": "content", "content": { "type": "text", "text": output } }
                    ],
                    "rawOutput": { "text": output }
                }));
            }
            _ => {}
        }
    });

    Ok(match outcome {
        Ok(TurnOutcome::Completed { .. }) | Ok(TurnOutcome::StationarityEnded) => "end_turn".to_string(),
        Ok(TurnOutcome::MaxTurnsReached { .. }) => "max_turn_requests".to_string(),
        // the client's session/cancel (or a rare internal abort) — the turn
        // really was cancelled, so this stop reason is honest
        Ok(TurnOutcome::Cancelled { .. }) => "cancelled".to_string(),
        Err(_) => return Err("turn errored".into()),
    })
}
