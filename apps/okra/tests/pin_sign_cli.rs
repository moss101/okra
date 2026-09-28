//! Admin acceptance for `okra pin-sign`: the full enterprise loop is
//! provision key → sign policy → distribute → verify+trust on the daemon
//! side (`okra pin-status`). Every step must run through the real binary,
//! and a tampered envelope must be rejected end to end.

// Test harness: executes the compiled binary as the system under test (the
// no-raw-spawn ban targets production paths).
#![allow(clippy::disallowed_methods)]

use std::path::Path;
use std::process::Command;

fn okra(args: &[&str]) -> (i32, String, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_okra")).args(args).output().expect("run okra");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

const POLICY: &str = r#"{ "version": 1, "source": "corp-it", "sandboxCeiling": "read-only", "approvalMustAsk": true }"#;

#[test]
#[allow(clippy::disallowed_methods)] // test harness: runs the compiled binary
fn provision_sign_verify_trust_round_trip() {
    let td = tempfile::tempdir().unwrap();
    let key_file = td.path().join("admin.key");
    let policy_file = td.path().join("policy.json");
    let envelope_file = td.path().join("pin.envelope.json");
    std::fs::write(&policy_file, POLICY).unwrap();

    // 1. provision: fresh key, owner-only file, public key reported
    let (code, stdout, stderr) = okra(&["pin-sign", "--generate-key", key_file.to_str().unwrap()]);
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("\"publicKey\""), "{stdout}");
    let pubkey: String = {
        let v: serde_json::Value = serde_json::from_str(&stdout).unwrap();
        v["publicKey"].as_str().unwrap().to_string()
    };
    assert_eq!(pubkey.len(), 64, "{pubkey}");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&key_file).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "key file must be owner-only");
    }

    // 2. sign with the provisioned key file
    let (code, _, stderr) = okra(&[
        "pin-sign",
        "--policy",
        policy_file.to_str().unwrap(),
        "--key-file",
        key_file.to_str().unwrap(),
        "-o",
        envelope_file.to_str().unwrap(),
    ]);
    assert_eq!(code, 0, "{stderr}");
    let envelope_text = std::fs::read_to_string(&envelope_file).unwrap();
    let env: serde_json::Value = serde_json::from_str(&envelope_text).unwrap();
    assert_eq!(env["signer"].as_str().unwrap(), pubkey);
    assert_eq!(env["payload"].as_str().unwrap(), POLICY, "payload embedded verbatim");
    assert_eq!(env["signature"].as_str().unwrap().len(), 128);

    // 3. distribute: the daemon-side pin-status sees Enforced + trusted
    let trust_file = td.path().join("trust.json");
    std::fs::write(&trust_file, format!("[\"{pubkey}\"]")).unwrap();
    let (code, stdout, stderr) = okra(&[
        "pin-status",
        "--pin",
        envelope_file.to_str().unwrap(),
        "--trust-file",
        trust_file.to_str().unwrap(),
    ]);
    assert_eq!(code, 0, "{stderr}");
    assert!(stdout.contains("\"state\":\"enforced\""), "{stdout}");
    assert!(stdout.contains("\"signatureVerified\":true"), "{stdout}");
    assert!(stdout.contains("\"signerTrusted\":true"), "{stdout}");
    assert!(stdout.contains("\"source\":\"corp-it\""), "{stdout}");
}

#[test]
#[allow(clippy::disallowed_methods)] // test harness: runs the compiled binary
fn tampered_distribution_fails_closed_on_the_daemon_side() {
    let td = tempfile::tempdir().unwrap();
    let key_file = td.path().join("admin.key");
    let (code, stdout, _) = okra(&["pin-sign", "--generate-key", key_file.to_str().unwrap()]);
    assert_eq!(code, 0, "{stdout}");

    let policy_file = td.path().join("policy.json");
    std::fs::write(&policy_file, POLICY).unwrap();
    let (code, stdout, _) = okra(&[
        "pin-sign",
        "--policy",
        policy_file.to_str().unwrap(),
        "--key",
        &hex_of_file(&key_file),
        "-o",
        td.path().join("pin.envelope.json").to_str().unwrap(),
    ]);
    assert_eq!(code, 0, "{stdout}");

    // attacker flips a byte in the distributed envelope's payload
    let envelope_file = td.path().join("pin.envelope.json");
    let mut env: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&envelope_file).unwrap()).unwrap();
    env["payload"] = serde_json::Value::String(POLICY.replace("read-only", "danger-full-access"));
    std::fs::write(&envelope_file, serde_json::to_vec(&env).unwrap()).unwrap();

    let (code, stdout, _) = okra(&["pin-status", "--pin", envelope_file.to_str().unwrap()]);
    assert_eq!(code, 0);
    assert!(stdout.contains("\"state\":\"fail_closed\""), "{stdout}");
    assert!(stdout.contains("signature invalid"), "{stdout}");
}

#[test]
#[allow(clippy::disallowed_methods)] // test harness: runs the compiled binary
fn bad_inputs_exit_2_and_name_the_flaw() {
    let td = tempfile::tempdir().unwrap();
    let policy_file = td.path().join("policy.json");
    std::fs::write(&policy_file, "{\"sandboxCeiling\":\"nonsense\"}").unwrap();
    let key = "ab".repeat(32);

    // schema-invalid policy refuses to sign (exit 1, schema problem named)
    let (code, _, stderr) = okra(&["pin-sign", "--policy", policy_file.to_str().unwrap(), "--key", &key]);
    assert_eq!(code, 1);
    assert!(stderr.contains("schema mismatch"), "{stderr}");

    // short key refuses with the length named (exit 2)
    let (code, _, stderr) = okra(&["pin-sign", "--policy", policy_file.to_str().unwrap(), "--key", "abcd"]);
    assert_eq!(code, 2);
    assert!(stderr.contains("not 64 hex chars"), "{stderr}");

    // missing --key entirely (exit 2)
    let (code, _, stderr) = okra(&["pin-sign", "--policy", policy_file.to_str().unwrap()]);
    assert_eq!(code, 2);
    assert!(stderr.contains("--key"), "{stderr}");

    // unknown flag (exit 2)
    let (code, _, stderr) = okra(&["pin-sign", "--wat"]);
    assert_eq!(code, 2);
    assert!(stderr.contains("--wat"), "{stderr}");
}

fn hex_of_file(path: &Path) -> String {
    std::fs::read(path)
        .unwrap()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
