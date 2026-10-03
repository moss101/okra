//! G5 full loop (MASTER-PLAN §4): the parent-side launcher and the
//! kernel-confined child runner wired together. `okra subagent-launch`
//! creates a REAL git worktree grant, the confined child process runs the
//! task inside it (kernel self-confinement: writes outside the grant are
//! denied, an escape attempt is policy-refused), the parent collects the
//! work as a branch commit for review — and the parent checkout stays
//! untouched throughout.


// Test harness: these acceptance tests execute the compiled crate binary as
// the system under test. The no-raw-spawn/canonicalize bans target production
// paths (production spawning goes through okra_policy's confined runner); the
// acceptance harness must exercise the real binary end-to-end.
#![allow(clippy::disallowed_methods)]
use std::process::Command;

const TASK_JSON: &str = r##"{
  "task": "G5 loop: write the deliverable inside the grant, attempt an escape",
  "files": [
    { "path": "deliverable.md", "content": "# deliverable\nproduced inside the worktree grant\n" },
    { "path": "../escape-attempt.txt", "content": "must never land anywhere" }
  ]
}"##;

fn git_available() -> bool {
    std::process::Command::new("git")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

// The subagent launcher FAILS CLOSED where no kernel sandbox exists:
// on windows the nono stub reports Unavailable and confined child
// launches are refused (the G5 contract — isolation enforced by the
// kernel, not policy). The happy-path test needs Landlock/Seatbelt.
#[cfg(unix)]
#[test]
fn g5_launcher_confined_child_and_collect_work() {
    if !git_available() {
        return;
    }
    let td = tempfile::tempdir().unwrap();

    // parent repository: sentinel + initial commit
    let repo_path = td.path().join("main");
    std::fs::create_dir_all(&repo_path).unwrap();
    std::fs::write(repo_path.join("sentinel.txt"), b"main workspace sentinel").unwrap();
    std::fs::write(repo_path.join("task.json"), TASK_JSON).unwrap();
    let repo = okra_host::git::GitRepository::init(&repo_path).unwrap();
    let config = repo.root().join(".git").join("config");
    let mut cfg = std::fs::read_to_string(&config).unwrap_or_default();
    if !cfg.contains("user.name") {
        cfg.push_str("\n[user]\n\tname = okra-test\n\temail = okra-test@example.com\n");
        std::fs::write(&config, cfg).unwrap();
    }
    repo.commit_all("initial").unwrap();
    let base_hash = repo.head().unwrap().hash;

    let worktree = td.path().join("grant");

    let bin = env!("CARGO_BIN_EXE_okra");
    let out = Command::new(bin)
        .args([
            "subagent-launch",
            "--repo",
            &repo_path.to_string_lossy(),
            "--name",
            "g5-loop",
            "--worktree",
            &worktree.to_string_lossy(),
            "--task-spec",
            &repo_path.join("task.json").to_string_lossy(),
            "--task",
            "produce deliverable.md inside the grant",
            "--parent-session",
            "sess-parent-main",
        ])
        .output()
        .expect("run subagent-launch");
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();

    let verdict_line = stdout
        .lines()
        .find(|l| l.starts_with("ORCHESTRATION "))
        .unwrap_or_else(|| panic!("no ORCHESTRATION verdict: {stdout}\n{stderr}"));
    let verdict: serde_json::Value =
        serde_json::from_str(verdict_line.trim_start_matches("ORCHESTRATION "))
            .unwrap_or_else(|e| panic!("verdict parse: {e}\n{stdout}\n{stderr}"));

    // child verdict: confined run passed inside the worktree
    assert_eq!(
        verdict["child"]["passed"],
        serde_json::Value::Bool(true),
        "child: {verdict}"
    );
    assert_eq!(
        verdict["child"]["kernel_write_outside_grant"],
        "denied",
        "escape write denied by the kernel"
    );

    // collect_work: the child's work landed as a commit on the branch
    let commit = verdict["commit"].as_str().expect("branch commit hash");
    assert_eq!(commit.len(), 40);
    assert_ne!(commit, base_hash, "child commit is new work");

    // the deliverable is reachable from the branch in the shared store
    let show = Command::new("git")
        .args([
            "-C",
            &repo_path.to_string_lossy(),
            "show",
            "g5-loop:deliverable.md",
        ])
        .output()
        .unwrap();
    assert!(
        String::from_utf8_lossy(&show.stdout).contains("inside the worktree grant"),
        "deliverable on the branch: {}",
        String::from_utf8_lossy(&show.stdout)
    );

    // parent checkout untouched: same HEAD, clean tree, sentinel intact
    assert_eq!(repo.head().unwrap().hash, base_hash);
    assert!(!repo.is_dirty().unwrap());
    assert_eq!(
        std::fs::read_to_string(repo_path.join("sentinel.txt")).unwrap(),
        "main workspace sentinel"
    );
    assert_eq!(verdict["parent_clean"], serde_json::Value::Bool(true));
    // fork linkage: the child's kernel session header links to the parent
    assert_eq!(
        verdict["child"]["parent_session"].as_str(),
        Some("sess-parent-main"),
        "fork linkage recorded in the child session header"
    );

    // escape attempt never landed in the parent
    assert!(!repo_path.join("escape-attempt.txt").exists());

    // worktree removed after orchestration; the branch remains
    assert!(!worktree.exists() || !worktree.join(".git").exists());
    let branches = Command::new("git")
        .args(["-C", &repo_path.to_string_lossy(), "branch", "--list", "g5-loop"])
        .output()
        .unwrap();
    assert!(
        String::from_utf8_lossy(&branches.stdout).contains("g5-loop"),
        "branch survives cleanup"
    );
}
