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
