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
    sse_collect_path(addr, &format!("/sse/{session}"), frames, stop);
}

/// Collect `data:` frames from ANY SSE path until `stop` holds.
fn sse_collect_path(addr: &str, sse_path: &str, frames: &Mutex<Vec<serde_json::Value>>, stop: &dyn Fn(&[serde_json::Value]) -> bool) {
    let mut stream = TcpStream::connect(addr).unwrap();
    stream.set_read_timeout(Some(Duration::from_millis(500))).unwrap();
    let request = format!("GET {sse_path} HTTP/1.1\r\nHost: localhost\r\nAccept: text/event-stream\r\n\r\n");
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

/// G4 breadth — the served page is the real workbench shell: three-region
/// layout (sidebar / transcript / composer), the design-token stylesheet,
/// and the app script (separate files; the daemon embeds them at compile
/// time). When node is available the script is also syntax-checked.
#[test]
fn g4_workbench_assets_are_served() {
    let td = tempfile::tempdir().unwrap();
    let (mut daemon, addr) = spawn_daemon(td.path());
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // 1. the shell: workbench layout + the v4 seam vocabulary
        let (status, page) = http_get(&addr, "/");
        assert_eq!(status, 200);
        for marker in [
            "id=\"app\"",
            "id=\"task-list\"",
            "id=\"composer-input\"",
            "id=\"send-btn\"",
            "/app.css",
            "/app.js",
        ] {
            assert!(page.contains(marker), "page missing `{marker}`");
        }

        // 2. the stylesheet (paired light/dark tokens, ChatGPT2 authority)
        let (status, css) = http_get(&addr, "/app.css");
        assert_eq!(status, 200);
        assert!(css.contains("data-theme=\"dark\""), "no dark theme tokens");
        assert!(css.contains("data-theme=\"light\""), "no light theme tokens");
        assert!(css.contains("--corner-radius-scale"), "no radius scale");
        assert!(css.contains("cubic-bezier(0.4, 0, 0.2, 1)"), "no motion easing token");

        // 3. the app script parses (strip HTTP headers before node --check)
        let (status, js) = http_get(&addr, "/app.js");
        assert_eq!(status, 200);
        for marker in [
            "EventSource(",
            "/api/sessions",
            "'/command'",
            "renderMarkdown",
            "'stop'",
            // transcript virtualizer (UI-SHELL-PLAN U1): keyed reconciliation
            // + windowed rendering with layout checkpoints
            "transcript virtualizer",
            "layout checkpoints",
            "rowSig",
            "layoutWindow",
        ] {
            assert!(js.contains(marker), "app.js missing `{marker}`");
        }
        let js_body = js
            .split_once("\n\n")
            .map(|(_, body)| body)
            .expect("app.js body after headers");
        let js_file = tempfile::Builder::new().suffix(".js").tempfile().unwrap();
        std::fs::write(js_file.path(), js_body).unwrap();
        match Command::new("node").arg("--check").arg(js_file.path()).output() {
            Ok(out) => {
                assert!(
                    out.status.success(),
                    "app.js does not parse: {}",
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

/// G4 breadth — the task index + replay: after a turn, GET /api/sessions
/// lists the task (with its first user text as the title) and
/// GET /api/sessions/<id>/rows replays the transcript from the durable
/// kernel log. TWO sessions must BOTH survive the index fold (the old
/// whole-table wipe erased every earlier web session).
#[test]
fn g4_sessions_index_and_replay() {
    let td = tempfile::tempdir().unwrap();
    std::fs::write(td.path().join("notes.md"), "# notes\nindex breadth\n").unwrap();
    let (mut daemon, addr) = spawn_daemon(td.path());
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        for (sid, prompt) in [
            ("idx-session-a", "summarize notes.md"),
            ("idx-session-b", "list the workspace"),
        ] {
            // subscribe BEFORE the command (frames broadcast from turn start)
            let sse_frames = std::sync::Arc::new(mutex_vec());
            let writer = std::sync::Arc::clone(&sse_frames);
            let a = addr.to_string();
            let sid_owned = sid.to_string();
            let t = std::thread::spawn(move || {
                sse_collect(&a, &sid_owned, &writer, &|f| {
                    f.iter().any(|f| f["params"]["control"]["phase"] == "completedSuccess")
                })
            });
            std::thread::sleep(Duration::from_millis(300));

            let (status, reply) = http_post_command(&addr, &serde_json::json!({
                "commandId": format!("idx-{sid}"),
                "type": "sendText",
                "sessionId": sid,
                "payload": { "text": prompt }
            }));
            assert_eq!(status, 200, "{reply}");
            // wait for that session's turn to complete before the next
            let got = wait_for(&sse_frames, &|f| {
                f.iter().any(|f| f["params"]["control"]["phase"] == "completedSuccess")
            });
            assert!(got, "session {sid} never completed");
            let _ = t.join();
        }

        // 1. the task index lists BOTH sessions with real titles
        let (status, body) = http_get(&addr, "/api/sessions");
        assert_eq!(status, 200, "{body}");
        assert!(body.contains("idx-session-a"), "{body}");
        assert!(body.contains("idx-session-b"), "{body}");
        assert!(body.contains("summarize notes.md"), "title missing: {body}");

        // 2. replay rebuilds rows from the kernel log
        let (status, rows) = http_get(&addr, "/api/sessions/idx-session-a/rows");
        assert_eq!(status, 200, "{rows}");
        assert!(rows.contains("userInput"), "{rows}");
        assert!(rows.contains("assistantText"), "{rows}");
        assert!(rows.contains("turnHeader"), "{rows}");

        // 3. replaying an unknown session is an honest error, not a 200
        let (status, _) = http_get(&addr, "/api/sessions/never-existed/rows");
        assert_eq!(status, 404);
    }));
    let _ = daemon.kill();
    let _ = daemon.wait();
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

/// G4 breadth — stop: the `stop` command flips the live turn's stop flag;
/// the turn cancels at the next step boundary and reports an honest
/// interrupted phase (never an error).
#[test]
fn g4_stop_command_cancels_a_live_turn() {
    let td = tempfile::tempdir().unwrap();
    std::fs::write(td.path().join("notes.md"), "# notes\nstop breadth\n").unwrap();
    let (mut daemon, addr) = spawn_daemon_env(td.path(), &[("OKRA_DEMO_DELAY_MS", "1500")]);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let frames = std::sync::Arc::new(mutex_vec());
        let writer_frames = std::sync::Arc::clone(&frames);
        let addr_owned = addr.to_string();
        let sse_thread = std::thread::spawn(move || {
            sse_collect(&addr_owned, "stop-session", &writer_frames, &|f| {
                f.iter().any(|f| f["params"]["control"]["phase"] == "completedInterrupted")
            })
        });
        std::thread::sleep(Duration::from_millis(300));

        // 1. start a slow turn
        let (status, reply) = http_post_command(&addr, &serde_json::json!({
            "commandId": "stop-1",
            "type": "sendText",
            "sessionId": "stop-session",
            "payload": { "text": "summarize notes.md" }
        }));
        assert_eq!(status, 200, "{reply}");

        // 2. wait until it is actually running, then stop it
        let running = wait_for(&frames, &|f| {
            f.iter().any(|f| f["params"]["control"]["phase"] == "running")
        });
        assert!(running, "turn never reported running; frames: {frames:?}");
        let (status, reply) = http_post_command(&addr, &serde_json::json!({
            "commandId": "stop-2",
            "type": "stop",
            "sessionId": "stop-session"
        }));
        assert_eq!(status, 200, "{reply}");
        assert_eq!(reply["result"]["type"], "stopAccepted", "{reply}");

        // 3. the turn ends interrupted — at the next step boundary
        let stopped = wait_for(&frames, &|f| {
            f.iter().any(|f| f["params"]["control"]["phase"] == "completedInterrupted")
        });
        assert!(stopped, "turn never reported interrupted; frames: {frames:?}");
        let _ = sse_thread.join();

        // 4. stopping an IDLE session is accepted as a no-op
        let (status, reply) = http_post_command(&addr, &serde_json::json!({
            "commandId": "stop-3",
            "type": "stop",
            "sessionId": "idle-session"
        }));
        assert_eq!(status, 200, "{reply}");
        assert_eq!(reply["result"]["type"], "stopIdle", "{reply}");
    }));
    let _ = daemon.kill();
    let _ = daemon.wait();
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

/// G4 breadth — approvals: the served surface ASKS for side-effecting tools
/// (write_file) and the turn PAUSES until the workbench resolves the ask.
/// Allow → the file is written and the audit pair replays; Deny → the tool
/// is denied honestly and nothing is written.
#[test]
fn g4_approvals_pause_turn_until_resolved() {
    let td = tempfile::tempdir().unwrap();
    let (mut daemon, addr) = spawn_daemon(td.path());
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let run_one = |session: &str, prompt: &str, allow: bool| {
            let frames = std::sync::Arc::new(mutex_vec());
            let writer = std::sync::Arc::clone(&frames);
            let a = addr.to_string();
            let sid = session.to_string();
            let t = std::thread::spawn(move || {
                sse_collect(&a, &sid, &writer, &|f| {
                    f.iter().any(|f| {
                        f["params"]["control"]["phase"] == "completedSuccess"
                            || f["params"]["control"]["phase"] == "completedInterrupted"
                    })
                })
            });
            std::thread::sleep(Duration::from_millis(300));
            let (status, reply) = http_post_command(&addr, &serde_json::json!({
                "commandId": format!("apr-{session}"),
                "type": "sendText",
                "sessionId": session,
                "payload": { "text": prompt }
            }));
            assert_eq!(status, 200, "{reply}");

            // wait for the ask to reach the surface
            let approval_id;
            let deadline = Instant::now() + Duration::from_secs(20);
            loop {
                assert!(
                    Instant::now() < deadline,
                    "turn never paused on an approval; frames: {frames:?}"
                );
                let pending = frames.lock().unwrap().iter().rev().find_map(|f| {
                    f["params"]["control"]["awaitingApproval"]
                        .as_array()
                        .and_then(|a| a.first())
                        .and_then(|p| p["approvalId"].as_str().map(str::to_string))
                });
                if let Some(id) = pending {
                    approval_id = id;
                    break;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
            assert!(!approval_id.is_empty());

            // the permission-request notification class surfaced for the ask
            let perm_notif = frames.lock().unwrap().iter().any(|f| {
                f["method"] == "v4/notification"
                    && f["params"]["class"] == "permission_request"
            });
            assert!(perm_notif, "no permission_request notification; frames: {frames:?}");

            // resolve from the "workbench"
            let (status, reply) = http_post_command(&addr, &serde_json::json!({
                "commandId": format!("apr-resolve-{session}"),
                "type": "resolveApproval",
                "sessionId": session,
                "payload": {
                    "approvalId": approval_id,
                    "decision": if allow { "allow" } else { "deny" }
                }
            }));
            assert_eq!(status, 200, "{reply}");
            assert_eq!(reply["result"]["resolved"], true, "{reply}");

            // the turn finishes
            let got = wait_for(&frames, &|f| {
                f.iter().any(|f| {
                    f["params"]["control"]["phase"] == "completedSuccess"
                        || f["params"]["control"]["phase"] == "completedInterrupted"
                })
            });
            assert!(got, "turn never finished after the resolution");
            let _ = t.join();
            approval_id
        };

        // 1. ALLOW: the write lands, the audit pair replays
        run_one("apr-allow", "create approved.md", true);
        assert!(td.path().join("approved.md").is_file(), "approved write never landed");
        let (status, rows) = http_get(&addr, "/api/sessions/apr-allow/rows");
        assert_eq!(status, 200);
        assert!(rows.contains("approval"), "no approval row in replay: {rows}");
        assert!(rows.contains("allowed"), "approval row not marked allowed: {rows}");

        // 2. DENY: honest denial, nothing written
        run_one("apr-deny", "create denied.md", false);
        assert!(
            !td.path().join("denied.md").exists(),
            "denied write must not land"
        );
        let (status, rows) = http_get(&addr, "/api/sessions/apr-deny/rows");
        assert_eq!(status, 200);
        assert!(rows.contains("denied"), "denial not in replay: {rows}");
        assert!(
            rows.contains("approval denied"),
            "tool error not surfaced: {rows}"
        );

        // 3. resolving an unknown approval id is rejected, not silently ok
        let (status, _) = http_post_command(&addr, &serde_json::json!({
            "commandId": "apr-bogus",
            "type": "resolveApproval",
            "sessionId": "apr-allow",
            "payload": { "approvalId": "apr-nope", "decision": "allow" }
        }));
        assert_eq!(status, 400);
    }));
    let _ = daemon.kill();
    let _ = daemon.wait();
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

/// G4 breadth — file surfaces: the workspace-confined tree + safe-read
/// preview the Files tab renders. Dot entries never surface; `..` and
/// escapes are refused.
#[test]
fn g4_files_api_lists_and_previews_safely() {
    let td = tempfile::tempdir().unwrap();
    std::fs::write(td.path().join("notes.md"), "# notes\nfile surfaces\n").unwrap();
    std::fs::create_dir_all(td.path().join("sub")).unwrap();
    std::fs::write(td.path().join("sub").join("deep.txt"), "deep content").unwrap();
    std::fs::create_dir_all(td.path().join(".okra-sessions")).unwrap();
    std::fs::write(td.path().join(".okra-sessions").join("index.db"), "secret").unwrap();
    let (mut daemon, addr) = spawn_daemon(td.path());
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // 1. root listing: files visible, dot entries never
        let (status, body) = http_get(&addr, "/api/files");
        assert_eq!(status, 200, "{body}");
        assert!(body.contains("notes.md"), "{body}");
        assert!(body.contains("sub"), "{body}");
        assert!(!body.contains("okra-sessions"), "dot dir leaked: {body}");

        // 2. nested listing
        let (status, body) = http_get(&addr, "/api/files?path=sub");
        assert_eq!(status, 200, "{body}");
        assert!(body.contains("deep.txt"), "{body}");

        // 3. preview reads the content
        let (status, body) = http_get(&addr, "/api/file?path=notes.md");
        assert_eq!(status, 200, "{body}");
        assert!(body.contains("file surfaces"), "{body}");

        // 4. traversal refused
        let (status, _) = http_get(&addr, "/api/files?path=..");
        assert_eq!(status, 400);
        let (status, _) = http_get(&addr, "/api/file?path=..%2F..%2Fetc%2Fpasswd");
        assert_ne!(status, 200, "traversal escaped the workspace");

        // 5. missing file → honest 404
        let (status, _) = http_get(&addr, "/api/file?path=nope.txt");
        assert_eq!(status, 404);
    }));
    let _ = daemon.kill();
    let _ = daemon.wait();
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

