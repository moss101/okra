//! `fake-mcp` — a cross-platform stdio JSON-RPC fixture for the MCP
//! integration tests (the sh-script stand-in was the one unix-only piece
//! of the windows bring-up). A PERSISTENT responder: loops until EOF and
//! counts `initialize` lines to a temp file — the initialize-exactly-once
//! proof for persistent MCP sessions.
//!
//! Contract (mirrors the tests' assertions):
//! - `initialize`  → protocolVersion 2024-11-05, serverInfo `fake-tools`,
//!   increments `<temp>/okra-mcp-init-count`;
//! - `tools/list`  → one tool: `probe-tool` (object schema);
//! - `tools/call`  → `echo: <text>` from the request's `"text"` field;
//! - anything else → empty result.

use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::process::ExitCode;

/// Pull the `"id":<n>` value out of a request line (no JSON dependency —
/// the fixture must stay a zero-config binary).
fn extract_id(req: &str) -> i64 {
    let Some(pos) = req.find("\"id\":") else { return 0 };
    let rest = &req[pos + "\"id\":".len()..];
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().unwrap_or(0)
}

/// Pull the `"text":"..."` value out of a tools/call request.
fn extract_text(req: &str) -> String {
    let Some(pos) = req.find("\"text\":\"") else { return String::new() };
    let rest = &req[pos + "\"text\":\"".len()..];
    rest.chars().take_while(|c| *c != '"' && *c != '\\').collect()
}

fn main() -> ExitCode {
    // `--count-file <path>` lets concurrent test instances stay isolated
    let args: Vec<String> = std::env::args().collect();
    let count_file = args
        .iter()
        .position(|a| a == "--count-file")
        .and_then(|i| args.get(i + 1))
        .map(PathBuf::from)
        .unwrap_or_else(|| std::env::temp_dir().join("okra-mcp-init-count"));
    let _ = std::fs::write(&count_file, "0");

    let stdin = std::io::stdin();
    let mut out = std::io::stdout();
    for line in stdin.lock().lines() {
        let Ok(req) = line else { break };
        let id = extract_id(&req);
        let body = if req.contains("initialize") {
            let n: u64 = std::fs::read_to_string(&count_file)
                .ok()
                .and_then(|s| s.trim().parse().ok())
                .unwrap_or(0);
            let _ = std::fs::write(&count_file, (n + 1).to_string());
            format!(
                r#"{{"jsonrpc":"2.0","id":{id},"result":{{"protocolVersion":"2024-11-05","serverInfo":{{"name":"fake-tools","version":"1.0"}}}}}}"#
            )
        } else if req.contains("tools/list") {
            format!(
                r#"{{"jsonrpc":"2.0","id":{id},"result":{{"tools":[{{"name":"probe-tool","description":"canned","inputSchema":{{"type":"object"}}}}]}}}}"#
            )
        } else if req.contains("tools/call") {
            let text = extract_text(&req);
            format!(
                r#"{{"jsonrpc":"2.0","id":{id},"result":{{"content":[{{"type":"text","text":"echo: {text}"}}]}}}}"#
            )
        } else {
            format!(r#"{{"jsonrpc":"2.0","id":{id},"result":{{}}}}"#)
        };
        if writeln!(out, "{body}").is_err() {
            // stdout closed: the parent is gone — exit cleanly
            return ExitCode::SUCCESS;
        }
        let _ = out.flush();
    }
    ExitCode::SUCCESS
}
