//! Sanctioned subprocess + HTTP execution site for the tools crate.
//!
//! clippy.toml bans raw `Command::spawn/status/output` so every child in
//! okra gets confinement accounting (okra-policy, M3 bash runner). The hook
//! runner and MCP stdio transport are the two SANCTIONED exceptions for M2:
//! both run user/admin-configured programs from the hook/MCP config with a
//! timeout, never model-supplied argv. Keep new spawn sites out of this
//! module.

use std::io::{BufRead, Read, Write};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::Value;

/// Captured output of a finished child process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapturedOutput {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: Option<i32>,
    /// True when the timeout fired and the child was killed.
    pub timed_out: bool,
}

#[allow(clippy::disallowed_methods)] // sanctioned site (see module docs)
fn spawn_captured_inner(
    program: &str,
    args: &[String],
    timeout: Duration,
    input: Option<&str>,
) -> CapturedOutput {
    let child = Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn();

    let mut child = match child {
        Ok(c) => c,
        Err(e) => {
            return CapturedOutput {
                stdout: String::new(),
                stderr: format!("spawn failed: {e}"),
                exit_code: None,
                timed_out: false,
            }
        }
    };
    // feed stdin (best effort) and drop it so the child sees EOF
    if let Some(input) = input
        && let Some(mut stdin) = child.stdin.take()
    {
        let _ = stdin.write_all(input.as_bytes());
    }
    drop(child.stdin.take());

    // poll with a deadline (std has no wait_timeout)
    let started = std::time::Instant::now();
    let mut timed_out = false;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let mut stdout = String::new();
                let mut stderr = String::new();
                if let Some(mut o) = child.stdout.take() {
                    let _ = o.read_to_string(&mut stdout);
                }
                if let Some(mut e) = child.stderr.take() {
                    let _ = e.read_to_string(&mut stderr);
                }
                return CapturedOutput {
                    stdout,
                    stderr,
                    exit_code: status.code(),
                    timed_out: false,
                };
            }
            Ok(None) => {
                if started.elapsed() >= timeout {
                    timed_out = true;
                    let _ = child.kill();
                    let mut stdout = String::new();
                    let mut stderr = String::new();
                    if let Some(mut o) = child.stdout.take() {
                        let _ = o.read_to_string(&mut stdout);
                    }
                    if let Some(mut e) = child.stderr.take() {
                        let _ = e.read_to_string(&mut stderr);
                    }
                    return CapturedOutput {
                        stdout,
                        stderr,
                        exit_code: None,
                        timed_out,
                    };
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(e) => {
                return CapturedOutput {
                    stdout: String::new(),
                    stderr: format!("wait failed: {e}"),
                    exit_code: None,
                    timed_out,
                }
            }
        }
    }
}

/// Run a program to completion with captured output and a timeout.
pub fn run_captured(
    program: &str,
    args: &[String],
    timeout: Duration,
    input: Option<&str>,
) -> CapturedOutput {
    spawn_captured_inner(program, args, timeout, input)
}

/// Sync HTTP POST of a JSON body (hook webhooks). Returns (status, body).
pub fn http_post_json(url: &str, body: &str, timeout: Duration) -> Result<(u16, String), String> {
    let agent = ureq::AgentBuilder::new().timeout(timeout).build();
    match agent.post(url).send_string(body) {
        Ok(resp) => {
            let status = resp.status();
            let mut text = String::new();
            let _ = resp.into_reader().read_to_string(&mut text);
            Ok((status, text))
        }
        Err(ureq::Error::Status(status, resp)) => {
            let mut text = String::new();
            let _ = resp.into_reader().read_to_string(&mut text);
            Ok((status, text))
        }
        Err(e) => Err(format!("http: {e}")),
    }
}

