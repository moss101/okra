//! G4 breadth (MASTER-PLAN §4): the browser-native surface. The daemon
//! port also speaks HTTP: `GET /health`, `GET /sse/<session>`
//! (text/event-stream — broadcasts arrive as `data:` frames), and
//! `POST /command` (the same v4 envelope as the NDJSON surfaces). A
//! browser needs nothing but EventSource + fetch to be a full surface.


// Test harness: these acceptance tests execute the compiled crate binary as
// the system under test. The no-raw-spawn/canonicalize bans target production
// paths (production spawning goes through okra_policy's confined runner); the
// acceptance harness must exercise the real binary end-to-end.
#![allow(clippy::disallowed_methods)]
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use std::path::Path;

/// Spawn the real daemon and read its advertised bind address from stderr.
fn spawn_daemon(cwd: &Path) -> (Child, String) {
    spawn_daemon_env(cwd, &[])
}

/// `extra_env`: (key, value) pairs set on the daemon process (e.g.
/// `OKRA_DEMO_DELAY_MS` to widen the steering window).
fn spawn_daemon_env(cwd: &Path, extra_env: &[(&str, &str)]) -> (Child, String) {
    let bin = env!("CARGO_BIN_EXE_okra");
    let mut cmd = Command::new(bin);
    cmd.args(["serve", "--tcp", "--cwd", &cwd.to_string_lossy()]);
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    let mut child = cmd
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn okra serve --tcp");
    let stderr = child.stderr.take().expect("stderr piped");
    let mut reader = BufReader::new(stderr);
    let deadline = Instant::now() + Duration::from_secs(15);
    let addr = loop {
        assert!(Instant::now() < deadline, "daemon never advertised its port");
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) => panic!("daemon exited before binding"),
            Ok(_) => {}
            Err(e) => panic!("read stderr: {e}"),
        }
        eprintln!("DAEMON: {}", line.trim());
        if let Some(rest) = line.strip_prefix("[serve-tcp] multi-surface daemon on ") {
            let rest = rest.trim();
            break rest.split(" (").next().unwrap_or(rest).to_string();
        }
    };
    // keep draining stderr so daemon audit lines surface in test output
    {
        let reader = reader;
        std::thread::spawn(move || {
            for line in reader.lines().map_while(Result::ok) {
                eprintln!("DAEMON: {}", line.trim());
            }
        });
    }
    (child, addr)
}

fn http_get(addr: &str, path: &str) -> (u16, String) {
    let mut stream = TcpStream::connect(addr).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let request = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).unwrap();
    let mut response = String::new();
    let mut reader = BufReader::new(stream);
    for line in reader.by_ref().lines().map_while(Result::ok) {
        response.push_str(&line);
        response.push('\n');
    }
    let status: u16 = response
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    (status, response)
}

fn http_post_command(addr: &str, envelope: &serde_json::Value) -> (u16, serde_json::Value) {
    http_post(addr, "/command", envelope)
}

fn http_post(addr: &str, path: &str, body_json: &serde_json::Value) -> (u16, serde_json::Value) {
    let mut stream = TcpStream::connect(addr).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    let body = body_json.to_string();
    let request = format!(
        "POST {path} HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    stream.write_all(request.as_bytes()).unwrap();
    let mut response = Vec::new();
    stream.read_to_end(&mut response).unwrap();
    let text = String::from_utf8_lossy(&response).into_owned();
    let status: u16 = text
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let body_start = text.find("\r\n\r\n").map(|i| i + 4).unwrap_or(0);
    let body = serde_json::from_str::<serde_json::Value>(text[body_start..].trim())
        .unwrap_or_else(|_| {
            eprintln!("POST RAW RESPONSE: {text:?}");
            serde_json::json!({})
        });
    (status, body)
}

/// Open an SSE connection and collect `data:` frames until `deadline`.
fn sse_collect(addr: &str, session: &str, frames: &Mutex<Vec<serde_json::Value>>, stop: &dyn Fn(&[serde_json::Value]) -> bool) {
    let mut stream = TcpStream::connect(addr).unwrap();
    stream.set_read_timeout(Some(Duration::from_millis(500))).unwrap();
    let request = format!("GET /sse/{session} HTTP/1.1\r\nHost: localhost\r\nAccept: text/event-stream\r\n\r\n");
    stream.write_all(request.as_bytes()).unwrap();

    // headers first
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut head = String::new();
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        head.push_str(&line);
        if line == "\r\n" || line.is_empty() {
            break;
        }
    }
    assert!(head.contains("200"), "SSE handshake failed: {head}");
    assert!(head.contains("text/event-stream"), "{head}");

    let deadline = Instant::now() + Duration::from_secs(30);
    let mut event = String::new();
    loop {
        if stop(&frames.lock().unwrap()) || Instant::now() > deadline {
            return;
        }
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) => return,
            Ok(_) => {}
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                continue
            }
            Err(e) => panic!("sse read: {e}"),
        }
        let line = line.trim_end_matches(['\n', '\r']);
        if let Some(data) = line.strip_prefix("data: ") {
            event.push_str(data);
        } else if line.is_empty() && !event.is_empty() {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&event) {
                frames.lock().unwrap().push(v);
            }
            event.clear();
        }
    }
}

