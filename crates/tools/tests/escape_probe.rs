//! Regression: a `..` in a write path must be REFUSED, never silently
//! normalized into the workspace (path-traversal hole found by the G5
//! subagent worktree test).

use okra_tools::builtins::resolve_in_workspace;
use serde_json::json;

#[test]
fn probe_escape() {
    let td = tempfile::tempdir().unwrap();
    let root = td.path().join("grant");
    std::fs::create_dir_all(&root).unwrap();

    // relative escape refused
    let r = resolve_in_workspace(&root, "../escape-attempt.txt");
    assert!(r.is_err(), "must refuse .. escape");
    // absolute-outside refused by the caller (starts_with root)
    let r = resolve_in_workspace(&root, "/tmp/escape-attempt.txt");
    assert!(r.is_err());
    // inside path resolves
    std::fs::write(root.join("in.txt"), b"x").unwrap();
    let r = resolve_in_workspace(&root, "in.txt");
    assert!(r.is_ok());

    // end-to-end: write_file with a .. path is refused
    let wf = okra_tools::builtins::ErasedWriteFile::new(root.clone());
    let stream = wf.execute(&json!({ "path": "../escape.txt", "content": "no" }));
    assert!(stream.terminal().unwrap().is_err());
    assert!(!root.parent().unwrap().join("escape.txt").exists());
}
