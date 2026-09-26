//! Rewind checkpoint domain tests (MASTER-PLAN §3 #38, from grok
//! checkpoint.rs): before/after snapshot semantics, multi-prompt restore,
//! external-modification detection, the durable JSONL mirror with lenient
//! reads and truncation, and the git domain reset.

use okra_host::checkpoints::{CheckpointError, CheckpointManager};
use okra_host::git::GitRepository;

fn git_available() -> bool {
    std::process::Command::new("git")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}


#[test]
fn before_is_first_wins_after_is_last_wins() {
    let td = tempfile::tempdir().unwrap();
    let mut mgr = CheckpointManager::new(td.path());
    mgr.begin_prompt(0);
    // the file is edited twice within the prompt
    mgr.record_operation(0, "a.txt", Some(b"original"), Some(b"edit-1"))
        .unwrap();
    mgr.record_operation(0, "a.txt", Some(b"edit-1"), Some(b"edit-2"))
        .unwrap();

    let checkpoint = mgr.get_checkpoint(0).unwrap();
    let before = &checkpoint.fs.before["a.txt"];
    let after = &checkpoint.fs.after["a.txt"];
    // before: first-wins (the pre-prompt state)
    assert_eq!(before.sha256, okra_host::plugins::store::sha256_hex(b"original"));
    // after: last-wins
    assert_eq!(after.sha256, okra_host::plugins::store::sha256_hex(b"edit-2"));
}

#[test]
fn restore_to_prompt_zero_reverts_and_truncates() {
    let td = tempfile::tempdir().unwrap();
    let mut mgr = CheckpointManager::new(td.path());

    // prompt 0: creates a.txt
    mgr.begin_prompt(0);
    mgr.record_operation(0, "a.txt", None, Some(b"prompt0 output"))
        .unwrap();
    mgr.finalize_prompt(0, None, None).unwrap();

    // prompt 1: a.txt edited, b.txt created
    mgr.begin_prompt(1);
    mgr.record_operation(1, "a.txt", Some(b"prompt0 output"), Some(b"prompt1 output"))
        .unwrap();
    mgr.record_operation(1, "b.txt", None, Some(b"new file")).unwrap();
    mgr.finalize_prompt(1, None, None).unwrap();
    std::fs::write(td.path().join("a.txt"), b"prompt1 output").unwrap();
    std::fs::write(td.path().join("b.txt"), b"new file").unwrap();
    assert_eq!(mgr.checkpoint_count(), 2);

    // rewind to the START of prompt 0
    let report = mgr.restore_to(None, 0).unwrap();
    // a.txt reverted to pre-prompt-0 = nonexistent → removed
    assert!(!td.path().join("a.txt").exists());
    assert!(report.removed.contains(&"a.txt".to_string()));
    // b.txt (created later) also removed
    assert!(!td.path().join("b.txt").exists());
    assert!(report.removed.contains(&"b.txt".to_string()));
    // everything truncated (>= 0 dropped)
    assert_eq!(mgr.checkpoint_count(), 0);
    assert!(matches!(
        mgr.restore_to(None, 0),
        Err(CheckpointError::Unknown(0))
    ));
}

#[test]
fn restore_recreates_deleted_files_and_reports_external_edits() {
    let td = tempfile::tempdir().unwrap();
    let mut mgr = CheckpointManager::new(td.path());
    std::fs::write(td.path().join("keep.txt"), b"before").unwrap();
    std::fs::write(td.path().join("gone.txt"), b"was here").unwrap();

    mgr.begin_prompt(0);
    mgr.record_operation(0, "keep.txt", Some(b"before"), Some(b"after")).unwrap();
    mgr.record_operation(0, "gone.txt", Some(b"was here"), None).unwrap();
    mgr.finalize_prompt(0, None, None).unwrap();
    // apply the prompt: keep edited, gone deleted
    std::fs::write(td.path().join("keep.txt"), b"after").unwrap();
    std::fs::remove_file(td.path().join("gone.txt")).unwrap();

    // EXTERNAL edit after the turn finished: keep.txt changed again
    std::fs::write(td.path().join("keep.txt"), b"externally edited").unwrap();

    let report = mgr.restore_to(None, 0).unwrap();
    // both files restored from before-snapshots
    assert_eq!(
        std::fs::read_to_string(td.path().join("keep.txt")).unwrap(),
        "before"
    );
    assert_eq!(
        std::fs::read_to_string(td.path().join("gone.txt")).unwrap(),
        "was here"
    );
    assert!(report.recreated.contains(&"gone.txt".to_string()));
    // the external edit was detected against the after-snapshot
    assert!(report
        .external_modifications
        .contains(&"keep.txt".to_string()));
}