/// A long-lived child with piped stdio and line-delimited JSON-RPC
/// request/response matching (N0022 — persistent MCP sessions).
///
/// One reader thread owns stdout and dispatches every response line to
/// the waiter holding its id; writers serialize on the child's stdin
/// mutex. The child lives until [`PersistentChild::kill`] or process
/// exit — callers own the lifecycle.
pub struct PersistentChild {
    stdin: Mutex<std::process::ChildStdin>,
    waiters: Arc<Mutex<HashMap<u64, mpsc::Sender<Value>>>>,
    next_id: AtomicU64,
    pid: u32,
    dead: Arc<AtomicBool>,
}

impl PersistentChild {
    /// Spawn `program args` with piped stdio. Returns an error string on
    /// spawn failure (never panics).
    #[allow(clippy::disallowed_methods)] // sanctioned site (see module docs)
    pub fn spawn(program: &str, args: &[String]) -> Result<PersistentChild, String> {
        let mut child = Command::new(program)
            .args(args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .map_err(|e| format!("persistent spawn: {e}"))?;
        let pid = child.id();
        let stdin = child.stdin.take().ok_or("persistent spawn: no stdin")?;
        let stdout = child.stdout.take().ok_or("persistent spawn: no stdout")?;

        // background the child so it outlives this handle's scope
        // (we never wait() it; kill is explicit)
        let waiters: Arc<Mutex<HashMap<u64, mpsc::Sender<Value>>>> =
            Arc::new(Mutex::new(std::collections::HashMap::new()));
        let dead = Arc::new(AtomicBool::new(false));

        {
            let waiters = Arc::clone(&waiters);
            let dead = Arc::clone(&dead);
            std::thread::spawn(move || {
                let mut child = child;
                let reader = std::io::BufReader::new(stdout);
                for line in reader.lines().map_while(Result::ok) {
                    let Ok(msg) = serde_json::from_str::<Value>(&line) else {
                        continue;
                    };
                    let Some(id) = msg.get("id").and_then(|v| v.as_u64()) else {
                        continue; // notifications: ignored for now
                    };
                    if let Some(w) = waiters.lock().unwrap().remove(&id) {
                        let _ = w.send(msg);
                    }
                }
                // EOF: child exited — wake every waiter with nothing
                // (they time out) and mark the session dead
                dead.store(true, Ordering::SeqCst);
                child.kill().ok();
            });
        }

        Ok(PersistentChild {
            stdin: Mutex::new(stdin),
            waiters,
            next_id: AtomicU64::new(0),
            pid,
            dead,
        })
    }

    /// Send one JSON-RPC request; returns the full response envelope once
    /// a line with the matching id arrives (or Err on timeout/dead pipe).
    pub fn request(
        &self,
        method: &str,
        params: &Value,
        timeout: Duration,
    ) -> Result<Value, String> {
        if self.dead.load(Ordering::SeqCst) {
            return Err(format!("mcp `{method}`: session ended"));
        }
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let envelope = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        {
            let (tx, rx) = mpsc::channel();
            self.waiters.lock().unwrap().insert(id, tx);
            let write = self.stdin.lock().unwrap().write_all(
                format!("{envelope}\n").as_bytes(),
            );
            if let Err(e) = write {
                self.waiters.lock().unwrap().remove(&id);
                return Err(format!("mcp `{method}`: stdin write failed: {e}"));
            }
            match rx.recv_timeout(timeout) {
                Ok(response) => Ok(response),
                Err(_) => {
                    self.waiters.lock().unwrap().remove(&id);
                    Err(format!("mcp `{method}` timed out"))
                }
            }
        }
    }

    pub fn pid(&self) -> u32 {
        self.pid
    }

    pub fn is_dead(&self) -> bool {
        self.dead.load(Ordering::SeqCst)
    }

    /// Best-effort shutdown: an MCP `exit` notification, then the stdin
    /// pipe drops (most servers exit on EOF). The dead flag makes later
    /// requests fail fast.
    pub fn kill(&self) {
        if self.dead.swap(true, Ordering::SeqCst) {
            return;
        }
        let exit = serde_json::json!({
            "jsonrpc": "2.0", "method": "notifications/cancelled",
            "params": {}
        });
        let _ = self
            .stdin
            .lock()
            .unwrap()
            .write_all(format!("{exit}\n").as_bytes());
    }
}
