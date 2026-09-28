//! M6 replay delivery: `GET /replay/<session-id>` on the daemon's existing
//! HTTP surface serves the standalone replay transcript. The bind stays
//! loopback (N0007 posture: no editor clients, and phone delivery needs an
//! operator tunnel) — this proves the artifact is browser-deliverable
//! through a surface that already exists.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

#[allow(clippy::disallowed_methods)] // test harness: runs the compiled binary
fn spawn_daemon(cwd: &Path) -> (Child, String) {
    let mut child = Command::new(env!("CARGO_BIN_EXE_okra"))
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
            break rest.split(" (").next().unwrap_or(rest).trim().to_string();
        }
    };
    std::thread::spawn(move || {
        for line in reader.lines().map_while(Result::ok) {
            eprintln!("DAEMON: {}", line.trim());
        }
    });
    (child, addr)
}

fn http_get(addr: &str, path: &str) -> (u16, String, String) {
    let mut stream = std::net::TcpStream::connect(addr).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    write!(stream, "GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n").unwrap();
    let mut reader = BufReader::new(stream);
    let mut status_line = String::new();
    reader.read_line(&mut status_line).unwrap();
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let mut headers = String::new();
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        if line == "\r\n" || line.is_empty() {
            break;
        }
        headers.push_str(&line);
    }
    let mut body = String::new();
    reader.read_to_string(&mut body).unwrap();
    (status, headers, body)
}

#[test]
#[allow(clippy::disallowed_methods)] // test harness: runs the compiled binary
fn replay_is_deliverable_over_the_http_surface() {
    let td = tempfile::tempdir().unwrap();
    // a session with content AND a hostile-content session (escaping check)
    let sessions = td.path().join(".okra-sessions");
    let header = okra_kernel::SessionHeader {
        version: okra_kernel::SESSION_FORMAT_VERSION,
        id: "session-http-replay".into(),
        created_at: 1_789_600_000_000.0,
        cwd: td.path().to_string_lossy().into_owned(),
        parent_session: None,
        is_seeded: false,
    };
    let mut handle = okra_kernel::SessionHandle::create(&sessions, &header).unwrap();
    handle
        .append(vec![okra_kernel::make_event(
            "user/message",
            serde_json::json!({ "text": "<img src=x onerror=alert(1)> replay me" }),
            || 1_789_600_000_000.0,
        )])
        .unwrap();
    drop(handle);

    let (mut daemon, addr) = spawn_daemon(td.path());
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // known session: the transcript, fully escaped
        let (status, headers, body) = http_get(&addr, "/replay/session-http-replay");
        assert_eq!(status, 200, "{body}");
        assert!(headers.to_lowercase().contains("text/html"), "{headers}");
        assert!(body.starts_with("<!doctype html>"), "{body}");
        assert!(body.contains("replay me"));
        assert!(
            !body.contains("<img src=x"),
            "hostile content must be escaped: {body}"
        );
        assert!(body.contains("&lt;img"), "{body}");

        // unknown session: 404 with the session id named
        let (status, _, body) = http_get(&addr, "/replay/session-never-was");
        assert_eq!(status, 404, "{body}");
        assert!(body.contains("session-never-was"), "{body}");

        // traversal attempts are rejected, not escaped into a read
        let (status, _, body) = http_get(&addr, "/replay/..%2F..%2Fetc");
        assert_eq!(status, 400, "path traversal must be rejected: {body}");
        assert!(body.contains("invalid session id"), "{body}");
    }));
    let _ = daemon.kill();
    let _ = daemon.wait();
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}