fn mutex_vec() -> std::sync::Mutex<Vec<serde_json::Value>> {
    std::sync::Mutex::new(Vec::new())
}

use std::sync::Mutex;

#[test]
fn g4_browser_surface_health_command_and_sse() {
    let td = tempfile::tempdir().unwrap();
    std::fs::write(td.path().join("notes.md"), "# notes\nbrowser breadth\n").unwrap();
    let (mut daemon, addr) = spawn_daemon(td.path());
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        run_http_gate(&addr);
    }));
    let _ = daemon.kill();
    let _ = daemon.wait();
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

fn run_http_gate(addr: &str) {
    // 1. health
    let (status, body) = http_get(addr, "/health");
    assert_eq!(status, 200);
    assert!(body.contains("\"ok\":true"), "{body}");

    // 2. SSE surface subscribes BEFORE the command (browser EventSource)
    let sse_frames = mutex_vec();
    {
        let frames = std::sync::Arc::new(sse_frames);
        let writer_frames = std::sync::Arc::clone(&frames);
        let addr_owned = addr.to_string();
        let sse_thread = std::thread::spawn(move || {
            sse_collect(&addr_owned, "browser-session", &writer_frames, &|frames| {
                // exit as soon as the completed turn (with the user input)
                // has been streamed
                frames.iter().any(|f| {
                    f["params"]["control"]["phase"] == "completedSuccess"
                        && f["params"]["rows"].as_array().map(|rows| {
                            rows.iter().any(|r| {
                                r["kind"] == "userInput"
                                    && r["text"].as_str().map(|t| t.contains("summarize notes.md")).unwrap_or(false)
                            })
                        }).unwrap_or(false)
                })
            });
        });
        // give the SSE handshake a moment to register the surface
        std::thread::sleep(Duration::from_millis(300));

        // the browser surface is registered with kind browser + sse capability
        let mut ndjson = TcpStream::connect(addr).unwrap();
        ndjson.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        ndjson
            .write_all(b"{\"id\":40,\"method\":\"surfaces/list\"}\n")
            .unwrap();
        let mut nd_reader = BufReader::new(ndjson.try_clone().unwrap());
        let mut reg = String::new();
        let reg_deadline = Instant::now() + Duration::from_secs(10);
        loop {
            assert!(Instant::now() < reg_deadline, "no surfaces/list reply");
            reg.clear();
            nd_reader.read_line(&mut reg).unwrap();
            if reg.contains("\"surfaces\"") {
                break;
            }
        }
        assert!(reg.contains("\"kind\":\"browser\""), "{reg}");
        assert!(reg.contains("\"sse\""), "{reg}");
        let _ = ndjson;

        // 2b. GET /scenes serves the offline starter catalog
    let (scenes_status, scenes_body) = http_get(addr, "/scenes");
    assert_eq!(scenes_status, 200);
    assert!(scenes_body.contains("repo-explain"), "{scenes_body}");
    assert!(scenes_body.contains("depth"), "{scenes_body}");

    // 3. POST /command drives a turn from the "browser"
        let (status, reply) = http_post_command(addr, &serde_json::json!({
            "commandId": "browser-1",
            "type": "sendText",
            "sessionId": "browser-session",
            "payload": { "text": "summarize notes.md" }
        }));
        assert_eq!(status, 200, "{reply}");
        assert_eq!(reply["status"], "accepted");

        // 4. the SSE surface must have received the projections
        let deadline = Instant::now() + Duration::from_secs(25);
        loop {
            let done = {
                let frames = frames.lock().unwrap();
                frames.iter().any(|f| {
                    f["params"]["control"]["phase"] == "completedSuccess"
                        && f["params"]["rows"].as_array().map(|rows| {
                            rows.iter().any(|r| {
                                r["kind"] == "userInput"
                                    && r["text"].as_str().map(|t| t.contains("summarize notes.md")).unwrap_or(false)
                            })
                        }).unwrap_or(false)
                })
            };
            if done || Instant::now() > deadline {
                assert!(done, "SSE surface never saw the completed turn");
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        let _ = sse_thread.join();
        let _ = writer_frames;
    }
}

/// Wait until `pred` holds for some collected SSE frame (30s deadline).
fn wait_for(frames: &Mutex<Vec<serde_json::Value>>, pred: &dyn Fn(&[serde_json::Value]) -> bool) -> bool {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        if pred(&frames.lock().unwrap()) {
            return true;
        }
        if Instant::now() > deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn completed_with_steered(frames: &[serde_json::Value], steered_text: &str) -> bool {
    frames.iter().any(|f| {
        f["params"]["control"]["phase"] == "completedSuccess"
            && f["params"]["rows"].as_array().map(|rows| {
                rows.iter().any(|r| {
                    r["kind"] == "userInput"
                        && r["text"].as_str().map(|t| t.contains(steered_text)).unwrap_or(false)
                })
            }).unwrap_or(false)
    })
}

/// G4 breadth — steering: a command on a session with a live turn must be
/// STEERING (`steeringQueued`, `[steered]` row on the same turn), never a
/// second parallel turn thread racing the same projection. The scripted
/// stub's `OKRA_DEMO_DELAY_MS` widens the running window so the second
/// command deterministically lands mid-turn.
#[test]
fn g4_command_on_running_session_steers_instead_of_parallel_turn() {
    let td = tempfile::tempdir().unwrap();
    std::fs::write(td.path().join("notes.md"), "# notes\nsteering breadth\n").unwrap();
    let (mut daemon, addr) = spawn_daemon_env(td.path(), &[("OKRA_DEMO_DELAY_MS", "1200")]);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let frames = std::sync::Arc::new(mutex_vec());
        let writer_frames = std::sync::Arc::clone(&frames);
        let addr_owned = addr.clone();
        let sse_thread = std::thread::spawn(move || {
            sse_collect(&addr_owned, "steer-session", &writer_frames, &|frames| {
                completed_with_steered(frames, "[steered] focus on the tools")
            })
        });
        std::thread::sleep(Duration::from_millis(300));

        // 1. start the turn (sampler now sleeping 1200ms per step)
        let (status, reply) = http_post_command(&addr, &serde_json::json!({
            "commandId": "steer-1",
            "type": "sendText",
            "sessionId": "steer-session",
            "payload": { "text": "summarize notes.md" }
        }));
        assert_eq!(status, 200, "{reply}");
        assert_eq!(reply["status"], "accepted");
        assert_eq!(reply["result"]["type"], "inputAccepted");

        // 2. a second command mid-turn is steering, not a parallel turn
        let (status2, reply2) = http_post_command(&addr, &serde_json::json!({
            "commandId": "steer-2",
            "type": "sendText",
            "sessionId": "steer-session",
            "payload": { "text": "focus on the tools" }
        }));
        assert_eq!(status2, 200, "{reply2}");
        assert_eq!(reply2["status"], "accepted", "{reply2}");
        assert_eq!(reply2["result"]["type"], "steeringQueued", "{reply2}");

        // 3. the steered text lands as a [steered] user row on the SAME turn
        let got = wait_for(&frames, &|frames| {
            completed_with_steered(frames, "[steered] focus on the tools")
        });
        assert!(got, "steered text never reached the live turn; frames: {frames:?}");

        // 4. single-turn-thread invariant: exactly ONE turnHeader in the
        // final projection (the old bug spawned a racing second turn)
        let tail = frames.lock().unwrap().last().cloned().unwrap_or_default();
        let headers = tail["params"]["rows"].as_array().map(|rows| {
            rows.iter().filter(|r| r["kind"] == "turnHeader").count()
        }).unwrap_or(0);
        assert_eq!(headers, 1, "expected exactly one turn thread, final frame: {tail}");
        let _ = sse_thread.join();
    }));
    let _ = daemon.kill();
    let _ = daemon.wait();
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

/// G4 breadth — `POST /steer`: the explicit browser steer endpoint queues
/// onto the session's steering queue (consumed by the live or next turn);
/// missing fields are a 400.
#[test]
fn g4_steer_endpoint_queues_text() {
    let td = tempfile::tempdir().unwrap();
    std::fs::write(td.path().join("notes.md"), "# notes\nsteer endpoint\n").unwrap();
    let (mut daemon, addr) = spawn_daemon_env(td.path(), &[("OKRA_DEMO_DELAY_MS", "1200")]);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // malformed steer → 400
        let (status, _) = http_post(&addr, "/steer", &serde_json::json!({ "text": "x" }));
        assert_eq!(status, 400);

        let frames = std::sync::Arc::new(mutex_vec());
        let writer_frames = std::sync::Arc::clone(&frames);
        let addr_owned = addr.clone();
        let sse_thread = std::thread::spawn(move || {
            sse_collect(&addr_owned, "steer-endpoint", &writer_frames, &|frames| {
                completed_with_steered(frames, "[steered] endpoint steer landed")
            })
        });
        std::thread::sleep(Duration::from_millis(300));

        // steer BEFORE the turn exists: queued, consumed by the next turn
        let (status, reply) = http_post(&addr, "/steer", &serde_json::json!({
            "sessionId": "steer-endpoint",
            "text": "endpoint steer landed"
        }));
        assert_eq!(status, 200, "{reply}");
        assert_eq!(reply["steered"], true, "{reply}");

        let (status, reply) = http_post_command(&addr, &serde_json::json!({
            "commandId": "steer-endpoint-1",
            "type": "sendText",
            "sessionId": "steer-endpoint",
            "payload": { "text": "summarize notes.md" }
        }));
        assert_eq!(status, 200, "{reply}");

        let got = wait_for(&frames, &|frames| {
            completed_with_steered(frames, "[steered] endpoint steer landed")
        });
        assert!(got, "POST /steer text never reached a turn; frames: {frames:?}");
        let _ = sse_thread.join();
    }));
    let _ = daemon.kill();
    let _ = daemon.wait();
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

