//! `okra serve --tcp ADDR` — the G4 multi-surface steering seam
//! (MASTER-PLAN §4 G4 groundwork).
//!
//! Loopback-only: many TCP clients attach to ONE daemon; projections
//! fan out to ALL surfaces, and any surface may steer the running turn.

use std::collections::{BTreeMap, VecDeque};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use okra_host::terminal::TerminalSession;

use crate::serve::{
    self, starter_scene_catalog, run_turn_streaming, uuid_v4, SamplerFactory,
    SessionProjection, SurfaceApprovalChannel,
};

pub struct SurfaceWriter {
    inner: Arc<Mutex<TcpStream>>,
    /// SSE framing: broadcasts go out as `data: {json}\n\n` (browser
    /// EventSource surfaces).
    pub sse: bool,
}

impl SurfaceWriter {
    pub fn send_line(&self, line: &[u8]) -> bool {
        let mut w = match self.inner.lock() { Ok(w) => w, Err(_) => return false };
        let payload: Vec<u8>;
        let bytes: &[u8] = if self.sse {
            let mut framed = b"data: ".to_vec();
            framed.extend_from_slice(line);
            framed.extend_from_slice(b"\n\n");
            payload = framed;
            &payload
        } else {
            line
        };
        w.write_all(bytes).and_then(|_| w.flush()).is_ok()
    }
}

/// The steering queue element: text + attachment paths (N0017 follow-up).
pub type SteeringChannel = Arc<Mutex<VecDeque<crate::serve::SteeredInput>>>;

pub struct TcpServeState {
    pub cwd: std::path::PathBuf,
    pub sessions_dir: std::path::PathBuf,
    /// The hello payload facts (device identity + capability list).
    pub config: okra_host::client_info::ClientConfig,
    pub sessions: Mutex<BTreeMap<String, Arc<Mutex<SessionProjection>>>>,
    pub steering: Mutex<BTreeMap<String, SteeringChannel>>,
    pub writers: Mutex<Vec<SurfaceWriter>>,
    /// G4 bookkeeping: every attached surface is registered here with a
    /// kind + capabilities, heartbeated per frame, and detached on exit.
    pub surfaces: Mutex<okra_host::surfaces::SurfaceRegistry>,
    /// Cross-session pub/sub: one session publishes, others poll.
    pub bus: Mutex<okra_host::broadcast::BroadcastBus>,
    /// Sessions with a live turn thread (G4 breadth): a command on a
    /// running session is STEERING — it queues onto the live turn instead
    /// of spawning a second parallel turn thread on the same projection
    /// (two writers raced rows/revision and interleaved control frames).
    pub running_turns: Mutex<std::collections::BTreeSet<String>>,
    /// Live stop flags keyed by session: the `stop` command flips the flag
    /// the turn thread installed; cancelled at the next step boundary.
    pub stop_flags: Mutex<BTreeMap<String, Arc<AtomicBool>>>,
    /// Live approval bridges keyed by session: `resolveApproval` answers
    /// the ask the workbench UI is showing.
    pub approval_bridges: Mutex<BTreeMap<String, Arc<SurfaceApprovalChannel>>>,
    /// Workbench terminals (N0012): PTY sessions keyed by id; each has an
    /// output-pump thread appending to a bounded scrollback the SSE
    /// endpoint streams incrementally.
    pub terminals: Mutex<BTreeMap<String, Arc<TermEntry>>>,
    /// Runtime MCP status from explicit probes (Tools tab).
    pub mcp_status: Mutex<BTreeMap<String, serde_json::Value>>,
    /// Per-turn sampler source (`--provider openai` → real network model;
    /// default → offline demo planner).
    pub sampler_factory: SamplerFactory,
    /// Human-readable sampler label for /health and the workbench top bar.
    pub sampler_label: String,
    next_static: std::sync::atomic::AtomicU64,
}

impl TcpServeState {
    pub fn new(
        cwd: std::path::PathBuf,
        sessions_dir: std::path::PathBuf,
        sampler_factory: SamplerFactory,
        sampler_label: String,
    ) -> Self {
        let home = okra_host::fsutil::home_dir().unwrap_or_else(|| cwd.clone());
        let config = okra_host::client_info::client_config(
            &home,
            &sessions_dir,
            env!("CARGO_PKG_VERSION"),
            vec![
                "ndjson".into(),
                "http+sse".into(),
                "steer".into(),
                "surfaces".into(),
            ],
        )
        .unwrap_or_else(|_| okra_host::client_info::ClientConfig {
            daemon: "okra".into(),
            protocol_version: 3,
            device_id: "unknown".into(),
            okra_version: env!("CARGO_PKG_VERSION").into(),
            sessions_dir: sessions_dir.clone(),
            capabilities: vec![],
        });
        TcpServeState {
            cwd,
            sessions_dir,
            config,
            sessions: Mutex::new(BTreeMap::new()),
            steering: Mutex::new(BTreeMap::new()),
            writers: Mutex::new(Vec::new()),
            surfaces: Mutex::new(okra_host::surfaces::SurfaceRegistry::new()),
            bus: Mutex::new(okra_host::broadcast::BroadcastBus::new(256)),
            running_turns: Mutex::new(std::collections::BTreeSet::new()),
            stop_flags: Mutex::new(BTreeMap::new()),
            approval_bridges: Mutex::new(BTreeMap::new()),
            terminals: Mutex::new(BTreeMap::new()),
            mcp_status: Mutex::new(BTreeMap::new()),
            sampler_factory,
            sampler_label,
            next_static: std::sync::atomic::AtomicU64::new(0),
        }
    }
    fn next_static(&self) -> u64 {
        self.next_static.fetch_add(1, Ordering::SeqCst)
    }
    pub fn broadcast_bytes(&self, line: &[u8]) {
        // NDJSON: every frame is newline-terminated
        let mut framed = line.to_vec();
        if framed.last() != Some(&b'\n') {
            framed.push(b'\n');
        }
        let mut writers = self.writers.lock().unwrap();
        let mut dead = Vec::new();
        for (i, w) in writers.iter().enumerate() {
            let ok = w.send_line(&framed);
            if !ok { dead.push(i); }
        }
        for i in dead.into_iter().rev() { writers.remove(i); }
    }
}

