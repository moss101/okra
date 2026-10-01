//! n0042 — mediation policies DIVERGE on a live daemon: under
//! `--mediation designated --designated workbench`, approvals only exist
//! when a BROWSER surface is attached. With none: the ask falls through
//! (unavailable → the write is refused) AND a resolveApproval from the
//! NDJSON surface is rejected with `okra.mediation.designatedAbsent`.
//! With a browser attached: the same resolution is accepted and the
//! write lands. First-responder remains the default (back-compat).

// Test harness: drives the compiled binary end-to-end.
#![allow(clippy::disallowed_methods)]
use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use std::path::Path;

fn spawn_daemon_args(cwd: &Path, extra: &[&str]) -> (Child, String) {
    let bin = env!("CARGO_BIN_EXE_okra");
    let mut cmd = Command::new(bin);
    cmd.args(["serve", "--tcp", "--cwd", &cwd.to_string_lossy()]);
    cmd.args(extra);
    let mut child = cmd.stderr(Stdio::piped()).spawn().expect("spawn");
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

/// A minimal NDJSON surface (the TUI's protocol, hand-rolled for the test).
struct NdSurface {
    reader: BufReader<TcpStream>,
    writer: TcpStream,
    next_id: u64,
    inbox: Vec<serde_json::Value>,
}

impl NdSurface {
    fn attach(addr: &str) -> NdSurface {
        let stream = TcpStream::connect(addr).unwrap();
        stream.set_read_timeout(Some(Duration::from_millis(100))).unwrap();
        let writer = stream.try_clone().unwrap();
        let mut s = NdSurface { reader: BufReader::new(stream), writer, next_id: 1, inbox: Vec::new() };
        let _ = s.call("hello", serde_json::json!({}));
        s
    }

    fn call(&mut self, method: &str, params: serde_json::Value) -> serde_json::Value {
        let id = self.next_id;
        self.next_id += 1;
        let line = serde_json::json!({ "id": id, "method": method, "params": params });
        self.writer
            .write_all(format!("{}\n", serde_json::to_string(&line).unwrap()).as_bytes())
            .unwrap();
        self.writer.flush().unwrap();
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            assert!(Instant::now() < deadline, "reply {id} never arrived");
            let mut buf = String::new();
            match self.reader.read_line(&mut buf) {
                Ok(0) => panic!("closed"),
                Ok(_) => {}
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock || e.kind() == std::io::ErrorKind::TimedOut => continue,
                Err(e) => panic!("read: {e}"),
            }
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(buf.trim()) {
                if v["id"].as_u64() == Some(id) {
                    return v;
                }
                self.inbox.push(v);
            }
        }
    }

    fn drain(&mut self) {
        let mut buf = String::new();
        loop {
            buf.clear();
            match self.reader.read_line(&mut buf) {
                Ok(0) => return,
                Ok(_) => {
                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(buf.trim()) {
                        self.inbox.push(v);
                    }
                }
                Err(_) => return,
            }
        }
    }

    fn subscribe(&mut self, session: &str) {
        let _ = self.call(
            "v4/conversation/subscribe",
            serde_json::json!({ "sessionId": session }),
        );
    }

    fn command(&mut self, cmd_type: &str, session: &str, payload: serde_json::Value) -> serde_json::Value {
        let id = self.next_id;
        self.call(
            "v4/command",
            serde_json::json!({
                "envelope": {
                    "commandId": format!("med-{id}"),
                    "type": cmd_type,
                    "sessionId": session,
                    "payload": payload,
                }
            }),
        )
    }

    fn phase(&self) -> String {
        self.inbox
            .iter()
            .rev()
            .find_map(|f| f["params"]["control"]["phase"].as_str().map(str::to_string))
            .unwrap_or_default()
    }

    fn pending_approval(&mut self) -> Option<String> {
        self.drain();
        self.inbox.iter().rev().find_map(|f| {
            f["params"]["control"]["awaitingApproval"]
                .as_array()
                .and_then(|a| a.first())
                .and_then(|p| p["approvalId"].as_str().map(str::to_string))
        })
    }
}

