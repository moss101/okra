//! Managed-pin RUNTIME enforcement: a pin at the managed path must clamp
//! the run that starts under it — provider denial, max-turns ceiling,
//! fail-closed restrictions — not just be reportable via pin-status.
//! `OKRA_MANAGED_PIN` points the binary at a test pin so tests never
//! touch the real `~/.okra`.

// Test harness: executes the compiled binary as the system under test (the
// no-raw-spawn ban targets production paths).
#![allow(clippy::disallowed_methods)]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

fn run(pin: Option<&Path>, args: &[&str]) -> (i32, String, String) {
    let td = tempfile::tempdir().unwrap();
    std::fs::write(td.path().join("notes.txt"), "okra reads files").unwrap();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_okra"));
    cmd.arg("--cwd").arg(td.path()).arg("--json");
    if let Some(p) = pin {
        cmd.env("OKRA_MANAGED_PIN", p);
    } else {
        cmd.env_remove("OKRA_MANAGED_PIN");
    }
    cmd.env_remove("OKRA_TRUST_FILE");
    cmd.args(args).arg("read notes.txt and summarize");
    let out = cmd.output().expect("run okra");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
#[allow(clippy::disallowed_methods)] // test harness: runs the compiled binary
fn provider_denied_by_pin_allowlist() {
    let td = tempfile::tempdir().unwrap();
    let pin = td.path().join("pin.json");
    std::fs::write(&pin, r#"{ "version": 1, "source": "corp-it", "providerAllowlist": ["otherco"] }"#).unwrap();
    let (code, _, stderr) = run(Some(&pin), &["--provider", "openai"]);
    assert_eq!(code, 1, "{stderr}");
    assert!(stderr.contains("denied by the managed policy pin"), "{stderr}");
    assert!(stderr.contains("corp-it"), "the pin source is named: {stderr}");
}

#[test]
#[allow(clippy::disallowed_methods)] // test harness: runs the compiled binary
fn max_turns_ceiling_clamps_the_run() {
    let td = tempfile::tempdir().unwrap();
    let pin = td.path().join("pin.json");
    std::fs::write(&pin, r#"{ "version": 1, "source": "corp-it", "maxTurnsCeiling": 1 }"#).unwrap();
    let (code, stdout, stderr) = run(Some(&pin), &[]);
    assert_eq!(code, 0, "{stderr}");
    assert!(stderr.contains("[pin] max-turns clamped to 1"), "{stderr}");
    assert!(stdout.contains("turn"), "a real turn ran: {stdout}");
}

#[test]
#[allow(clippy::disallowed_methods)] // test harness: runs the compiled binary
fn no_pin_changes_nothing_and_prints_nothing() {
    let (code, _, stderr) = run(None, &[]);
    assert_eq!(code, 0, "{stderr}");
    assert!(!stderr.contains("[pin]"), "NotConfigured must be silent: {stderr}");
}

// The test asserts kernel confinement actually APPLIED ([sandbox] line):
// on windows the sandbox stub reports Unavailable (fail-closed — no
// silent passthrough), so the confinement proof is unix-only. The pin's
// PROVIDER DENIAL half is platform-neutral (covered by the sibling tests).
#[cfg(unix)]
#[test]
#[allow(clippy::disallowed_methods)] // test harness: runs the compiled binary
fn fail_closed_pin_denies_all_providers_and_applies_read_only() {
    let td = tempfile::tempdir().unwrap();
    let pin = td.path().join("pin.json");
    std::fs::write(&pin, "{ broken").unwrap();
    // deny-all: any provider is refused under a corrupt pin
    let (code, _, stderr) = run(Some(&pin), &["--provider", "openai"]);
    assert_eq!(code, 1, "{stderr}");
    assert!(stderr.contains("[pin] fail-closed:"), "{stderr}");
    assert!(stderr.contains("denied by the managed policy pin"), "{stderr}");
    // and the plain path still runs, confined read-only (fail-closed
    // ceiling) — the existing [sandbox] line proves apply actually ran
    let (code, _, stderr) = run(Some(&pin), &[]);
    assert_eq!(code, 0, "{stderr}");
    assert!(stderr.contains("[pin] fail-closed:"), "{stderr}");
    assert!(stderr.contains("[sandbox]"), "confinement applied: {stderr}");
    assert!(
        stderr.contains("[pin] sandbox clamped to ReadOnly"),
        "the off→read-only clamp is announced: {stderr}"
    );
}

/// serve --tcp turns honor the pin ceiling too: the daemon runs under a
/// maxTurnsCeiling-1 pin, a web turn is issued, and the daemon's stderr
/// must announce the clamp (the plumb lives in run_turn_streaming, so
/// both the stdio and tcp serve surfaces get it).
#[test]
#[allow(clippy::disallowed_methods)] // test harness: runs the compiled binary
fn serve_turn_budget_honors_pin_ceiling() {
    let td = tempfile::tempdir().unwrap();
    let workspace = td.path().join("ws");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::write(workspace.join("notes.txt"), "okra reads files").unwrap();
    let pin = td.path().join("pin.json");
    std::fs::write(&pin, r#"{ "version": 1, "source": "corp-it", "maxTurnsCeiling": 1 }"#).unwrap();

    let mut child = Command::new(env!("CARGO_BIN_EXE_okra"))
        .args(["serve", "--tcp", "--cwd", &workspace.to_string_lossy()])
        .env("OKRA_MANAGED_PIN", &pin)
        .env_remove("OKRA_TRUST_FILE")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn okra serve --tcp");
    let stderr_handle = child.stderr.take().unwrap();
    // drain stderr on its own thread — the test polls the collected lines
    // with deadlines instead of blocking on a pipe that may go quiet
    let stderr_lines: Arc<std::sync::Mutex<Vec<String>>> = Arc::default();
    {
        let sink = Arc::clone(&stderr_lines);
        std::thread::spawn(move || {
            for line in BufReader::new(stderr_handle).lines().map_while(Result::ok) {
                sink.lock().unwrap().push(line.trim().to_string());
            }
        });
    }
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // wait for the port banner
        let deadline = Instant::now() + Duration::from_secs(15);
        let addr = loop {
            assert!(Instant::now() < deadline, "daemon never advertised its port; stderr: {:?}", stderr_lines.lock().unwrap());
            let found = stderr_lines.lock().unwrap().iter().find_map(|l| {
                l.strip_prefix("[serve-tcp] multi-surface daemon on ")
                    .map(|rest| rest.trim().split(" (").next().unwrap_or(rest).to_string())
            });
            if let Some(addr) = found {
                break addr;
            }
            std::thread::sleep(Duration::from_millis(50));
        };
        let stderr_contains = |needle: &str| {
            stderr_lines
                .lock()
                .unwrap()
                .iter()
                .any(|l| l.contains(needle))
        };

        // issue one web turn (the client chooses the session id)
        let session_id = "pin-session";
        let body = serde_json::json!({
            "commandId": "pin-1",
            "type": "sendText",
            "sessionId": session_id,
            "payload": { "text": "read notes.txt and summarize" }
        })
        .to_string();
        let req = format!(
            "POST /command HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(), body
        );
        TcpStream::connect(&addr).unwrap().write_all(req.as_bytes()).unwrap();

        // the turn starts → the clamp line must appear (non-blocking poll)
        let deadline = Instant::now() + Duration::from_secs(20);
        while !stderr_contains("[pin] max-turns clamped to 1") {
            assert!(Instant::now() < deadline, "no clamp line; stderr: {:?}", stderr_lines.lock().unwrap());
            std::thread::sleep(Duration::from_millis(50));
        }
        // the turn itself still completes (budget 1 is enough for the demo read)
        // — poll rows until the turnHeader is no longer running
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let mut s = TcpStream::connect(&addr).unwrap();
            s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
            let req = format!(
                "GET /api/sessions/{session_id}/rows HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
            );
            s.write_all(req.as_bytes()).unwrap();
            let mut resp = String::new();
            BufReader::new(s).read_to_string(&mut resp).unwrap();
            let body = resp.split("\r\n\r\n").nth(1).unwrap_or("{}");
            let running = serde_json::from_str::<serde_json::Value>(body)
                .ok()
                .and_then(|v| {
                    v["rows"].as_array().map(|rows| {
                        rows.iter().any(|r| {
                            r["kind"] == "turnHeader" && r["state"] == "running"
                        })
                    })
                })
                .unwrap_or(true);
            if !running {
                break;
            }
            assert!(Instant::now() < deadline, "turn never finished; rows: {resp}");
            std::thread::sleep(Duration::from_millis(200));
        }
    }));
    let _ = child.kill();
    let _ = child.wait();
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}
