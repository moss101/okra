//! `okra serve --tcp ADDR` — the G4 multi-surface steering seam
//! (MASTER-PLAN §4 G4 groundwork).
//!
//! Loopback-only: many TCP clients attach to ONE daemon; projections
//! fan out to ALL surfaces, and any surface may steer the running turn.

use std::collections::{BTreeMap, VecDeque};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::serve::{starter_scene_catalog, run_turn_streaming, uuid_v4, SessionProjection};

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

pub type SteeringChannel = Arc<Mutex<VecDeque<String>>>;

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
    next_static: std::sync::atomic::AtomicU64,
}

impl TcpServeState {
    pub fn new(cwd: std::path::PathBuf, sessions_dir: std::path::PathBuf) -> Self {
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
        state.surfaces.lock().unwrap().heartbeat(&surface_id);
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
                let list: Vec<serde_json::Value> = state
                    .surfaces
                    .lock()
                    .unwrap()
                    .list()
                    .iter()
                    .map(|s| serde_json::to_value(s).unwrap_or_default())
                    .collect();
                respond(id_field, serde_json::json!({ "surfaces": list }));
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
                    .map(|ch| ch.lock().unwrap().push_back(text)).is_some();
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
        queue.lock().unwrap().push_back(text);
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

    if method == "GET" && (path == "/" || path == "/index.html") {
        let page = browser_demo_page();
        let body = page.as_bytes();
        let head = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nAccess-Control-Allow-Origin: *\r\nConnection: close\r\n\r\n",
            body.len()
        );
        let mut w = stream;
        w.write_all(head.as_bytes())?;
        w.write_all(body)?;
        w.flush()?;
        return Ok(());
    }

    if method == "GET" && path == "/scenes" {
        let catalog = starter_scene_catalog();
        let body = serde_json::to_vec(&catalog.to_response_body()).unwrap_or_default();
        write_http(stream, 200, "OK", &body)?;
        return Ok(());
    }

