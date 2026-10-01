//! `okra workflow run` end-to-end: a two-step Rhai script where each step
//! is a REAL child okra turn (demo planner — offline, deterministic).
//! Proves the engine + step host + journal integration on the actual
//! binary, not just the crate.

#![allow(clippy::disallowed_methods)] // acceptance harness drives the real binary
use std::process::Command;

#[test]
fn workflow_run_executes_steps_as_real_turns_and_journals_them() {
    let td = tempfile::tempdir().unwrap();
    let ws = td.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    // the demo planner answers from a file it reads — give it one
    std::fs::write(ws.join("notes.txt"), "workflow step fixture content").unwrap();

    let script = r#"
        fn run() {
            let a = step("read", "read notes.txt and summarize it");
            let b = step("check", "read notes.txt again and confirm");
            a.len() > 0 && b.len() > 0
        }
    "#;
    let script_path = ws.join("two-step.rhai");
    std::fs::write(&script_path, script).unwrap();

    let bin = env!("CARGO_BIN_EXE_okra");
    let out = Command::new(bin)
        .current_dir(&ws)
        .args(["workflow", "run", "two-step.rhai"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "workflow exited {:?}\nstdout: {stdout}\nstderr: {stderr}",
        out.status.code()
    );
    assert!(stdout.contains("WORKFLOW "), "summary line present: {stdout}");
    assert!(stdout.contains("\"status\":\"completed\""), "run completed: {stdout}");

    // the journal carries both steps durably
    let journal_dir = ws.join(".okra").join("workflows");
    let entries: Vec<std::path::PathBuf> = std::fs::read_dir(&journal_dir)
        .expect("journal dir exists")
        .flatten()
        .map(|e| e.path())
        .collect();
    assert_eq!(entries.len(), 1, "one run file: {entries:?}");
    let raw = std::fs::read_to_string(&entries[0]).unwrap();
    assert!(raw.contains("step/started"));
    assert!(raw.contains("\"name\":\"read\""));
    assert!(raw.contains("\"name\":\"check\""));
    assert!(raw.contains("run/completed"));
}

#[test]
fn workflow_run_cancels_on_step_budget() {
    let td = tempfile::tempdir().unwrap();
    let ws = td.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::write(ws.join("notes.txt"), "fixture").unwrap();
    let script = r#"
        fn run() {
            step("one", "read notes.txt");
            step("two", "read notes.txt");
            step("three", "read notes.txt");
        }
    "#;
    std::fs::write(ws.join("over-budget.rhai"), script).unwrap();

    let bin = env!("CARGO_BIN_EXE_okra");
    let out = Command::new(bin)
        .current_dir(&ws)
        .args(["workflow", "run", "over-budget.rhai", "--max-steps", "2"])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code(), Some(130), "cancelled exit code: {stdout}");
    assert!(stdout.contains("\"status\":\"cancelled\""), "{stdout}");
    assert!(stdout.contains("steps"), "breach names the budget: {stdout}");
}
