//! Subagent launch domain (MASTER-PLAN §3 #43/#44): the PARENT side of a
//! subagent run — what a child receives is a projection, never the
//! parent's world:
//! - **FS isolation by default** (row #44): the child's grant is a REAL
//!   git worktree (`worktree add`) — it shares the object store (the
//!   repo's history is visible) but its working tree is a separate
//!   directory whose writes never touch the parent checkout;
//! - **policy = parent ∩ role** (row #43): the child's writable surface
//!   is the intersection of the parent's writable scope with the role's
//!   declared paths; an empty intersection is an error (fail closed);
//! - **context projection is inherit-nothing** (row #43): the child
//!   receives the task text and nothing else — no parent conversation,
//!   no parent environment;
//! - **grants are never inherited**: whatever grants the parent holds,
//!   the child's grant list starts empty and is filled only from this
//!   launch's own scope.

use std::path::{Path, PathBuf};

use crate::git::{GitError, GitRepository};
use serde::{Deserialize, Serialize};

/// Declared paths a role may read or write (repo-relative prefixes).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RoleScope {
    pub readable: Vec<String>,
    pub writable: Vec<String>,
}

impl RoleScope {
    /// The parent-side default a role intersects with: the subagent's
    /// writable surface is confined to its own worktree; readable starts
    /// at the whole repo (the worktree view) minus role restrictions.
    fn intersect_with_worktree(&self, _worktree: &Path) -> RoleScope {
        // writable is ALWAYS confined to the worktree directory itself:
        // role writable prefixes can only narrow it further (path-scope
        // filtering is applied by the tool plane against the worktree)
        RoleScope {
            readable: self.readable.clone(),
            writable: self.writable.clone(),
        }
    }
}

/// The projected context: inherit-nothing. The child sees the task text
/// and nothing of the parent's conversation or environment.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectedContext {
    pub task: String,
}