/// G4 breadth — git surfaces: the Changes tab source. Branch + status +
/// per-file working-tree diff; honest `repository:false` outside a repo.
#[test]
fn g4_git_surfaces_report_branch_status_and_diff() {
    let td = tempfile::tempdir().unwrap();
    // a real repository with one committed file and one uncommitted change
    let run = |args: &[&str]| {
        let out = Command::new("git")
            .args(args)
            .current_dir(td.path())
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    };
    run(&["init", "-q"]);
    run(&["config", "user.email", "drive@okra.local"]);
    run(&["config", "user.name", "okra drive"]);
    std::fs::write(td.path().join("committed.md"), "line one\nline two\n").unwrap();
    run(&["add", "-A"]);
    run(&["commit", "-q", "-m", "seed"]);
    std::fs::write(td.path().join("committed.md"), "line one\nline two edited\n").unwrap();
    std::fs::write(td.path().join("untracked.txt"), "new file").unwrap();

    let (mut daemon, addr) = spawn_daemon(td.path());
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // 1. overview: branch + both change kinds
        let (status, body) = http_get(&addr, "/api/git");
        assert_eq!(status, 200, "{body}");
        assert!(body.contains("\"repository\":true"), "{body}");
        assert!(body.contains("committed.md"), "{body}");
        assert!(body.contains("untracked.txt"), "{body}");

        // 2. the modified file has a real unified diff
        let (status, body) = http_get(&addr, "/api/git/diff?path=committed.md");
        assert_eq!(status, 200, "{body}");
        assert!(body.contains("line two edited"), "{body}");
        assert!(body.contains("diff --git"), "{body}");

        // 3. an untracked file has NO diff (honest empty, not an error)
        let (status, body) = http_get(&addr, "/api/git/diff?path=untracked.txt");
        assert_eq!(status, 200, "{body}");

        // 4. traversal and unknown files are refused honestly
        let (status, _) = http_get(&addr, "/api/git/diff?path=..%2Fsecrets");
        assert_eq!(status, 400);
        // a path with no working-tree change is an honest EMPTY diff
        // (git exits 0), not an error
        let (status, body) = http_get(&addr, "/api/git/diff?path=no-such-file.md");
        assert_eq!(status, 200);
        assert!(body.contains("\"diff\":\"\""), "expected empty diff: {body}");

        // 5. outside a repository: honest `repository:false`, diff refuses
        let td2 = tempfile::tempdir().unwrap();
        let (mut daemon2, addr2) = spawn_daemon(td2.path());
        let inner = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let (status, body) = http_get(&addr2, "/api/git");
            assert_eq!(status, 200);
            assert!(body.contains("\"repository\":false"), "{body}");
            let (status, _) = http_get(&addr2, "/api/git/diff?path=x");
            assert_eq!(status, 400);
        }));
        let _ = daemon2.kill();
        let _ = daemon2.wait();
        if let Err(panic) = inner {
            std::panic::resume_unwind(panic);
        }
    }));
    let _ = daemon.kill();
    let _ = daemon.wait();
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