#[test]
fn durable_mirror_survives_reopen_and_lenient_reads() {
    let td = tempfile::tempdir().unwrap();
    let mirror = td.path().join(".okra/rewind_points.jsonl");
    {
        let mut mgr = CheckpointManager::new(td.path()).with_durable_mirror(&mirror);
        mgr.begin_prompt(0);
        mgr.record_operation(0, "a.txt", None, Some(b"one")).unwrap();
        mgr.finalize_prompt(0, Some(serde_json::json!({ "turn": 0 })), None)
            .unwrap();
        mgr.begin_prompt(1);
        mgr.record_operation(1, "a.txt", Some(b"one"), Some(b"two")).unwrap();
        mgr.finalize_prompt(1, None, None).unwrap();
    }
    {
        let mut mgr = CheckpointManager::new(td.path()).with_durable_mirror(&mirror);
        mgr.load_durable_mirror(&mirror).unwrap();
        assert_eq!(mgr.checkpoint_count(), 2);
        // hunk delta survived (serde-default optional domain)
        assert!(mgr.get_checkpoint(0).unwrap().hunks.is_some());
        assert!(mgr.get_checkpoint(1).unwrap().hunks.is_none());
    }
    // a corrupted line is skipped, valid ones load
    let corrupted = format!(
        "{}\nNOT JSON AT ALL\n{}\n",
        "garbage", r#"{"promptIndex":2,"fs":{"promptIndex":2,"createdAtEpochMs":0}}"#
    );
    let bad_mirror = td.path().join("bad.jsonl");
    std::fs::write(&bad_mirror, corrupted).unwrap();
    let mut mgr = CheckpointManager::new(td.path());
    mgr.load_durable_mirror(&bad_mirror).unwrap();
    assert_eq!(mgr.checkpoint_count(), 1, "only the valid line loaded");
    assert!(mgr.get_checkpoint(2).is_some());
}

#[test]
fn truncate_rewrites_mirror_and_last_write_wins() {
    let td = tempfile::tempdir().unwrap();
    let mirror = td.path().join(".okra/rewind_points.jsonl");
    let mut mgr = CheckpointManager::new(td.path()).with_durable_mirror(&mirror);
    for idx in 0..3 {
        mgr.begin_prompt(idx);
        mgr.record_operation(idx, "a.txt", Some(b"x"), Some(b"y")).unwrap();
        mgr.finalize_prompt(idx, None, None).unwrap();
    }
    // last-write-wins: re-finalizing prompt 1 updates in place
    mgr.finalize_prompt(1, Some(serde_json::json!({ "hunk": true })), None)
        .unwrap();
    assert!(mgr.get_checkpoint(1).unwrap().hunks.is_some());

    mgr.truncate(1).unwrap();
    assert_eq!(mgr.checkpoint_count(), 1);
    assert!(mgr.get_checkpoint(0).is_some());
    assert!(mgr.get_checkpoint(1).is_none());
    // the mirror was rewritten: reopen sees exactly the truncated set
    let mut reloaded = CheckpointManager::new(td.path());
    reloaded.load_durable_mirror(&mirror).unwrap();
    assert_eq!(reloaded.checkpoint_count(), 1);
}

#[test]
fn git_domain_captures_and_resets() {
    if !git_available() {
        return;
    }
    let (td, repo) = init_repo();
    let mut mgr = CheckpointManager::new(td.path());
    // initial tracked state
    std::fs::write(repo.root().join("tracked.txt"), b"v1").unwrap();
    repo.commit_all("base").unwrap();
    let base_hash = repo.head().unwrap().hash;

    // prompt 0: tracked file rewritten + an untracked file appears
    mgr.begin_prompt(0);
    mgr.record_operation(0, "tracked.txt", Some(b"v1"), Some(b"v2"))
        .unwrap();
    std::fs::write(repo.root().join("tracked.txt"), b"v2").unwrap();
    std::fs::write(repo.root().join("untracked.txt"), b"scratch").unwrap();
    mgr.finalize_prompt(0, None, Some(&repo)).unwrap();

    // restore with the git domain: HEAD reset to the captured hash
    let report = mgr.restore_to(Some(&repo), 0).unwrap();
    assert_eq!(report.git_reset_to.as_deref(), Some(base_hash.as_str()));
    assert_eq!(
        std::fs::read_to_string(repo.root().join("tracked.txt")).unwrap(),
        "v1",
        "reset --hard restored the tracked file"
    );
    assert!(repo.head().unwrap().hash == base_hash);
}

fn init_repo() -> (tempfile::TempDir, GitRepository) {
    let td = tempfile::tempdir().unwrap();
    let repo = GitRepository::init(&td.path().join("repo")).unwrap();
    let config = repo.root().join(".git").join("config");
    let mut cfg = std::fs::read_to_string(&config).unwrap_or_default();
    if !cfg.contains("user.name") {
        cfg.push_str("\n[user]\n\tname = okra-test\n\temail = okra-test@example.com\n");
        std::fs::write(&config, cfg).unwrap();
    }
    (td, repo)
}
