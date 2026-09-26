//! Git domain tests (M3 strangler): status, commit, and REAL worktrees —
//! the worktree API backs G5 subagent grants (shared object store, isolated
//! working tree).

use okra_host::git::GitRepository;

fn git_available() -> bool {
    std::process::Command::new("git")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[test]
fn init_status_commit_roundtrip() {
    if !git_available() { return; }
    let td = tempfile::tempdir().unwrap();
    let repo = GitRepository::init(&td.path().join("repo")).unwrap();
    std::fs::write(repo.root().join("a.txt"), b"v1").unwrap();

    assert!(repo.is_dirty().unwrap(), "untracked file = dirty");
    let entries = repo.status().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].path, "a.txt");

    let hash = repo.commit_all("first").unwrap();
    assert_eq!(hash.len(), 40);
    assert!(!repo.is_dirty().unwrap(), "clean after commit");
    let head = repo.head().unwrap();
    assert_eq!(head.hash, hash);
}

#[test]
fn worktrees_are_isolated_but_shared() {
    if !git_available() { return; }
    let td = tempfile::tempdir().unwrap();
    let repo = GitRepository::init(&td.path().join("repo")).unwrap();
    std::fs::write(repo.root().join("base.txt"), b"base").unwrap();
    repo.commit_all("base").unwrap();

    let wt_path = td.path().join("worktree-sub");
    repo.worktree_add("subagent-1", &wt_path).unwrap();
    let listed = repo.worktree_list().unwrap();
    assert_eq!(listed.len(), 2, "main + subagent worktree");

    // the worktree is a REAL working tree on a different branch
    let wt_repo = GitRepository::open(&wt_path).unwrap();
    let head = wt_repo.head().unwrap();
    assert_eq!(head.branch, "subagent-1");
    let root_canon = wt_repo.root().canonicalize().unwrap();
    let wt_canon = wt_path.canonicalize().unwrap();
    assert!(root_canon.starts_with(wt_canon), "own directory");

    // writes inside the worktree do not touch the main tree
    std::fs::write(wt_path.join("sub.txt"), b"only in worktree").unwrap();
    assert!(wt_repo.is_dirty().unwrap());
    assert!(!repo.is_dirty().unwrap(), "main tree untouched");

    wt_repo.commit_all("sub work").unwrap();
    let wt_path_str = wt_path.to_path_buf();
    repo.worktree_remove(&wt_path_str).unwrap();
    assert_eq!(repo.worktree_list().unwrap().len(), 1);
    assert!(!wt_path_str.exists());
}

#[test]
fn non_repo_fails_closed() {
    let td = tempfile::tempdir().unwrap();
    assert!(matches!(
        GitRepository::open(td.path()),
        Err(okra_host::git::GitError::NotARepository(_) | okra_host::git::GitError::Git(_)) | Err(okra_host::git::GitError::Io(_))
    ));
}