pub fn serve_tcp(state: Arc<TcpServeState>, listener: std::net::TcpListener) -> ! {
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let state = Arc::clone(&state);
        std::thread::spawn(move || handle_client(&state, stream));
    }
    unreachable!()
}

fn handle_client(state: &Arc<TcpServeState>, stream: TcpStream) {
    let read_half = match stream.try_clone() { Ok(s) => s, Err(_) => return };
    let write_half = match stream.try_clone() { Ok(s) => s, Err(_) => return };
    let own_writer = Arc::new(Mutex::new(stream));

    // HTTP upgrade check (browser breadth): the first byte of an HTTP
    // request is the method's first letter ('G'ET / 'P'OST); NDJSON frames
    // always start with '{'. Browsers cannot speak the line protocol, so
    // they branch here: GET /health, GET /sse/<session>, POST /command.
    // The branch owns the connection to the end of its lifecycle and
    // returns.
    //
    // The request bytes may still be in flight when connect completes, so
    // wait (bounded) for the first byte before deciding; then clear the
    // timeout so idle NDJSON surfaces are never dropped.
    {
        let _ = read_half.set_read_timeout(Some(Duration::from_millis(100)));
        let mut decided = false;
        let mut is_http = false;
        for _ in 0..40 {
            let mut peek = [0u8; 1];
            match read_half.peek(&mut peek) {
                Ok(n) if n >= 1 => {
                    is_http = peek[0] == b'G' || peek[0] == b'P';
                    decided = true;
                    break;
                }
                Ok(0) => return,
                Err(e)
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::TimedOut =>
                {
                    continue
                }
                Err(_) => return,
                // guards don't count for exhaustiveness; unreachable
                Ok(_) => return,
            }
        }
        let _ = read_half.set_read_timeout(None);
        if !decided {
            return;
        }
        if is_http {
            let _ = http_handle(state, write_half);
            return;
        }
    }

    // register this surface for broadcasts; the broadcaster prunes dead
    // writers when a surface disconnects
    state.writers.lock().unwrap().push(SurfaceWriter {
        inner: Arc::clone(&own_writer),
        sse: false,
    });
    // register in the surface registry (kind Cli: NDJSON line protocol)
    let surface_id = state
        .surfaces
        .lock()
        .unwrap()
        .attach(okra_host::surfaces::SurfaceKind::Cli, vec!["steer".into()])
        .map(|(id, _)| id)
        .unwrap_or_default();
    let respond = move |id: u64, result: serde_json::Value| {
        let mut w = own_writer.lock().unwrap();
        let _ = w.write_all(
            serde_json::to_vec(&serde_json::json!({ "id": id, "result": result })).unwrap_or_default().as_slice(),
        );
        let _ = w.write_all(b"\n");
        let _ = w.flush();
    };
    let reader = std::io::BufReader::new(read_half);
    for line in reader.lines().map_while(Result::ok) {
        if line.trim().is_empty() { continue; }
        {
            let mut surfaces = state.surfaces.lock().unwrap();
            surfaces.heartbeat(&surface_id);
            // leadership renewal rides the same heartbeat cadence
            surfaces.renew_leader(&surface_id);
        }
        let Ok(msg) = serde_json::from_str::<serde_json::Value>(&line) else { continue; };
        let id_field = msg["id"].as_u64().unwrap_or(0);
        let method = msg["method"].as_str().unwrap_or_default().to_string();
        let params = msg["params"].clone();
        match method.as_str() {
            "hello" => respond(
                id_field,
                serde_json::to_value(&state.config).unwrap_or_default(),
            ),
            "ping" => respond(id_field, serde_json::json!({"pong":true})),
            "surfaces/list" => {
                let surfaces = state.surfaces.lock().unwrap();
                let list: Vec<serde_json::Value> = surfaces
                    .list()
                    .iter()
                    .map(|s| serde_json::to_value(s).unwrap_or_default())
                    .collect();
                let leader = surfaces.leader().map(|(id, term)| {
                    serde_json::json!({ "surfaceId": id, "term": term })
                });
                respond(
                    id_field,
                    serde_json::json!({ "surfaces": list, "leader": leader }),
                );
            }
            "roster/claim" => {
                // G4 leader/roster: exactly one leading surface per daemon;
                // followers attach alongside (same projections, no parallel
                // leadership). Leadership releases on detach/expiry.
                let decision = state.surfaces.lock().unwrap().claim_leader(&surface_id);
                match decision {
                    Ok(d) => respond(id_field, serde_json::to_value(d).unwrap_or_default()),
                    Err(e) => respond(id_field, serde_json::json!({ "error": e.to_string() })),
                }
            }
            "v4/conversation/subscribe" => {
                let session_id = params["sessionId"].as_str().unwrap_or_default().to_string();
                state.steering.lock().unwrap().entry(session_id)
                    .or_insert_with(|| Arc::new(Mutex::new(VecDeque::new())));
                respond(id_field, serde_json::json!({
                    "ack": {"subscriptionId":format!("tcp-{}",state.next_static()),"mode":"snapshot","logEpoch":"0"}
                }));
            }
            "v4/steer" => {
                let session_id = params["sessionId"].as_str().unwrap_or_default().to_string();
                let text = params["text"].as_str().unwrap_or_default().to_string();
                let queued = state.steering.lock().unwrap().get(&session_id)
                    .map(|ch| ch.lock().unwrap().push_back(crate::serve::SteeredInput {
                        text, attachments: Vec::new(),
                    })).is_some();
                respond(id_field, serde_json::json!({"steered":queued}));
            }
            "v4/command" => {
                let envelope = &msg["params"]["envelope"];
                let reply = command_accept(state, envelope);
                respond(id_field, reply);
            }
            "broadcast/send" => {
                let from = params["fromSession"].as_str().unwrap_or_default().to_string();
                if from.is_empty() {
                    respond(id_field, serde_json::json!({"error":"fromSession required"}));
                } else {
                    state.surfaces.lock().unwrap().heartbeat(&surface_id);
                    let topic = params["topic"].as_str().unwrap_or("general").to_string();
                    let payload = params.get("payload").cloned().unwrap_or(serde_json::Value::Null);
                    let id = state.bus.lock().unwrap().publish(&topic, &from, payload);
                    respond(id_field, serde_json::json!({"broadcastId": id}));
                }
            }
            "broadcast/receive" => {
                let subscriber = params["sessionId"].as_str().unwrap_or_default().to_string();
                if subscriber.is_empty() {
                    respond(id_field, serde_json::json!({"error":"sessionId required"}));
                } else {
                    let topics: Vec<String> = params["topics"]
                        .as_array()
                        .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
                        .unwrap_or_default();
                    state
                        .bus
                        .lock()
                        .unwrap()
                        .subscribe(&subscriber, Some(topics));
                    let delivered =
                        state.bus.lock().unwrap().poll(&subscriber).unwrap_or_default();
                    let delivered: Vec<serde_json::Value> = delivered
                        .into_iter()
                        .map(|b| serde_json::to_value(b).unwrap_or_default())
                        .collect();
                    respond(id_field, serde_json::json!({"broadcasts": delivered}));
                }
            }
            _ => {}
        }
    }
    state.surfaces.lock().unwrap().detach(&surface_id);
}

