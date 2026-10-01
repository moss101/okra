//! #56 headless flags: the `--json-schema` contract must cover EVERY
//! LoopEvent variant the stream can emit (drift guard), and `--tools`
//! filters the registry for the run (reported, fail-closed on empty).

// Test harness: drives the compiled binary end-to-end.
#![allow(clippy::disallowed_methods)]
use std::process::Command;

use okra_agent_core::loop_::LoopEvent;

#[test]
fn json_schema_covers_every_loop_event_variant() {
    let out = Command::new(env!("CARGO_BIN_EXE_okra"))
        .arg("--json-schema")
        .output()
        .unwrap();
    assert!(out.status.success());
    let schema: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("schema is valid JSON");

    // every REAL variant, serialized the way --json emits it
    let samples: Vec<LoopEvent> = vec![
        LoopEvent::TurnStarted { turn: 1 },
        LoopEvent::Phase { phase: "Idle".into() },
        LoopEvent::TextDelta { text: "x".into() },
        LoopEvent::ToolCallStarted { id: "c".into(), name: "read_file".into(), args_json: "{}".into() },
        LoopEvent::ToolCallProgress { id: "c".into(), text: "...".into() },
        LoopEvent::ToolCallFinished { id: "c".into(), name: "read_file".into(), is_error: false, output: "ok".into() },
        LoopEvent::SteeringInjected { text: "steer".into() },
        LoopEvent::Nudge { reason: "stationary".into() },
        LoopEvent::CompactionNotice { note: "compacted".into() },
        LoopEvent::TurnFinished { outcome: "{}".into() },
        LoopEvent::Error { message: "boom".into() },
    ];
    let listed: Vec<String> = schema["oneOf"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v["properties"]["event"]["const"].as_str().unwrap_or_default().to_string())
        .collect();
    for sample in &samples {
        let wire = serde_json::to_value(sample).unwrap();
        let tag = wire["event"].as_str().unwrap_or_default().to_string();
        assert!(
            listed.contains(&tag),
            "schema is missing the `{tag}` event — update ndjson_event_schema()"
        );
    }
}

#[test]
fn tools_flag_filters_the_registry_and_refuses_empty_matches() {
    let td = tempfile::tempdir().unwrap();
    std::fs::write(td.path().join("notes.txt"), "fixture").unwrap();

    // allowlist read-only tools: the run works and reports the drops
    let out = Command::new(env!("CARGO_BIN_EXE_okra"))
        .args([
            "--cwd", &td.path().to_string_lossy(),
            "--tools", "read_*,list_dir",
            "--json",
            "read notes.txt and summarize it",
        ])
        .output()
        .unwrap();
    assert!(out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("write_file") && stderr.contains("edit_file") && stderr.contains("filtered out"),
        "drops reported: {stderr}"
    );

    // a pattern matching nothing is refused honestly
    let out = Command::new(env!("CARGO_BIN_EXE_okra"))
        .args([
            "--cwd", &td.path().to_string_lossy(),
            "--tools", "nothing_*",
            "hi",
        ])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("matched no tools"));
}

#[test]
fn worktree_flag_runs_the_task_in_a_real_created_worktree() {
    let td = tempfile::tempdir().unwrap();
    let ws = td.path().join("repo");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::write(ws.join("notes.txt"), "worktree fixture").unwrap();
    let git = |args: &[&str]| {
        let out = std::process::Command::new("git")
            .args(["-C", &ws.to_string_lossy()])
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).into_owned()
    };
    git(&["init", "-q"]);
    git(&["add", "."]);
    git(&["-c", "user.email=t@t", "-c", "user.name=t", "commit", "-q", "-m", "init"]);

    // missing path → CREATED as a real worktree, task runs inside it
    let wt = td.path().join("task-wt");
    let out = Command::new(env!("CARGO_BIN_EXE_okra"))
        .args([
            "--cwd", &ws.to_string_lossy(),
            "--worktree", &wt.to_string_lossy(),
            "--json",
            "read notes.txt and summarize it",
        ])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(wt.join(".git").exists(), "a REAL worktree materialized");
    assert!(wt.join("notes.txt").exists(), "the checkout carries the repo files");
    let branches = git(&["branch", "--list", "worktree-*"]);
    assert!(branches.contains("worktree-"), "the worktree branch exists: {branches}");

    // the demo planner read THE WORKTREE's copy (session log lives there too)
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("read_file"), "the turn ran tools: {stdout}");
}
