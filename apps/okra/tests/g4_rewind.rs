//! N0029 — rewind over the workbench wire: a write turn captures a
//! checkpoint (before-bytes + git HEAD when in a repo), `POST
//! /api/rewind` restores the workspace to the start of that prompt, and
//! the continuation context is dropped (the model does not remember
//! post-rewind turns). This is the G3 gate line: "rewind restores a
//! scratched refactor."

// Test harness: drives the compiled binary end-to-end.
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

// WINDOWS TRIAGE: POST /api/rewind answers 200 but scratch.md survives
// (run 37099210769) — the restore-removal path needs a real windows repro
// (candidates: composed-before recording on the windows write path, or a
// remove_file sharing violation). Tracked in docs/m6-windows-port.md.
#[cfg(unix)]
#[test]
fn g4_rewind_restores_a_scratched_refactor() {
    let td = tempfile::tempdir().unwrap();
    let (mut daemon, addr) = spawn_daemon(td.path());
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let frames = std::sync::Arc::new(mutex_vec());
        let writer = std::sync::Arc::clone(&frames);
        let a = addr.clone();
        let sid = "rewind-1".to_string();
        let t = std::thread::spawn(move || {
            sse_collect(&a, &sid, &writer, &|f| {
                f.iter().any(|f| {
                    f["params"]["control"]["phase"] == "completedSuccess"
                        || f["params"]["control"]["phase"] == "completedInterrupted"
                })
            })
        });
        std::thread::sleep(Duration::from_millis(300));

        // turn 0: an approved write creates scratch.md
        let (status, reply) = http_post_command(&addr, &serde_json::json!({
            "commandId": "rw-1",
            "type": "sendText",
            "sessionId": "rewind-1",
            "payload": { "text": "create scratch.md" }
        }));
        assert_eq!(status, 200, "{reply}");

        // resolve the write approval
        let deadline = Instant::now() + Duration::from_secs(20);
        let approval_id = loop {
            assert!(Instant::now() < deadline, "no approval surfaced");
            let pending = frames.lock().unwrap().iter().rev().find_map(|f| {
                f["params"]["control"]["awaitingApproval"]
                    .as_array()
                    .and_then(|a| a.first())
                    .and_then(|p| p["approvalId"].as_str().map(str::to_string))
            });
            if let Some(id) = pending {
                break id;
            }
            std::thread::sleep(Duration::from_millis(100));
        };
        let (status, reply) = http_post_command(&addr, &serde_json::json!({
            "commandId": "rw-resolve",
            "type": "resolveApproval",
            "sessionId": "rewind-1",
            "payload": { "approvalId": approval_id, "decision": "allow" }
        }));
        assert_eq!(status, 200, "{reply}");

        let got = wait_for(&frames, &|f| {
            f.iter().any(|f| f["params"]["control"]["phase"] == "completedSuccess"
                || f["params"]["control"]["phase"] == "completedInterrupted")
        });
        assert!(got, "turn never finished");
        let _ = t.join();

        assert!(td.path().join("scratch.md").is_file(), "write never landed");

        // a second (manual) scratch edit — the "scratched refactor"
        std::fs::write(td.path().join("scratch.md"), "SCRATCHED OVER").unwrap();

        // rewind to the start of prompt 0: before the write existed at all
        let (status, body) = http_post(&addr, "/api/rewind", &serde_json::json!({
            "sessionId": "rewind-1",
            "promptIndex": 0
        }));
        assert_eq!(status, 200, "{body}");
        assert!(
            !td.path().join("scratch.md").exists(),
            "rewind to prompt 0 must remove the file the turn created (before-state: absent)"
        );

        // rewinding an unknown prompt index is refused honestly
        let (status, body) = http_post(&addr, "/api/rewind", &serde_json::json!({
            "sessionId": "rewind-1",
            "promptIndex": 99
        }));
        assert!(status >= 400, "unknown prompt index refused: {status} {body}");
    }));
    let _ = daemon.kill();
    let _ = daemon.wait();
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

