//! Managed-pin RUNTIME enforcement: a pin at the managed path must clamp
//! the run that starts under it — provider denial, max-turns ceiling,
//! fail-closed restrictions — not just be reportable via pin-status.
//! `OKRA_MANAGED_PIN` points the binary at a test pin so tests never
//! touch the real `~/.okra`.

// Test harness: executes the compiled binary as the system under test (the
// no-raw-spawn ban targets production paths).
#![allow(clippy::disallowed_methods)]

use std::path::Path;
use std::process::Command;

fn run(pin: Option<&Path>, args: &[&str]) -> (i32, String, String) {
    let td = tempfile::tempdir().unwrap();
    std::fs::write(td.path().join("notes.txt"), "okra reads files").unwrap();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_okra"));
    cmd.arg("--cwd").arg(td.path()).arg("--json");
    if let Some(p) = pin {
        cmd.env("OKRA_MANAGED_PIN", p);
    } else {
        cmd.env_remove("OKRA_MANAGED_PIN");
    }
    cmd.env_remove("OKRA_TRUST_FILE");
    cmd.args(args).arg("read notes.txt and summarize");
    let out = cmd.output().expect("run okra");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

#[test]
#[allow(clippy::disallowed_methods)] // test harness: runs the compiled binary
fn provider_denied_by_pin_allowlist() {
    let td = tempfile::tempdir().unwrap();
    let pin = td.path().join("pin.json");
    std::fs::write(&pin, r#"{ "version": 1, "source": "corp-it", "providerAllowlist": ["otherco"] }"#).unwrap();
    let (code, _, stderr) = run(Some(&pin), &["--provider", "openai"]);
    assert_eq!(code, 1, "{stderr}");
    assert!(stderr.contains("denied by the managed policy pin"), "{stderr}");
    assert!(stderr.contains("corp-it"), "the pin source is named: {stderr}");
}

#[test]
#[allow(clippy::disallowed_methods)] // test harness: runs the compiled binary
fn max_turns_ceiling_clamps_the_run() {
    let td = tempfile::tempdir().unwrap();
    let pin = td.path().join("pin.json");
    std::fs::write(&pin, r#"{ "version": 1, "source": "corp-it", "maxTurnsCeiling": 1 }"#).unwrap();
    let (code, stdout, stderr) = run(Some(&pin), &[]);
    assert_eq!(code, 0, "{stderr}");
    assert!(stderr.contains("[pin] max-turns clamped to 1"), "{stderr}");
    assert!(stdout.contains("turn"), "a real turn ran: {stdout}");
}

#[test]
#[allow(clippy::disallowed_methods)] // test harness: runs the compiled binary
fn no_pin_changes_nothing_and_prints_nothing() {
    let (code, _, stderr) = run(None, &[]);
    assert_eq!(code, 0, "{stderr}");
    assert!(!stderr.contains("[pin]"), "NotConfigured must be silent: {stderr}");
}

#[test]
#[allow(clippy::disallowed_methods)] // test harness: runs the compiled binary
fn fail_closed_pin_denies_all_providers_and_applies_read_only() {
    let td = tempfile::tempdir().unwrap();
    let pin = td.path().join("pin.json");
    std::fs::write(&pin, "{ broken").unwrap();
    // deny-all: any provider is refused under a corrupt pin
    let (code, _, stderr) = run(Some(&pin), &["--provider", "openai"]);
    assert_eq!(code, 1, "{stderr}");
    assert!(stderr.contains("[pin] fail-closed:"), "{stderr}");
    assert!(stderr.contains("denied by the managed policy pin"), "{stderr}");
    // and the plain path still runs, confined read-only (fail-closed
    // ceiling) — the existing [sandbox] line proves apply actually ran
    let (code, _, stderr) = run(Some(&pin), &[]);
    assert_eq!(code, 0, "{stderr}");
    assert!(stderr.contains("[pin] fail-closed:"), "{stderr}");
    assert!(stderr.contains("[sandbox]"), "confinement applied: {stderr}");
    assert!(
        stderr.contains("[pin] sandbox clamped to ReadOnly"),
        "the off→read-only clamp is announced: {stderr}"
    );
}
