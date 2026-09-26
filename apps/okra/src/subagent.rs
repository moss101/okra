//! `okra run-subagent` — G5: a subagent run inside an isolated worktree
//! (fs-copy grant) with **kernel-enforced** confinement via nono
//! (MASTER-PLAN §4 G5: "enforced by sandbox, not policy").
//!
//! Sequence:
//! 1. kernel probe — apply nono self-confinement with the grant as the only
//!    writable surface, then physically attempt a write inside (must work)
//!    and outside (must be denied by the kernel, EPERM);
//! 2. run the coding task through the full agent loop;
//! 3. report the verdict. Even a policy-bypassing write would hit the
//!    kernel wall — the enforcement is not in the tool's path check.

use okra_agent_core::loop_::{Agent, AgentConfig, PolicyToolExecutor};
use okra_kernel as kernel;
use okra_policy::approval::{ApprovalPolicy, ApprovalService};
use okra_policy::{NonoSandboxBackend, SandboxExecutionPolicy, SandboxMode, SelfConfinement};
use okra_providers::Sampler;
use okra_tools::Registry;
use serde_json::json;
use std::sync::Arc;

use crate::task::{TaskPlanner, TaskSpec};

pub fn run_subagent(grant: &std::path::Path, spec_path: &std::path::Path) -> Result<serde_json::Value, String> {
    if !grant.is_dir() {
        return Err(format!("grant dir {} is not a directory", grant.display()));
    }
    let spec_raw = std::fs::read_to_string(spec_path)
        .map_err(|e| format!("read task spec: {e}"))?;
    let spec: TaskSpec = serde_json::from_str(&spec_raw)
        .map_err(|e| format!("task spec parse: {e}"))?;

    let sessions_root = grant.join(".okra-sessions");
    std::fs::create_dir_all(&sessions_root)
        .map_err(|e| format!("create sessions dir: {e}"))?;
    let mut extra_writable = vec![sessions_root.clone()];

    // ---- 1. kernel probe (BEFORE the irreversible apply we can only plan;
    //      after apply we OBSERVE the verdicts) ----
    let outside_probe = std::env::temp_dir().join(format!(
        "okra-subagent-escape-{}",
        std::process::id()
    ));

    // ---- 2. apply kernel confinement: grant is the only writable surface
    //      (plus the session log dir) ----
    let policy = SandboxExecutionPolicy {
        mode: SandboxMode::WorkspaceWrite,
        workspace_root: grant.to_path_buf(),
        session_id: Some("subagent".into()),
    };
    let backend = NonoSandboxBackend::new();
    let report = backend
        .apply_to_self(&policy, &extra_writable)
        .map_err(|e| format!("kernel confinement failed (fail closed): {e}"))?;

    // ---- 3. run the task ----
    let mut registry = Registry::new();
    let rf = okra_tools::builtins::read_file_tool(grant.to_path_buf());
    let entry = rf.entry();
    registry
        .register(okra_tools::ErasedTool::simple(
            entry,
            vec![okra_tools::ResourceAccess::read_file("*")],
            move |args| rf.execute(args, None),
        ))
        .map_err(|e| format!("register read_file: {e}"))?;
    let wf = okra_tools::builtins::ErasedWriteFile::new(grant.to_path_buf());
    let entry = wf.entry();
    registry
        .register(okra_tools::ErasedTool::simple(
            entry,
            vec![okra_tools::ResourceAccess::write_file("*")],
            move |args| wf.execute(args),
        ))
        .map_err(|e| format!("register write_file: {e}"))?;
    let ef = okra_tools::builtins::ErasedEditFile::new(grant.to_path_buf());
    let entry = ef.entry();
    registry
        .register(okra_tools::ErasedTool::simple(
            entry,
            vec![okra_tools::ResourceAccess::file(
                okra_tools::FileAccessOperation::Readwrite,
                "*",
            )],
            move |args| ef.execute(args),
        ))
        .map_err(|e| format!("register edit_file: {e}"))?;

    let approvals = ApprovalService::new(ApprovalPolicy::Ask);
    let mut executor = PolicyToolExecutor::new(registry, approvals);
    executor.ceiling = okra_policy::ToolApprovalCeiling::UnattendedAllowed;

    let header = kernel::SessionHeader {
        version: kernel::SESSION_FORMAT_VERSION,
        id: "subagent".into(),
        created_at: kernel::wall_clock(),
        cwd: grant.to_string_lossy().into_owned(),
        parent_session: None,
        is_seeded: false,
    };
    let session = match kernel::SessionHandle::open(
        &sessions_root,
        "subagent",
        kernel::SessionAccess::Write,
    ) {
        Ok(h) => h,
        Err(kernel::HandleError::NotFound(_)) => kernel::SessionHandle::create(&sessions_root, &header)
            .map_err(|e| format!("create session: {e}"))?,
        Err(e) => return Err(format!("open session: {e}")),
    };

    let sampler: Arc<dyn Sampler> = Arc::new(TaskPlanner::new(spec));
    let mut agent = Agent::new(
        AgentConfig { max_steps: 32, unattended: true, ..Default::default() },
        sampler,
        Box::new(executor),
        session,
    );
    let outcome = agent.run_turn("execute the task inside your grant", &mut |_| {})?;
    let task_completed = matches!(
        outcome,
        okra_agent_core::turn::TurnOutcome::Completed { .. }
    );

    // ---- 4. verify the deliverables inside the grant ----
    let spec: TaskSpec = serde_json::from_str(&spec_raw).map_err(|e| format!("{e}"))?;
    let mut files_ok = true;
    for f in &spec.files {
        let (path, expected) = (&f.path, TaskSpec::final_content(f));
        // paths that ESCAPE the grant were policy-refused by design — the
        // kernel probe below proves the backstop. Skip them here.
        let candidate = grant.join(path);
        let escapes = path.starts_with("..")
            || okra_tools::pipeline::confine_lexical(grant, &candidate)
                .map(|lex| !lex.starts_with(grant))
                .unwrap_or(true);
        if escapes {
            continue;
        }
        match std::fs::read_to_string(grant.join(path)) {
            Ok(actual) if actual == *expected => {}
            _ => files_ok = false,
        }
    }

    // ---- 5. kernel verdict: write inside grant (allowed) and outside
    //      (denied by the KERNEL — the process is confined) ----
    let write_inside = std::fs::write(grant.join(".kernel-probe"), b"probe").is_ok();
    let write_outside = std::fs::write(&outside_probe, b"escape").is_ok();
    if write_outside {
        let _ = std::fs::remove_file(&outside_probe); // clean the accident
    }
    extra_writable.clear();

    let passed = task_completed && files_ok && write_inside && !write_outside;
    Ok(json!({
        "grant": grant.to_string_lossy(),
        "enforcement": format!("{:?}", report.enforcement),
        "platform": report.platform,
        "task_completed": task_completed,
        "files_verified": files_ok,
        "kernel_write_inside_grant": if write_inside { "ok" } else { "failed" },
        "kernel_write_outside_grant": if write_outside { "ALLOWED" } else { "denied" },
        "passed": passed,
    }))
}