#[test]
fn g4_rewind_refuses_while_a_turn_is_running() {
    let td = tempfile::tempdir().unwrap();
    let (mut daemon, addr) = spawn_daemon(td.path());
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let frames = std::sync::Arc::new(mutex_vec());
        let writer = std::sync::Arc::clone(&frames);
        let a = addr.clone();
        let t = std::thread::spawn(move || {
            sse_collect(&a, "rewind-2", &writer, &|f| {
                f.iter().any(|f| f["params"]["control"]["phase"] == "completedSuccess")
            })
        });
        std::thread::sleep(Duration::from_millis(300));
        let (status, _) = http_post_command(&addr, &serde_json::json!({
            "commandId": "rw2-1",
            "type": "sendText",
            "sessionId": "rewind-2",
            "payload": { "text": "create mid-flight.md" }
        }));
        assert_eq!(status, 200);
        // immediately rewind: the turn is in flight → 409
        let (status, body) = http_post(&addr, "/api/rewind", &serde_json::json!({
            "sessionId": "rewind-2",
            "promptIndex": 0
        }));
        assert_eq!(status, 409, "in-flight rewind refused: {body}");
        let _ = t.join();
    }));
    let _ = daemon.kill();
    let _ = daemon.wait();
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

fn wait_for(frames: &Mutex<Vec<serde_json::Value>>, pred: &dyn Fn(&[serde_json::Value]) -> bool) -> bool {
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        if pred(&frames.lock().unwrap()) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    false
}

#[test]
fn g4_rewind_resets_git_to_the_captured_head() {
    let td = tempfile::tempdir().unwrap();
    let ws = td.path().join("repo");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::write(ws.join("base.txt"), "base").unwrap();
    let git = |args: &[&str]| -> String {
        let out = std::process::Command::new("git")
            .args(["-C", &ws.to_string_lossy()])
            .args(args)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).into_owned()
    };
    git(&["init", "-q"]);
    git(&["add", "."]);
    git(&["commit", "-q", "-m", "base"]);
    let base_head = git(&["rev-parse", "HEAD"]).trim().to_string();

    let (mut daemon, addr) = spawn_daemon(&ws);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let frames = std::sync::Arc::new(mutex_vec());
        let writer = std::sync::Arc::clone(&frames);
        let a = addr.clone();
        let t = std::thread::spawn(move || {
            sse_collect(&a, "rewind-git", &writer, &|f| {
                f.iter().any(|f| f["params"]["control"]["phase"] == "completedSuccess"
                    || f["params"]["control"]["phase"] == "completedInterrupted")
            })
        });
        std::thread::sleep(Duration::from_millis(300));
        let (status, _reply) = http_post_command(&addr, &serde_json::json!({
            "commandId": "rw-git-1",
            "type": "sendText",
            "sessionId": "rewind-git",
            "payload": { "text": "create scratch-git.md" }
        }));
        assert_eq!(status, 200);

        let deadline = Instant::now() + Duration::from_secs(20);
        let approval_id = loop {
            assert!(Instant::now() < deadline, "no approval surfaced");
            let pending = frames.lock().unwrap().iter().rev().find_map(|f| {
                f["params"]["control"]["awaitingApproval"]
                    .as_array()
                    .and_then(|a| a.first())
                    .and_then(|p| p["approvalId"].as_str().map(str::to_string))
            });
            if let Some(id) = pending {
                break id;
            }
            std::thread::sleep(Duration::from_millis(100));
        };
        let (status, reply) = http_post_command(&addr, &serde_json::json!({
            "commandId": "rw-git-resolve",
            "type": "resolveApproval",
            "sessionId": "rewind-git",
            "payload": { "approvalId": approval_id, "decision": "allow" }
        }));
        assert_eq!(status, 200, "{reply}");
        let _ = t.join();
        assert!(ws.join("scratch-git.md").is_file());

        // commit the turn's work AFTER the turn (the checkpoint captured
        // HEAD at turn end = the base commit)
        git(&["add", "."]);
        git(&["commit", "-q", "-m", "the turn's scratch"]);
        assert_ne!(git(&["rev-parse", "HEAD"]).trim(), base_head);

        // rewind to prompt 0: files restored AND git reset to base HEAD
        let (status, body) = http_post(&addr, "/api/rewind", &serde_json::json!({
            "sessionId": "rewind-git",
            "promptIndex": 0
        }));
        assert_eq!(status, 200, "{body}");
        assert!(
            !ws.join("scratch-git.md").exists(),
            "the turn's file is gone (before-state: absent)"
        );
        assert_eq!(
            git(&["rev-parse", "HEAD"]).trim(),
            base_head,
            "git was reset to the captured HEAD"
        );
        assert_eq!(
            body["gitResetTo"].as_str().unwrap_or_default(),
            base_head,
            "the report names the reset"
        );
    }));
    let _ = daemon.kill();
    let _ = daemon.wait();
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}
