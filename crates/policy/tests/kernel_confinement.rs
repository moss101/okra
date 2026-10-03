// The tests in this file apply REAL kernel confinement (Landlock on
// Linux, Seatbelt on macOS): unix-only by definition. On windows the nono
// stub reports Unavailable and these fail closed (by design).
#![cfg(unix)]
//! REAL kernel-enforcement evidence (macOS Seatbelt / Linux Landlock):
//! a child process applies nono self-confinement and then physically
//! attempts reads/writes. The parent asserts the kernel verdicts.
//!
//! Runs as a child process because `Sandbox::apply` is irreversible —
//! applying it in-process would confine the whole test runner.


// Test harness: these acceptance tests execute the compiled crate binary as
// the system under test. The no-raw-spawn/canonicalize bans target production
// paths (production spawning goes through okra_policy's confined runner); the
// acceptance harness must exercise the real binary end-to-end.
#![allow(clippy::disallowed_methods)]
use std::io::Read;
use std::process::Command;

use okra_policy::{
    NonoSandboxBackend, SandboxExecutionPolicy, SandboxMode, SelfConfinement,
};

#[test]
fn kernel_denies_writes_outside_policy_after_apply() {
    if std::env::var("OKRA_SANDBOX_PROBE").is_ok() {
        // child mode: apply the kernel sandbox, probe reads/writes, print
        run_probe_child();
    } else {
        // parent mode: spawn the probe child (irreversible apply)
        parent_spawn_and_assert();
    }
}

fn parent_spawn_and_assert() {

    // parent: set up a workspace with one readable file, spawn self as child
    let td = tempfile::tempdir().unwrap();
    let ws = td.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::write(ws.join("in.txt"), b"readable").unwrap();

    let exe = std::env::current_exe().unwrap();
    let out = Command::new(exe)
        .args([
            "kernel_denies_writes_outside_policy_after_apply",
            "--exact",
            "--nocapture",
        ])
        .env("OKRA_SANDBOX_PROBE", "1")
        .env("OKRA_PROBE_WS", &ws)
        .output()
        .expect("spawn probe child");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "probe child failed: {stderr}"
    );
    // the child prints its verdict as the last line of stdout
    let stdout = String::from_utf8_lossy(&out.stdout);
    let verdict_line = stdout
        .lines()
        .rev()
        .find(|l| l.starts_with("VERDICT "))
        .expect("verdict line");
    let verdict = verdict_line.trim_start_matches("VERDICT ");
    assert_eq!(
        verdict,
        "read_inside=ok write_inside=denied write_outside=denied",
        "kernel must deny writes outside the read-only policy"
    );
}

#[allow(dead_code)] // reached only in probe-child mode (see above)
fn run_probe_child() -> ! {
    let ws = std::path::PathBuf::from(
        std::env::var("OKRA_PROBE_WS").expect("probe workspace"),
    );

    // 1. apply read-only confinement: workspace read, sessions/temp RW
    let policy = SandboxExecutionPolicy {
        mode: SandboxMode::ReadOnly,
        workspace_root: ws.clone(),
        session_id: Some("probe".into()),
    };
    let backend = NonoSandboxBackend::blocking_network();
    let report = match backend.apply_to_self(&policy, &[]) {
        Ok(r) => r,
        Err(e) => {
            // honest unavailability on unsupported kernels: report and pass
            println!("VERDICT unsupported ({e})");
            std::process::exit(0);
        }
    };
    assert_eq!(report.enforcement, okra_policy::SandboxEnforcement::Full);

    // 2. read inside the workspace: must work
    let mut buf = String::new();
    let read_inside = std::fs::File::open(ws.join("in.txt"))
        .and_then(|mut f| f.read_to_string(&mut buf))
        .is_ok();
    let read_inside = read_inside && buf == "readable";

    // 3. write inside the read-only workspace: kernel must DENY
    let write_inside = std::fs::write(ws.join("out.txt"), b"x").is_ok();

    // 4. write outside the workspace (temp dir): kernel must DENY
    let outside = std::env::temp_dir().join(format!("okra-probe-out-{}", std::process::id()));
    let write_outside = std::fs::write(&outside, b"x").is_ok();
    let _ = std::fs::remove_file(&outside);

    println!(
        "VERDICT read_inside={} write_inside={} write_outside={}",
        ok(read_inside),
        denied(write_inside),
        denied(write_outside)
    );
    std::process::exit(0);

    fn ok(v: bool) -> &'static str {
        if v { "ok" } else { "failed" }
    }
    fn denied(v: bool) -> &'static str {
        if v { "ALLOWED" } else { "denied" }
    }
}
