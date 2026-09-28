//! G4 remainder (MASTER-PLAN §3 #57): the ACP gateway — okra as an Agent
//! Client Protocol agent over stdio, the seam editors (Zed et al.) drive.
//! A scripted ACP client plays the editor: initialize handshake (version
//! negotiation), session/new, session/prompt with streamed session/update
//! notifications (agent_message_chunk, tool_call, tool_call_update), and
//! JSON-RPC error conventions.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant};

#[allow(clippy::disallowed_methods)] // test harness: executes the compiled binary
fn spawn_agent(cwd: &Path) -> Child {
    spawn_agent_env(cwd, &[])
}

#[allow(clippy::disallowed_methods)] // test harness: executes the compiled binary
fn spawn_agent_env(cwd: &Path, extra_env: &[(&str, &str)]) -> Child {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_okra"));
    cmd.args(["serve", "--acp", "--cwd", &cwd.to_string_lossy()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    cmd.spawn().expect("spawn okra serve --acp")
}

struct AcpClient {
    stdin: std::process::ChildStdin,
    reader: BufReader<std::process::ChildStdout>,
    next_id: i64,
}

impl AcpClient {
    fn send(&mut self, value: serde_json::Value) {
        let mut line = serde_json::to_vec(&value).unwrap();
        line.push(b'\n');
        self.stdin.write_all(&line).unwrap();
        self.stdin.flush().unwrap();
    }

    fn request(&mut self, method: &str, params: serde_json::Value) -> serde_json::Value {
        let id = self.next_id;
        self.next_id += 1;
        self.send(serde_json::json!({
            "jsonrpc": "2.0", "id": id, "method": method, "params": params
        }));
        self.wait_for_reply(id)
    }

    /// Read messages until the JSON-RPC reply for `id` arrives, collecting
    /// every notification seen along the way (session/update et al.).
    fn wait_for_reply(&mut self, id: i64) -> serde_json::Value {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            assert!(Instant::now() < deadline, "timed out waiting for reply {id}");
            let mut line = String::new();
            match self.reader.read_line(&mut line) {
                Ok(0) => panic!("agent closed stdout while waiting for reply {id}"),
                Ok(_) => {}
                Err(e) => panic!("read agent stdout: {e}"),
            }
            let msg: serde_json::Value = serde_json::from_str(line.trim())
                .unwrap_or_else(|e| panic!("bad JSON from agent: {e}: {line}"));
            if msg["id"] == serde_json::json!(id) && (msg["result"].is_object() || msg["error"].is_object()) {
                return msg;
            }
            if msg["method"] == "session/update" {
                UPDATES.lock().unwrap().push(msg["params"].clone());
            }
        }
    }
}

static UPDATES: Mutex<Vec<serde_json::Value>> = Mutex::new(Vec::new());

/// Tests run in parallel in ONE process and share the global update feed,
/// so a drain is only meaningful for one specific session — filter by it.
fn drain_updates(session_id: &str) -> Vec<serde_json::Value> {
    std::mem::take(&mut *UPDATES.lock().unwrap())
        .into_iter()
        .filter(|u| u["sessionId"] == serde_json::Value::String(session_id.to_string()))
        .collect()
}

fn kill(mut child: Child) {
    let _ = child.kill();
    let _ = child.wait();
}

#[test]
fn g4_acp_editor_seam_end_to_end() {
    let td = tempfile::tempdir().unwrap();
    // the EDITOR's workspace differs from the daemon's launch cwd: the turn
    // must read from the workspace passed in session/new (ACP `cwd`)
    let workspace = td.path().join("zed-project");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::write(workspace.join("notes.md"), "# acp notes\nthe editor seam works\n").unwrap();
    let mut agent = spawn_agent(td.path());
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut client = AcpClient {
            stdin: agent.stdin.take().unwrap(),
            reader: BufReader::new(agent.stdout.take().unwrap()),
            next_id: 1,
        };

        // 1. initialize: version echo + capabilities + agentInfo
        let init = client.request(
            "initialize",
            serde_json::json!({
                "protocolVersion": 1,
                "clientInfo": { "name": "scripted-editor", "version": "0.0.1" }
            }),
        );
        assert!(init["error"].is_null(), "{init}");
        assert_eq!(init["result"]["protocolVersion"], 1);
        assert_eq!(init["result"]["agentInfo"]["name"], "okra");
        assert_eq!(init["result"]["agentCapabilities"]["loadSession"], false);

        // 1b. version negotiation: an unsupported request answers with OURS
        let future = client.request("initialize", serde_json::json!({ "protocolVersion": 99 }));
        assert_eq!(future["result"]["protocolVersion"], 1, "{future}");

        // 2. session/new
        let new = client.request("session/new", serde_json::json!({ "cwd": workspace }));
        assert!(new["error"].is_null(), "{new}");
        let session_id = new["result"]["sessionId"].as_str().expect("sessionId").to_string();
        assert!(session_id.starts_with("acp-"));

        // 3. session/prompt streams updates, then replies with a stop reason
        let prompt = client.request("session/prompt", serde_json::json!({
            "sessionId": session_id,
            "prompt": [ { "type": "text", "text": "summarize notes.md" } ]
        }));
        assert!(prompt["error"].is_null(), "{prompt}");
        assert_eq!(prompt["result"]["stopReason"], "end_turn", "{prompt}");

        let updates = drain_updates(&session_id);
        assert!(!updates.is_empty(), "no session/update notifications streamed");
        for u in &updates {
            assert_eq!(u["sessionId"], session_id, "every update carries the sessionId: {u}");
        }

        // assistant text streamed as agent_message_chunk with a text block
        let chunks: Vec<&serde_json::Value> = updates.iter().filter(|u| {
            u["update"]["sessionUpdate"] == "agent_message_chunk"
        }).collect();
        assert!(!chunks.is_empty(), "no agent_message_chunk in {updates:?}");
        let text: String = chunks
            .iter()
            .map(|u| u["update"]["content"]["text"].as_str().unwrap_or_default())
            .collect();
        assert!(!text.is_empty());

        // the read_file tool call: in_progress → completed, with content
        let started: Vec<&serde_json::Value> = updates.iter().filter(|u| {
            u["update"]["sessionUpdate"] == "tool_call"
                && u["update"]["title"] == "read_file"
        }).collect();
        assert_eq!(started.len(), 1, "one read_file tool_call expected: {updates:?}");
        assert_eq!(started[0]["update"]["status"], "in_progress");
        assert_eq!(started[0]["update"]["kind"], "read");
        let call_id = started[0]["update"]["toolCallId"].as_str().unwrap().to_string();
        let finished: Vec<&serde_json::Value> = updates.iter().filter(|u| {
            u["update"]["sessionUpdate"] == "tool_call_update"
                && u["update"]["toolCallId"] == call_id
        }).collect();
        assert_eq!(finished.len(), 1);
        assert_eq!(finished[0]["update"]["status"], "completed");
        assert!(!finished[0]["update"]["content"].as_array().unwrap().is_empty());
        let tool_text = finished[0]["update"]["content"][0]["content"]["text"]
            .as_str()
            .unwrap_or_default();
        assert!(tool_text.contains("acp notes"), "tool output carries the file content: {tool_text}");
        assert!(text.contains("acp notes"), "the assistant answers from the read: {text}");

        // 4. error conventions: unknown session, empty prompt, unknown method
        let unknown = client.request("session/prompt", serde_json::json!({
            "sessionId": "acp-does-not-exist",
            "prompt": [ { "type": "text", "text": "hi" } ]
        }));
        assert_eq!(unknown["error"]["code"], -32002, "{unknown}");
        let empty = client.request("session/prompt", serde_json::json!({
            "sessionId": session_id,
            "prompt": [ { "type": "image", "uri": "x", "mimeType": "image/png" } ]
        }));
        assert_eq!(empty["error"]["code"], -32602, "{empty}");
        let unknown_method = client.request("session/yoga", serde_json::json!({}));
        assert_eq!(unknown_method["error"]["code"], -32601, "{unknown_method}");

        // 5. session/cancel is accepted as a notification (no reply, no error)
        client.send(serde_json::json!({
            "jsonrpc": "2.0", "method": "session/cancel", "params": { "sessionId": session_id }
        }));
    }));
    kill(agent);
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

