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

// ---------------------------------------------------------------------------
// Repo-aware file reads (row #52: read-git-file-binary, gh-pr-revision-file)
// ---------------------------------------------------------------------------

use okra_host::git::{FileSource, GhRunner, GitError, MAX_GIT_FILE_BYTES};
use std::path::Path;

fn commit_file(repo: &GitRepository, rel: &str, contents: &[u8], message: &str) -> String {
    let path = repo.root().join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, contents).unwrap();
    repo.commit_all(message).unwrap()
}

fn init_repo_with_file() -> (tempfile::TempDir, GitRepository) {
    let td = tempfile::tempdir().unwrap();
    let repo = GitRepository::init(&td.path().join("repo")).unwrap();
    std::fs::write(
        repo.root().join(".gitconfig"),
        b"",
    )
    .unwrap_or_default();
    let _ = std::fs::remove_file(repo.root().join(".gitconfig"));
    // silence git identity failures for commit_all in sandboxes
    let config = repo.root().join(".git").join("config");
    let mut cfg = std::fs::read_to_string(&config).unwrap_or_default();
    if !cfg.contains("user.name") {
        cfg.push_str(
            "\n[user]\n\tname = okra-test\n\temail = okra-test@example.com\n",
        );
        std::fs::write(&config, cfg).unwrap();
    }
    commit_file(&repo, "src/lib.rs", b"v1 contents", "first");
    (td, repo)
}

#[test]
fn reads_working_tree_head_and_refs() {
    if !git_available() { return; }
    let (_td, repo) = init_repo_with_file();

    // working tree has v1
    let wt = repo
        .read_file("src/lib.rs", &FileSource::WorkingTree, MAX_GIT_FILE_BYTES)
        .unwrap();
    assert_eq!(wt, b"v1 contents");

    // edit the working tree: WorkingTree sees v2, HEAD still sees v1
    std::fs::write(repo.root().join("src/lib.rs"), b"v2 contents").unwrap();
    let wt = repo
        .read_file("src/lib.rs", &FileSource::WorkingTree, MAX_GIT_FILE_BYTES)
        .unwrap();
    assert_eq!(wt, b"v2 contents");
    let head = repo
        .read_file("src/lib.rs", &FileSource::Head, MAX_GIT_FILE_BYTES)
        .unwrap();
    assert_eq!(head, b"v1 contents");

    // explicit ref: the first commit hash
    let hash = repo.commit_all("second");
    let at_ref = repo
        .read_file("src/lib.rs", &FileSource::Ref("HEAD~1".into()), MAX_GIT_FILE_BYTES)
        .unwrap();
    assert_eq!(at_ref, b"v1 contents");
    let _ = hash;
}

#[test]
fn binary_reads_are_byte_exact() {
    if !git_available() { return; }
    let (_td, repo) = init_repo_with_file();
    let mut bytes = vec![0xFF, 0xFE, 0x00, 0x80];
    bytes.extend_from_slice("café ☕".as_bytes());
    bytes.push(0x00);
    commit_file(&repo, "assets/logo.bin", &bytes, "binary");
    let read = repo
        .read_file_binary("HEAD", "assets/logo.bin", MAX_GIT_FILE_BYTES)
        .unwrap();
    assert_eq!(read, bytes, "no UTF-8 lossy corruption");
}

#[test]
fn unsafe_paths_and_refs_are_rejected() {
    if !git_available() { return; }
    let (_td, repo) = init_repo_with_file();
    for bad in ["../escape", "/abs", "a\\b", "", "a/./../b"] {
        assert!(
            matches!(
                repo.read_file_binary("HEAD", bad, 1024),
                Err(GitError::UnsafePath(_))
            ),
            "{bad}"
        );
    }
    for bad_ref in ["-o/etc/x", "main extra", "a..b", ""] {
        assert!(
            matches!(
                repo.read_file_binary(bad_ref, "src/lib.rs", 1024),
                Err(GitError::UnsafeRef(_))
            ),
            "{bad_ref}"
        );
    }
}

#[test]
fn size_cap_bounds_reads() {
    if !git_available() { return; }
    let (_td, repo) = init_repo_with_file();
    commit_file(&repo, "big.bin", &vec![7u8; 1000], "big");
    assert!(matches!(
        repo.read_file_binary("HEAD", "big.bin", 100),
        Err(GitError::TooLarge { max: 100 })
    ));
    assert_eq!(
        repo.read_file_binary("HEAD", "big.bin", 1000).unwrap().len(),
        1000
    );
}

struct FakeGh {
    head_ref: String,
    fail: bool,
}

impl GhRunner for FakeGh {
    fn pr_head_branch(&self, _repo: &Path, pr: u32) -> Result<String, GitError> {
        if self.fail {
            return Err(GitError::Git(format!("PR {pr} not found")));
        }
        Ok(self.head_ref.clone())
    }
}

#[test]
fn pr_revision_read_uses_origin_ref_of_pr_head() {
    if !git_available() { return; }
    let (_td, repo) = init_repo_with_file();

    // simulate a fetched PR branch: origin/feature-x points at a commit
    // carrying a different revision of the file
    commit_file(&repo, "src/lib.rs", b"pr revision contents", "feature work");
    let head = repo.head().unwrap();
    run_remote_ref(&repo, "origin/feature-x", &head.hash);

    let gh = FakeGh { head_ref: "feature-x".to_string(), fail: false };
    let read = repo
        .read_file_at_pr(&gh, 42, "src/lib.rs", MAX_GIT_FILE_BYTES)
        .unwrap();
    assert_eq!(read, b"pr revision contents");
    // working tree still has v2 from the fixture
    let wt = repo
        .read_file("src/lib.rs", &FileSource::WorkingTree, MAX_GIT_FILE_BYTES)
        .unwrap();
    assert_eq!(wt, b"pr revision contents");
    let _ = wt;

    // unsafe paths are rejected before any gh call
    let gh = FakeGh { head_ref: "feature-x".to_string(), fail: true };
    assert!(matches!(
        repo.read_file_at_pr(&gh, 42, "../escape", 1024),
        Err(GitError::UnsafePath(_))
    ));
}

#[test]
fn pr_revision_read_fetches_when_local_ref_missing() {
    if !git_available() { return; }
    let (_td, repo) = init_repo_with_file();
    // no origin/feature-x ref: the read must fail with the fetch error,
    // not silently fall back to the working tree or HEAD
    let gh = FakeGh { head_ref: "no-such-branch".to_string(), fail: false };
    let e = repo
        .read_file_at_pr(&gh, 7, "src/lib.rs", MAX_GIT_FILE_BYTES)
        .unwrap_err();
    assert!(e.to_string().contains("fetch") || matches!(e, GitError::Git(_)), "{e}");
}

/// Create a fake fetched remote ref without network: point
/// `refs/remotes/origin/<name>` at a commit.
fn run_remote_ref(repo: &GitRepository, name: &str, hash: &str) {
    std::process::Command::new("git")
        .arg("-C")
        .arg(repo.root())
        .args(["update-ref", &format!("refs/remotes/{name}"), hash])
        .output()
        .expect("update-ref");
}