// ---------------------------------------------------------------------------
// HTTP + SSE surface (browser breadth)
// ---------------------------------------------------------------------------

/// Minimal HTTP/1.1 handling on the same port for browser surfaces:
/// - `GET /health`            → 200 JSON liveness
/// - `GET /sse/<session>`     → 200 text/event-stream; broadcasts flow
///   as SSE `data:` frames for that session until the client disconnects
/// - `POST /command`          → body is the same v4 command envelope as
///   the NDJSON protocol; responds with the accept/reject JSON (a command
///   on a running session is steering: `result.type == "steeringQueued"`)
/// - `POST /steer`            → body `{sessionId, text}` queued onto the
///   session's steering queue (consumed by the live or next turn)
fn http_handle(state: &Arc<TcpServeState>, stream: TcpStream) -> std::io::Result<()> {
    let read_half = stream.try_clone()?;
    let mut reader = BufReader::new(read_half);

    // request line + headers
    let mut request_line = String::new();
    reader.read_line(&mut request_line)?;
    let mut content_length = 0usize;
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header)? == 0 {
            break;
        }
        let trimmed = header.trim();
        if trimmed.is_empty() {
            break;
        }
        if let Some(value) = trimmed
            .strip_prefix("content-length:")
            .or_else(|| trimmed.strip_prefix("Content-Length:"))
        {
            content_length = value.trim().parse().unwrap_or(0);
        }
    }

    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_string();
    let path = parts.next().unwrap_or_default().to_string();

    if method == "POST" && path == "/steer" {
        // Explicit browser steering: body {sessionId, text} is pushed onto
        // the session's steering queue, which the live turn drains at every
        // projection event (steered text becomes a `[steered]` user row).
        // Queued while idle → consumed by the next turn's first event.
        let mut body = vec![0u8; content_length];
        reader.read_exact(&mut body)?;
        let parsed: serde_json::Value = serde_json::from_slice(&body)
            .unwrap_or(serde_json::Value::Null);
        let session_id = parsed["sessionId"].as_str().unwrap_or_default().to_string();
        let text = parsed["text"].as_str().unwrap_or_default().to_string();
        if session_id.is_empty() || text.is_empty() {
            write_http(stream, 400, "bad request", br#"{"error":"sessionId and text required"}"#)?;
            return Ok(());
        }
        let queue = state.steering.lock().unwrap()
            .entry(session_id.clone())
            .or_insert_with(|| Arc::new(Mutex::new(VecDeque::new())))
            .clone();
        queue.lock().unwrap().push_back(crate::serve::SteeredInput {
            text,
            attachments: Vec::new(),
        });
        let queued_len = queue.lock().unwrap().len();
        let reply = serde_json::json!({ "steered": true, "queued_len": queued_len });
        eprintln!("[serve-tcp] steered via /steer: session={session_id} queued_len={queued_len}");
        write_http(stream, 200, "OK", serde_json::to_vec(&reply).unwrap_or_default().as_slice())?;
        return Ok(());
    }

    if method == "POST" && path == "/command" {
        let mut body = vec![0u8; content_length];
        reader.read_exact(&mut body)?;
        let Ok(body_text) = String::from_utf8(body) else {
            write_http(stream, 400, "bad request", b"{\"error\":\"body not utf-8\"}")?;
            return Ok(());
        };
        let Ok(envelope) = serde_json::from_str::<serde_json::Value>(&body_text) else {
            write_http(stream, 400, "bad request", b"{\"error\":\"body not json\"}")?;
            return Ok(());
        };
        let reply = command_accept(state, &envelope);
        write_http(
            stream,
            if reply["status"] == "rejected" { 400 } else { 200 },
            if reply["status"] == "rejected" { "rejected" } else { "accepted" },
            serde_json::to_vec(&reply).unwrap_or_default().as_slice(),
        )?;
        return Ok(());
    }

    // ---- workbench terminals (N0012) ----
    // POST /api/term/open  {program?}      -> {id}
    // GET  /api/term                       -> {ids}
    // GET  /api/term/<id>/sse              -> incremental output stream
    // POST /api/term/<id>/keys  {data}     -> write as if typed
    // POST /api/term/<id>/resize {rows,cols}
    // POST /api/term/<id>/close
    if method == "POST" && path == "/api/term/open" {
        let mut body = vec![0u8; content_length];
        reader.read_exact(&mut body)?;
        let parsed: serde_json::Value = serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null);
        let program = parsed["program"].as_str().map(str::to_string);
        return match term_open(state, program) {
            Ok(id) => write_http(
                stream,
                200,
                "OK",
                serde_json::to_vec(&serde_json::json!({ "id": id }))
                    .unwrap_or_default()
                    .as_slice(),
            ),
            Err(e) => write_http(
                stream,
                500,
                "error",
                serde_json::to_vec(&serde_json::json!({ "error": e }))
                    .unwrap_or_default()
                    .as_slice(),
            ),
        };
    }

    if method == "GET" && path == "/api/term" {
        let ids: Vec<String> = state.terminals.lock().unwrap().keys().cloned().collect();
        return write_http(
            stream,
            200,
            "OK",
            serde_json::to_vec(&serde_json::json!({ "ids": ids }))
                .unwrap_or_default()
                .as_slice(),
        );
    }

    if let Some(rest) = path.strip_prefix("/api/term/") {
        let (id, action) = match rest.split_once('/') {
            Some((id, action)) => (id.to_string(), action.to_string()),
            None => (rest.to_string(), String::new()),
        };
        let entry = state.terminals.lock().unwrap().get(&id).map(Arc::clone);

        if method == "GET" && action == "sse" {
            let Some(entry) = entry else {
                return write_http(stream, 404, "not found", b"{\"error\":\"no such terminal\"}");
            };
            return term_sse(stream, entry);
        }

        if method == "POST" {
            let mut body = vec![0u8; content_length];
            if content_length > 0 {
                reader.read_exact(&mut body)?;
            }
            let parsed: serde_json::Value =
                serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null);
            let Some(entry) = entry else {
                return write_http(stream, 404, "not found", b"{\"error\":\"no such terminal\"}");
            };
            match action.as_str() {
                "keys" => {
                    let data = parsed["data"].as_str().unwrap_or_default();
                    let n = entry
                        .session
                        .lock()
                        .unwrap()
                        .write(data.as_bytes())
                        .map_err(|e| e.to_string());
                    return match n {
                        Ok(n) => write_http(
                            stream,
                            200,
                            "OK",
                            serde_json::to_vec(&serde_json::json!({ "written": n }))
                                .unwrap_or_default()
                                .as_slice(),
                        ),
                        Err(e) => write_http(
                            stream,
                            500,
                            "error",
                            serde_json::to_vec(&serde_json::json!({ "error": e }))
                                .unwrap_or_default()
                                .as_slice(),
                        ),
                    };
                }
                "resize" => {
                    let rows = parsed["rows"].as_u64().unwrap_or(24) as u16;
                    let cols = parsed["cols"].as_u64().unwrap_or(80) as u16;
                    let r = entry
                        .session
                        .lock()
                        .unwrap()
                        .resize(okra_host::terminal::TerminalSize { rows, cols })
                        .map_err(|e| e.to_string());
                    let (status, body) = match r {
                        Ok(()) => (200, serde_json::json!({ "resized": true })),
                        Err(e) => (500, serde_json::json!({ "error": e })),
                    };
                    return write_http(
                        stream,
                        status,
                        if status == 200 { "OK" } else { "error" },
                        serde_json::to_vec(&body).unwrap_or_default().as_slice(),
                    );
                }
                "close" => {
                    entry.closed.store(true, Ordering::Relaxed);
                    state.terminals.lock().unwrap().remove(&id);
                    return write_http(
                        stream,
                        200,
                        "OK",
                        b"{\"closed\":true}"[..].into(),
                    );
                }
                _ => {}
            }
        }
    }

    // staging + commit (N0014): read-write git operations
    if method == "POST" && (path == "/api/git/stage" || path == "/api/git/unstage") {
        let mut body = vec![0u8; content_length];
        reader.read_exact(&mut body)?;
        let parsed: serde_json::Value =
            serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null);
        let paths: Vec<String> = parsed["paths"]
            .as_array()
            .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
            .unwrap_or_default();
        return match serve::git_stage(&state.cwd, &paths, path == "/api/git/unstage") {
            Ok(body) => write_http(
                stream,
                200,
                "OK",
                serde_json::to_vec(&body).unwrap_or_default().as_slice(),
            ),
            Err((code, msg)) => write_http(
                stream,
                code,
                "error",
                serde_json::to_vec(&serde_json::json!({ "error": msg }))
                    .unwrap_or_default()
                    .as_slice(),
            ),
        };
    }
    if method == "POST" && path == "/api/git/commit" {
        let mut body = vec![0u8; content_length];
        reader.read_exact(&mut body)?;
        let parsed: serde_json::Value =
            serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null);
        let message = parsed["message"].as_str().unwrap_or_default();
        return match serve::git_commit(&state.cwd, message) {
            Ok(body) => write_http(
                stream,
                200,
                "OK",
                serde_json::to_vec(&body).unwrap_or_default().as_slice(),
            ),
            Err((code, msg)) => write_http(
                stream,
                code,
                "error",
                serde_json::to_vec(&serde_json::json!({ "error": msg }))
                    .unwrap_or_default()
                    .as_slice(),
            ),
        };
    }

    if method == "POST" && (path == "/api/mcp/probe") {
        let mut body = vec![0u8; content_length];
        if content_length > 0 {
            reader.read_exact(&mut body)?;
        }
        let parsed: serde_json::Value =
            serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null);
        let name = parsed["name"].as_str().map(str::to_string);
        return match serve::mcp_probe(&state.cwd, &state.mcp_status, name.as_deref()) {
            Ok(body) => write_http(
                stream,
                200,
                "OK",
                serde_json::to_vec(&body).unwrap_or_default().as_slice(),
            ),
            Err((code, msg)) => write_http(
                stream,
                code,
                "error",
                serde_json::to_vec(&serde_json::json!({ "error": msg }))
                    .unwrap_or_default()
                    .as_slice(),
            ),
        };
    }
    if path == "/api/skills" {
        let body = serve::skills_listing(&state.cwd);
        return write_http(
            stream,
            200,
            "OK",
            serde_json::to_vec(&body).unwrap_or_default().as_slice(),
        );
    }
    if path == "/api/mcp" {
        let body = serve::mcp_listing(&state.cwd, &state.mcp_status);
        return write_http(
            stream,
            200,
            "OK",
            serde_json::to_vec(&body).unwrap_or_default().as_slice(),
        );
    }

    if path == "/api/files" || path.starts_with("/api/files?") {
        let query = path.split_once('?').map(|(_, q)| q).unwrap_or("");
        let rel = query
            .split('&')
            .find_map(|kv| kv.strip_prefix("path="))
            .unwrap_or("");
        return match serve::files_listing(&state.cwd, rel) {
            Ok(body) => write_http(
                stream,
                200,
                "OK",
                serde_json::to_vec(&body).unwrap_or_default().as_slice(),
            ),
            Err((code, msg)) => write_http(
                stream,
                code,
                "error",
                serde_json::to_vec(&serde_json::json!({ "error": msg }))
                    .unwrap_or_default()
                    .as_slice(),
            ),
        };
    }
    if path == "/api/files/search" || path.starts_with("/api/files/search?") {
        let query = path.split_once('?').map(|(_, q)| q).unwrap_or("");
        let q = query
            .split('&')
            .find_map(|kv| kv.strip_prefix("q="))
            .unwrap_or("");
        return match serve::file_search(&state.cwd, q) {
            Ok(body) => write_http(
                stream,
                200,
                "OK",
                serde_json::to_vec(&body).unwrap_or_default().as_slice(),
            ),
            Err((code, msg)) => write_http(
                stream,
                code,
                "error",
                serde_json::to_vec(&serde_json::json!({ "error": msg }))
                    .unwrap_or_default()
                    .as_slice(),
            ),
        };
    }
    if path == "/api/file" || path.starts_with("/api/file?") {
        let query = path.split_once('?').map(|(_, q)| q).unwrap_or("");
        let rel = query
            .split('&')
            .find_map(|kv| kv.strip_prefix("path="))
            .unwrap_or("");
        return match serve::file_preview(&state.cwd, rel) {
            Ok(body) => write_http(
                stream,
                200,
                "OK",
                serde_json::to_vec(&body).unwrap_or_default().as_slice(),
            ),
            Err((code, msg)) => write_http(
                stream,
                code,
                "error",
                serde_json::to_vec(&serde_json::json!({ "error": msg }))
                    .unwrap_or_default()
                    .as_slice(),
            ),
        };
    }

    if path == "/api/git" {
        return match serve::git_overview(&state.cwd) {
            Ok(body) => write_http(
                stream,
                200,
                "OK",
                serde_json::to_vec(&body).unwrap_or_default().as_slice(),
            ),
            Err((code, msg)) => write_http(
                stream,
                code,
                "error",
                serde_json::to_vec(&serde_json::json!({ "error": msg }))
                    .unwrap_or_default()
                    .as_slice(),
            ),
        };
    }
    if path == "/api/git/diff" || path.starts_with("/api/git/diff?") {
        let query = path.split_once('?').map(|(_, q)| q).unwrap_or("");
        let rel = query
            .split('&')
            .find_map(|kv| kv.strip_prefix("path="))
            .unwrap_or("");
        return match serve::git_diff(&state.cwd, rel) {
            Ok(body) => write_http(
                stream,
                200,
                "OK",
                serde_json::to_vec(&body).unwrap_or_default().as_slice(),
            ),
            Err((code, msg)) => write_http(
                stream,
                code,
                "error",
                serde_json::to_vec(&serde_json::json!({ "error": msg }))
                    .unwrap_or_default()
                    .as_slice(),
            ),
        };
    }

    if path == "/api/sessions" {
        let sessions = state.sessions.lock().unwrap();
        let list = serve::list_session_summaries(&state.sessions_dir, &sessions);
        drop(sessions);
        let body = serde_json::to_vec(&serde_json::json!({ "sessions": list }))
            .unwrap_or_default();
        return write_http(stream, 200, "OK", &body);
    }

    if let Some(rest) = path.strip_prefix("/api/sessions/") {
        let session_id = rest.trim_end_matches("/rows").trim_start_matches('/');
        if session_id.is_empty() {
            let body =
                serde_json::to_vec(&serde_json::json!({ "error": "session id required" }))
                    .unwrap_or_default();
            return write_http(stream, 400, "bad request", &body);
        }
        let kernel_name = format!("session-{session_id}");
        match serve::session_events(&state.sessions_dir, &kernel_name)
            .and_then(|events| serve::rows_from_kernel_events(&events))
        {
            Ok((rows, control)) => {
                let body = serde_json::to_vec(&serde_json::json!({
                    "sessionId": session_id,
                    "rows": rows,
                    "control": control,
                }))
                .unwrap_or_default();
                return write_http(stream, 200, "OK", &body);
            }
            Err(e) => {
                // unknown session → 404; a log we cannot safely
                // reconstruct → 422 (honest refusal, not silent data)
                let (status, reason) = if e.contains("not found") || e.contains("No such") {
                    (404, "not found")
                } else {
                    (422, "unreplayable")
                };
                let body =
                    serde_json::to_vec(&serde_json::json!({ "error": e })).unwrap_or_default();
                return write_http(stream, status, reason, &body);
            }
        }
    }

    if method == "GET" {
        // static workbench assets (embedded; the daemon stays
        // dependency-free — no build step, no node_modules)
        match path.as_str() {
            "/" | "/index.html" => {
                return write_http_content(
                    stream,
                    200,
                    "OK",
                    "text/html; charset=utf-8",
                    include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../ui/index.html")),
                );
            }
            "/app.css" => {
                return write_http_content(
                    stream,
                    200,
                    "OK",
                    "text/css; charset=utf-8",
                    include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../ui/app.css")),
                );
            }
            "/app.js" => {
                return write_http_content(
                    stream,
                    200,
                    "OK",
                    "text/javascript; charset=utf-8",
                    include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../ui/app.js")),
                );
            }
            _ => {}
        }

        // ---- file surfaces: workspace-confined tree + safe-read preview ----
        // GET /api/files?path=rel ("" = root) — dot entries (incl.
        // .okra-sessions) are never listed; symlinks are reported but never
        // followed; the resolved path must stay inside the workspace.
}

    if method == "GET" && path == "/scenes" {
        let catalog = starter_scene_catalog();
        let body = serde_json::to_vec(&catalog.to_response_body()).unwrap_or_default();
        write_http(stream, 200, "OK", &body)?;
        return Ok(());
    }

    if method == "GET" && path == "/health" {
        let body = serde_json::to_vec(&serde_json::json!({
            "ok": true,
            "daemon": "okra",
            "version": env!("CARGO_PKG_VERSION"),
            "cwd": state.cwd.to_string_lossy(),
            "sampler": state.sampler_label,
        }))
        .unwrap_or_default();
        write_http(stream, 200, "OK", &body)?;
        return Ok(());
    }

    // `GET /replay/<session-id>`: the M6 mobile replay artifact over the
    // existing HTTP surface — a standalone phone-friendly transcript, no
    // JavaScript or external assets. Loopback bind keeps it local; phone
    // delivery needs an operator-provided tunnel (documented posture).
    if method == "GET" && path.starts_with("/replay/") {
        let session_id = path.trim_start_matches("/replay/").to_string();
        if session_id.is_empty() || session_id.contains("..") || session_id.contains('/') {
            write_http(stream, 400, "bad request", br#"{"error":"invalid session id"}"#)?;
            return Ok(());
        }
        match okra_host::export_session_replay(&state.sessions_dir, &session_id) {
            Ok(html) => {
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n",
                    html.len()
                );
                let mut w = stream;
                w.write_all(head.as_bytes())?;
                w.write_all(html.as_bytes())?;
                w.flush()?;
                return Ok(());
            }
            Err(e) => {
                let body = serde_json::json!({ "error": e.0 }).to_string();
                write_http(stream, 404, "not found", body.as_bytes())?;
                return Ok(());
            }
        }
    }

    if method == "GET" && path.starts_with("/sse/") {
        let session_id = path.trim_start_matches("/sse/").to_string();
        // ensure steering queue exists so POST /command + v4/steer can reach it
        state.steering.lock().unwrap().entry(session_id.clone())
            .or_insert_with(|| Arc::new(Mutex::new(VecDeque::new())));
        let mut writer = stream.try_clone()?;
        writer.write_all(b"HTTP/1.1 200 OK\r\n")?;
        writer.write_all(b"Content-Type: text/event-stream\r\n")?;
        writer.write_all(b"Cache-Control: no-cache\r\n")?;
        writer.write_all(b"Connection: keep-alive\r\n\r\n")?;
        writer.write_all(b": connected\n\n")?;
        writer.flush()?;
        state.writers.lock().unwrap().push(SurfaceWriter {
            inner: Arc::new(Mutex::new(writer)),
            sse: true,
        });
        // register as a Browser surface; heartbeats tick on client bytes
        let sse_surface_id = state
            .surfaces
            .lock()
            .unwrap()
            .attach(
                okra_host::surfaces::SurfaceKind::Browser,
                vec!["sse".into(), "steer".into()],
            )
            .map(|(id, _)| id)
            .unwrap_or_default();
        // hold the connection open reading (and discarding) client bytes;
        // broadcasts flow through the registered SSE writer. When the
        // browser disconnects, read fails and we return: the broadcaster
        // prunes the dead writer and the registry detaches the surface.
        let mut reader = BufReader::new(stream);
        let mut scratch = [0u8; 256];
        loop {
            match reader.read(&mut scratch) {
                Ok(0) => {
                    state.surfaces.lock().unwrap().detach(&sse_surface_id);
                    return Ok(());
                }
                Ok(n) => {
                    state.surfaces.lock().unwrap().heartbeat(&sse_surface_id);
                    let _ = n;
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => {
                    state.surfaces.lock().unwrap().detach(&sse_surface_id);
                    return Ok(());
                }
            }
        }
    }

    write_http(stream, 404, "not found", b"{\"error\":\"unknown path\"}")
}

