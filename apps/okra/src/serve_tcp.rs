//! `okra serve --tcp ADDR` — the G4 multi-surface steering seam
//! (MASTER-PLAN §4 G4 groundwork).
//!
//! Loopback-only: many TCP clients attach to ONE daemon; projections
//! fan out to ALL surfaces, and any surface may steer the running turn.

use std::collections::{BTreeMap, VecDeque};
use std::io::{BufRead, Write};
use std::net::TcpStream;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use crate::serve::{run_turn_streaming, uuid_v4, SessionProjection};

pub struct SurfaceWriter {
    inner: Arc<Mutex<TcpStream>>,
}

impl SurfaceWriter {
    pub fn send_line(&self, line: &[u8]) -> bool {
        let mut w = match self.inner.lock() { Ok(w) => w, Err(_) => return false };
        w.write_all(line).and_then(|_| w.flush()).is_ok()
    }
}

pub type SteeringChannel = Arc<Mutex<VecDeque<String>>>;

pub struct TcpServeState {
    pub cwd: std::path::PathBuf,
    pub sessions_dir: std::path::PathBuf,
    pub sessions: Mutex<BTreeMap<String, Arc<Mutex<SessionProjection>>>>,
    pub steering: Mutex<BTreeMap<String, SteeringChannel>>,
    pub writers: Mutex<Vec<SurfaceWriter>>,
    next_static: std::sync::atomic::AtomicU64,
}

impl TcpServeState {
    pub fn new(cwd: std::path::PathBuf, sessions_dir: std::path::PathBuf) -> Self {
        TcpServeState {
            cwd,
            sessions_dir,
            sessions: Mutex::new(BTreeMap::new()),
            steering: Mutex::new(BTreeMap::new()),
            writers: Mutex::new(Vec::new()),
            next_static: std::sync::atomic::AtomicU64::new(0),
        }
    }
    fn next_static(&self) -> u64 {
        self.next_static.fetch_add(1, Ordering::SeqCst)
    }
    #[allow(dead_code)]
    pub fn broadcast(&self, value: &serde_json::Value) {
        let line = serde_json::to_vec(value).unwrap_or_default();
        let mut writers = self.writers.lock().unwrap();
        let mut dead = Vec::new();
        for (i, w) in writers.iter().enumerate() {
            let ok = w.send_line(&line);
            if !ok { dead.push(i); }
        }
        for i in dead.into_iter().rev() { writers.remove(i); }
    }

    #[allow(dead_code)]
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
    let own_writer = Arc::new(Mutex::new(stream));
    // register this surface for broadcasts; the broadcaster prunes dead
    // writers when a surface disconnects
    state.writers.lock().unwrap().push(SurfaceWriter {
        inner: Arc::clone(&own_writer),
    });
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
        let Ok(msg) = serde_json::from_str::<serde_json::Value>(&line) else { continue; };
        let id_field = msg["id"].as_u64().unwrap_or(0);
        let method = msg["method"].as_str().unwrap_or_default().to_string();
        let params = msg["params"].clone();
        match method.as_str() {
            "hello" => respond(id_field, serde_json::json!({"daemon":"okra","protocolVersion":3})),
            "ping" => respond(id_field, serde_json::json!({"pong":true})),
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
                let command_id = envelope["commandId"].as_str().unwrap_or("cmd").to_string();
                let cmd_type = envelope["type"].as_str().unwrap_or_default().to_string();
                let session_id = envelope["sessionId"].as_str().map(str::to_string)
                    .unwrap_or_else(|| format!("tcp-{}", uuid_v4()));
                let text = envelope["payload"]["text"].as_str().unwrap_or_default().to_string();
                if cmd_type != "createSession" && cmd_type != "sendText" {
                    respond(id_field, serde_json::json!({"commandId":command_id,"status":"rejected","reasonCode":"g4.unsupported","revisionAtDecision":0}));
                    continue;
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
                let result = if cmd_type == "createSession" {
                    serde_json::json!({"type":"createSession","sessionId":session_id,
                        "input":{"delivery":"startNow","inputId":input_id}})
                } else {
                    serde_json::json!({"type":"inputAccepted","delivery":"startNow","inputId":input_id})
                };
                respond(id_field, serde_json::json!({
                    "commandId":command_id,"status":"accepted","revisionAtDecision":0,"result":result
                }));
                // spawn the turn: projections broadcast to ALL surfaces
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
                });
            }
            _ => {}
        }
    }
}