/// Hold a browser SSE surface open (the designated client, attached).
fn hold_browser_surface(addr: &str, session: &str) -> std::thread::JoinHandle<()> {
    let addr = addr.to_string();
    let session = session.to_string();
    std::thread::spawn(move || {
        match TcpStream::connect(&addr) {
            Ok(mut stream) => {
            // NO read timeout: a blocking read holds the surface open
            // until the daemon closes (the thread dies with the process)
            stream.set_read_timeout(None).ok();
            let _ = stream.write_all(
                format!("GET /sse/{session} HTTP/1.1\r\nHost: localhost\r\nAccept: text/event-stream\r\n\r\n").as_bytes(),
            );
            let _ = stream.flush();
            // blocking read LOOP: each read parks until data — the
            // surface stays attached until the daemon closes
            let mut scratch = [0u8; 512];
            loop {
                match std::io::Read::read(&mut stream, &mut scratch) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
            }
            }
            Err(e) => eprintln!("[hold] CONNECT FAILED: {e}"),
        }
    })
}

#[test]
fn designated_mediation_diverges_on_browser_attachment() {
    let td = tempfile::tempdir().unwrap();
    let mediation: &[&str] = &["--mediation", "designated", "--designated", "workbench"];

    // ---- no browser attached: the designation answers nothing ----
    let (mut daemon, addr) = spawn_daemon_args(td.path(), mediation);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut nd = NdSurface::attach(&addr);
        // the UI's actual first-message flow: text rides WITH
        // createSession (one turn, no empty-session turn, no steering)
        let created = nd.command("createSession", "", serde_json::json!({ "text": "create designated-a.md" }));
        let session = created["result"]["result"]["sessionId"].as_str().unwrap().to_string();
        assert_eq!(created["result"]["status"], "accepted");
        nd.subscribe(&session);

        // under the designated policy with the browser ABSENT, the
        // mediator answers nothing BEFORE the bridge is consulted — so
        // no approval card registers, and any resolution attempt is
        // refused at the command gate (the designation is absent)
        std::thread::sleep(Duration::from_millis(500));
        let refused = nd.command(
            "resolveApproval",
            &session,
            serde_json::json!({ "approvalId": "med-absent", "decision": "allow" }),
        );
        assert_eq!(refused["result"]["status"], "rejected", "{refused}");
        assert_eq!(refused["result"]["reasonCode"], "okra.mediation.designatedAbsent");

        // the executor's waterfall denies the side-effecting tool
        // (unavailable): the turn completes WITHOUT the write
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            nd.drain();
            let phase = nd.phase();
            if phase.starts_with("completed") || phase == "failed" {
                break;
            }
            assert!(Instant::now() < deadline, "turn never ended (phase={phase})");
            std::thread::sleep(Duration::from_millis(150));
        }
        assert!(
            !td.path().join("designated-a.md").exists(),
            "the write was refused — no designated surface to answer the ask"
        );
    }));
    let _ = daemon.kill();
    let _ = daemon.wait();
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }

    // ---- browser attached: the same resolution is accepted ----
    let (mut daemon, addr) = spawn_daemon_args(td.path(), mediation);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        // the designated surface attaches FIRST — the turn's ask
        // consults the probe at ask time, so the browser must be live
        // BEFORE the turn spawns (any session id: registration is
        // connection-scoped)
        let browser = hold_browser_surface(&addr, "designated-holder");
        std::thread::sleep(Duration::from_millis(600));

        let mut nd = NdSurface::attach(&addr);
        let created = nd.command("createSession", "", serde_json::json!({ "text": "create designated-b.md" }));
        let session = created["result"]["result"]["sessionId"].as_str().unwrap().to_string();
        assert_eq!(created["result"]["status"], "accepted");
        nd.subscribe(&session);

        let deadline = Instant::now() + Duration::from_secs(20);
        let approval = loop {
            assert!(Instant::now() < deadline, "ask never surfaced");
            if let Some(id) = nd.pending_approval() {
                break id;
            }
            std::thread::sleep(Duration::from_millis(100));
        };

        // the resolution counts now (designated surface is LIVE)
        let resolved = nd.command(
            "resolveApproval",
            &session,
            serde_json::json!({ "approvalId": approval, "decision": "allow" }),
        );
        assert_eq!(resolved["result"]["status"], "accepted", "{resolved}");

        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            nd.drain();
            if nd.phase().starts_with("completed") || nd.phase() == "failed" {
                break;
            }
            assert!(Instant::now() < deadline, "turn never ended");
            std::thread::sleep(Duration::from_millis(150));
        }
        assert!(td.path().join("designated-b.md").is_file(), "approved write landed");
        // the holder thread is detached on purpose: it exits when the
        // daemon (killed below) closes the SSE stream — joining here
        // would block forever on a healthy connection
        drop(browser);
    }));
    let _ = daemon.kill();
    let _ = daemon.wait();
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}
