//! G5 depth (MASTER-PLAN §4): the confined subagent child is driven by a
//! REAL OpenAI-compatible sampler making real HTTP calls to a local model
//! endpoint — not the offline TaskPlanner. The mock server verifies the
//! Authorization header flowed, serves a scripted tool-call turn then an
//! end turn, and the orchestrator harvests token usage into the verdict.


// Test harness: these acceptance tests execute the compiled crate binary as
// the system under test. The no-raw-spawn/canonicalize bans target production
// paths (production spawning goes through okra_policy's confined runner); the
// acceptance harness must exercise the real binary end-to-end.
#![allow(clippy::disallowed_methods)]
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::Duration;

const TASK_JSON: &str = r##"{
  "task": "G5 real-sampler: create deliverable.md via the write_file tool",
  "files": []
}"##;

fn git_available() -> bool {
    std::process::Command::new("git")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Minimal OpenAI-compatible endpoint: POST /v1/chat/completions.
/// Step 1 → tool_call write_file; step 2 → content + stop. Rejects
/// requests without the Authorization header (proves auth flows).
fn spawn_mock_model(authorization_expected: &str) -> (std::thread::JoinHandle<()>, String) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let step = Arc::new(Mutex::new(0u32));
    let auth_expected: &'static str = Box::leak(authorization_expected.to_string().into_boxed_str());
    let handle = std::thread::spawn(move || {
        for stream in listener.incoming() {
            let mut stream = match stream {
                Ok(s) => s,
                Err(_) => continue,
            };
            stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut request_line = String::new();
            if reader.read_line(&mut request_line).unwrap_or(0) == 0 {
                continue;
            }
            let mut content_length = 0usize;
            let mut authorization = String::new();
            loop {
                let mut header = String::new();
                if reader.read_line(&mut header).unwrap_or(0) == 0 {
                    break;
                }
                let trimmed = header.trim();
                if trimmed.is_empty() {
                    break;
                }
                let lower = trimmed.to_ascii_lowercase();
                if let Some(v) = lower.strip_prefix("content-length:") {
                    content_length = v.trim().parse().unwrap_or(0);
                }
                if lower.starts_with("authorization:") {
                    authorization = trimmed.to_string();
                }
            }
            let mut body = vec![0u8; content_length];
            if content_length > 0 {
                reader.read_exact(&mut body).unwrap();
            }
            let _ = &request_line;

            if !authorization.contains(auth_expected) {
                let body = b"{\"error\":\"unauthorized\"}";
                let head = format!("HTTP/1.1 401 Unauthorized\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
                stream.write_all(head.as_bytes()).unwrap();
                stream.write_all(body).unwrap();
                continue;
            }

            let n = *step.lock().unwrap();
            *step.lock().unwrap() += 1;
            let payload = if n == 0 {
                serde_json::json!({
                    "choices": [{
                        "message": {
                            "role": "assistant",
                            "content": null,
                            "tool_calls": [{
                                "id": "call-real-1",
                                "type": "function",
                                "function": {
                                    "name": "write_file",
                                    "arguments": "{\"path\": \"deliverable.md\", \"content\": \"# written by the real sampler\\n\"}"
                                }
                            }]
                        },
                        "finish_reason": "tool_calls"
                    }],
                    "usage": {"prompt_tokens": 21, "completion_tokens": 13}
                })
            } else {
                serde_json::json!({
                    "choices": [{
                        "message": {"role": "assistant", "content": "deliverable written"},
                        "finish_reason": "stop"
                    }],
                    "usage": {"prompt_tokens": 48, "completion_tokens": 6}
                })
            };
            let body = serde_json::to_vec(&payload).unwrap();
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            stream.write_all(head.as_bytes()).unwrap();
            stream.write_all(&body).unwrap();
        }
    });
    (handle, addr)
}

fn model_url(mock_addr: &str) -> String {
    format!("http://{mock_addr}/v1")
}

#[test]
fn g5_real_sampler_drives_confined_child() {
    if !git_available() {
        return;
    }
    let td = tempfile::tempdir().unwrap();

    // parent repository with a sentinel + initial commit
    let repo_path = td.path().join("main");
    std::fs::create_dir_all(&repo_path).unwrap();
    std::fs::write(repo_path.join("sentinel.txt"), b"parent sentinel").unwrap();
    std::fs::write(repo_path.join("task.json"), TASK_JSON).unwrap();
    let repo = okra_host::git::GitRepository::init(&repo_path).unwrap();
    let config = repo.root().join(".git").join("config");
    let mut cfg = std::fs::read_to_string(&config).unwrap_or_default();
    if !cfg.contains("user.name") {
        cfg.push_str("\n[user]\n\tname = okra-test\n\temail = okra-test@example.com\n");
        std::fs::write(&config, cfg).unwrap();
    }
    repo.commit_all("initial").unwrap();

    let worktree = td.path().join("grant");

    // mock model endpoint: auth header enforced, two scripted turns
    let (_mock_thread_handle, mock_addr) = spawn_mock_model("Bearer test-model-key");

    let bin = env!("CARGO_BIN_EXE_okra");
    let out = Command::new(bin)
        .args([
            "subagent-launch",
            "--repo",
            &repo_path.to_string_lossy(),
            "--name",
            "real-sampler",
            "--worktree",
            &worktree.to_string_lossy(),
            "--task-spec",
            &repo_path.join("task.json").to_string_lossy(),
            "--task",
            "create the deliverable",
        ])
        .env("OKRA_SUBAGENT_BASE_URL", model_url(&mock_addr))
        .env("OKRA_SUBAGENT_API_KEY", "test-model-key")
        .env("OKRA_SUBAGENT_MODEL", "okra-test-model")
        .output()
        .expect("run subagent-launch");
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();

    let verdict_line = stdout
        .lines()
        .find(|l| l.starts_with("ORCHESTRATION "))
        .unwrap_or_else(|| panic!("no verdict: {stdout}\n{stderr}"));
    let verdict: serde_json::Value =
        serde_json::from_str(verdict_line.trim_start_matches("ORCHESTRATION "))
            .unwrap_or_else(|e| panic!("parse: {e}\n{stdout}\n{stderr}"));

    // the child reports it ran on the REAL sampler
    assert_eq!(
        verdict["child"]["sampler"], "openai",
        "the real OpenAI-compatible sampler drove the turn: {verdict}"
    );
    eprintln!("VERDICT: {verdict}");
    assert_eq!(verdict["child"]["passed"], serde_json::Value::Bool(true), "{verdict}");
    assert_eq!(
        verdict["child"]["kernel_write_outside_grant"], "denied",
        "kernel isolation still holds with a real sampler"
    );

    // token usage was harvested into the orchestration verdict
    let usage = &verdict["usage"];
    assert_eq!(usage["totals"]["inputTokens"], 21 + 48);
    assert_eq!(usage["totals"]["outputTokens"], 13 + 6);
    assert_eq!(usage["totals"]["turns"], 2);

    // the deliverable written via the sampler's tool call landed on the
    // branch commit
    let commit = verdict["commit"].as_str().expect("branch commit");
    let show = Command::new("git")
        .args([
            "-C",
            &repo_path.to_string_lossy(),
            "show",
            "real-sampler:deliverable.md",
        ])
        .output()
        .unwrap();
    assert!(
        String::from_utf8_lossy(&show.stdout).contains("written by the real sampler"),
        "deliverable on branch: {}",
        String::from_utf8_lossy(&show.stdout)
    );
    assert_ne!(commit.len(), 0);

    // parent untouched
    assert_eq!(verdict["parent_clean"], serde_json::Value::Bool(true));
}
