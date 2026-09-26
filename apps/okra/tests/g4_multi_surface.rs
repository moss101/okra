//! G4 gate (MASTER-PLAN §4): ONE daemon session under the control of
//! SIMULTANEOUS surfaces. Two independent TCP clients attach to the real
//! `okra serve --tcp` daemon; projections fan out to both; a turn driven
//! by one surface is steered from the other; both surfaces observe the
//! same session state, and the TUI renders the same rows as a third
//! surface.

use std::io::{BufRead, BufReader};
use std::net::TcpStream;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use okra_tui::projection_row_line;

struct Surface {
    reader: BufReader<TcpStream>,
    writer: TcpStream,
}

impl Surface {
    fn attach(addr: &str) -> Surface {
        let stream = TcpStream::connect(addr).expect("connect to daemon");
        stream
            .set_read_timeout(Some(Duration::from_millis(500)))
            .unwrap();
        let writer = stream.try_clone().unwrap();
        let mut surface = Surface {
            reader: BufReader::new(stream),
            writer,
        };
        surface.send(1, r#"{"method":"hello"}"#);
        let reply = surface.read_reply(1);
        assert_eq!(reply["result"]["daemon"], "okra", "handshake");
        surface
    }

    fn send(&mut self, id: u64, body: &str) {
        use std::io::Write as _;
        let line = format!("{{\"id\":{id},{}\n", body.trim_start_matches('{'));
        self.writer.write_all(line.as_bytes()).unwrap();
        self.writer.flush().unwrap();
    }

    /// Read lines until the reply with `id` arrives.
    fn read_reply(&mut self, id: u64) -> serde_json::Value {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            assert!(Instant::now() < deadline, "timed out waiting for reply {id}");
            let Some(line) = read_line_timeout(&mut self.reader) else {
                continue;
            };
            let Ok(msg) = serde_json::from_str::<serde_json::Value>(line.trim()) else {
                continue;
            };
            if msg["id"].as_u64() == Some(id) {
                return msg;
            }
        }
    }

    /// Read lines until `predicate` holds for the buffered inbox.
    fn read_until(
        &mut self,
        inbox: &mut Vec<serde_json::Value>,
        predicate: &dyn Fn(&[serde_json::Value]) -> bool,
    ) {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if predicate(inbox) {
                return;
            }
            assert!(Instant::now() < deadline, "timed out; inbox={inbox:?}");
            let Some(line) = read_line_timeout(&mut self.reader) else {
                continue;
            };
            if let Ok(msg) = serde_json::from_str::<serde_json::Value>(line.trim()) {
                inbox.push(msg);
            }
        }
    }
}

/// One read with the socket timeout applied; None on timeout.
fn read_line_timeout(reader: &mut BufReader<TcpStream>) -> Option<String> {
    let mut line = String::new();
    match reader.read_line(&mut line) {
        Ok(0) => panic!("daemon closed the connection"),
        Ok(_) => Some(line),
        Err(e)
            if e.kind() == std::io::ErrorKind::WouldBlock
                || e.kind() == std::io::ErrorKind::TimedOut =>
        {
            None
        }
        Err(e) => panic!("read: {e}"),
    }
}

fn projections<'a>(inbox: &'a [serde_json::Value]) -> Vec<&'a serde_json::Value> {
    inbox
        .iter()
        .filter(|m| m["method"] == "v4/projection")
        .map(|m| &m["params"])
        .collect()
}

fn has_row_where(
    projection: &serde_json::Value,
    needle: &dyn Fn(&serde_json::Value) -> bool,
) -> bool {
    projection["rows"]
        .as_array()
        .map(|rows| rows.iter().any(needle))
        .unwrap_or(false)
}

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
        if let Some(rest) = line.strip_prefix("[serve-tcp] multi-surface daemon on ") {
            let rest = rest.trim();
            let addr = rest.split(" (").next().unwrap_or(rest);
            break addr.to_string();
        }
    };
    (child, addr)
}

use std::path::Path;

