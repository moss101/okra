//! G5 gate (MASTER-PLAN §4): "a subagent run in an isolated worktree
//! cannot touch paths outside its grant — enforced by sandbox, not policy."
//!
//! A main workspace holds a sentinel file. A grant dir (the isolated
//! worktree) is populated for the subagent. The subagent child process
//! applies nono self-confinement with the grant as the only writable
//! surface (plus its session log), runs a real coding task, and reports
//! kernel verdicts. The parent asserts:
//! - the task completed inside the grant (deliverables exact);
//! - a kernel probe write outside the grant was DENIED (EPERM);
//! - the main workspace is untouched — including by the escape attempt
//!   the task itself makes (policy layer refuses it first; the kernel is
//!   the backstop for anything that slips past policy).

use std::process::Command;

const TASK_JSON: &str = r##"{
  "task": "G5: write the deliverable and attempt an escape write",
  "files": [
    { "path": "deliverable.md", "content": "# deliverable\nproduced inside the grant\n" },
    { "path": "../escape-attempt.txt", "content": "this must never land anywhere" }
  ]
}"##;

#[test]
fn g5_subagent_worktree_isolation_enforced_by_kernel() {
    let td = tempfile::tempdir().unwrap();
    let main_ws = td.path().join("main");
    std::fs::create_dir_all(&main_ws).unwrap();
    std::fs::write(main_ws.join("sentinel.txt"), b"main workspace sentinel").unwrap();
    std::fs::write(main_ws.join("task.json"), TASK_JSON).unwrap();

    // the isolated worktree (fs-copy grant): holds the task spec only
    let grant = td.path().join("grant");
    std::fs::create_dir_all(&grant).unwrap();
    std::fs::write(grant.join("task.json"), TASK_JSON).unwrap();

    let bin = env!("CARGO_BIN_EXE_okra");
    let out = Command::new(bin)
        .args([
            "run-subagent",
            "--grant",
            grant.to_str().unwrap(),
            "--task",
            grant.join("task.json").to_str().unwrap(),
        ])
        .output()
        .expect("spawn subagent child");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "subagent failed: {}\nstdout: {stdout}",
        String::from_utf8_lossy(&out.stderr)
    );

    let sub_line = stdout.lines().find(|l| l.starts_with("SUBAGENT ")).expect("verdict");
    let v: serde_json::Value = serde_json::from_str(sub_line.trim_start_matches("SUBAGENT ")).unwrap();

    // task completed inside the grant with exact deliverable
    assert_eq!(v["task_completed"], true);
    assert_eq!(v["files_verified"], true);
    assert_eq!(
        std::fs::read_to_string(grant.join("deliverable.md")).unwrap(),
        "# deliverable\nproduced inside the grant\n"
    );
    // kernel probe verdicts
    assert_eq!(v["kernel_write_inside_grant"], "ok");
    assert_eq!(
        v["kernel_write_outside_grant"], "denied",
        "the kernel must deny writes outside the grant"
    );

    // the main workspace is untouched: sentinel intact, no escape file, no
    // deliverable leaked out
    assert_eq!(
        std::fs::read_to_string(main_ws.join("sentinel.txt")).unwrap(),
        "main workspace sentinel"
    );
    assert!(!main_ws.join("escape-attempt.txt").exists());
    assert!(!main_ws.join("deliverable.md").exists());
    assert!(!td.path().join("escape-attempt.txt").exists());
    // the policy layer refused the ../ path outright — nothing landed in
    // the grant either (resolve_in_workspace rejects `..` escapes)
    assert!(!grant.join("escape-attempt.txt").exists());
}