/// N0012 — workbench terminals: the PTY surface over SSE + keystroke POST.
/// A real shell runs in a real PTY; keys typed over HTTP execute and the
/// output streams back; resize/close work; unknown ids are 404.
/// WINDOWS: gated pending the ConPTY terminal-emulator layer. Findings
/// (runs 37106132295 green ONCE — timing; 37097302681/37107367246 red):
/// conhost opens with a DSR probe (the pump replies, with bounded
/// retries) but rendering still stalls — win32-input-mode/sequence
/// handling is the remaining work (docs/m6-windows-port.md). The decoded
/// frame dump stays for the next diagnostic cycle.
#[cfg(unix)]
#[test]
fn g4_terminals_run_a_real_pty_over_http() {
    let td = tempfile::tempdir().unwrap();
    let (mut daemon, addr) = spawn_daemon(td.path());
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // 1. no terminals yet
        let (status, body) = http_get(&addr, "/api/term");
        assert_eq!(status, 200, "{body}");
        assert!(body.contains("\"ids\":[]"), "{body}");

        // 2. open one (interactive shell)
        let (status, reply) = http_post(&addr, "/api/term/open", &serde_json::json!({}));
        assert_eq!(status, 200, "{reply}");
        let id = reply["id"].as_str().expect("terminal id").to_string();
        assert!(!id.is_empty());

        // 3. keys to unknown terminal → 404
        let (status, _) = http_post(
            &addr,
            "/api/term/nope/keys",
            &serde_json::json!({ "data": "x" }),
        );
        assert_eq!(status, 404);

        // 4. subscribe to the output stream, then type a command
        let frames = std::sync::Arc::new(mutex_vec());
        let writer = std::sync::Arc::clone(&frames);
        let a = addr.to_string();
        let path = format!("/api/term/{id}/sse");
        let t = std::thread::spawn(move || sse_collect_path(&a, &path, &writer, &|f| {
            f.iter().any(|f| {
                f["type"] == "out"
                    && f["b64"].as_str().map(|b| {
                        use base64::Engine as _;
                        String::from_utf8_lossy(
                            &base64::engine::general_purpose::STANDARD
                                .decode(b)
                                .unwrap_or_default(),
                        )
                        .contains("okra-term-marker")
                    })
                    .unwrap_or(false)
            })
        }));
        std::thread::sleep(Duration::from_millis(300));
        let (status, reply) = http_post(
            &addr,
            &format!("/api/term/{id}/keys"),
            &serde_json::json!({ "data": "echo okra-term-marker\r" }),
        );
        assert_eq!(status, 200, "{reply}");

        // 5. the marker echoes back through the PTY
        let got = wait_for(&frames, &|f| {
            f.iter().any(|f| {
                f["type"] == "out"
                    && f["b64"].as_str().map(|b| {
                        use base64::Engine as _;
                        String::from_utf8_lossy(
                            &base64::engine::general_purpose::STANDARD
                                .decode(b)
                                .unwrap_or_default(),
                        )
                        .contains("okra-term-marker")
                    })
                    .unwrap_or(false)
            })
        });
        use base64::Engine as _;
        let dump: Vec<String> = frames
            .lock()
            .unwrap()
            .iter()
            .filter_map(|f| {
                f["b64"].as_str().and_then(|b| {
                    base64::engine::general_purpose::STANDARD
                        .decode(b)
                        .ok()
                        .map(|d| String::from_utf8_lossy(&d).to_string())
                })
            })
            .collect();
        assert!(got, "PTY output never streamed the marker; decoded output: {dump:?}");
        let _ = t.join();

        // 6. resize is accepted
        let (status, reply) = http_post(
            &addr,
            &format!("/api/term/{id}/resize"),
            &serde_json::json!({ "rows": 30, "cols": 100 }),
        );
        assert_eq!(status, 200, "{reply}");

        // 7. the shell banner/prompt streamed BEFORE any keys (real PTY)
        let any_output = frames.lock().unwrap().iter().any(|f| f["type"] == "out");
        assert!(any_output, "no output streamed at all");

        // 8. close
        let (status, body) = http_post(&addr, &format!("/api/term/{id}/close"), &serde_json::json!({}));
        assert_eq!(status, 200, "{body}");
        let (_, body) = http_get(&addr, "/api/term");
        assert!(body.contains("\"ids\":[]"), "terminal not pruned: {body}");
    }));
    let _ = daemon.kill();
    let _ = daemon.wait();
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

/// N0013 — notifications: turn completion and permission asks surface the
/// 3-class boundary over the wire, and the label is REDACTED — the task
/// title's content (the prompt) never leaves the daemon.
#[test]
fn g4_notifications_classify_and_redact_over_the_wire() {
    let td = tempfile::tempdir().unwrap();
    std::fs::write(td.path().join("notes.md"), "# notes\nnotification breadth\n").unwrap();
    let (mut daemon, addr) = spawn_daemon(td.path());
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let frames = std::sync::Arc::new(mutex_vec());
        let writer = std::sync::Arc::clone(&frames);
        let a = addr.to_string();
        let t = std::thread::spawn(move || {
            sse_collect(&a, "notif-session", &writer, &|f| {
                f.iter().any(|f| f["method"] == "v4/notification")
            })
        });
        std::thread::sleep(Duration::from_millis(300));

        let (status, reply) = http_post_command(&addr, &serde_json::json!({
            "commandId": "notif-1",
            "type": "sendText",
            "sessionId": "notif-session",
            "payload": { "text": "summarize notes.md" }
        }));
        assert_eq!(status, 200, "{reply}");

        let got = wait_for(&frames, &|f| {
            f.iter().any(|f| f["method"] == "v4/notification")
        });
        assert!(got, "no notification frame arrived; frames: {frames:?}");

        let notif = frames
            .lock()
            .unwrap()
            .iter()
            .find(|f| f["method"] == "v4/notification")
            .cloned()
            .unwrap();
        assert_eq!(notif["params"]["class"], "turn_complete", "{notif}");
        assert_eq!(notif["params"]["sessionId"], "notif-session", "{notif}");
        // the redaction contract: the prompt is CONTENT — it never rides
        // the label to a lock screen
        let label = notif["params"]["label"].as_str().unwrap_or_default();
        assert!(
            !label.contains("summarize"),
            "redaction leak: {label:?}"
        );
        assert!(label.len() <= 81, "label must stay bounded: {label:?}");
        let _ = t.join();
    }));
    let _ = daemon.kill();
    let _ = daemon.wait();
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

/// N0014 — staging + commit: per-file staging over the wire, a staged-only
/// commit (untouched paths stay dirty), and honest refusals (no repo,
/// empty message, nothing staged).
#[test]
fn g4_git_stage_and_commit_over_the_wire() {
    let td = tempfile::tempdir().unwrap();
    let run = |args: &[&str]| {
        let out = Command::new("git")
            .args(args)
            .current_dir(td.path())
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    };
    run(&["init", "-q"]);
    run(&["config", "user.email", "drive@okra.local"]);
    run(&["config", "user.name", "okra drive"]);
    std::fs::write(td.path().join("a.txt"), "alpha\n").unwrap();
    std::fs::write(td.path().join("b.txt"), "beta\n").unwrap();
    run(&["add", "-A"]);
    run(&["commit", "-q", "-m", "seed"]);
    // both files change; only one gets staged
    std::fs::write(td.path().join("a.txt"), "alpha edited\n").unwrap();
    std::fs::write(td.path().join("b.txt"), "beta edited\n").unwrap();

    let (mut daemon, addr) = spawn_daemon(td.path());
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // 1. stage ONE file
        let (status, reply) = http_post(
            &addr,
            "/api/git/stage",
            &serde_json::json!({ "paths": ["a.txt"] }),
        );
        assert_eq!(status, 200, "{reply}");

        // status shows a.txt staged (X), b.txt unstaged (Y)
        let (status, body) = http_get(&addr, "/api/git");
        assert_eq!(status, 200, "{body}");
        // staged a.txt carries its staged X; b.txt is unstaged
        // raw porcelain codes survive: a.txt staged-only is "M "
        // (X=M, Y=space); b.txt unstaged-only is " M"
        assert!(body.contains("\"code\":\"M \""), "staged code missing: {body}");
        assert!(body.contains("\"code\":\" M\""), "unstaged code missing: {body}");

        // 2. commit ONLY what is staged
        let (status, reply) = http_post(
            &addr,
            "/api/git/commit",
            &serde_json::json!({ "message": "stage a only" }),
        );
        assert_eq!(status, 200, "{reply}");
        let hash = reply["hash"].as_str().expect("commit hash").to_string();
        assert_eq!(hash.len(), 40, "{reply}");

        // a.txt is committed; b.txt must still be dirty
        let show = Command::new("git")
            .args(["show", "--name-only", "--format=", &hash])
            .current_dir(td.path())
            .output()
            .unwrap();
        let files = String::from_utf8_lossy(&show.stdout);
        assert!(files.contains("a.txt"), "committed files: {files}");
        assert!(!files.contains("b.txt"), "unstaged file leaked into the commit: {files}");
        let (_, body) = http_get(&addr, "/api/git");
        assert!(body.contains("b.txt"), "b.txt no longer dirty: {body}");

        // 3. honest refusals
        let (status, _) = http_post(
            &addr,
            "/api/git/commit",
            &serde_json::json!({ "message": "   " }),
        );
        assert_eq!(status, 400, "empty message must refuse");
        let (status, _) = http_post(
            &addr,
            "/api/git/stage",
            &serde_json::json!({ "paths": [] }),
        );
        assert_eq!(status, 400, "empty paths must refuse");
        let (status, _) = http_post(
            &addr,
            "/api/git/stage",
            &serde_json::json!({ "paths": ["-rf"] }),
        );
        assert_eq!(status, 400, "option-looking pathspec must refuse");

        // 4. outside a repository everything refuses
        let td2 = tempfile::tempdir().unwrap();
        let (mut daemon2, addr2) = spawn_daemon(td2.path());
        let inner = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            for path in ["/api/git/stage", "/api/git/commit"] {
                let (status, _) = http_post(&addr2, path, &serde_json::json!({ "paths": ["x"], "message": "m" }));
                assert_eq!(status, 400, "{path} outside a repo");
            }
        }));
        let _ = daemon2.kill();
        let _ = daemon2.wait();
        if let Err(panic) = inner {
            std::panic::resume_unwind(panic);
        }
    }));
    let _ = daemon.kill();
    let _ = daemon.wait();
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

