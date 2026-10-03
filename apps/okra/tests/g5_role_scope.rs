//! G5 role-scope projection at the tool plane: when the orchestrator sets
//! OKRA_SUBAGENT_WRITABLE (the projected role ∩ parent scope), writes
//! outside the allowlist are refused by the write_file wrapper BEFORE the
//! kernel — defense in depth on top of the nono confinement (which stays
//! the hard backstop). An unset variable means no tool-plane restriction.


// Test harness: these acceptance tests execute the compiled crate binary as
// the system under test. The no-raw-spawn/canonicalize bans target production
// paths (production spawning goes through okra_policy's confined runner); the
// acceptance harness must exercise the real binary end-to-end.
#![allow(clippy::disallowed_methods)]
use std::process::Command;

#[cfg(unix)]
fn spec(path: &str) -> String {
    format!(
        r#"{{"task":"probe","files":[{{"path":"{path}","content":"role-scoped deliverable\n"}}]}}"#
    )
}

#[cfg(unix)]
fn run_child(grant: &std::path::Path, spec_path: &std::path::Path, writable: Option<&str>) -> (std::process::Output, String) {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_okra"));
    cmd.args(["run-subagent", "--grant", grant.to_str().unwrap(), "--task", spec_path.to_str().unwrap()]);
    if let Some(w) = writable {
        cmd.env("OKRA_SUBAGENT_WRITABLE", w);
    } else {
        cmd.env_remove("OKRA_SUBAGENT_WRITABLE");
    }
    let out = cmd.output().expect("spawn subagent child");
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    (out, stdout)
}

#[cfg(unix)]
fn verdict(stdout: &str) -> serde_json::Value {
    let line = stdout.lines().find(|l| l.starts_with("SUBAGENT ")).expect("verdict line");
    serde_json::from_str(line.trim_start_matches("SUBAGENT ")).unwrap()
}

// Role-scoped writes run through the confined child turn: on windows the
// sandbox stub reports Unavailable and confined launches are refused
// (fail-closed by design). Unlocks with the restricted-token sandbox.
#[cfg(unix)]
#[test]
fn write_inside_declared_role_scope_lands() {
    let td = tempfile::tempdir().unwrap();
    let grant = td.path().join("grant");
    std::fs::create_dir_all(&grant).unwrap();
    let spec_path = grant.join("task.json");
    std::fs::write(&spec_path, spec("docs/deliverable.md")).unwrap();

    let (out, stdout) = run_child(&grant, &spec_path, Some("docs"));
    assert!(out.status.success(), "stdout: {stdout}");
    let v = verdict(&stdout);
    assert_eq!(v["files_verified"], true);
    assert_eq!(
        std::fs::read_to_string(grant.join("docs/deliverable.md")).unwrap(),
        "role-scoped deliverable\n"
    );
}

// Role-scoped writes run through the confined child turn: on windows the
// sandbox stub reports Unavailable and confined launches are refused
// (fail-closed by design). Unlocks with the restricted-token sandbox.
#[cfg(unix)]
#[test]
fn write_outside_declared_role_scope_is_refused_at_the_tool_plane() {
    let td = tempfile::tempdir().unwrap();
    let grant = td.path().join("grant");
    std::fs::create_dir_all(&grant).unwrap();
    let spec_path = grant.join("task.json");
    std::fs::write(&spec_path, spec("deliverable.md")).unwrap();

    let (out, stdout) = run_child(&grant, &spec_path, Some("docs"));
    // the verdict line still reports; the child exits non-zero because the
    // deliverable could not be produced inside the declared scope
    let v = verdict(&stdout);
    assert_eq!(v["files_verified"], false, "stdout: {stdout}");
    assert!(!out.status.success());
    assert!(!grant.join("deliverable.md").exists());
}
