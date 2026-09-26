//! G4 breadth (MASTER-PLAN §4): the browser-native surface. The daemon
//! port also speaks HTTP: `GET /health`, `GET /sse/<session>`
//! (text/event-stream — broadcasts arrive as `data:` frames), and
//! `POST /command` (the same v4 envelope as the NDJSON surfaces). A
//! browser needs nothing but EventSource + fetch to be a full surface.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use std::path::Path;

/// Spawn the real daemon and read its advertised bind address from stderr.
fn spawn_daemon(cwd: &Path) -> (Child, String) {
    let bin = env!("CARGO_BIN_EXE_okra");
    let mut child = Command::new(bin)
        .args(["serve", "--tcp", "--cwd", &cwd.to_string_lossy()])
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
        let mut reader = reader;
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
    let mut stream = TcpStream::connect(addr).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(30))).unwrap();
    let body = envelope.to_string();
    let request = format!(
        "POST /command HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
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
