//! Regression (G3 dogfood day-1 finding): overwriting a file used to RESET
//! its mode — the atomic write renamed a fresh temp file over the target,
//! so a 0755 script silently lost +x and the dogfood harness had to chmod
//! by hand. An existing target's mode belongs to the file, not to the
//! write: write_file AND edit_file must carry it across the rename.

#![cfg(unix)]

use okra_tools::builtins::{ErasedEditFile, ErasedWriteFile};
use serde_json::json;
use std::os::unix::fs::PermissionsExt;

fn mode_of(path: &std::path::Path) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

#[test]
fn overwrite_preserves_exec_bit() {
    let td = tempfile::tempdir().unwrap();
    let root = td.path().join("ws");
    std::fs::create_dir_all(&root).unwrap();
    let script = root.join("run.sh");
    std::fs::write(&script, b"#!/bin/sh\necho v1\n").unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

    let wf = ErasedWriteFile::new(root.clone());
    let stream = wf.execute(&json!({ "path": "run.sh", "content": "#!/bin/sh\necho v2\n" }));
    assert!(stream.terminal().unwrap().is_ok());
    assert_eq!(
        std::fs::read_to_string(&script).unwrap(),
        "#!/bin/sh\necho v2\n",
        "content updated"
    );
    assert_eq!(mode_of(&script), 0o755, "exec bit survives the overwrite");
}

#[test]
fn overwrite_preserves_restrictive_mode() {
    let td = tempfile::tempdir().unwrap();
    let root = td.path().join("ws");
    std::fs::create_dir_all(&root).unwrap();
    let secret = root.join("secret.env");
    std::fs::write(&secret, b"A=1").unwrap();
    std::fs::set_permissions(&secret, std::fs::Permissions::from_mode(0o600)).unwrap();

    let wf = ErasedWriteFile::new(root.clone());
    let stream = wf.execute(&json!({ "path": "secret.env", "content": "A=2\n" }));
    assert!(stream.terminal().unwrap().is_ok());
    assert_eq!(mode_of(&secret), 0o600, "0600 must not widen to 0644");
}

#[test]
fn new_file_gets_default_mode_without_exec() {
    let td = tempfile::tempdir().unwrap();
    let root = td.path().join("ws");
    std::fs::create_dir_all(&root).unwrap();
    let wf = ErasedWriteFile::new(root.clone());
    let stream = wf.execute(&json!({ "path": "fresh.txt", "content": "hi" }));
    assert!(stream.terminal().unwrap().is_ok());
    assert_eq!(
        mode_of(&root.join("fresh.txt")) & 0o111,
        0,
        "a brand-new write never invents +x"
    );
}

#[test]
fn edit_file_preserves_exec_bit() {
    let td = tempfile::tempdir().unwrap();
    let root = td.path().join("ws");
    std::fs::create_dir_all(&root).unwrap();
    let script = root.join("run.sh");
    std::fs::write(&script, b"#!/bin/sh\necho v1\n").unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

    let ef = ErasedEditFile::new(root.clone());
    let stream = ef.execute(&json!({
        "path": "run.sh", "oldText": "echo v1", "newText": "echo v2"
    }));
    assert!(stream.terminal().unwrap().is_ok());
    assert_eq!(mode_of(&script), 0o755, "edit_file routes through the same atomic write");
}