/// G4 breadth — the served page is a real steering surface: subscribes via
/// EventSource, renders projections, and posts commands/steers. When node
/// is available the inline script is also syntax-checked (the original demo
/// page carried a SyntaxError — a newline inside a string literal — that
/// only a real parse catches).
#[test]
fn g4_demo_page_is_a_steering_surface() {
    let td = tempfile::tempdir().unwrap();
    let (mut daemon, addr) = spawn_daemon(td.path());
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let (status, page) = http_get(&addr, "/");
        assert_eq!(status, 200);
        for marker in [
            "EventSource(",
            "post('/command'",
            "post('/steer'",
            "id=\"prompt\"",
            "id=\"steer\"",
            "id=\"session\"",
            "onmessage",
            "/scenes",
        ] {
            assert!(page.contains(marker), "page missing `{marker}`");
        }

        // syntax-check the inline script with node, when node exists
        let script = page
            .split("<script>")
            .nth(1)
            .and_then(|s| s.split("</script>").next())
            .expect("page has an inline script");
        let js = tempfile::Builder::new().suffix(".js").tempfile().unwrap();
        std::fs::write(js.path(), script).unwrap();
        match Command::new("node").arg("--check").arg(js.path()).output() {
            Ok(out) => {
                assert!(
                    out.status.success(),
                    "demo page script does not parse: {}",
                    String::from_utf8_lossy(&out.stderr)
                );
            }
            Err(_) => eprintln!("node not available; skipped script syntax check"),
        }
    }));
    let _ = daemon.kill();
    let _ = daemon.wait();
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}
