//! N0038 — sessions as a FIRST-CLASS surface (Devin study: a sessions
//! PAGE, not a side panel): the served workbench ships the full-canvas
//! sessions view (searchable grid over /api/sessions) and the topbar
//! entry point; /api/sessions already exists and is covered by
//! g4_sessions_index_and_replay.

// Test harness: drives the compiled binary end-to-end.
#![allow(clippy::disallowed_methods)]
use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use std::path::Path;

fn spawn_daemon(cwd: &Path) -> (Child, String) {
    let bin = env!("CARGO_BIN_EXE_okra");
    let mut cmd = Command::new(bin);
    cmd.args(["serve", "--tcp", "--cwd", &cwd.to_string_lossy()]);
    let mut child = cmd.stderr(Stdio::piped()).spawn().expect("spawn okra serve --tcp");
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

#[test]
fn workbench_ships_the_sessions_page_surface() {
    let td = tempfile::tempdir().unwrap();
    let (mut daemon, addr) = spawn_daemon(td.path());
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let (status, index) = http_get(&addr, "/");
        assert_eq!(status, 200);
        assert!(index.contains("sessions-page"), "the full-canvas view exists: {}", &index[..index.len().min(400)]);
        assert!(index.contains("sessions-view-btn"), "the topbar entry point exists");
        assert!(index.contains("sessions-search"), "searchable");
        assert!(index.contains("sessions-grid"), "the grid host exists");

        let (status, js) = http_get(&addr, "/app.js");
        assert_eq!(status, 200);
        assert!(js.contains("renderSessionsPage"), "the renderer shipped");
        assert!(js.contains("toggleSessionsPage"), "the toggle shipped");
        assert!(js.contains("/api/sessions"), "feeds off the sessions index");

        let (status, css) = http_get(&addr, "/app.css");
        assert_eq!(status, 200);
        assert!(css.contains("sessions-grid"), "the page styles shipped");
    }));
    let _ = daemon.kill();
    let _ = daemon.wait();
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}