/// Static-body HTTP writer for the embedded workbench assets.
fn write_http_content(
    mut stream: TcpStream,
    status: u16,
    reason: &str,
    content_type: &str,
    body: &str,
) -> std::io::Result<()> {
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nCache-Control: no-cache\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(body.as_bytes())?;
    stream.flush()
}

fn write_http(mut stream: TcpStream, status: u16, reason: &str, body: &[u8]) -> std::io::Result<()> {
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()
}

/// One workbench terminal: the PTY session (write/resize) plus the
/// bounded scrollback its pump thread feeds.
pub struct TermEntry {
    pub session: Arc<Mutex<TerminalSession>>,
    pub scroll: Arc<Mutex<TermScroll>>,
    pub closed: Arc<AtomicBool>,
}

/// Bounded scrollback: `buf` holds the bytes from absolute offset `base`
/// to `written`; readers that lag past `base` get a reset+snapshot.
#[derive(Default)]
pub struct TermScroll {
    pub buf: Vec<u8>,
    pub base: u64,
    pub written: u64,
}

impl TermScroll {
    const CAP: usize = 256 * 1024;

    fn push(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
        self.written += bytes.len() as u64;
        if self.buf.len() > Self::CAP {
            let drop = self.buf.len() - Self::CAP;
            self.buf.drain(..drop);
            self.base += drop as u64;
        }
    }

