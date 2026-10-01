//! n0041 — the workflow→wire link end to end: POST /api/workflow/run
//! validates, runs the engine with live projection, and CHANGE-ONLY
//! workflowRun.* deltas reach a real SSE surface. The test reconstructs
//! the run state by applying the deltas through the protocol's OWN
//! reducer — the TS contract's roundtrip, over a live daemon.

// Test harness: drives the compiled binary end-to-end.
#![allow(clippy::disallowed_methods)]
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use std::path::Path;

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
        if let Some(rest) = line.strip_prefix("[serve-tcp] multi-surface daemon on ") {
            break rest.trim().split(" (").next().unwrap_or(rest.trim()).to_string();
        }
    };
    std::thread::spawn(move || {
        for line in reader.lines().map_while(Result::ok) {
            eprintln!("DAEMON: {}", line.trim());
        }
    });
    (child, addr)
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
        .unwrap_or_else(|_| serde_json::json!({}));
    (status, body)
}

fn http_get(addr: &str, path: &str) -> (u16, String) {
    let mut stream = TcpStream::connect(addr).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    let request = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).unwrap();
    let mut response = String::new();
    let mut reader = BufReader::new(stream);
    loop {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {
                response.push_str(line.trim_end_matches(['\n', '\r']));
                response.push('\n');
            }
            Err(_) => break,
        }
    }
    let status: u16 = response
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    (status, response)
}

fn sse_collect(addr: &str, frames: &std::sync::Mutex<Vec<serde_json::Value>>, stop: &dyn Fn(&[serde_json::Value]) -> bool) {
    let mut stream = TcpStream::connect(addr).unwrap();
    stream.set_read_timeout(Some(Duration::from_millis(500))).unwrap();
    let request = "GET /sse/workflow-watch HTTP/1.1\r\nHost: localhost\r\nAccept: text/event-stream\r\n\r\n";
    stream.write_all(request.as_bytes()).unwrap();
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
    let deadline = Instant::now() + Duration::from_secs(60);
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

#[test]
fn workflow_run_projects_livedeltas_through_the_protocol_reducer() {
    let td = tempfile::tempdir().unwrap();
    std::fs::write(td.path().join("notes.txt"), "workflow wire fixture").unwrap();
    let (mut daemon, addr) = spawn_daemon(td.path());
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let frames: std::sync::Mutex<Vec<serde_json::Value>> = std::sync::Mutex::new(Vec::new());
        let watch = std::sync::Arc::new(frames);
        let writer = std::sync::Arc::clone(&watch);
        let a = addr.clone();
        let t = std::thread::spawn(move || {
            sse_collect(&a, &writer, &|f| {
                f.iter().any(|frame| {
                    frame.get("method").and_then(|m| m.as_str()) == Some("v4/workflowRuns")
                        && frame["params"]["deltas"]
                            .as_array()
                            .is_some_and(|d| d.iter().any(|op| {
                                op["op"] == "workflowRun.updated"
                                    && op["run"]["status"] == "completed"
                            }))
                })
            })
        });
        std::thread::sleep(Duration::from_millis(300));

        // 1) invalid script → 422 with the validation findings
        let (status, body) = http_post(
            &addr,
            "/api/workflow/run",
            &serde_json::json!({ "script": "let x = 1;" }),
        );
        assert_eq!(status, 422, "{body}");
        assert_eq!(body["findings"][0]["code"], "no_entry_function");

        // 2) valid two-step workflow → accepted, deltas stream
        let script = r#"
            fn run() {
                let a = step("first", "read notes.txt and summarize it");
                let b = step("second", "read notes.txt again and confirm");
                a.len() > 0 && b.len() > 0
            }
        "#;
        let (status, body) = http_post(
            &addr,
            "/api/workflow/run",
            &serde_json::json!({ "script": script, "maxSteps": 8 }),
        );
        assert_eq!(status, 200, "{body}");
        let run_id = body["runId"].as_str().unwrap().to_string();
        assert!(!run_id.is_empty());

        // the SSE surface sees the deltas; wait for the terminal one
        let _ = t.join();
        let frames = watch.lock().unwrap();
        let wf_frames: Vec<&serde_json::Value> = frames
            .iter()
            .filter(|f| f.get("method").and_then(|m| m.as_str()) == Some("v4/workflowRuns"))
            .collect();
        assert!(!wf_frames.is_empty(), "workflow deltas reached the SSE surface");

        // 3) reconstruct through the PROTOCOL's own reducer: applying the
        // deltas in arrival order must yield the run with status completed
        // and both step nodes — the TS contract over a live wire
        let mut applied = okra_protocol::ConversationState {
            rows: Vec::new(),
            state: serde_json::Map::new(),
        };
        let mut ops_count = 0usize;
        for frame in &wf_frames {
            for op in frame["params"]["deltas"].as_array().unwrap_or(&vec![]) {
                let Ok(delta) = serde_json::from_value::<okra_protocol::ConversationDelta>(op.clone()) else {
                    panic!("delta fails the WIRE serde shape: {op}");
                };
                ops_count += 1;
                okra_protocol::apply_delta(&mut applied, &delta);
            }
        }
        assert!(ops_count >= 4, "birth + step nodes + status transitions: {ops_count} ops");
        let runs = applied
            .state
            .get("workflowRuns")
            .and_then(|v| v["runs"].as_array())
            .expect("workflowRuns state key present")
            .clone();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0]["runId"], serde_json::json!(run_id));
        assert_eq!(runs[0]["status"], "completed");
        let nodes = runs[0]["nodes"].as_array().cloned().unwrap_or_default();
        assert_eq!(nodes.len(), 2, "both step nodes projected: {nodes:?}");
        assert!(nodes.iter().all(|n| n["phase"] == "completed"));

        // 4) the durable journal agrees (poll fallback)
        let (status, body) = http_get_status(&addr, &run_id);
        assert_eq!(status, 200);
        assert_eq!(body["status"], "completed");
        assert_eq!(body["steps"], 2);
    }));
    let _ = daemon.kill();
    let _ = daemon.wait();
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

fn http_get_status(addr: &str, run_id: &str) -> (u16, serde_json::Value) {
    let (status, raw) = http_get(addr, &format!("/api/workflow/status?runId={run_id}"));
    // http_get trims CRs, so the header/body boundary is a blank line
    let body_start = raw.find("\n\n").map(|i| i + 2).unwrap_or(0);
    let body = serde_json::from_str::<serde_json::Value>(raw[body_start..].trim())
        .unwrap_or_else(|_| serde_json::json!({}));
    (status, body)
}
