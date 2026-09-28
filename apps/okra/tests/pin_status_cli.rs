//! Operator acceptance for `okra pin-status`: the rollout loop is
//! deploy → verify fingerprint → trust. All three pin states must report
//! honestly through the real binary.

// Test harness: executes the compiled binary as the system under test (the
// no-raw-spawn ban targets production paths).
#![allow(clippy::disallowed_methods)]

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

#[test]
#[allow(clippy::disallowed_methods)]
fn pin_status_reports_signer_trust_from_the_provisioned_file() {
    use ed25519_dalek::{Signer, SigningKey};

    let bin = env!("CARGO_BIN_EXE_okra");
    let td = tempfile::tempdir().unwrap();
    let key = SigningKey::from_bytes(&[7u8; 32]);
    let signer_hex: String = key
        .verifying_key()
        .to_bytes()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let policy_json = r#"{ "source": "org-it", "sandboxCeiling": "read-only" }"#;
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(policy_json.as_bytes());
    let sig_hex: String = key
        .sign(&h.finalize())
        .to_bytes()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let pin_path = td.path().join("managed-policy.json");
    std::fs::write(
        &pin_path,
        serde_json::json!({
            "payload": policy_json,
            "signer": signer_hex,
            "signature": sig_hex,
        })
        .to_string(),
    )
    .unwrap();

    let run_with = |trust_file: Option<&std::path::Path>| {
        let mut cmd = Command::new(bin);
        cmd.arg("pin-status").arg("--pin").arg(&pin_path);
        if let Some(tf) = trust_file {
            cmd.arg("--trust-file").arg(tf);
        }
        eprintln!("TEST CMD ARGS: {:?}", cmd);
        let out = cmd.output().expect("run pin-status");
        String::from_utf8_lossy(&out.stdout).into_owned()
    };

    // no trust file: signature verified against the embedded key, but the
    // trust state is UNKNOWN (null) — never silently "trusted"
    let stdout = run_with(None);
    assert!(stdout.contains("\"signerTrusted\":null"), "{stdout}");
    assert!(stdout.contains("\"signatureVerified\":true"), "{stdout}");

    // provisioned trust file containing the signer → trusted
    let trust = td.path().join("trust.json");
    std::fs::write(&trust, serde_json::json!([signer_hex]).to_string()).unwrap();
    let stdout = run_with(Some(&trust));
    assert!(stdout.contains("\"signerTrusted\":true"), "{stdout}");

    // trust file without the signer → the pin FAILS CLOSED: a valid
    // signature from an unapproved key is a lockdown, not a soft "no"
    std::fs::write(&trust, serde_json::json!(["deadbeef"]).to_string()).unwrap();
    let stdout = run_with(Some(&trust));
    assert!(stdout.contains("\"state\":\"fail_closed\""), "{stdout}");
    assert!(stdout.contains("pin signer not trusted"), "{stdout}");
}