/// session/cancel must actually abort a RUNNING turn: the prompt runs on a
/// worker thread (the reader keeps consuming stdin), the cancel flips the
/// session's stop flag, and the prompt reply reports the honest stop reason.
/// A follow-up prompt on the same session proves the standard repair path.
#[test]
#[allow(clippy::disallowed_methods)] // test harness: executes the compiled binary
fn g4_acp_cancel_aborts_running_turn_and_session_recovers() {
    let td = tempfile::tempdir().unwrap();
    let workspace = td.path().join("ws");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::write(workspace.join("notes.md"), "# cancel probe\n").unwrap();
    // stretch every planner step so the turn is reliably mid-flight
    let mut agent = spawn_agent_env(td.path(), &[("OKRA_DEMO_DELAY_MS", "1500")]);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut client = AcpClient {
            stdin: agent.stdin.take().unwrap(),
            reader: BufReader::new(agent.stdout.take().unwrap()),
            next_id: 1,
        };
        client.request(
            "initialize",
            serde_json::json!({ "protocolVersion": 1, "clientInfo": { "name": "c", "version": "0" } }),
        );
        let new = client.request("session/new", serde_json::json!({ "cwd": workspace }));
        let session_id = new["result"]["sessionId"].as_str().unwrap().to_string();

        // prompt 1 goes out but we do NOT wait for its reply
        client.send(serde_json::json!({
            "jsonrpc": "2.0", "id": 10, "method": "session/prompt",
            "params": { "sessionId": session_id, "prompt": [ { "type": "text", "text": "long task" } ] }
        }));
        std::thread::sleep(Duration::from_millis(500)); // turn is mid-sample now

        // a second prompt on the same session while one runs: refused honestly
        client.send(serde_json::json!({
            "jsonrpc": "2.0", "id": 11, "method": "session/prompt",
            "params": { "sessionId": session_id, "prompt": [ { "type": "text", "text": "interleave" } ] }
        }));
        let busy = client.wait_for_reply(11);
        assert_eq!(busy["error"]["code"], -32000, "{busy}");

        // the cancel lands while prompt 1's turn is still running
        client.send(serde_json::json!({
            "jsonrpc": "2.0", "method": "session/cancel", "params": { "sessionId": session_id }
        }));
        let cancelled = client.wait_for_reply(10);
        assert!(cancelled["error"].is_null(), "{cancelled}");
        assert_eq!(cancelled["result"]["stopReason"], "cancelled", "{cancelled}");

        // the same session takes a new turn (standard repair path)
        let after = client.request("session/prompt", serde_json::json!({
            "sessionId": session_id,
            "prompt": [ { "type": "text", "text": "summarize notes.md" } ]
        }));
        assert_eq!(after["result"]["stopReason"], "end_turn", "{after}");
    }));
    kill(agent);
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}

