// PTY semantics are the unix second-pass surface: on windows the
// ConPTY terminal-emulator layer is the tracked remaining work
// (docs/m6-windows-port.md — the DSR probe reply alone does not unstick
// conhost rendering).
#![cfg(unix)]
//! Terminal/PTY domain tests: real pseudo-terminal sessions — programs see
//! a TTY, output streams to the host, exit codes propagate.

use okra_host::{TerminalHost, TerminalSession, TerminalSize};

#[test]
fn pty_runs_program_and_streams_output() {
    let mut session = TerminalSession::spawn(
        "/bin/sh",
        &["-c".into(), "echo okra-pty-marker; echo tty-check: $(test -t 0 && echo IS_TTY || echo NOT_TTY)".into()],
        &std::env::temp_dir(),
        TerminalSize::default(),
    )
    .unwrap();
    let output = session.read_to_end().unwrap();
    assert!(output.contains("okra-pty-marker"), "{output}");
    assert!(output.contains("tty-check: IS_TTY"), "PTY must look like a TTY: {output}");
    let code = session.wait().unwrap();
    assert_eq!(code, 0);
}

#[test]
fn host_manages_named_sessions() {
    let mut host = TerminalHost::new();
    host
        .open("t1", "/bin/sh", &["-c".into(), "echo one; sleep 5".into()], &std::env::temp_dir())
        .unwrap();
    host.open("t2", "/bin/sh", &["-c".into(), "echo two".into()], &std::env::temp_dir())
        .unwrap();
    assert_eq!(host.ids(), vec!["t1".to_string(), "t2".to_string()]);

    let t2 = host.get("t2").unwrap();
    let _ = t2.read_to_end();
    let _ = t2.wait();
    host.close("t2");

    // t1 still alive (sleeping)
    assert!(host.get("t1").is_some());
    host.close("t1");
    assert!(host.get("t1").is_none());
}

#[test]
fn resize_applies() {
    let session = TerminalSession::spawn(
        "/bin/sh",
        &["-c".into(), "sleep 1".into()],
        &std::env::temp_dir(),
        TerminalSize { rows: 24, cols: 80 },
    )
    .unwrap();
    session.resize(TerminalSize { rows: 40, cols: 120 }).expect("resize");
    session.resize(TerminalSize { rows: 10, cols: 40 }).expect("resize back");
    let _ = session.wait();
}