    /// New bytes since `last`, or ALL of it when the reader lagged past
    /// the retained window (reset=true).
    fn since(&self, last: u64) -> Result<(Vec<u8>, bool), u64> {
        if last < self.base {
            return Err(self.written);
        }
        if self.written > last {
            let from = (last - self.base) as usize;
            return Ok((self.buf[from..].to_vec(), false));
        }
        Ok((Vec::new(), false))
    }
}

/// The terminal output stream: incremental base64 chunks from the
/// scrollback (reset+snapshot when a reader lags past the retained
/// window). Client disconnects are detected by the 250ms read-timeout
/// peek, exactly like the v4 SSE surface.
fn term_sse(
    stream: TcpStream,
    entry: Arc<TermEntry>,
) -> std::io::Result<()> {
    use base64::Engine as _;
    let mut writer = stream.try_clone()?;
    writer.write_all(b"HTTP/1.1 200 OK\r\n")?;
    writer.write_all(b"Content-Type: text/event-stream\r\n")?;
    writer.write_all(b"Cache-Control: no-cache\r\n")?;
    writer.write_all(b"Connection: keep-alive\r\n\r\n")?;
    writer.write_all(b": connected\n\n")?;
    writer.flush()?;

    let mut reader = BufReader::new(stream.try_clone()?);
    let _ = reader.get_ref().set_read_timeout(Some(Duration::from_millis(250)));
    let mut last: u64 = 0;
    let mut first = true;
    let mut scratch = [0u8; 256];
    loop {
        // client-gone check (nonblocking peek through the timeout)
        match reader.read(&mut scratch) {
            Ok(0) => return Ok(()),
            Ok(_) => {}
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                // normal idle tick — fall through to the send pass
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return Ok(()),
        }

        let send = |w: &mut std::net::TcpStream, frame: serde_json::Value| -> std::io::Result<()> {
            let mut line = b"data: ".to_vec();
            line.extend_from_slice(serde_json::to_vec(&frame).unwrap_or_default().as_slice());
            line.extend_from_slice(b"\n\n");
            w.write_all(&line)?;
            w.flush()
        };

        let snap = {
            let scroll = entry.scroll.lock().unwrap();
            if first || last < scroll.base {
                // (re)start from the retained window
                last = scroll.base;
                Some((scroll.buf.clone(), true))
            } else {
                scroll.since(last).ok()
            }
        };
        if let Some((bytes, reset)) = snap
            && (!bytes.is_empty() || reset)
        {
                if reset && !first {
                    send(
                        &mut writer,
                        serde_json::json!({ "type": "reset" }),
                    )?;
                }
                first = false;
                last += bytes.len() as u64;
                send(
                    &mut writer,
                    serde_json::json!({
                        "type": "out",
                        "b64": base64::engine::general_purpose::STANDARD.encode(&bytes),
                    }),
                )?;
        }
        if entry.closed.load(Ordering::Relaxed) {
            // one final drain, then tell the client the PTY is gone
            std::thread::sleep(Duration::from_millis(150));
            let tail = {
                let scroll = entry.scroll.lock().unwrap();
                scroll.since(last).ok()
            };
            if let Some((bytes, _)) = tail && !bytes.is_empty() {
                send(
                    &mut writer,
                    serde_json::json!({
                        "type": "out",
                        "b64": base64::engine::general_purpose::STANDARD.encode(&bytes),
                    }),
                )?;
            }
            send(&mut writer, serde_json::json!({ "type": "exit" }))?;
            // the stream ends client-side; the session stays open for
            // reattach until /close prunes it
            return Ok(());
        }
    }
}