/// The pin max-turns ceiling reaches the ACP surface too: under a
/// maxTurnsCeiling-1 pin the editor turn is clamped (daemon stderr names
/// it) and still ends with an honest stop reason.
#[test]
#[allow(clippy::disallowed_methods)] // test harness: runs the compiled binary
fn g4_acp_turn_budget_honors_pin_ceiling() {
    use std::process::Command as Cmd;
    let td = tempfile::tempdir().unwrap();
    let workspace = td.path().join("ws");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::write(workspace.join("notes.md"), "# probe\n").unwrap();
    let pin = td.path().join("pin.json");
    std::fs::write(&pin, r#"{ "version": 1, "source": "corp-it", "maxTurnsCeiling": 1 }"#).unwrap();

    let mut cmd = Cmd::new(env!("CARGO_BIN_EXE_okra"));
    cmd.args(["serve", "--acp", "--cwd", &workspace.to_string_lossy()])
        .env("OKRA_MANAGED_PIN", &pin)
        .env_remove("OKRA_TRUST_FILE")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut agent = cmd.spawn().expect("spawn okra serve --acp");
    let stderr = std::io::BufReader::new(agent.stderr.take().unwrap());
    let stderr_lines: std::sync::Arc<std::sync::Mutex<Vec<String>>> = std::sync::Arc::default();
    {
        let sink = std::sync::Arc::clone(&stderr_lines);
        std::thread::spawn(move || {
            for line in stderr.lines().map_while(Result::ok) {
                sink.lock().unwrap().push(line);
            }
        });
    }
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut client = AcpClient {
            stdin: agent.stdin.take().unwrap(),
            reader: BufReader::new(agent.stdout.take().unwrap()),
            next_id: 1,
        };
        client.request(
            "initialize",
            serde_json::json!({ "protocolVersion": 1, "clientInfo": { "name": "c", "version": "0" } }),
        );
        let new = client.request("session/new", serde_json::json!({}));
        let session_id = new["result"]["sessionId"].as_str().unwrap().to_string();
        let prompt = client.request("session/prompt", serde_json::json!({
            "sessionId": session_id,
            "prompt": [ { "type": "text", "text": "read notes.md" } ]
        }));
        // the clamped turn still returns an HONEST stop reason
        assert!(prompt["error"].is_null(), "{prompt}");
        assert!(
            prompt["result"]["stopReason"].is_string(),
            "a stop reason is always reported: {prompt}"
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if stderr_lines.lock().unwrap().iter().any(|l| l.contains("[pin] max-turns clamped to 1")) {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(
            stderr_lines.lock().unwrap().iter().any(|l| l.contains("[pin] max-turns clamped to 1")),
            "daemon stderr must name the clamp: {:?}",
            stderr_lines.lock().unwrap()
        );
    }));
    kill(agent);
    if let Err(panic) = result {
        std::panic::resume_unwind(panic);
    }
}