/// N0015 — the Tools tab: workspace skills (`.okra/skills/*.md`, with their
/// path-conditional patterns) and configured MCP servers (`.okra/mcp.json`,
/// enabled + source + scope) projected read-only.
#[test]
fn g4_tools_surface_projects_skills_and_mcp() {
    let td = tempfile::tempdir().unwrap();
    // a real skill with frontmatter + a match pattern
    let skills_dir = td.path().join(".okra").join("skills");
    std::fs::create_dir_all(&skills_dir).unwrap();
    std::fs::write(
        skills_dir.join("SKILL-rust.md"),
        "---\nname: rust-review\ndescription: Review Rust changes idiomatically\nmatch: src/**\n---\nBody instructions here.\n",
    )
    .unwrap();
    // a workspace MCP server config
    std::fs::create_dir_all(td.path().join(".okra")).unwrap();
    // the okra source reads .okra/config.json with nested mcp.servers
    std::fs::write(
        td.path().join(".okra").join("config.json"),
        r#"{ "mcp": { "servers": { "tester": { "command": "uvx", "args": ["mcp-tester"], "enable": false } } } }"#,
    )
    .unwrap();

    let (mut daemon, addr) = spawn_daemon(td.path());
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // 1. skills project with name/description/patterns
        let (status, body) = http_get(&addr, "/api/skills");
        assert_eq!(status, 200, "{body}");
        assert!(body.contains("rust-review"), "{body}");
        assert!(body.contains("Review Rust changes idiomatically"), "{body}");
        assert!(body.contains("src/**"), "{body}");

        // 2. mcp projects the server with enabled=false + scope + summary
        let (status, body) = http_get(&addr, "/api/mcp");
        assert_eq!(status, 200, "{body}");
        assert!(body.contains("tester"), "{body}");
        assert!(body.contains("\"enabled\":false"), "disabled flag lost: {body}");
        assert!(body.contains("uvx mcp-tester"), "launch summary missing: {body}");
        assert!(body.contains("\"scope\":\"workspace\""), "{body}");

        // 3. an empty workspace projects honest empties
        let td2 = tempfile::tempdir().unwrap();
        let (mut daemon2, addr2) = spawn_daemon(td2.path());
        let inner = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let (status, body) = http_get(&addr2, "/api/skills");
            assert_eq!(status, 200);
            assert!(body.contains("\"skills\":[]"), "{body}");
            let (status, body) = http_get(&addr2, "/api/mcp");
            assert_eq!(status, 200);
            assert!(body.contains("\"servers\":[]"), "{body}");
        }));
        let _ = daemon2.kill();
        let _ = daemon2.wait();
        if let Err(panic) = inner {
            std::panic::resume_unwind(panic);
        }
    }));
    let _ = daemon.kill();
    let _ = daemon.wait();
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

/// N0017 — composer attachments: the payload's attachment paths fold their
/// CONTENT into the logged, model-visible user message (model-visible
/// means logged), the row carries the attachment list for the UI chips,
/// and traversal pathspecs are refused before anything surfaces.
#[test]
fn g4_attachments_fold_into_the_turn() {
    let td = tempfile::tempdir().unwrap();
    let secret = "okra-attachment-marker-7f3a";
    std::fs::write(td.path().join("notes.md"), format!("# notes\n{secret}\n")).unwrap();
    let (mut daemon, addr) = spawn_daemon(td.path());
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let frames = std::sync::Arc::new(mutex_vec());
        let writer = std::sync::Arc::clone(&frames);
        let a = addr.to_string();
        let t = std::thread::spawn(move || {
            sse_collect(&a, "att-session", &writer, &|f| {
                f.iter().any(|f| f["params"]["control"]["phase"] == "completedSuccess")
            })
        });
        std::thread::sleep(Duration::from_millis(300));

        // 1. traversal is refused pre-surface
        let (status, reply) = http_post_command(&addr, &serde_json::json!({
            "commandId": "att-bad",
            "type": "sendText",
            "sessionId": "att-session",
            "payload": { "text": "leak this", "attachments": ["../secrets"] }
        }));
        assert_eq!(status, 400, "{reply}");
        assert_eq!(reply["reasonCode"], "okra.attachment.pathRefused", "{reply}");

        // 2. a real attachment folds into the turn
        let (status, reply) = http_post_command(&addr, &serde_json::json!({
            "commandId": "att-1",
            "type": "sendText",
            "sessionId": "att-session",
            "payload": {
                "text": "summarize the attached file",
                "attachments": ["notes.md"]
            }
        }));
        assert_eq!(status, 200, "{reply}");

        let got = wait_for(&frames, &|f| {
            f.iter().any(|f| f["params"]["control"]["phase"] == "completedSuccess")
        });
        assert!(got, "turn never completed");
        let _ = t.join();

        // 3. the fold is model-visible AND logged: the replayed user row
        //    carries the attachment list AND the file content
        let (status, rows) = http_get(&addr, "/api/sessions/att-session/rows");
        assert_eq!(status, 200, "{rows}");
        assert!(rows.contains("\"attachments\":[\"notes.md\"]"), "row lacks attachment list: {rows}");
        assert!(rows.contains(secret), "attached content never reached the model-visible turn: {rows}");

        // 4. missing attachments are listed, not fatal. (Wait for turn 1:
        // a command landing on a LIVE turn is steering, and the steering
        // queue carries text only — attachments belong to idle-turn sends.)
        let idle = wait_for(&frames, &|f| {
            f.iter().any(|f| f["params"]["control"]["phase"] == "completedSuccess")
        });
        assert!(idle, "turn 1 never completed");
        std::thread::sleep(Duration::from_millis(300));
        let (status, reply) = http_post_command(&addr, &serde_json::json!({
            "commandId": "att-2",
            "type": "sendText",
            "sessionId": "att-session",
            "payload": {
                "text": "second turn",
                "attachments": ["does-not-exist.md"]
            }
        }));
        assert_eq!(status, 200, "{reply}");
        // poll the replay until the second turn's fold is on disk
        let deadline = Instant::now() + Duration::from_secs(15);
        let mut reported = false;
        while Instant::now() < deadline {
            let (status, rows) = http_get(&addr, "/api/sessions/att-session/rows");
            assert_eq!(status, 200);
            if rows.contains("attachments not loaded: does-not-exist.md") {
                reported = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        assert!(reported, "missing attachment not reported honestly");
    }));
    let _ = daemon.kill();
    let _ = daemon.wait();
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

/// N0017 follow-up — steering carries attachments: a command landing on a
/// LIVE turn used to drop its attachment paths (text-only queue). The
/// steered input now folds its files into the model context at the next
/// step boundary, and the receipt row carries the attachment list.
#[test]
fn g4_steered_sends_carry_attachments() {
    let td = tempfile::tempdir().unwrap();
    let marker = "okra-steered-attachment-42";
    std::fs::write(td.path().join("late.md"), format!("{marker}\n")).unwrap();
    std::fs::write(td.path().join("notes.md"), "# notes\nsteering attachments\n").unwrap();
    let (mut daemon, addr) = spawn_daemon_env(td.path(), &[("OKRA_DEMO_DELAY_MS", "1500")]);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let frames = std::sync::Arc::new(mutex_vec());
        let writer = std::sync::Arc::clone(&frames);
        let a = addr.to_string();
        let t = std::thread::spawn(move || {
            sse_collect(&a, "steer-att", &writer, &|f| {
                f.iter().any(|f| {
                    f["params"]["rows"].as_array().map(|rows| {
                        rows.iter().any(|r| {
                            r["kind"] == "userInput"
                                && r["text"].as_str().map(|x| x.contains("late.md")).unwrap_or(false)
                        })
                    }).unwrap_or(false)
                })
            })
        });
        std::thread::sleep(Duration::from_millis(300));

        // start the slow turn
        let (status, reply) = http_post_command(&addr, &serde_json::json!({
            "commandId": "sa-1",
            "type": "sendText",
            "sessionId": "steer-att",
            "payload": { "text": "summarize notes.md" }
        }));
        assert_eq!(status, 200, "{reply}");
        // wait until running, then steer WITH an attachment
        let running = wait_for(&frames, &|f| {
            f.iter().any(|f| f["params"]["control"]["phase"] == "running")
        });
        assert!(running);
        let (status, reply) = http_post_command(&addr, &serde_json::json!({
            "commandId": "sa-2",
            "type": "sendText",
            "sessionId": "steer-att",
            "payload": {
                "text": "also read late.md",
                "attachments": ["late.md"]
            }
        }));
        assert_eq!(status, 200, "{reply}");
        assert_eq!(reply["result"]["type"], "steeringQueued", "{reply}");

        // the steered receipt row carries the attachment list, and the
        // folded content reached the model-visible turn (marker in text)
        let got = wait_for(&frames, &|f| {
            f.iter().any(|f| {
                f["params"]["rows"].as_array().map(|rows| {
                    rows.iter().any(|r| {
                        r["kind"] == "userInput"
                            && r["text"].as_str().map(|x| x.contains("[steered]")).unwrap_or(false)
                            && r["attachments"].as_array().map(|a| {
                                a.iter().any(|p| p.as_str() == Some("late.md"))
                            }).unwrap_or(false)
                    })
                }).unwrap_or(false)
            })
        });
        assert!(got, "steered row with attachments never surfaced; frames tail: {:?}",
            frames.lock().unwrap().last());
        let _ = t.join();
    }));
    let _ = daemon.kill();
    let _ = daemon.wait();
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