// ---------------------------------------------------------------------------
// Full G5 loop: launcher (real git worktree grant) → confined child run →
// collect_work for parent review. MASTER-PLAN §4 G5 end-to-end.
// ---------------------------------------------------------------------------

/// Orchestrate one subagent run against a parent repository:
/// 1. launch — `git worktree add` the child's grant (isolated working
///    tree, shared object store), policy = parent ∩ role, inherit-nothing
///    context;
/// 2. run — the confined `run-subagent` child process applies nono
///    self-confinement with the worktree as its only writable surface and
///    executes the task;
/// 3. collect — commit inside the worktree branch and return the hash for
///    parent review; the parent checkout is never touched.
///
/// The task spec is copied into the worktree before the child runs (the
/// child's world is the worktree), and child runtime artifacts
/// (`.okra-sessions/`, the spec copy) are removed before the work commit.
pub fn orchestrate_subagent(
    repo_path: &std::path::Path,
    name: &str,
    worktree_path: &std::path::Path,
    role: okra_host::subagent::RoleScope,
    task: &str,
    spec_path: &std::path::Path,
) -> Result<serde_json::Value, String> {
    use okra_host::subagent::{SubagentGrant, SubagentLauncher};
    use std::process::Command;

    let repo = okra_host::git::GitRepository::open(repo_path)
        .map_err(|e| format!("open parent repo: {e}"))?;
    // the launcher holds the parent's grants but never passes them down
    let launcher = SubagentLauncher::new(repo.clone(), vec!["parent-session-grant".into()]);
    let grant: SubagentGrant = launcher
        .launch(name, worktree_path, &role, task)
        .map_err(|e| format!("launch: {e}"))?;

    // stage the task spec INSIDE the worktree (the child's world)
    let spec_copy = worktree_path.join("task.json");
    std::fs::copy(spec_path, &spec_copy)
        .map_err(|e| format!("stage task spec: {e}"))?;

    // confined child run: real process, kernel self-confinement inside
    let bin = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("okra")))
        .unwrap_or_else(|| std::path::PathBuf::from("okra"));
    // sanctioned site: the orchestrator spawns the confined child runner
    // itself (the child then re-confines to the worktree); arguments are
    // host-built paths, never model text
    #[allow(clippy::disallowed_methods)]
    let output = Command::new(&bin)
        .args([
            "run-subagent",
            "--grant",
            &worktree_path.to_string_lossy(),
            "--task",
            &spec_copy.to_string_lossy(),
        ])
        .output()
        .map_err(|e| format!("spawn child: {e}"))?;
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let child_line = stdout
        .lines()
        .find(|l| l.starts_with("SUBAGENT "))
        .ok_or_else(|| {
            format!(
                "child produced no SUBAGENT verdict (exit {:?}): {}",
                output.status.code(),
                String::from_utf8_lossy(&output.stderr)
            )
        })?;
    let child: serde_json::Value =
        serde_json::from_str(child_line.trim_start_matches("SUBAGENT "))
            .map_err(|e| format!("child verdict parse: {e}"))?;

    // collect: drop runtime artifacts, commit the child's work on the branch
    let _ = std::fs::remove_dir_all(worktree_path.join(".okra-sessions"));
    let _ = std::fs::remove_file(&spec_copy);
    // only commit if the child actually produced something
    std::fs::remove_file(worktree_path.join(".kernel-probe")).ok();
    let dirty = okra_host::git::GitRepository::open(worktree_path)
        .and_then(|r| r.is_dirty())
        .unwrap_or(false);
    let commit = if dirty {
        SubagentLauncher::collect_work(&grant, "subagent work").ok()
    } else {
        None
    };

    // parent checkout untouched?
    let parent_repo = okra_host::git::GitRepository::open(repo_path)
        .map_err(|e| format!("reopen parent repo: {e}"))?;
    let parent_clean = !parent_repo.is_dirty().unwrap_or(false);
    let parent_sentinel = std::fs::read_to_string(repo_path.join("sentinel.txt")).ok();

    launcher.cleanup(&grant).map_err(|e| format!("cleanup: {e}"))?;

    let passed = child["passed"] == serde_json::Value::Bool(true) && parent_clean;
    Ok(serde_json::json!({
        "orchestration": "g5-full-loop",
        "branch": grant.branch,
        "worktree": worktree_path.to_string_lossy(),
        "child": child,
        "commit": commit,
        "parent_clean": parent_clean,
        "parent_sentinel": parent_sentinel,
        "passed": passed,
    }))
}