/// Open a terminal in the workspace: interactive shell by default.
fn term_open(state: &Arc<TcpServeState>, program: Option<String>) -> Result<String, String> {
    let mut ids = state.terminals.lock().unwrap();
    let id = format!("t{}", ids.len() + 1);
    let shell = program.unwrap_or_else(|| {
        std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string())
    });
    let (session, mut reader) =
        TerminalSession::spawn_split(&shell, &[], &state.cwd, Default::default())?;
    let entry = Arc::new(TermEntry {
        session: Arc::new(Mutex::new(session)),
        scroll: Arc::new(Mutex::new(TermScroll::default())),
        closed: Arc::new(AtomicBool::new(false)),
    });
    ids.insert(id.clone(), Arc::clone(&entry));
    drop(ids);

    // output pump: owns the split reader, feeds the scrollback
    let scroll = Arc::clone(&entry.scroll);
    let closed = Arc::clone(&entry.closed);
    std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break, // EOF: child exited
                Ok(n) => scroll.lock().unwrap().push(&buf[..n]),
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
            if closed.load(Ordering::Relaxed) {
                break;
            }
        }
        closed.store(true, Ordering::Relaxed);
    });
    Ok(id)
}

/// Shared v4 command acceptance for both the NDJSON and HTTP surfaces.
fn command_accept(state: &Arc<TcpServeState>, envelope: &serde_json::Value) -> serde_json::Value {
    let command_id = envelope["commandId"].as_str().unwrap_or("cmd").to_string();
    let cmd_type = envelope["type"].as_str().unwrap_or_default().to_string();
    let session_id = envelope["sessionId"].as_str().map(str::to_string)
        .unwrap_or_else(|| format!("tcp-{}", uuid_v4()));

    // ---- stop: flip the session's live stop flag (no-op when idle) ----
    if cmd_type == "stop" {
        let stopped = state
            .stop_flags
            .lock()
            .unwrap()
            .get(&session_id)
            .map(|f| {
                f.store(true, Ordering::Relaxed);
                true
            })
            .unwrap_or(false);
        eprintln!("[serve-tcp] stop: session={session_id} live={stopped}");
        return serde_json::json!({
            "commandId": command_id,
            "status": "accepted",
            "revisionAtDecision": 0,
            "result": { "type": if stopped { "stopAccepted" } else { "stopIdle" } }
        });
    }

    // ---- resolveApproval: answer the ask the workbench is showing ----
    if cmd_type == "resolveApproval" {
        let approval_id = envelope["payload"]["approvalId"].as_str().unwrap_or_default();
        let allow = envelope["payload"]["decision"].as_str() == Some("allow");
        let resolved = state
            .approval_bridges
            .lock()
            .unwrap()
            .get(&session_id)
            .map(|b| b.resolve(approval_id, allow))
            .unwrap_or(false);
        eprintln!("[serve-tcp] resolveApproval: session={session_id} id={approval_id} allow={allow} known={resolved}");
        return serde_json::json!({
            "commandId": command_id,
            "status": if resolved { "accepted" } else { "rejected" },
            "revisionAtDecision": 0,
            "result": { "type": "approvalResolved", "resolved": resolved }
        });
    }

    let text = envelope["payload"]["text"].as_str().unwrap_or_default().to_string();
    let attachments: Vec<String> = envelope["payload"]["attachments"]
        .as_array()
        .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
        .unwrap_or_default();
    // refuse traversal BEFORE anything surfaces
    for a in &attachments {
        if a.split(['/', '\\']).any(|seg| seg == "..") || a.starts_with('-') {
            return serde_json::json!({
                "commandId": command_id,
                "status": "rejected",
                "reasonCode": "okra.attachment.pathRefused",
                "message": format!("attachment path refused: {a}"),
                "revisionAtDecision": 0,
            });
        }
    }
    if cmd_type != "createSession" && cmd_type != "sendText" {
        return serde_json::json!({"commandId":command_id,"status":"rejected","reasonCode":"g4.unsupported","revisionAtDecision":0});
    }
    {
        let mut sessions = state.sessions.lock().unwrap();
        sessions.entry(session_id.clone()).or_insert_with(|| {
            Arc::new(Mutex::new(SessionProjection::new(session_id.clone(), state.cwd.clone())))
        });
    }
    let steer_queue = state.steering.lock().unwrap()
        .entry(session_id.clone()).or_insert_with(|| Arc::new(Mutex::new(VecDeque::new()))).clone();
    let input_id = format!("in-{}", state.next_static());

    // G4 breadth: claim the turn gate atomically. If the session already
    // has a live turn, this command is steering — queue onto the live turn
    // and report `steeringQueued` (the drain becomes a `[steered]` user row
    // at the next projection event). This is also the browser's steer path:
    // a double-clicked "send" or POST /steer can no longer spawn a second
    // parallel turn thread on the same projection.
    let gate_acquired = state
        .running_turns
        .lock().unwrap()
        .insert(session_id.clone());
    if !gate_acquired {
        steer_queue.lock().unwrap().push_back(crate::serve::SteeredInput {
            text,
            attachments: envelope["payload"]["attachments"]
                .as_array()
                .map(|a| a.iter().filter_map(|v| v.as_str().map(str::to_string)).collect())
                .unwrap_or_default(),
        });
        eprintln!("[serve-tcp] steered: session={session_id} cmd={command_id} type={cmd_type}");
        return serde_json::json!({
            "commandId": command_id,
            "status": "accepted",
            "revisionAtDecision": 0,
            "result": { "type": "steeringQueued", "delivery": "queued", "inputId": input_id }
        });
    }
    let result = if cmd_type == "createSession" {
        serde_json::json!({"type":"createSession","sessionId":session_id,
            "input":{"delivery":"startNow","inputId":input_id}})
    } else {
        serde_json::json!({"type":"inputAccepted","delivery":"startNow","inputId":input_id})
    };
    let accepted = serde_json::json!({
        "commandId":command_id,"status":"accepted","revisionAtDecision":0,"result":result
    });
    eprintln!("[serve-tcp] command accepted: session={session_id} cmd={command_id} type={cmd_type}");
    // install this turn's stop flag + approval bridge before the thread
    // starts so an early `stop`/`resolveApproval` can never race either
    // into existence
    let stop_flag = Arc::new(AtomicBool::new(false));
    state
        .stop_flags
        .lock()
        .unwrap()
        .insert(session_id.clone(), Arc::clone(&stop_flag));
    let bridge = Arc::new(SurfaceApprovalChannel::new(Arc::clone(&stop_flag)));
    state
        .approval_bridges
        .lock()
        .unwrap()
        .insert(session_id.clone(), Arc::clone(&bridge));
    // spawn the turn: projections broadcast to ALL surfaces (NDJSON + SSE)
    let state2 = Arc::clone(state);
    let turn_session = session_id.clone();
    let turn_topic = format!("conversation/{session_id}");
    let turn_cwd = state.cwd.clone();
    let turn_sdir = state.sessions_dir.clone();
    let factory = Arc::clone(&state.sampler_factory);
    std::thread::spawn(move || {
        // worklist: every entry (the original send + each steered input)
        // becomes its OWN turn, carrying its own attachments
        let mut work = std::collections::VecDeque::new();
        work.push_back(crate::serve::SteeredInput { text, attachments });
        while let Some(entry) = work.pop_front() {
            let projection = Arc::clone(
                state2.sessions.lock().unwrap().get(&turn_session).unwrap(),
            );
            let b = Arc::clone(&state2);
            let broadcast = Arc::new(move |m: &str, p: serde_json::Value| {
                let v = serde_json::json!({"method":m,"params":p});
                let line = serde_json::to_vec(&v).unwrap_or_default();
                b.broadcast_bytes(&line);
            });
            let _ = run_turn_streaming(
                broadcast, turn_topic.clone(), turn_session.clone(),
                turn_cwd.clone(), turn_sdir.clone(), entry.text,
                projection, Some(Arc::clone(&steer_queue)),
                Arc::clone(&stop_flag), &factory,
                Arc::clone(&bridge), false,
                entry.attachments,
            );
            let queued: Vec<crate::serve::SteeredInput> = {
                let mut q = steer_queue.lock().unwrap(); q.drain(..).collect()
            };
            if queued.is_empty() { break; }
            for e in queued {
                work.push_back(e);
            }
        }
        // release the turn gate LAST: a command landing just before this is
        // queued and drains at the next turn's first projection event (as a
        // `[steered]` row), so no text is lost and only one turn thread per
        // session ever exists.
        state2.running_turns.lock().unwrap().remove(&turn_session);
        state2.stop_flags.lock().unwrap().remove(&turn_session);
        state2.approval_bridges.lock().unwrap().remove(&turn_session);
    });
    accepted
}