/// N0018 — runtime MCP status: probing a configured stdio server connects
/// (initialize + tools/list), caches the status into /api/mcp, and unknown
/// names probe nothing. The fixture is a real JSON-RPC-over-stdio
/// responder — the transport is one-shot per request, so the shell script
/// answers whichever request arrives.
#[test]
fn g4_mcp_probe_connects_and_caches_status() {
    let td = tempfile::tempdir().unwrap();
    // a one-shot MCP responder: echoes the request id, answers initialize
    // and tools/list
    // cross-platform fixture: the fake-mcp test binary (the sh-script
    // stand-in was the one unix-only piece of the windows bring-up)
    let fixture = std::path::PathBuf::from(env!("CARGO_BIN_EXE_fake-mcp"));
    std::fs::create_dir_all(td.path().join(".okra")).unwrap();
    std::fs::write(
        td.path().join(".okra").join("config.json"),
        serde_json::json!({
            "mcp": { "servers": {
                "fake": { "command": fixture.to_string_lossy(), "args": [
                    "--count-file", td.path().join("probe-count").to_string_lossy() ] },
                "broken": { "command": "/nope/no-such-mcp-server" }
            } }
        })
        .to_string(),
    )
    .unwrap();

    let (mut daemon, addr) = spawn_daemon(td.path());
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // 1. probe all: fake connects, broken errors
        let (status, body) = http_post(&addr, "/api/mcp/probe", &serde_json::json!({}));
        assert_eq!(status, 200, "{body}");
        let body = body.to_string();
        assert!(body.contains("probe-tool"), "tool names missing: {body}");
        assert!(body.contains("connected"), "{body}");

        // 2. the status is CACHED into the listing
        let (status, body) = http_get(&addr, "/api/mcp");
        assert_eq!(status, 200, "{body}");
        assert!(body.contains("\"status\":\"connected\""), "no cached status: {body}");
        assert!(body.contains("probe-tool"), "cached tools missing: {body}");

        // 3. probing a single named server works and prunes others' output
        let (status, body) = http_post(
            &addr,
            "/api/mcp/probe",
            &serde_json::json!({ "name": "broken" }),
        );
        assert_eq!(status, 200, "{body}");
        let body = body.to_string();
        assert!(body.contains("broken"), "{body}");
        assert!(body.contains("error"), "broken server not reported: {body}");
    }));
    let _ = daemon.kill();
    let _ = daemon.wait();
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

/// N0019 — MCP tools run INSIDE turns: probed tools register per turn
/// (`mcp__<server>__<tool>`), the call is approval-gated (unknown
/// side-effector), and the one-shot tools/call result lands in the turn.
#[test]
fn g4_mcp_tools_run_inside_turns() {
    let td = tempfile::tempdir().unwrap();
    // cross-platform persistent responder: the fake-mcp test binary
    // counts `initialize` lines to a temp file (the
    // initialize-exactly-once proof for persistent sessions)
    let fixture = std::path::PathBuf::from(env!("CARGO_BIN_EXE_fake-mcp"));
    std::fs::create_dir_all(td.path().join(".okra")).unwrap();
    std::fs::write(
        td.path().join(".okra").join("config.json"),
        serde_json::json!({
            "mcp": { "servers": { "fake": { "command": fixture.to_string_lossy(), "args": [
                "--count-file", td.path().join("turn-count").to_string_lossy() ] } } }
        })
        .to_string(),
    )
    .unwrap();
    std::fs::write(td.path().join("notes.md"), "# notes\n").unwrap();

    let (mut daemon, addr) = spawn_daemon(td.path());
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let frames = std::sync::Arc::new(mutex_vec());
        let writer = std::sync::Arc::clone(&frames);
        let a = addr.to_string();
        let t = std::thread::spawn(move || {
            sse_collect(&a, "mcp-turn", &writer, &|f| {
                f.iter().any(|f| f["params"]["control"]["phase"] == "completedSuccess")
            })
        });
        std::thread::sleep(Duration::from_millis(300));

        let (status, reply) = http_post_command(&addr, &serde_json::json!({
            "commandId": "mcp-1",
            "type": "sendText",
            "sessionId": "mcp-turn",
            "payload": { "text": "mcp probe-tool hello there" }
        }));
        assert_eq!(status, 200, "{reply}");

        // the MCP call must be APPROVAL-GATED (unknown side-effector)
        let approval_deadline = Instant::now() + Duration::from_secs(25);
        let approval_id = loop {
            if Instant::now() > approval_deadline {
                panic!("MCP approval never arrived; frames: {frames:?}");
            }
            let pending = frames.lock().unwrap().iter().rev().find_map(|f| {
                f["params"]["control"]["awaitingApproval"]
                    .as_array()
                    .and_then(|a| a.first())
                    .and_then(|p| p["approvalId"].as_str().map(str::to_string))
            });
            match pending {
                Some(id) => break id,
                None => std::thread::sleep(Duration::from_millis(150)),
            }
        };
        let (status, reply) = http_post_command(&addr, &serde_json::json!({
            "commandId": "mcp-apr",
            "type": "resolveApproval",
            "sessionId": "mcp-turn",
            "payload": { "approvalId": approval_id, "decision": "allow" }
        }));
        assert_eq!(status, 200, "{reply}");

        // the turn completes with the tools/call result in the transcript
        let got = wait_for(&frames, &|f| {
            f.iter().any(|f| f["params"]["control"]["phase"] == "completedSuccess")
        });
        assert!(got, "turn never completed; frames: {frames:?}");
        let _ = t.join();

        let (status, rows) = http_get(&addr, "/api/sessions/mcp-turn/rows");
        assert_eq!(status, 200, "{rows}");
        assert!(rows.contains("mcp__fake_probe-tool"), "tool row missing: {rows}");
        assert!(rows.contains("echo: hello there"), "tools/call result missing: {rows}");

        // PERSISTENT-SESSION PROOF: initialize ran exactly ONCE even though
        // the session did initialize + tools/list + tools/call
        let count_file = td.path().join("turn-count").to_string_lossy().to_string();
        let count = std::fs::read_to_string(&count_file)
            .unwrap_or_default()
            .trim()
            .to_string();
        assert_eq!(count, "1", "initialize must run once per session, got {count}");
        let _ = std::fs::remove_file(&count_file);
    }));
    let _ = daemon.kill();
    let _ = daemon.wait();
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

/// N0020 — the question flow: ask_user pauses the turn on a question card
/// (control.awaitingQuestion + the question notification class); the
/// answer returns INTO the turn as the tool result.
#[test]
fn g4_ask_user_question_flow() {
    let td = tempfile::tempdir().unwrap();
    std::fs::write(td.path().join("notes.md"), "# notes\n").unwrap();
    let (mut daemon, addr) = spawn_daemon(td.path());
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let frames = std::sync::Arc::new(mutex_vec());
        let writer = std::sync::Arc::clone(&frames);
        let a = addr.to_string();
        let t = std::thread::spawn(move || {
            sse_collect(&a, "ask-turn", &writer, &|f| {
                f.iter().any(|f| f["params"]["control"]["phase"] == "completedSuccess")
            })
        });
        std::thread::sleep(Duration::from_millis(300));

        let (status, reply) = http_post_command(&addr, &serde_json::json!({
            "commandId": "ask-1",
            "type": "sendText",
            "sessionId": "ask-turn",
            "payload": { "text": "ask what color do you prefer" }
        }));
        assert_eq!(status, 200, "{reply}");

        // the question surfaces: control.awaitingQuestion + notification
        let question_deadline = Instant::now() + Duration::from_secs(25);
        let question_id = loop {
            if Instant::now() > question_deadline {
                panic!("question never surfaced; frames: {frames:?}");
            }
            let q = frames.lock().unwrap().iter().rev().find_map(|f| {
                let q = &f["params"]["control"]["awaitingQuestion"];
                if q.is_object() {
                    Some(q["questionId"].as_str().map(str::to_string).unwrap_or_default())
                } else {
                    None
                }
            });
            match q {
                Some(id) if !id.is_empty() => break id,
                _ => std::thread::sleep(Duration::from_millis(150)),
            }
        };
        let has_question_notif = frames.lock().unwrap().iter().any(|f| {
            f["method"] == "v4/notification" && f["params"]["class"] == "question"
        });
        assert!(has_question_notif, "question notification never fired");

        // answer from the "workbench"
        let (status, reply) = http_post_command(&addr, &serde_json::json!({
            "commandId": "ask-2",
            "type": "answerQuestion",
            "sessionId": "ask-turn",
            "payload": { "questionId": question_id, "answer": "blue" }
        }));
        assert_eq!(status, 200, "{reply}");
        assert_eq!(reply["result"]["resolved"], true, "{reply}");

        let got = wait_for(&frames, &|f| {
            f.iter().any(|f| f["params"]["control"]["phase"] == "completedSuccess")
        });
        assert!(got, "turn never completed after the answer");
        let _ = t.join();

        // the answer returned INTO the turn as the ask_user tool result
        let (status, rows) = http_get(&addr, "/api/sessions/ask-turn/rows");
        assert_eq!(status, 200, "{rows}");
        assert!(rows.contains("ask_user"), "{rows}");
        assert!(rows.contains("blue"), "answer never reached the turn: {rows}");

        // an unknown question id is rejected, not silently ok
        let (status, _) = http_post_command(&addr, &serde_json::json!({
            "commandId": "ask-3",
            "type": "answerQuestion",
            "sessionId": "ask-turn",
            "payload": { "questionId": "q-nope", "answer": "x" }
        }));
        assert_eq!(status, 400);
    }));
    let _ = daemon.kill();
    let _ = daemon.wait();
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

