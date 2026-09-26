//! Git domain — the M3 strangler's third host domain (MASTER-PLAN §3 #48:
//! "git" among the re-homed domains; the worktree API also backs G5's
//! subagent isolation).
//!
//! Thin, contained wrapper over the `git` CLI (the donor shells out too):
//! - `is_repo` / `open`
//! - `status` — porcelain entries (staged/unstaged/untracked)
//! - `worktree_add` / `worktree_remove` — isolated worktrees for subagent
//!   grants (G5: the grant dir IS a real worktree, not a copy)
//! - `commit_all` — stage everything + commit, returns the hash
//! - `head` — current branch + commit hash
//! - `read_file` / `read_file_binary` — repo-aware file reads
//!   (MASTER-PLAN §3 #52, ChatGPT2 docs/03 §5: `read-git-file-binary`),
//!   from the working tree, HEAD, or any ref, binary-safe with a size cap
//! - `read_file_at_pr` — `gh-pr-revision-file`: resolve a PR's head via
//!   an injectable GitHub CLI runner, then read the file at that
//!   revision
//!
//! Subprocess spawning is contained here (sanctioned site, same pattern as
//! okra-tools `process.rs`): the `git` binary path is host-configured, and
//! arguments never come from the model.

use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitStatusEntry {
    pub code: String,
    pub path: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitHead {
    pub branch: String,
    pub hash: String,
}

#[derive(Debug, thiserror::Error)]
pub enum GitError {
    #[error("not a git repository: {0}")]
    NotARepository(String),
    #[error("git failed: {0}")]
    Git(String),
    #[error("file exceeds the read cap: {max} bytes")]
    TooLarge { max: u64 },
    #[error("unsafe repository path: {0}")]
    UnsafePath(String),
    #[error("unsafe git ref: {0}")]
    UnsafeRef(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

#[allow(clippy::disallowed_methods)] // sanctioned site: host git runner
fn run_git(repo: &Path, args: &[&str]) -> Result<String, GitError> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .map_err(GitError::Io)?;
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    if !output.status.success() {
        return Err(GitError::Git(stderr.trim().to_string()));
    }
    Ok(stdout)
}

/// A git repository handle.
#[derive(Debug, Clone)]
pub struct GitRepository {
    root: PathBuf,
}

impl GitRepository {
    /// Open an existing repository (fails when `path` is not inside one).
    pub fn open(path: &Path) -> Result<GitRepository, GitError> {
        let out = run_git(path, &["rev-parse", "--show-toplevel"])?;
        let root = PathBuf::from(out.trim());
        Ok(GitRepository { root })
    }

    /// `git init` a new repository.
    pub fn init(path: &Path) -> Result<GitRepository, GitError> {
        std::fs::create_dir_all(path)?;
        run_git(path, &["init", "-q"])?;
        Ok(GitRepository { root: path.to_path_buf() })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn head(&self) -> Result<GitHead, GitError> {
        let branch = run_git(&self.root, &["rev-parse", "--abbrev-ref", "HEAD"])?;
        let hash = run_git(&self.root, &["rev-parse", "HEAD"])?;
        Ok(GitHead {
            branch: branch.trim().to_string(),
            hash: hash.trim().to_string(),
        })
    }

    /// Porcelain status entries: `XY path`.
    pub fn status(&self) -> Result<Vec<GitStatusEntry>, GitError> {
        let out = run_git(&self.root, &["status", "--porcelain"])?;
        let mut entries = Vec::new();
        for line in out.lines() {
            if line.len() < 4 {
                continue;
            }
            entries.push(GitStatusEntry {
                code: line[..2].trim().to_string(),
                path: line[3..].trim().to_string(),
            });
        }
        Ok(entries)
    }

    pub fn is_dirty(&self) -> Result<bool, GitError> {
        Ok(!self.status()?.is_empty())
    }

    /// Stage all changes and commit. Returns the new commit hash.
    pub fn commit_all(&self, message: &str) -> Result<String, GitError> {
        run_git(&self.root, &["add", "-A"])?;
        run_git(&self.root, &["commit", "-q", "-m", message])?;
        // `git commit -q` prints nothing: read the hash explicitly
        Ok(run_git(&self.root, &["rev-parse", "HEAD"])?.trim().to_string())
    }

    /// Create an isolated worktree at `path` on a new branch — the G5
    /// subagent grant surface (a REAL worktree, sharing the object store).
    pub fn worktree_add(&self, name: &str, path: &Path) -> Result<(), GitError> {
        run_git(
            &self.root,
            &["worktree", "add", "-q", "-b", name, path.to_str().unwrap_or_default()],
        )?;
        Ok(())
    }

    pub fn worktree_remove(&self, path: &Path) -> Result<(), GitError> {
        run_git(
            &self.root,
            &["worktree", "remove", "--force", path.to_str().unwrap_or_default()],
        )?;
        Ok(())
    }

    pub fn worktree_list(&self) -> Result<Vec<PathBuf>, GitError> {
        let out = run_git(&self.root, &["worktree", "list", "--porcelain"])?;
        let mut paths = Vec::new();
        for line in out.lines() {
            if let Some(p) = line.strip_prefix("worktree ") {
                paths.push(PathBuf::from(p));
            }
        }
        Ok(paths)
    }
}

/// Repo-aware file reads (row #52: `read-git-file-binary` /
/// `gh-pr-revision-file`). Where a file's bytes come from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileSource {
    /// The working tree as it sits on disk.
    WorkingTree,
    /// The HEAD commit.
    Head,
    /// Any revision: branch name, tag, or commit hash.
    Ref(String),
}

/// Default read cap: 8 MiB (okra's own default, matching the safe-read
/// philosophy of a bounded read).
pub const MAX_GIT_FILE_BYTES: u64 = 8 * 1024 * 1024;

/// Validate one repo-relative POSIX path: non-empty, no absolute or
/// `..` components, no backslashes, no drive prefixes.
fn validate_repo_relative_path(path: &str) -> Result<(), GitError> {
    let is_windows_absolute = path.len() >= 2
        && path.as_bytes()[1] == b':'
        && path.as_bytes()[0].is_ascii_alphabetic();
    if path.is_empty()
        || path.starts_with('/')
        || path.contains('\\')
        || is_windows_absolute
        || path.split('/').any(|part| part.is_empty() || part == "." || part == "..")
    {
        return Err(GitError::UnsafePath(path.to_string()));
    }
    Ok(())
}

/// Validate a revision token: a single positional word without option
/// dashes or separators that could turn `show <ref>:<path>` into
/// something else.
fn validate_ref(git_ref: &str) -> Result<(), GitError> {
    if git_ref.is_empty()
        || git_ref.starts_with('-')
        || git_ref.contains(':')
        || git_ref.contains(char::is_whitespace)
        || git_ref.contains("..")
    {
        return Err(GitError::UnsafeRef(git_ref.to_string()));
    }
    Ok(())
}

#[allow(clippy::disallowed_methods)] // sanctioned site: host git runner
fn run_git_bytes(repo: &Path, args: &[&str]) -> Result<Vec<u8>, GitError> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .map_err(GitError::Io)?;
    if !output.status.success() {
        return Err(GitError::Git(
            String::from_utf8_lossy(&output.stderr).trim().to_string(),
        ));
    }
    Ok(output.stdout)
}

impl GitRepository {
    /// `read-git-file-binary`: read a file's exact bytes at a revision —
    /// binary-safe (no UTF-8 lossy conversion), bounded by `max_bytes`.
    pub fn read_file_binary(
        &self,
        git_ref: &str,
        rel_path: &str,
        max_bytes: u64,
    ) -> Result<Vec<u8>, GitError> {
        validate_ref(git_ref)?;
        validate_repo_relative_path(rel_path)?;
        let spec = format!("{git_ref}:{rel_path}");
        let bytes = run_git_bytes(&self.root, &["show", &spec])?;
        if bytes.len() as u64 > max_bytes {
            return Err(GitError::TooLarge { max: max_bytes });
        }
        Ok(bytes)
    }

    /// Read a file from the source the caller asked for — working tree,
    /// HEAD, or an explicit ref.
    pub fn read_file(
        &self,
        rel_path: &str,
        source: &FileSource,
        max_bytes: u64,
    ) -> Result<Vec<u8>, GitError> {
        match source {
            FileSource::WorkingTree => {
                validate_repo_relative_path(rel_path)?;
                let bytes = std::fs::read(self.root.join(rel_path))?;
                if bytes.len() as u64 > max_bytes {
                    return Err(GitError::TooLarge { max: max_bytes });
                }
                Ok(bytes)
            }
            FileSource::Head => self.read_file_binary("HEAD", rel_path, max_bytes),
            FileSource::Ref(git_ref) => {
                self.read_file_binary(git_ref, rel_path, max_bytes)
            }
        }
    }

    /// `gh-pr-revision-file`: read a file as of a PR's revision. The PR's
    /// head branch is resolved through the injectable GitHub CLI runner;
    /// the file is read at `origin/<head-branch>` (fetching on demand via
    /// the runner when the local ref is missing).
    pub fn read_file_at_pr(
        &self,
        gh: &dyn GhRunner,
        pr: u32,
        rel_path: &str,
        max_bytes: u64,
    ) -> Result<Vec<u8>, GitError> {
        validate_repo_relative_path(rel_path)?;
        let head_branch = gh.pr_head_branch(self.root(), pr)?;
        let remote_ref = format!("origin/{head_branch}");
        match self.read_file_binary(&remote_ref, rel_path, max_bytes) {
            Ok(bytes) => Ok(bytes),
            Err(GitError::Git(_)) => {
                // local ref missing or stale: fetch just that branch
                run_git(&self.root, &["fetch", "-q", "origin", &head_branch])?;
                self.read_file_binary(&remote_ref, rel_path, max_bytes)
            }
            Err(e) => Err(e),
        }
    }
}

/// The GitHub CLI seam (`gh`) — injectable so PR reads are testable
/// without network or credentials.
pub trait GhRunner: Send + Sync {
    /// `gh pr view <pr> --json headRefName` in the repo; returns the
    /// PR's head branch name.
    fn pr_head_branch(&self, repo: &Path, pr: u32) -> Result<String, GitError>;
}

/// The real runner: shells out to `gh` (sanctioned site).
pub struct RealGh;

impl GhRunner for RealGh {
    #[allow(clippy::disallowed_methods)] // sanctioned site: host gh runner
    fn pr_head_branch(&self, repo: &Path, pr: u32) -> Result<String, GitError> {
        let output = Command::new("gh")
            .current_dir(repo)
            .args([
                "pr",
                "view",
                &pr.to_string(),
                "--json",
                "headRefName",
                "-q",
                ".headRefName",
            ])
            .output()
            .map_err(GitError::Io)?;
        if !output.status.success() {
            return Err(GitError::Git(
                String::from_utf8_lossy(&output.stderr).trim().to_string(),
            ));
        }
        let branch = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if branch.is_empty() {
            return Err(GitError::Git(format!("PR {pr} has no head branch")));
        }
        if validate_ref(&branch).is_err() {
            return Err(GitError::UnsafeRef(branch));
        }
        Ok(branch)
    }
}
