//! Operator acceptance for `okra pin-status`: the rollout loop is
//! deploy → verify fingerprint → trust. All three pin states must report
//! honestly through the real binary.

use std::process::Command;

fn run(bin: &str, pin: Option<&std::path::Path>) -> (String, String) {
    let mut cmd = Command::new(bin);
    cmd.arg("pin-status");
    if let Some(p) = pin {
        cmd.arg("--pin").arg(p);
    }
    let out = cmd.output().expect("run okra pin-status");
    (
        out.status.code().unwrap_or(-1).to_string(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
    )
}

#[test]
#[allow(clippy::disallowed_methods)] // test harness: runs the compiled binary
fn pin_status_reports_all_three_states_honestly() {
    let bin = env!("CARGO_BIN_EXE_okra");
    let td = tempfile::tempdir().unwrap();

    // 1. no pin → NotConfigured, no sha256
    let (code, stdout) = run(bin, None);
    assert_eq!(code, "0");
    assert!(stdout.contains("\"state\":\"not_configured\""), "{stdout}");
    assert!(stdout.contains("\"sha256\":null"), "{stdout}");

    // 2. valid pin → Enforced with the exact bytes' fingerprint and source
    let pin_path = td.path().join("managed-policy.json");
    let body = r#"{ "version": 1, "source": "org-it", "sandboxCeiling": "workspace-write", "approvalMustAsk": true }"#;
    std::fs::write(&pin_path, body).unwrap();
    let (code, stdout) = run(bin, Some(&pin_path));
    assert_eq!(code, "0");
    assert!(stdout.contains("\"state\":\"enforced\""), "{stdout}");
    assert!(stdout.contains("\"source\":\"org-it\""), "{stdout}");
    assert!(stdout.contains("\"sandboxCeiling\":\"workspace-write\""), "{stdout}");
    assert!(stdout.contains("\"approvalMustAsk\":true"), "{stdout}");
    // fingerprint matches the deployed bytes exactly
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(body.as_bytes());
    let expected = format!("\"sha256\":\"{:x}\"", h.finalize());
    assert!(stdout.contains(&expected), "{stdout} vs {expected}");

    // 3. corrupt pin → FailClosed with the reason named
    std::fs::write(&pin_path, "{ broken").unwrap();
    let (code, stdout) = run(bin, Some(&pin_path));
    assert_eq!(code, "0", "reporting a broken pin is a success, not a crash");
    assert!(stdout.contains("\"state\":\"fail_closed\""), "{stdout}");
    assert!(stdout.contains("pin is not valid JSON"), "{stdout}");
}