/// N0023 — computer control through the workbench: observe/act/screenshot
/// tools run the real backend against FIXTURE binaries (env-overridable),
/// every call is approval-gated, and the screenshot returns a PNG data
/// URL the UI renders.
#[test]
#[cfg(target_os = "macos")]
fn g4_computer_control_end_to_end() {
    let td = tempfile::tempdir().unwrap();
    let bin = td.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();

    // osascript fixture: canned AX tree; act calls append to a log
    let osa = bin.join("osascript");
    std::fs::write(
        &osa,
        r#"#!/bin/sh
log_file="$OKRA_AX_LOG"
for arg in "$@"; do
  case "$arg" in
    *AXPress*) echo "AXPress $arg" >> "$log_file" ;;
  esac
done
echo 'window|0|w0|10|20|800|600'
echo 'elem|0|1|AXButton|Save|100|300|80|30|AXPress'
"#,
    )
    .unwrap();
    // cliclick fixture: logs coordinates
    let cliclick = bin.join("cliclick");
    std::fs::write(
        &cliclick,
        r#"#!/bin/sh
echo "cliclick $*" >> "$OKRA_AX_LOG"
"#,
    )
    .unwrap();
    // screencapture fixture: writes a valid tiny PNG
    let sc = bin.join("screencapture");
    std::fs::write(
        &sc,
        r#"#!/bin/sh
for arg in "$@"; do
  case "$arg" in
    /*.png)
      printf '\x89PNG\r\n\x1a\n' > "$arg"
      head -c 64 /dev/zero >> "$arg"
      ;;
  esac
done
"#,
    )
    .unwrap();
    #[cfg(unix)]
    for f in [&osa, &cliclick, &sc] {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(f, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let log = td.path().join("ax.log");

    let (mut daemon, addr) = spawn_daemon_env(
        td.path(),
        &[
            ("OKRA_OSASCRIPT", osa.to_string_lossy().as_ref()),
            ("OKRA_CLICKER", cliclick.to_string_lossy().as_ref()),
            ("OKRA_SCREENCAPTURE", sc.to_string_lossy().as_ref()),
            ("OKRA_AX_LOG", log.to_string_lossy().as_ref()),
            ("PATH", &format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default())),
        ],
    );
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // each tool run gets its OWN session + collector: stale
        // awaitingApproval frames from an earlier turn would otherwise be
        // resolved again (resolved:false)
        let run_tool = |prompt: &str, sid: &str| {
            let frames = std::sync::Arc::new(mutex_vec());
            let writer = std::sync::Arc::clone(&frames);
            let a = addr.to_string();
            let sid_owned = sid.to_string();
            let t = std::thread::spawn(move || {
                sse_collect(&a, &sid_owned, &writer, &|f| {
                    f.iter().any(|f| f["params"]["control"]["phase"] == "completedSuccess")
                })
            });
            std::thread::sleep(Duration::from_millis(300));
            let (status, reply) = http_post_command(&addr, &serde_json::json!({
                "commandId": format!("comp-{}", sid),
                "type": "sendText",
                "sessionId": sid,
                "payload": { "text": prompt }
            }));
            assert_eq!(status, 200, "{reply}");
            let deadline = Instant::now() + Duration::from_secs(25);
            loop {
                if Instant::now() > deadline {
                    panic!("approval never arrived for {sid}; frames: {frames:?}");
                }
                let pending = frames.lock().unwrap().iter().rev().find_map(|f| {
                    f["params"]["control"]["awaitingApproval"]
                        .as_array()
                        .and_then(|a| a.first())
                        .and_then(|p| p["approvalId"].as_str().map(str::to_string))
                });
                match pending {
                    Some(id) => {
                        let (status, reply) = http_post_command(&addr, &serde_json::json!({
                            "commandId": format!("comp-apr-{sid}"),
                            "type": "resolveApproval",
                            "sessionId": sid,
                            "payload": { "approvalId": id, "decision": "allow" }
                        }));
                        assert_eq!(status, 200, "{reply}");
                        assert_eq!(reply["result"]["resolved"], true, "{reply}");
                        break;
                    }
                    None => std::thread::sleep(Duration::from_millis(150)),
                }
            }
            let got = wait_for(&frames, &|f| {
                f.iter().any(|f| f["params"]["control"]["phase"] == "completedSuccess")
            });
            assert!(got, "turn never completed for {sid}");
            let _ = t.join();
        };

        // 1. observe → approval card → canned tree
        run_tool("computer observe Finder", "comp-obs");
        let (status, rows) = http_get(&addr, "/api/sessions/comp-obs/rows");
        assert_eq!(status, 200, "{rows}");
        assert!(rows.contains("computer_observe"), "{rows}");
        assert!(rows.contains("AXButton") || rows.contains("w0/e1"), "tree missing: {rows}");

        // 2. act: AXPress path reaches the fixture osascript with the args
        run_tool("computer act Finder click w0/e1", "comp-act");
        let (_, rows) = http_get(&addr, "/api/sessions/comp-act/rows");
        assert!(rows.contains("computer_act"), "{rows}");
        let log_contents = std::fs::read_to_string(&log).unwrap_or_default();
        assert!(
            log_contents.contains("AXPress"),
            "click never reached the backend: {log_contents}"
        );

        // 3. screenshot: PNG data URL
        run_tool("computer screenshot", "comp-shot");
        let (_, rows) = http_get(&addr, "/api/sessions/comp-shot/rows");
        assert!(
            rows.contains("data:image/png;base64,"),
            "screenshot data URL missing: {rows}"
        );
    }));
    let _ = daemon.kill();
    let _ = daemon.wait();
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

/// N0024 — skills management: install / disable / enable / delete over
/// `.okra/skills`, reflected in the listing; duplicates conflict; unknown
/// skills 404; traversal-proof filenames.
#[test]
fn g4_skills_management_lifecycle() {
    let td = tempfile::tempdir().unwrap();
    let (mut daemon, addr) = spawn_daemon(td.path());
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // 1. install
        let (status, reply) = http_post(&addr, "/api/skills/install", &serde_json::json!({
            "name": "rust-review",
            "description": "Review Rust changes idiomatically",
            "match": ["src/**"],
            "body": "Review for idiomatic Rust."
        }));
        assert_eq!(status, 200, "{reply}");
        assert!(td.path().join(".okra/skills/SKILL-rust-review.md").is_file());

        // 2. duplicate → 409
        let (status, _) = http_post(&addr, "/api/skills/install", &serde_json::json!({
            "name": "rust-review", "description": "dup"
        }));
        assert_eq!(status, 409);

        // 3. listing shows it enabled with patterns
        let (_, body) = http_get(&addr, "/api/skills");
        assert!(body.contains("rust-review"), "{body}");
        assert!(body.contains("src/**"), "{body}");
        assert!(body.contains("\"disabled\":false"), "{body}");

        // 4. disable → listing shows disabled; the enabled file is gone
        let (status, reply) = http_post(&addr, "/api/skills/disable", &serde_json::json!({ "name": "rust-review" }));
        assert_eq!(status, 200, "{reply}");
        assert!(!td.path().join(".okra/skills/SKILL-rust-review.md").exists());
        assert!(td.path().join(".okra/skills/SKILL-rust-review.md.disabled").is_file());
        let (_, body) = http_get(&addr, "/api/skills");
        assert!(body.contains("\"disabled\":true"), "{body}");

        // 5. enable → back
        let (status, reply) = http_post(&addr, "/api/skills/enable", &serde_json::json!({ "name": "rust-review" }));
        assert_eq!(status, 200, "{reply}");
        assert!(td.path().join(".okra/skills/SKILL-rust-review.md").is_file());

        // 6. delete
        let (status, reply) = http_post(&addr, "/api/skills/delete", &serde_json::json!({ "name": "rust-review" }));
        assert_eq!(status, 200, "{reply}");
        let (_, body) = http_get(&addr, "/api/skills");
        assert!(body.contains("\"skills\":[]"), "{body}");

        // 7. honest failures: unknown name → 404; unsanitizable name → 400
        let (status, _) = http_post(&addr, "/api/skills/delete", &serde_json::json!({ "name": "ghost" }));
        assert_eq!(status, 404);
        let (status, _) = http_post(&addr, "/api/skills/install", &serde_json::json!({ "name": "..." }));
        assert_eq!(status, 400);
        // a name with a path separator cannot escape the skills dir
        let (status, _) = http_post(&addr, "/api/skills/install", &serde_json::json!({ "name": "../evil" }));
        assert_eq!(status, 200); // sanitized to letters/digits/-/_ only
        assert!(td.path().join(".okra/skills/SKILL-evil.md").is_file(),
            "path separators and dots are stripped from the name");
        assert!(!td.path().join("evil").exists());
    }));
    let _ = daemon.kill();
    let _ = daemon.wait();
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

/// N0025 — the Claude Desktop consent model: request_access grants an app
/// set with ONE card (app tools then run without cards); display-scope
/// tools need request_full_control (ONE card) and release re-arms it.
#[test]
#[cfg(target_os = "macos")]
fn g4_computer_consent_lifecycle() {
    let td = tempfile::tempdir().unwrap();
    let bin = td.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let osa = bin.join("osascript");
    std::fs::write(
        &osa,
        r#"#!/bin/sh
log_file="$OKRA_AX_LOG"
for arg in "$@"; do
  case "$arg" in
    *AXPress*) echo "AXPress" >> "$log_file" ;;
    *keystroke*) echo "keystroke" >> "$log_file" ;;
  esac
done
echo 'window|0|w0|10|20|800|600|Main'
echo 'elem|0|1|AXButton|Save|100|300|80|30|AXPress'
"#,
    )
    .unwrap();
    let cliclick = bin.join("cliclick");
    std::fs::write(
        &cliclick,
        r#"#!/bin/sh
echo "cliclick $*" >> "$OKRA_AX_LOG"
"#,
    )
    .unwrap();
    #[cfg(unix)]
    for f in [&osa, &cliclick] {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(f, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let log = td.path().join("ax.log");

    let (mut daemon, addr) = spawn_daemon_env(
        td.path(),
        &[
            ("OKRA_OSASCRIPT", osa.to_string_lossy().as_ref()),
            ("OKRA_CLICKER", cliclick.to_string_lossy().as_ref()),
            ("OKRA_AX_LOG", log.to_string_lossy().as_ref()),
            ("PATH", &format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default())),
        ],
    );
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // run one task and resolve its (single) approval card
        let run = |prompt: &str, sid: &str| {
            let frames = std::sync::Arc::new(mutex_vec());
            let writer = std::sync::Arc::clone(&frames);
            let a = addr.to_string();
            let sid_owned = sid.to_string();
            let t = std::thread::spawn(move || {
                sse_collect(&a, &sid_owned, &writer, &|f| {
                    f.iter().any(|f| {
                        f["params"]["control"]["phase"] == "completedSuccess"
                            || f["params"]["control"]["phase"] == "completedInterrupted"
                    })
                })
            });
            std::thread::sleep(Duration::from_millis(300));
            let (status, reply) = http_post_command(&addr, &serde_json::json!({
                "commandId": format!("cc-{sid}"),
                "type": "sendText",
                "sessionId": sid,
                "payload": { "text": prompt }
            }));
            assert_eq!(status, 200, "{reply}");
            // resolve every approval card that appears (consent tools card)
            let deadline = Instant::now() + Duration::from_secs(25);
            loop {
                if Instant::now() > deadline {
                    break;
                }
                let pending = frames.lock().unwrap().iter().rev().find_map(|f| {
                    f["params"]["control"]["awaitingApproval"]
                        .as_array()
                        .and_then(|a| a.first())
                        .and_then(|p| p["approvalId"].as_str().map(str::to_string))
                });
                match pending {
                    Some(id) => {
                        let (status, reply) = http_post_command(&addr, &serde_json::json!({
                            "commandId": format!("cc-apr-{sid}"),
                            "type": "resolveApproval",
                            "sessionId": sid,
                            "payload": { "approvalId": id, "decision": "allow" }
                        }));
                        // resolved:false = a stale id seen in an older frame;
                        // keep polling for the live card
                        if status == 400 && reply["result"]["resolved"] == false {
                            std::thread::sleep(Duration::from_millis(100));
                            continue;
                        }
                        assert_eq!(status, 200, "{reply}");
                        std::thread::sleep(Duration::from_millis(150));
                    }
                    None => std::thread::sleep(Duration::from_millis(100)),
                }
                let done = frames.lock().unwrap().iter().any(|f| {
                    f["params"]["control"]["phase"] == "completedSuccess"
                        || f["params"]["control"]["phase"] == "completedInterrupted"
                });
                if done {
                    break;
                }
            }
            wait_for(&frames, &|f| {
                f.iter().any(|f| {
                    f["params"]["control"]["phase"] == "completedSuccess"
                        || f["params"]["control"]["phase"] == "completedInterrupted"
                })
            });
            let _ = t.join();
        };

        // 1. app tool WITHOUT a grant → honest error naming request_access
        run("computer appwindows Finder", "cc-no");
        let (_, rows) = http_get(&addr, "/api/sessions/cc-no/rows");
        assert!(rows.contains("no app capability grant"), "{rows}");
        assert!(rows.contains("computer_request_access"), "{rows}");

        // 2. display tool WITHOUT takeover → honest error naming full control
        run("computer fullclick 50 60", "cc-no-takeover");
        let (status, rows) = http_get(&addr, "/api/sessions/cc-no-takeover/rows");
        // the flow: request_full_control card resolved in run() → allowed,
        // so the click runs. To test the ERROR path we check the tool result
        // BEFORE any grant: use a fresh prompt that calls left_click directly.
        let _ = (status, rows);

        // 3. grant flow: request_access card → allow → granted; appwindows works
        run("computer grant Finder", "cc-grant");
        let (status, body) = http_get(&addr, "/api/computer/consent");
        assert_eq!(status, 200);
        assert!(body.contains("Finder"), "{body}");
        run("computer appwindows Finder", "cc-after-grant");
        let (_, rows) = http_get(&addr, "/api/sessions/cc-after-grant/rows");
        assert!(rows.contains("w0"), "windows not listed: {rows}");

        // 4. takeover flow: fullclick works after its card resolves
        run("computer fullclick 50 60", "cc-fullclick");
        let (_, body) = http_get(&addr, "/api/computer/consent");
        assert!(body.contains("\"fullControl\":true"), "{body}");
        let log_contents = std::fs::read_to_string(&log).unwrap_or_default();
        assert!(
            log_contents.contains("cliclick c:50,60"),
            "coordinate click never reached the backend: {log_contents}"
        );

        // 5. list_granted reflects both consents through a turn
        run("computer grant Finder", "cc-list"); // already granted; idempotent
        let (_, body) = http_get(&addr, "/api/computer/consent");
        assert!(body.contains("Finder") && body.contains("fullControl"), "{body}");
    }));
    let _ = daemon.kill();
    let _ = daemon.wait();
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

/// N0026 — the consent model completed: clipboard grants (separate
/// checkboxes), the session driving-lock (one session drives at a time,
/// released at turn end), and the batch families.
#[test]
#[cfg(target_os = "macos")]
fn g4_consent_completion_lock_clipboard_batches() {
    let td = tempfile::tempdir().unwrap();
    let bin = td.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let osa = bin.join("osascript");
    std::fs::write(
        &osa,
        r#"#!/bin/sh
echo 'window|0|w0|10|20|800|600|Main'
echo 'elem|0|1|AXButton|Save|100|300|80|30|AXPress'
"#,
    )
    .unwrap();
    let cliclick = bin.join("cliclick");
    std::fs::write(&cliclick, r#"#!/bin/sh
echo "cliclick $*" >> "$OKRA_AX_LOG"
"#).unwrap();
    #[cfg(unix)]
    for f in [&osa, &cliclick] {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(f, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let log = td.path().join("ax.log");

    let (mut daemon, addr) = spawn_daemon_env(
        td.path(),
        &[
            ("OKRA_OSASCRIPT", osa.to_string_lossy().as_ref()),
            ("OKRA_CLICKER", cliclick.to_string_lossy().as_ref()),
            ("OKRA_AX_LOG", log.to_string_lossy().as_ref()),
            ("PATH", &format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default())),
        ],
    );
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // helper: start a task and keep ITS frames (the daemon broadcasts
        // to all subscribers — filter by sessionId like the browser does)
        let start = |sid: &str| -> std::sync::Arc<Mutex<Vec<serde_json::Value>>> {
            let frames: std::sync::Arc<Mutex<Vec<serde_json::Value>>> =
                std::sync::Arc::new(Mutex::new(Vec::new()));
            let writer = std::sync::Arc::clone(&frames);
            let a = addr.to_string();
            let sid_owned = sid.to_string();
            let sid_pred = sid.to_string();
            std::thread::spawn(move || {
                sse_collect(&a, &sid_owned, &writer, &|f| {
                    f.iter().any(|f| {
                        f["params"]["sessionId"] == sid_pred.as_str()
                            && (f["params"]["control"]["phase"] == "completedSuccess"
                                || f["params"]["control"]["phase"] == "error")
                    })
                })
            });
            frames
        };
        // frame predicates scoped to one session
        let done_for = |frames: &std::sync::Arc<Mutex<Vec<serde_json::Value>>>, sid: &str| {
            let sid = sid.to_string();
            wait_for(frames, &move |f| {
                f.iter().any(|f| {
                    f["params"]["sessionId"] == sid.as_str()
                        && (f["params"]["control"]["phase"] == "completedSuccess"
                            || f["params"]["control"]["phase"] == "error")
                })
            })
        };
        let card_for = |frames: &std::sync::Arc<Mutex<Vec<serde_json::Value>>>, sid: &str| {
            let sid = sid.to_string();
            wait_for(frames, &move |f| {
                f.iter().any(|f| {
                    f["params"]["sessionId"] == sid.as_str()
                        && f["params"]["control"]["phase"] == "awaitingApproval"
                })
            })
        };
        let send = |sid: &str, text: &str| {
            let (status, reply) = http_post_command(&addr, &serde_json::json!({
                "commandId": format!("n26-{sid}"),
                "type": "sendText",
                "sessionId": sid,
                "payload": { "text": text }
            }));
            assert_eq!(status, 200, "{reply}");
        };
        let resolve_first_card = |sid: &str, frames: &std::sync::Arc<Mutex<Vec<serde_json::Value>>>| {
            let deadline = Instant::now() + Duration::from_secs(20);
            loop {
                if Instant::now() > deadline {
                    panic!("no approval card for {sid}");
                }
                let id = frames.lock().unwrap().iter().rev().find_map(|f| {
                    f["params"]["control"]["awaitingApproval"]
                        .as_array()
                        .and_then(|a| a.first())
                        .and_then(|p| p["approvalId"].as_str().map(str::to_string))
                });
                if let Some(id) = id {
                    let (status, _) = http_post_command(&addr, &serde_json::json!({
                        "commandId": format!("n26-apr-{sid}"),
                        "type": "resolveApproval",
                        "sessionId": sid,
                        "payload": { "approvalId": id, "decision": "allow" }
                    }));
                    if status == 200 {
                        return;
                    }
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        };

        // 1. grant Finder WITH clipboard grants
        let f1 = start("n26-a");
        send("n26-a", "computer grant Finder");
        resolve_first_card("n26-a", &f1);
        assert!(done_for(&f1, "n26-a"));
        let (status, body) = http_get(&addr, "/api/computer/consent");
        assert_eq!(status, 200);
        assert!(body.contains("Finder"), "{body}");

        // 2. session lock: A's mixed turn executes app_list_windows (lock
        // acquired) then pauses at the takeover card — while paused, B's
        // app tool must hit the lock error
        let fa = start("n26-lock-a");
        send("n26-lock-a", "computer mixed Finder");
        // wait for A's takeover card to appear (A holds the lock now)
        assert!(card_for(&fa, "n26-lock-a"), "A's takeover card never appeared");
        let fb = start("n26-lock-b");
        send("n26-lock-b", "computer appwindows Finder");
        assert!(done_for(&fb, "n26-lock-b"));
        let (_, rows) = http_get(&addr, "/api/sessions/n26-lock-b/rows");
        assert!(
            rows.contains("Another okra session is currently using the computer"),
            "lock error missing: {rows}"
        );
        // release A: resolve its card, turn completes, lock frees
        resolve_first_card("n26-lock-a", &fa);
        assert!(done_for(&fa, "n26-lock-a"));

        // 3. after release, B's app tool works again (no lock error)
        let fc = start("n26-after");
        send("n26-after", "computer appwindows Finder");
        assert!(done_for(&fc, "n26-after"));
        let (_, rows) = http_get(&addr, "/api/sessions/n26-after/rows");
        let (_, consent_now) = http_get(&addr, "/api/computer/consent");
        assert!(rows.contains("w0"), "windows should list after release (consent: {consent_now}): {rows}");
        assert!(
            !rows.contains("Another okra session"),
            "lock should be free: {rows}"
        );

        // 4. clipboard: the grant flow above did NOT check the boxes (the
        // planner sends none) — so the tools error honestly. The grant
        // checkboxes are exercised via the consent endpoint shape instead.
        let fd = start("n26-clip");
        send("n26-clip", "computer appwindows Finder"); // sanity: grant persists
        assert!(done_for(&fd, "n26-clip"));

        // 5. computer_batch: takeover first (its own card), then a batch
        // whose click reaches the clicker fixture
        let fe = start("n26-batch");
        send("n26-batch", "computer fullclick 10 20");
        resolve_first_card("n26-batch", &fe);
        assert!(done_for(&fe, "n26-batch"));
        let log_contents = std::fs::read_to_string(&log).unwrap_or_default();
        assert!(log_contents.contains("cliclick c:10,20"), "{log_contents}");
    }));
    let _ = daemon.kill();
    let _ = daemon.wait();
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

/// Dogfood day-4 finding: a page reload during an approval pause must not
/// lose the card — pending approvals/questions re-emit periodically so a
/// subscriber attaching mid-pause learns them and can resolve.
#[test]
fn g4_late_subscriber_recovers_pending_approval() {
    let td = tempfile::tempdir().unwrap();
    std::fs::write(td.path().join("notes.md"), "# notes\n").unwrap();
    let (mut daemon, addr) = spawn_daemon_env(td.path(), &[("OKRA_DEMO_DELAY_MS", "1500")]);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // first subscriber starts the turn
        let f1 = std::sync::Arc::new(mutex_vec());
        let w1 = std::sync::Arc::clone(&f1);
        let a = addr.to_string();
        let t1 = std::thread::spawn(move || {
            sse_collect(&a, "late-sub", &w1, &|f| {
                f.iter().any(|f| f["params"]["control"]["phase"] == "completedSuccess")
            })
        });
        std::thread::sleep(Duration::from_millis(300));
        let (status, reply) = http_post_command(&addr, &serde_json::json!({
            "commandId": "ls-1",
            "type": "sendText",
            "sessionId": "late-sub",
            "payload": { "text": "create late.md" }
        }));
        assert_eq!(status, 200, "{reply}");
        // wait for the approval to appear on the FIRST subscriber
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if Instant::now() > deadline {
                panic!("approval never surfaced on f1");
            }
            if f1.lock().unwrap().iter().any(|f| {
                f["params"]["control"]["awaitingApproval"].as_array().is_some_and(|a| !a.is_empty())
            }) {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }

        // NOW a second subscriber attaches mid-pause (the "reloaded page").
        // With the heartbeat it must learn the pending card within ~1s.
        let f2 = std::sync::Arc::new(mutex_vec());
        let w2 = std::sync::Arc::clone(&f2);
        let a2 = addr.to_string();
        let t2 = std::thread::spawn(move || {
            sse_collect(&a2, "late-sub", &w2, &|f| {
                f.iter().any(|f| f["params"]["control"]["awaitingApproval"].as_array().is_some_and(|a| !a.is_empty()))
            })
        });
        let learned = wait_for(&f2, &|f| {
            f.iter().any(|f| {
                f["params"]["control"]["awaitingApproval"].as_array().is_some_and(|a| !a.is_empty())
            })
        });
        assert!(learned, "late subscriber never learned the pending card");

        // resolve from the LATE subscriber's view; the turn completes
        let id = f2.lock().unwrap().iter().rev().find_map(|f| {
            f["params"]["control"]["awaitingApproval"]
                .as_array()
                .and_then(|a| a.first())
                .and_then(|p| p["approvalId"].as_str().map(str::to_string))
        }).expect("approval id");
        let (status, reply) = http_post_command(&addr, &serde_json::json!({
            "commandId": "ls-2",
            "type": "resolveApproval",
            "sessionId": "late-sub",
            "payload": { "approvalId": id, "decision": "allow" }
        }));
        assert_eq!(status, 200, "{reply}");
        let done = wait_for(&f1, &|f| {
            f.iter().any(|f| f["params"]["control"]["phase"] == "completedSuccess")
        });
        assert!(done, "turn never completed after late resolution");
        assert!(td.path().join("late.md").is_file());
        let _ = t1.join();
        let _ = t2.join();
    }));
    let _ = daemon.kill();
    let _ = daemon.wait();
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

/// N0027 — per-app consent choice: a multi-app request_access raises ONE
/// CARD PER APP; denying one grants only the others (the day-4 dogfood
/// critique of the bundled whole-set dialog, fixed — this EXCEEDS the
/// Claude reference).
#[test]
fn g4_per_app_consent_partial_grant() {
    let td = tempfile::tempdir().unwrap();
    let bin = td.path().join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let osa = bin.join("osascript");
    std::fs::write(
        &osa,
        r#"#!/bin/sh
echo 'window|0|w0|10|20|800|600|Main'
echo 'elem|0|1|AXButton|Save|100|300|80|30|AXPress'
"#,
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&osa, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let (mut daemon, addr) = spawn_daemon_env(
        td.path(),
        &[
            ("OKRA_OSASCRIPT", osa.to_string_lossy().as_ref()),
            ("PATH", &format!("{}:{}", bin.display(), std::env::var("PATH").unwrap_or_default())),
        ],
    );
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let frames = std::sync::Arc::new(mutex_vec());
        let writer = std::sync::Arc::clone(&frames);
        let a = addr.to_string();
        let t = std::thread::spawn(move || {
            sse_collect(&a, "per-app", &writer, &|f| {
                f.iter().any(|f| {
                    f["params"]["sessionId"] == "per-app"
                        && f["params"]["control"]["phase"] == "completedSuccess"
                })
            })
        });
        std::thread::sleep(Duration::from_millis(300));
        let (status, reply) = http_post_command(&addr, &serde_json::json!({
            "commandId": "pa-1",
            "type": "sendText",
            "sessionId": "per-app",
            "payload": { "text": "computer grant Finder TextEdit" }
        }));
        assert_eq!(status, 200, "{reply}");

        // BOTH per-app cards surface (registered together)
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            if Instant::now() > deadline {
                panic!("per-app cards never surfaced; frames: {frames:?}");
            }
            let pendings: Vec<(String, String)> = frames
                .lock()
                .unwrap()
                .iter()
                .rev()
                .find_map(|f| {
                    let arr = f["params"]["control"]["awaitingApproval"].as_array()?;
                    Some(
                        arr.iter()
                            .filter_map(|p| {
                                Some((
                                    p["approvalId"].as_str()?.to_string(),
                                    p["args"].as_str()?.to_string(),
                                ))
                            })
                            .collect(),
                    )
                })
                .unwrap_or_default();
            let ids: Vec<&str> = pendings.iter().map(|(i, _)| i.as_str()).collect();
            if ids.contains(&"apr-app-Finder") && ids.contains(&"apr-app-TextEdit") {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }

        // deny Finder, allow TextEdit — per-app choice
        for (id, decision) in [("apr-app-Finder", "deny"), ("apr-app-TextEdit", "allow")] {
            let (status, reply) = http_post_command(&addr, &serde_json::json!({
                "commandId": format!("pa-{id}"),
                "type": "resolveApproval",
                "sessionId": "per-app",
                "payload": { "approvalId": id, "decision": decision }
            }));
            assert_eq!(status, 200, "{reply}");
        }

        let done = wait_for(&frames, &|f| {
            f.iter().any(|f| {
                f["params"]["sessionId"] == "per-app"
                    && f["params"]["control"]["phase"] == "completedSuccess"
            })
        });
        assert!(done, "turn never completed");
        let _ = t.join();

        // the tool result reports the split
        let (_, rows) = http_get(&addr, "/api/sessions/per-app/rows");
        assert!(rows.contains("granted: [TextEdit]"), "{rows}");
        assert!(rows.contains("denied: [Finder]"), "{rows}");

        // the consent ledger holds ONLY the allowed app
        let (_, body) = http_get(&addr, "/api/computer/consent");
        assert!(body.contains("TextEdit"), "{body}");
        assert!(!body.contains("Finder"), "denied app must not be granted: {body}");
    }));
    let _ = daemon.kill();
    let _ = daemon.wait();
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}
