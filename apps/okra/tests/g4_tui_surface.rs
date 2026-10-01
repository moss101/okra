//! n0042 — the TUI as a daemon surface, end to end: `okra tui --attach
//! --smoke` drives a REAL session over the NDJSON v4 protocol — renders
//! the projection, resolves an approval (y), and reaches a terminal
//! phase; a second drive DENIES and the write never lands. The G4 line
//! ("one live session visible and steerable from browser + TUI
//! simultaneously") — this proves the TUI leg against the same daemon
//! the browser tests drive.

// Test harness: drives the compiled binary end-to-end.
#![allow(clippy::disallowed_methods)]
use std::io::{BufRead, BufReader, Write};
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

#[test]
fn tui_surface_smoke_allows_and_denies_over_the_live_daemon() {
    let td = tempfile::tempdir().unwrap();

    // ---- ALLOW: the TUI surface resolves the write approval with y ----
    let (mut daemon, addr) = spawn_daemon(td.path());
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let out = Command::new(env!("CARGO_BIN_EXE_okra"))
            .args([
                "tui",
                "--attach", &addr,
                "--smoke", "create allowed-tui.md",
            ])
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        assert!(
            out.status.success(),
            "tui smoke failed: {stderr}\n--- stdout ---\n{stdout}"
        );
        // the projection rendered into scrollback lines
        assert!(stdout.contains("turn ·") || stdout.contains("▸"), "rows rendered: {stdout}");
        // the composer's turn completed
        assert!(td.path().join("allowed-tui.md").is_file(), "approved write landed");
    }));
    let _ = daemon.kill();
    let _ = daemon.wait();
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }

    // ---- DENY: the write never lands, the turn still ends honestly ----
    let (mut daemon, addr) = spawn_daemon(td.path());
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let out = Command::new(env!("CARGO_BIN_EXE_okra"))
            .args([
                "tui",
                "--attach", &addr,
                "--smoke", "create denied-tui.md",
                "--deny-approvals",
            ])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "denied smoke still completes the turn: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            !td.path().join("denied-tui.md").exists(),
            "denied write must not land"
        );
    }));
    let _ = daemon.kill();
    let _ = daemon.wait();
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

#[test]
fn tui_surface_attaches_to_an_existing_session_and_sees_history() {
    let td = tempfile::tempdir().unwrap();
    std::fs::write(td.path().join("notes.txt"), "tui surface fixture").unwrap();
    let (mut daemon, addr) = spawn_daemon(td.path());
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // session 1: the smoke creates a session and completes a turn
        let out = Command::new(env!("CARGO_BIN_EXE_okra"))
            .args(["tui", "--attach", &addr, "--smoke", "read notes.txt and summarize it"])
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        // discover the session from the index
        let sessions: serde_json::Value = ureq_less_get(&addr, "/api/sessions");
        let id = sessions["sessions"][0]["id"].as_str().unwrap().to_string();
        assert!(!id.is_empty(), "session indexed: {sessions}");

        // attach to the SAME session by id: the replayed projection rows
        // of turn 1 are visible to the new subscriber immediately
        let out = Command::new(env!("CARGO_BIN_EXE_okra"))
            .args([
                "tui", "--attach", &addr,
                "--session", &id,
                "--smoke", "read notes.txt again",
            ])
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        assert!(
            out.status.success(),
            "attach-to-existing failed: {}\n--- stdout ---\n{stdout}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(
            stdout.contains("read notes.txt and summarize it"),
            "turn 1's rows replay to the late TUI subscriber: {stdout}"
        );
        assert!(
            stdout.contains("read notes.txt again"),
            "turn 2's rows render too: {stdout}"
        );
    }));
    let _ = daemon.kill();
    let _ = daemon.wait();
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

fn ureq_less_get(addr: &str, path: &str) -> serde_json::Value {
    use std::net::TcpStream;
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
    let body_start = response.find("\n\n").map(|i| i + 2).unwrap_or(0);
    serde_json::from_str(response[body_start..].trim()).unwrap_or(serde_json::json!({}))
}
