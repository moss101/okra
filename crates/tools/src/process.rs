//! Sanctioned subprocess + HTTP execution site for the tools crate.
//!
//! clippy.toml bans raw `Command::spawn/status/output` so every child in
//! okra gets confinement accounting (okra-policy, M3 bash runner). The hook
//! runner and MCP stdio transport are the two SANCTIONED exceptions for M2:
//! both run user/admin-configured programs from the hook/MCP config with a
//! timeout, never model-supplied argv. Keep new spawn sites out of this
//! module.

use std::io::{Read, Write};
use std::process::Command;
use std::time::Duration;

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
