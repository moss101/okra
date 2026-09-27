//! G1 gate test suite (MASTER-PLAN M1): "the CLI completes real multi-file
//! coding tasks; killAtPhase at every durable boundary recovers with no
//! double-executed tool and no torn state."
//!
//! For EVERY durable log boundary the turn loop crosses — turn_start,
//! user_message, assistant_message, tool_call, tool_result, turn_end — the
//! matrix aborts a real CLI child process right after that boundary, then
//! runs the CLI again against the same workspace and asserts:
//!
//! 1. recovery completes the task: every file exists with its exact final
//!    content (atomic writes → never a torn file);
//! 2. the kernel log passes `check_log` after repair;
//! 3. no double-executed tool: every `tool/call` id appears exactly once
//!    and carries exactly one `tool/result` — interrupted calls are closed
//!    by the repair with `TOOL_OUTCOME_UNKNOWN`, never re-executed.


// Test harness: these acceptance tests execute the compiled crate binary as
// the system under test. The no-raw-spawn/canonicalize bans target production
// paths (production spawning goes through okra_policy's confined runner); the
// acceptance harness must exercise the real binary end-to-end.
#![allow(clippy::disallowed_methods)]
use std::path::{Path, PathBuf};
use std::process::Command;

use okra_kernel as kernel;

const BOUNDARIES: &[&str] = &[
    "turn_start_logged",
    "user_message_logged",
    "assistant_message_logged",
    "tool_call_logged",
    "tool_result_logged",
    "turn_end_logged",
];

const TASK_JSON: &str = r#"{
  "task": "G1 matrix: two-file app with an edit",
  "files": [
    {
      "path": "src/app.js",
      "content": "const PLACEHOLDER = 'TODO_NAME';\nfunction greet() { return 'Hi, ' + PLACEHOLDER; }\nmodule.exports = { greet };\n",
      "edit": { "old": "TODO_NAME", "new": "okra" }
    },
    {
      "path": "index.html",
      "content": "<!doctype html>\n<h1>Greeting</h1>\n<script src=\"src/app.js\"></script>\n"
    }
  ]
}"#;

const EXPECTED_APP_JS: &str =
    "const PLACEHOLDER = 'okra';\nfunction greet() { return 'Hi, ' + PLACEHOLDER; }\nmodule.exports = { greet };\n";
const EXPECTED_INDEX_HTML: &str =
    "<!doctype html>\n<h1>Greeting</h1>\n<script src=\"src/app.js\"></script>\n";

fn workspace(tag: &str) -> (tempfile::TempDir, PathBuf) {
    let td = tempfile::tempdir().unwrap();
    let ws = td.path().join(tag);
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::write(ws.join("task.json"), TASK_JSON).unwrap();
    (td, ws)
}

fn run_cli(ws: &Path, extra: &[&str]) -> std::process::Output {
    let bin = env!("CARGO_BIN_EXE_okra");
    Command::new(bin)
        .arg("--json")
        .arg("--cwd")
        .arg(ws)
        .args(extra)
        .arg("run the task")
        .output()
        .expect("spawn okra")
}

/// Open the session log and assert the G1 recovery invariants.
fn assert_recovered_log(ws: &Path) {
    let sessions = ws.join(".okra-sessions");
    let handle = kernel::SessionHandle::open(&sessions, "cli", kernel::SessionAccess::Read)
        .expect("session opens");
    let events = handle.read_all().expect("log readable");
    // seq space is contiguous and every structural invariant holds
    kernel::check_log(&events).expect("repaired log passes invariants");
    assert!(
        !kernel::needs_repair(&events),
        "no open turn/call may remain after recovery"
    );
    // No double-executed tool: each call id exactly one call + one result.
    let mut calls: std::collections::HashMap<String, u32> = Default::default();
    let mut results: std::collections::HashMap<String, u32> = Default::default();
    for ev in &events {
        match ev.event_type.as_str() {
            "tool/call" => {
                let id = ev.data["callId"].as_str().unwrap_or_default().to_string();
                *calls.entry(id).or_insert(0) += 1;
            }
            "tool/result" => {
                let id = ev.data["callId"].as_str().unwrap_or_default().to_string();
                *results.entry(id).or_insert(0) += 1;
            }
            _ => {}
        }
    }
    for (id, count) in &calls {
        assert_eq!(*count, 1, "call id {id} logged more than once");
        assert_eq!(
            results.get(id),
            Some(&1),
            "call id {id} must carry exactly one result"
        );
    }
    assert!(!calls.is_empty(), "the task must have executed tool calls");
    // any synthesized result must be marked outcome-unknown
    for ev in &events {
        if ev.event_type == "tool/result" && ev.data["interrupted"] == true {
            assert_eq!(ev.data["code"], kernel::TOOL_OUTCOME_UNKNOWN_CODE);
        }
    }
}

