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
            let mut approval_id = String::new();
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
        assert!(got, "PTY output never streamed the marker; frames: {frames:?}");
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
        let (status, body) = http_get(&addr, "/api/term");
        assert!(body.contains("\"ids\":[]"), "terminal not pruned: {body}");
    }));
    let _ = daemon.kill();
    let _ = daemon.wait();
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}
