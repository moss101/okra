//! N0037 — the standalone computer MCP server: JSON-RPC handshake over
//! stdio, tools/list, and the fail-closed consent model. The macOS
//! backend binaries are env-overridable (`OKRA_OSASCRIPT` etc.), so the
//! granted-path call runs against a script fixture — hermetic.

// Test harness: drives the compiled binary end-to-end.
#![allow(clippy::disallowed_methods)]
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};

struct Server {
    child: Child,
    reader: BufReader<std::process::ChildStdout>,
}

impl Server {
    fn spawn(extra_args: &[&str]) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_okra"))
            .args(["mcp-serve", "--computer"])
            .args(extra_args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .expect("spawn mcp-serve");
        // drain stderr in the background so it never blocks the child
        if let Some(stderr) = child.stderr.take() {
            std::thread::spawn(move || {
                for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                    eprintln!("MCPSERVER: {line}");
                }
            });
        }
        let reader = BufReader::new(child.stdout.take().expect("stdout piped"));
        Server { child, reader }
    }

    fn send(&mut self, frame: &serde_json::Value) {
        let stdin = self.child.stdin.as_mut().expect("stdin open");
        writeln!(stdin, "{}", serde_json::to_string(frame).unwrap()).unwrap();
        stdin.flush().unwrap();
    }

    fn read_result(&mut self) -> serde_json::Value {
        loop {
            let mut line = String::new();
            match self.reader.read_line(&mut line) {
                Ok(0) => panic!("server closed stdout"),
                Ok(_) => {}
                Err(e) => panic!("read: {e}"),
            }
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(&line)
                && (v.get("result").is_some() || v.get("error").is_some())
            {
                return v;
            }
        }
    }

    fn rpc(&mut self, id: i64, method: &str, params: serde_json::Value) -> serde_json::Value {
        self.send(&serde_json::json!({
            "jsonrpc": "2.0", "id": id, "method": method, "params": params
        }));
        self.read_result()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn handshake_list_and_fail_closed_consent() {
    let mut server = Server::spawn(&[]);

    // initialize → serverInfo + tools capability
    let init = server.rpc(1, "initialize", serde_json::json!({}));
    assert_eq!(init["result"]["serverInfo"]["name"], "okra-computer");
    assert!(init["result"]["capabilities"]["tools"].is_object());

    server.send(&serde_json::json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }));

    // tools/list → the computer family
    let list = server.rpc(2, "tools/list", serde_json::json!({}));
    let names: Vec<&str> = list["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap_or_default())
        .collect();
    for expected in ["computer_observe", "computer_act", "computer_screenshot", "computer_list_apps"] {
        assert!(names.contains(&expected), "tool {expected} listed: {names:?}");
    }

    // observe without consent → isError with the flag named
    let denied = server.rpc(
        3,
        "tools/call",
        serde_json::json!({ "name": "computer_observe", "arguments": { "app": "Finder" } }),
    );
    assert_eq!(denied["result"]["isError"], true);
    let text = denied["result"]["content"][0]["text"].as_str().unwrap_or_default();
    assert!(text.contains("--allow-app Finder"), "the refusal names the fix: {text}");

    // screenshot without full control → fail-closed
    let shot = server.rpc(4, "tools/call", serde_json::json!({ "name": "computer_screenshot", "arguments": {} }));
    assert_eq!(shot["result"]["isError"], true);
    assert!(
        shot["result"]["content"][0]["text"].as_str().unwrap_or_default().contains("--allow-full-control"),
        "names the missing flag"
    );

    // unknown tool → JSON-RPC error
    let unknown = server.rpc(5, "tools/call", serde_json::json!({ "name": "nope", "arguments": {} }));
    assert!(unknown.get("error").is_some());
}

// unix semantics: a #!/bin/sh osascript stand-in with the exec bit
#[test]
#[cfg(unix)]
fn granted_app_observable_through_the_hermetic_backend() {
    // the AX backend shells out to env-overridable binaries: a fixture
    // script stands in for osascript
    let td = tempfile::tempdir().unwrap();
    let fixture = td.path().join("osascript-fixture.sh");
    std::fs::write(
        &fixture,
        "#!/bin/sh\n# hermetic AX tree: one button element\necho '{\"role\":\"window\",\"children\":[{\"role\":\"button\",\"id\":\"btn1\",\"label\":\"OK\"}]}'\n",
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&fixture, std::fs::Permissions::from_mode(0o755)).unwrap();

    let mut child = Command::new(env!("CARGO_BIN_EXE_okra"))
        .args(["mcp-serve", "--computer", "--allow-app", "FixtureApp"])
        .env("OKRA_OSASCRIPT", &fixture)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("spawn");
    // stderr drain like Server::spawn
    if let Some(stderr) = child.stderr.take() {
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                eprintln!("MCPSERVER: {line}");
            }
        });
    }
    let reader = BufReader::new(child.stdout.take().expect("stdout piped"));
    let mut server = Server { child, reader };

    let init = server.rpc(1, "initialize", serde_json::json!({}));
    assert!(init.get("result").is_some());

    // granted app: observe runs the fixture (error or tree — depends on
    // the fixture's parse path — but NOT a consent refusal)
    let allowed = server.rpc(
        2,
        "tools/call",
        serde_json::json!({ "name": "computer_observe", "arguments": { "app": "FixtureApp" } }),
    );
    let text = allowed["result"]["content"][0]["text"].as_str().unwrap_or_default();
    assert!(
        !text.contains("consent missing"),
        "the granted app is past the consent gate: {text}"
    );

    // another app still refused
    let other = server.rpc(
        3,
        "tools/call",
        serde_json::json!({ "name": "computer_observe", "arguments": { "app": "OtherApp" } }),
    );
    assert_eq!(other["result"]["isError"], true);
    assert!(other["result"]["content"][0]["text"].as_str().unwrap_or_default().contains("--allow-app OtherApp"));
}