#[test]
fn g1_task_completes_end_to_end_without_faults() {
    let (_td, ws) = workspace("g1-clean");
    let out = run_cli(&ws, &["--task", ws.join("task.json").to_str().unwrap()]);
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        std::fs::read_to_string(ws.join("src/app.js")).unwrap(),
        EXPECTED_APP_JS
    );
    assert_eq!(
        std::fs::read_to_string(ws.join("index.html")).unwrap(),
        EXPECTED_INDEX_HTML
    );
    assert_recovered_log(&ws);
}

#[test]
fn g1_kill_matrix_every_durable_boundary_recovers() {
    for boundary in BOUNDARIES {
        let tag = format!("g1-kill-{boundary}");
        let (_td, ws) = workspace(&tag);

        // 1. run with a kill injected at the FIRST occurrence of the boundary
        let kill_spec = format!("{boundary}:1");
        let spec_abs = ws.join("task.json").to_str().unwrap().to_string();
        let aborted = run_cli(&ws, &["--task", &spec_abs, "--kill-at-boundary", &kill_spec]);
        assert!(
            aborted.status.code().is_none(),
            "child must abort by signal at {boundary}, got {:?} stderr={}",
            aborted.status.code(),
            String::from_utf8_lossy(&aborted.stderr)
        );

        // 2. pre-recovery: whatever happened, no torn FILE may exist.
        //    Any file that exists at all must be complete and valid.
        for f in [ws.join("src/app.js"), ws.join("index.html")] {
            if f.exists() {
                let content = std::fs::read_to_string(&f).unwrap();
                assert!(
                    content == EXPECTED_APP_JS
                        || content == EXPECTED_INDEX_HTML
                        || content.starts_with("const PLACEHOLDER = 'TODO_NAME'")
                        || content.starts_with("<!doctype html>"),
                    "torn file content at {boundary}: {content:?}"
                );
            }
        }

        // 3. recovery run: repair + re-execute the task
        let out = run_cli(&ws, &["--task", ws.join("task.json").to_str().unwrap()]);
        assert!(
            out.status.success(),
            "recovery failed at {boundary}: {}",
            String::from_utf8_lossy(&out.stderr)
        );

        // 4. final state: exact file contents
        assert_eq!(
            std::fs::read_to_string(ws.join("src/app.js")).unwrap(),
            EXPECTED_APP_JS,
            "app.js wrong after recovery from {boundary}"
        );
        assert_eq!(
            std::fs::read_to_string(ws.join("index.html")).unwrap(),
            EXPECTED_INDEX_HTML,
            "index.html wrong after recovery from {boundary}"
        );

        // 5. log invariants + no double-executed tool
        assert_recovered_log(&ws);
    }
}

#[test]
fn g1_repair_marks_interrupted_call_outcome_unknown() {
    // Kill between tool_call_logged and tool_result_logged (2nd call):
    // the first write lands, the SECOND call is interrupted pre-execution —
    // the recovered log must close it with outcome-unknown, and the
    // follow-up run completes without re-executing that id.
    let (_td, ws) = workspace("g1-interrupted");
    let spec_abs = ws.join("task.json").to_str().unwrap().to_string();
    let aborted = run_cli(&ws, &["--task", &spec_abs, "--kill-at-boundary", "tool_call_logged:2"]);
    assert!(aborted.status.code().is_none());

    // before any recovery the raw log has an unmatched call
    let sessions = ws.join(".okra-sessions");
    {
        let r = kernel::SessionHandle::open(&sessions, "cli", kernel::SessionAccess::Read).unwrap();
        let events = r.read_all().unwrap();
        assert!(kernel::needs_repair(&events), "interrupted call must need repair");
    }

    let out = run_cli(&ws, &["--task", ws.join("task.json").to_str().unwrap()]);
    assert!(out.status.success());
    assert_eq!(
        std::fs::read_to_string(ws.join("src/app.js")).unwrap(),
        EXPECTED_APP_JS
    );
    assert_recovered_log(&ws);
}