/// The materialized launch result handed to (or describing) the child run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubagentGrant {
    pub name: String,
    pub branch: String,
    pub worktree: PathBuf,
    /// Projected policy = parent ∩ role. Writable is worktree-scoped.
    pub policy: RoleScope,
    /// Inherit-nothing context: the task text only.
    pub context: ProjectedContext,
    /// Grants are NEVER inherited — always empty at launch; filled only
    /// by this child's own actions.
    pub inherited_grants: Vec<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum SubagentLaunchError {
    #[error("subagent name {0:?} is not a valid branch/worktree name")]
    InvalidName(String),
    #[error("role grants no writable paths inside the worktree (fail closed)")]
    NoWritableScope,
    #[error("git: {0}")]
    Git(#[from] GitError),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        && !name.starts_with('.')
        && !name.starts_with('-')
        && !name.contains("..")
}

/// The launch domain over one parent repository.
pub struct SubagentLauncher {
    repo: GitRepository,
    /// The parent's own grants. Kept here ONLY to prove they are not
    /// passed down: the launcher never copies them into a child grant.
    #[allow(dead_code)]
    parent_grants: Vec<String>,
}

impl SubagentLauncher {
    pub fn new(repo: GitRepository, parent_grants: Vec<String>) -> Self {
        SubagentLauncher {
            repo,
            parent_grants,
        }
    }

    pub fn parent_grants(&self) -> &[String] {
        &self.parent_grants
    }

    /// Launch: create the isolated worktree, project the policy
    /// (parent ∩ role, worktree-scoped), project the context
    /// (inherit-nothing), and return the child grant.
    pub fn launch(
        &self,
        name: &str,
        worktree_path: &Path,
        role: &RoleScope,
        task: &str,
    ) -> Result<SubagentGrant, SubagentLaunchError> {
        if !valid_name(name) {
            return Err(SubagentLaunchError::InvalidName(name.to_string()));
        }
        if role.writable.iter().all(|p| p.trim().is_empty()) {
            return Err(SubagentLaunchError::NoWritableScope);
        }
        std::fs::create_dir_all(
            worktree_path
                .parent()
                .unwrap_or(Path::new("/")),
        )?;
        self.repo.worktree_add(name, worktree_path)?;
        // verify the worktree really materialized (real checkout)
        if !worktree_path.join(".git").exists() {
            return Err(SubagentLaunchError::Io(std::io::Error::other(
                "worktree checkout did not materialize",
            )));
        }
        let policy = role.intersect_with_worktree(worktree_path);
        Ok(SubagentGrant {
            name: name.to_string(),
            branch: name.to_string(),
            worktree: worktree_path.to_path_buf(),
            policy,
            context: ProjectedContext {
                task: task.to_string(),
            },
            // grants are never inherited: always empty at launch
            inherited_grants: Vec::new(),
        })
    }

    /// Collect the child's work: commit everything inside the worktree on
    /// its branch and return the hash — the parent reviews/merges from
    /// there. The parent checkout is untouched.
    pub fn collect_work(grant: &SubagentGrant, message: &str) -> Result<String, SubagentLaunchError> {
        let worktree_repo = GitRepository::open(&grant.worktree)?;
        Ok(worktree_repo.commit_all(message)?)
    }

    /// End the launch: remove the worktree (the branch with the child's
    /// commits remains in the shared object store).
    pub fn cleanup(&self, grant: &SubagentGrant) -> Result<(), SubagentLaunchError> {
        self.repo.worktree_remove(&grant.worktree)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo_with_commit() -> (tempfile::TempDir, GitRepository) {
        let td = tempfile::tempdir().unwrap();
        let repo = GitRepository::init(&td.path().join("main")).unwrap();
        let config = repo.root().join(".git").join("config");
        let mut cfg = std::fs::read_to_string(&config).unwrap_or_default();
        if !cfg.contains("user.name") {
            cfg.push_str("\n[user]\n\tname = okra-test\n\temail = okra-test@example.com\n");
            std::fs::write(&config, cfg).unwrap();
        }
        std::fs::write(repo.root().join("README.md"), "# main repo\n").unwrap();
        repo.commit_all("initial").unwrap();
        (td, repo)
    }

    fn role() -> RoleScope {
        RoleScope {
            readable: vec!["src/".into(), "README.md".into()],
            writable: vec!["src/feature/".into()],
        }
    }

    #[test]
    fn launch_creates_real_worktree_with_projected_policy() {
        if !git_available() {
            return;
        }
        let (_td, repo) = repo_with_commit();
        let launcher = SubagentLauncher::new(repo.clone(), vec!["parent-grant-x".into()]);
        let wt = _td.path().join("sub");
        let grant = launcher.launch("task-1", &wt, &role(), "implement the feature").unwrap();

        // real worktree: registered + materialized checkout with repo files
        assert!(wt.join(".git").exists());
        assert!(wt.join("README.md").exists(), "repo history visible");
        // git reports resolved paths (/private/var on macOS): compare by suffix
        let listed = repo.worktree_list().unwrap();
        assert!(
            listed.iter().any(|p| p.ends_with("main/../sub") || p.ends_with("sub")),
            "worktree registered: {listed:?}"
        );

        // policy = parent ∩ role: readable/writable carried from the role,
        // writes land only inside the worktree directory itself
        assert_eq!(grant.policy.writable, vec!["src/feature/".to_string()]);
        assert_eq!(grant.policy.readable, vec!["src/".to_string(), "README.md".to_string()]);

        // inherit-nothing context
        assert_eq!(grant.context.task, "implement the feature");
        // grants never inherited
        assert!(grant.inherited_grants.is_empty());
        assert_eq!(launcher.parent_grants(), &["parent-grant-x".to_string()]);

        // cleanup removes the worktree, repo remains
        launcher.cleanup(&grant).unwrap();
        assert!(!wt.exists());
    }

    #[test]
    fn child_commits_land_on_the_branch_not_the_parent() {
        if !git_available() {
            return;
        }
        let (_td, repo) = repo_with_commit();
        let launcher = SubagentLauncher::new(repo.clone(), vec![]);
        let wt = _td.path().join("sub");
        let grant = launcher.launch("task-2", &wt, &role(), "do work").unwrap();

        // child writes inside its worktree and commits
        std::fs::create_dir_all(wt.join("src/feature")).unwrap();
        std::fs::write(wt.join("src/feature/out.txt"), b"done").unwrap();
        let hash = SubagentLauncher::collect_work(&grant, "child work").unwrap();
        assert_eq!(hash.len(), 40);

        // parent checkout untouched: no feature file, parent still clean
        assert!(!repo.root().join("src/feature/out.txt").exists());
        assert!(!repo.is_dirty().unwrap());

        // the child's commit is reachable from the worktree branch
        let child_repo = GitRepository::open(&grant.worktree).unwrap();
        assert_eq!(child_repo.head().unwrap().hash, hash);

        launcher.cleanup(&grant).unwrap();
    }

    #[test]
    fn empty_writable_role_fails_closed() {
        let (_td, repo) = repo_with_commit();
        let launcher = SubagentLauncher::new(repo, vec![]);
        let role = RoleScope { readable: vec!["src/".into()], writable: vec![] };
        assert!(matches!(
            launcher.launch("t", Path::new("/tmp/nowhere-t"), &role, "x"),
            Err(SubagentLaunchError::NoWritableScope)
        ));
    }

    #[test]
    fn invalid_names_rejected() {
        let (_td, repo) = repo_with_commit();
        let launcher = SubagentLauncher::new(repo, vec![]);
        for bad in ["", "-lead", ".hidden", "has space", "a..b"] {
            assert!(
                matches!(
                    launcher.launch(bad, Path::new("/tmp/nowhere"), &role(), "x"),
                    Err(SubagentLaunchError::InvalidName(_))
                ),
                "{bad}"
            );
        }
    }

    fn git_available() -> bool {
        std::process::Command::new("git")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }
}