#[test]
fn g4_one_session_two_surfaces_cross_steering() {
    let td = tempfile::tempdir().unwrap();
    std::fs::write(
        td.path().join("notes.md"),
        "# notes\nthe workspace memo\n",
    )
    .unwrap();
    std::fs::write(td.path().join("todo.md"), "# todo\nship the gate\n").unwrap();

    let (mut daemon, addr) = spawn_daemon(td.path());
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        run_gate(&addr);
    }));
    let _ = daemon.kill();
    let _ = daemon.wait();
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

fn run_gate(addr: &str) {
    // two surfaces attach to the SAME daemon
    let mut tui_surface = Surface::attach(addr);
    let mut second = Surface::attach(addr);

    // both subscribe to the same session
    tui_surface.send(
        2,
        r#"{"method":"v4/conversation/subscribe","params":{"sessionId":"gate-session"}}"#,
    );
    second.send(
        2,
        r#"{"method":"v4/conversation/subscribe","params":{"sessionId":"gate-session"}}"#,
    );
    let ack_a = tui_surface.read_reply(2);
    let ack_b = second.read_reply(2);
    assert!(ack_a["result"]["ack"]["subscriptionId"].is_string());
    assert!(ack_b["result"]["ack"]["subscriptionId"].is_string());

    // surface B steers BEFORE any turn: the daemon must hold the queue
    second.send(
        3,
        r#"{"method":"v4/steer","params":{"sessionId":"gate-session","text":"also check the todo file"}}"#,
    );
    let steered = second.read_reply(3);
    assert_eq!(steered["result"]["steered"], true);

    // surface A drives the turn
    tui_surface.send(
        4,
        r#"{"method":"v4/command","params":{"envelope":{"commandId":"cmd-1","type":"sendText","sessionId":"gate-session","payload":{"text":"summarize notes.md"}}}}"#,
    );
    let accepted = tui_surface.read_reply(4);
    assert_eq!(accepted["result"]["status"], "accepted");

    // BOTH surfaces observe the turn, including the cross-surface steer
    let mut inbox_a: Vec<serde_json::Value> = Vec::new();
    let mut inbox_b: Vec<serde_json::Value> = Vec::new();
    let turn_complete_with_steer = |inbox: &[serde_json::Value]| {
        projections(inbox).iter().any(|p| {
            p["control"]["phase"] == "completedSuccess"
                && has_row_where(p, &|row| {
                    row["kind"] == "userInput"
                        && row["text"]
                            .as_str()
                            .map(|t| t.contains("also check the todo file"))
                            .unwrap_or(false)
                })
        })
    };
    tui_surface.read_until(&mut inbox_a, &turn_complete_with_steer);
    second.read_until(&mut inbox_b, &turn_complete_with_steer);

    // fan-out equality: both surfaces ended on the SAME session state
    let projections_a: Vec<&serde_json::Value> = projections(&inbox_a);
    let projections_b: Vec<&serde_json::Value> = projections(&inbox_b);
    let final_a = projections_a.last().expect("tui saw projections");
    let final_b = projections_b.last().expect("second saw projections");
    assert_eq!(final_a["rows"], final_b["rows"], "identical session state");
    assert_eq!(final_a["revision"], final_b["revision"]);
    assert!(has_row_where(final_a, &|row| {
        row["kind"] == "userInput"
            && row["text"]
                .as_str()
                .map(|t| t.contains("summarize notes.md"))
                .unwrap_or(false)
    }));
    assert!(has_row_where(final_a, &|row| {
        row["kind"] == "userInput"
            && row["text"]
                .as_str()
                .map(|t| t.contains("also check the todo file"))
                .unwrap_or(false)
    }));

    // the TUI renders the same rows as scrollback: turn header, both user
    // inputs (original + steered) and tool activity are visible
    let scrollback: Vec<String> = final_a["rows"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(projection_row_line)
        .collect();
    let joined = scrollback.join("\n");
    assert!(joined.contains("── turn ──"), "{joined}");
    assert!(joined.contains("> summarize notes.md"), "{joined}");
    assert!(joined.contains("> [steered] also check the todo file"), "{joined}");
    assert!(joined.contains("✓ read_file"), "tool activity rendered: {joined}");
}
