//! CLI acceptance for `okra export-replay`: a session written through the
//! kernel in this process exports via the real binary to a standalone HTML
//! file (stdout form and -o file form), and unknown sessions error honestly.

use std::process::Command;

#[test]
#[allow(clippy::disallowed_methods)] // test harness: runs the compiled binary
fn export_replay_cli_round_trip() {
    let td = tempfile::tempdir().unwrap();
    let header = kernel::SessionHeader {
        version: kernel::SESSION_FORMAT_VERSION,
        id: "session-cli-export".into(),
        created_at: 1_789_600_000_000.0,
        cwd: td.path().to_string_lossy().into_owned(),
        parent_session: None,
        is_seeded: false,
    };
    // the CLI resolves --sessions as cwd/.okra-sessions by default
    let sessions_root = td.path().join(".okra-sessions");
    let mut handle = kernel::SessionHandle::create(&sessions_root, &header).unwrap();
    handle
        .append(vec![kernel::make_event(
            "user/message",
            serde_json::json!({ "text": "export me via the cli" }),
            || 1_789_600_000_000.0,
        )])
        .unwrap();
    drop(handle);

    let out_path = td.path().join("replay.html");
    let bin = env!("CARGO_BIN_EXE_okra");
    let run = |args: &[&str]| {
        let out = Command::new(bin)
            .args(args)
            .current_dir(td.path())
            .output()
            .expect("run okra export-replay");
        (
            out.status,
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    };

    // -o file form: writes the transcript and reports the byte count
    let out_arg = out_path.to_string_lossy().into_owned();
    let (status, stdout, stderr) = run(&["export-replay", "session-cli-export", "-o", &out_arg]);
    assert!(status.success(), "{stderr}");
    assert!(stdout.contains("wrote"), "{stdout}");
    let html = std::fs::read_to_string(&out_path).unwrap();
    assert!(html.starts_with("<!doctype html>"));
    assert!(html.contains("export me via the cli"));
    assert!(html.contains("1 events"), "{html}");

    // stdout form: prints the HTML document itself
    let (status, stdout, stderr) = run(&["export-replay", "session-cli-export"]);
    assert!(status.success(), "{stderr}");
    assert!(stdout.starts_with("<!doctype html>"), "{stdout}");
    assert!(stdout.contains("export me via the cli"));

    // unknown session → non-zero with a named error
    let (status, _, stderr) = run(&["export-replay", "session-never-was"]);
    assert!(!status.success());
    assert!(stderr.contains("open session"), "{stderr}");
}

use okra_kernel as kernel;