    if method == "GET" && path == "/health" {
        let body = serde_json::to_vec(&serde_json::json!({"ok": true, "daemon": "okra"})).unwrap_or_default();
        write_http(stream, 200, "OK", &body)?;
        return Ok(());
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

/// The browser demo surface: one static page that subscribes to the SSE
/// stream, renders every projection row, and drives turns + steering with
/// fetch. Kept dependency-free so the daemon serves it from one string.
/// NOTE: embedded in `r#"…"#` — the body must not contain the `"#` pair.
fn browser_demo_page() -> String {
    let page = r#"<!doctype html>
<html>
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>okra surface</title>
<style>
  :root { color-scheme: dark; }
  * { box-sizing: border-box; }
  body { font-family: ui-monospace, Menlo, Consolas, monospace; background: #14161a; color: #d6dae0; margin: 0; padding: 1.1rem; max-width: 46rem; margin-inline: auto; }
  h1 { font-size: 1.05rem; margin: 0 0 .35rem; }
  .sub { color: #8a919c; font-size: .78rem; margin-bottom: 1rem; }
  .dot { display: inline-block; width: .6rem; height: .6rem; border-radius: 50%; background: #b3413c; margin-right: .35rem; vertical-align: 1px; }
  .dot.on { background: #4fae54; }
  .panel { border: 1px solid #2a2e35; border-radius: 8px; padding: .75rem .85rem; margin-bottom: .85rem; background: #191c21; }
  label { font-size: .7rem; color: #8a919c; display: block; margin-bottom: .25rem; text-transform: uppercase; letter-spacing: .05em; }
  .mt { margin-top: .6rem; }
  textarea, input[type=text] { width: 100%; background: #101216; color: #e8eaee; border: 1px solid #2a2e35; border-radius: 6px; padding: .45rem; font: inherit; }
  button { background: #2f6feb; border: 0; color: white; border-radius: 6px; padding: .45rem .9rem; font: inherit; cursor: pointer; margin-top: .45rem; }
  button.alt { background: #3a3f47; }
  .chips span { display: inline-block; border: 1px solid #2a2e35; border-radius: 999px; padding: .15rem .6rem; margin: .2rem .25rem 0 0; font-size: .72rem; color: #a9b1ba; }
  .row { border-left: 3px solid #2a2e35; padding: .35rem .6rem; margin: .4rem 0; border-radius: 4px; background: #101216; white-space: pre-wrap; word-break: break-word; font-size: .8rem; }
  .row.userInput { border-left-color: #2f6feb; }
  .row.assistantText { border-left-color: #4fae54; }
  .row.toolCall { border-left-color: #b58a3c; }
  .row.turnHeader { border-left-color: #5a616c; color: #9aa2ad; }
  .badge { font-size: .62rem; text-transform: uppercase; letter-spacing: .05em; color: #8a919c; display: block; margin-bottom: .15rem; }
  #steerstate { color: #8a919c; font-size: .75rem; margin-left: .5rem; }
  .foot { color: #5a616c; font-size: .7rem; margin-top: 1.2rem; }
</style>
</head>
<body>
<h1>okra surface</h1>
<div class="sub">
  <span class="dot" id="conn"></span><span id="connlabel">connecting</span>
  · <span id="daemon">daemon: ?</span>
  · frames <span id="frames">0</span>
  · phase <span id="phase">-</span>
</div>
<div class="panel">
  <label>session</label>
  <input type="text" id="session" value="">
  <label class="mt">prompt</label>
  <textarea id="prompt" rows="2" placeholder="summarize notes.md"></textarea>
  <button id="send">send turn</button>
  <span id="sendstate"></span>
</div>
<div class="panel">
  <label>steer (queued onto the live turn; runs as its own turn when idle)</label>
  <input type="text" id="steer" placeholder="focus on the tools section">
  <button class="alt" id="steerbtn">steer</button>
  <span id="steerstate"></span>
</div>
<div class="panel">
  <label>starter scenes · GET /scenes</label>
  <div class="chips" id="scenes">loading…</div>
</div>
<div id="transcript"></div>
<div class="foot">GET /sse/&lt;session&gt; streams every projection frame; POST /command and POST /steer drive the same v4 seam the NDJSON and Electron surfaces use.</div>
<script>
'use strict';
var sessionEl = document.getElementById('session');
sessionEl.value = 'browser-' + Math.random().toString(36).slice(2, 6);
var frames = 0;
var es = null;

function esc(s) {
  return String(s).replace(/[&<>]/g, function (c) {
    return { '&': '&amp;', '<': '&lt;', '>': '&gt;' }[c];
  });
}
function post(path, body, onreply) {
  fetch(path, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify(body)
  })
    .then(function (r) { return r.json(); })
    .then(onreply)
    .catch(function (e) { note('steerstate', 'error: ' + e); });
}
function note(id, t) { document.getElementById(id).textContent = t; }
function resultKind(r) {
  return r && r.result && r.result.type ? r.result.type : (r && r.status);
}
function send() {
  var text = document.getElementById('prompt').value.trim();
  if (!text) { return; }
  post('/command', {
    commandId: 'browser-' + Date.now(),
    type: 'sendText',
    sessionId: sessionEl.value,
    payload: { text: text }
  }, function (r) { note('sendstate', '→ ' + resultKind(r)); });
}
function steer() {
  var text = document.getElementById('steer').value.trim();
  if (!text) { return; }
  post('/steer', { sessionId: sessionEl.value, text: text }, function (r) {
    note('steerstate', '→ ' + (r && r.steered ? 'steered' : 'rejected'));
  });
}
document.getElementById('send').addEventListener('click', send);
document.getElementById('steerbtn').addEventListener('click', steer);
document.getElementById('prompt').addEventListener('keydown', function (e) {
  if (e.key === 'Enter' && !e.shiftKey) { e.preventDefault(); send(); }
});
document.getElementById('steer').addEventListener('keydown', function (e) {
  if (e.key === 'Enter') { e.preventDefault(); steer(); }
});
sessionEl.addEventListener('change', function () { connect(); });

fetch('/health').then(function (r) { return r.json(); }).then(function (h) {
  document.getElementById('daemon').textContent = 'daemon: ' + h.daemon + ' (ok=' + h.ok + ')';
});
fetch('/scenes').then(function (r) { return r.json(); }).then(function (c) {
  var host = document.getElementById('scenes');
  host.textContent = '';
  var data = (c && c.data) || [];
  if (!data.length) { host.textContent = 'none'; return; }
  data.forEach(function (s) {
    var opts = [];
    Object.keys(s.options || {}).forEach(function (k) {
      (s.options[k].items || []).forEach(function (it) {
        var names = it.contents || {};
        opts.push(names.en || Object.keys(names).map(function (t) { return names[t]; })[0] || it.id);
      });
    });
    var el = document.createElement('span');
    el.textContent = s.namespace + ':' + s.scene + (opts.length ? ' [' + opts.join(' / ') + ']' : '');
    host.appendChild(el);
  });
});

function rowHead(r) {
  if (r.kind === 'toolCall') { return 'tool ' + r.toolName + ' · ' + r.status; }
  if (r.kind === 'turnHeader') { return 'turn · ' + r.state; }
  if (r.kind === 'assistantText') { return 'assistant · ' + (r.state || ''); }
  if (r.kind === 'userInput') { return 'user'; }
  return r.kind;
}
function rowBody(r) {
  if (r.kind === 'toolCall') {
    var out = r.output && r.output.text ? String(r.output.text) : '';
    if (out.length > 400) { out = out.slice(0, 400) + '…'; }
    return out || (r.error && r.error.message) || '';
  }
  return r.text || '';
}
function render(params) {
  var t = document.getElementById('transcript');
  t.textContent = '';
  (params.rows || []).forEach(function (r) {
    var div = document.createElement('div');
    div.className = 'row ' + r.kind;
    var b = document.createElement('span');
    b.className = 'badge';
    b.textContent = rowHead(r);
    div.appendChild(b);
    div.appendChild(document.createTextNode(rowBody(r)));
    t.appendChild(div);
  });
}
function connect() {
  if (es) { es.close(); }
  es = new EventSource('/sse/' + encodeURIComponent(sessionEl.value));
  es.onopen = function () {
    document.getElementById('conn').className = 'dot on';
    document.getElementById('connlabel').textContent = 'connected (sse)';
  };
  es.onerror = function () {
    document.getElementById('conn').className = 'dot';
    document.getElementById('connlabel').textContent = 'reconnecting';
  };
  es.onmessage = function (e) {
    frames += 1;
    document.getElementById('frames').textContent = String(frames);
    var msg;
    try { msg = JSON.parse(e.data); } catch (_) { return; }
    if (!msg.params) { return; }
    if (msg.params.control) {
      document.getElementById('phase').textContent = msg.params.control.phase || '-';
    }
    render(msg.params);
  };
}
connect();
</script>
</body>
</html>
"#;
    page.trim_end().to_string()
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

/// Shared v4 command acceptance for both the NDJSON and HTTP surfaces.
fn command_accept(state: &Arc<TcpServeState>, envelope: &serde_json::Value) -> serde_json::Value {
    let command_id = envelope["commandId"].as_str().unwrap_or("cmd").to_string();
    let cmd_type = envelope["type"].as_str().unwrap_or_default().to_string();
    let session_id = envelope["sessionId"].as_str().map(str::to_string)
        .unwrap_or_else(|| format!("tcp-{}", uuid_v4()));
    let text = envelope["payload"]["text"].as_str().unwrap_or_default().to_string();
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
        steer_queue.lock().unwrap().push_back(text);
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
    // spawn the turn: projections broadcast to ALL surfaces (NDJSON + SSE)
    let state2 = Arc::clone(state);
    let turn_session = session_id.clone();
    let turn_topic = format!("conversation/{session_id}");
    let turn_cwd = state.cwd.clone();
    let turn_sdir = state.sessions_dir.clone();
    std::thread::spawn(move || {
        let mut current_input = text;
        loop {
            let projection = Arc::clone(
                state2.sessions.lock().unwrap().get(&turn_session).unwrap(),
            );
            let mut notify = |m: &str, p: serde_json::Value| {
                let v = serde_json::json!({"method":m,"params":p});
                let line = serde_json::to_vec(&v).unwrap_or_default();
                state2.broadcast_bytes(&line);
            };
            let _ = run_turn_streaming(
                &mut notify, turn_topic.clone(), turn_session.clone(),
                turn_cwd.clone(), turn_sdir.clone(), current_input,
                projection, Some(Arc::clone(&steer_queue)),
            );
            let queued: Vec<String> = {
                let mut q = steer_queue.lock().unwrap(); q.drain(..).collect()
            };
            if queued.is_empty() { break; }
            current_input = queued.join("\n");
        }
        // release the turn gate LAST: a command landing just before this is
        // queued and drains at the next turn's first projection event (as a
        // `[steered]` row), so no text is lost and only one turn thread per
        // session ever exists.
        state2.running_turns.lock().unwrap().remove(&turn_session);
    });
    accepted
}
