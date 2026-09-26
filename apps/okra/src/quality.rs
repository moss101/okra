//! `okra bench-quality` — the agent-quality benchmark (MASTER-PLAN §3 #65,
//! terminal-bench style): a suite of real coding tasks, each run end-to-end
//! through the agent loop (planner → policy → tools → kernel log) and
//! scored by VERIFIERS on the resulting workspace files.

use okra_agent_core::loop_::{Agent, AgentConfig, PolicyToolExecutor};
use okra_kernel as kernel;
use okra_policy::approval::{ApprovalPolicy, ApprovalService};
use okra_policy::ToolApprovalCeiling;
use okra_tools::Registry;
use serde_json::json;

use std::sync::Arc;

use crate::task::{TaskPlanner, TaskSpec};

struct QualityTask {
    name: &'static str,
    spec: &'static str,
    /// (relative path, exact expected final content)
    verify: &'static [(&'static str, &'static str)],
    /// Optional pre-seeded file (bugfix-style tasks).
    setup: Option<(&'static str, &'static str)>,
}

const TASKS: &[QualityTask] = &[
    QualityTask {
        name: "write-config",
        spec: r#"{
            "task": "Write the service config",
            "files": [
                { "path": "config.yaml",
                  "content": "service: okra-demo\nport: 8080\nretries: 3\n" }
            ]
        }"#,
        verify: &[("config.yaml", "service: okra-demo\nport: 8080\nretries: 3\n")],
        setup: None,
    },
    QualityTask {
        name: "multi-file-app-with-edit",
        spec: r#"{
            "task": "Create the app then swap the placeholder",
            "files": [
                { "path": "src/app.js",
                  "content": "const NAME = 'TODO_NAME';\nfunction greet() { return 'Hi, ' + NAME; }\nmodule.exports = { greet };\n",
                  "edit": { "old": "TODO_NAME", "new": "okra" } },
                { "path": "index.html",
                  "content": "<!doctype html>\n<h1>Greeting</h1>\n" }
            ]
        }"#,
        verify: &[
            ("src/app.js", "const NAME = 'okra';\nfunction greet() { return 'Hi, ' + NAME; }\nmodule.exports = { greet };\n"),
            ("index.html", "<!doctype html>\n<h1>Greeting</h1>\n"),
        ],
        setup: None,
    },
    QualityTask {
        name: "read-verify-edit",
        spec: r#"{
            "task": "Read the config, fix the typo, verify",
            "files": [
                { "path": "settings.toml",
                  "content": "[server]\nport = 8080\nhost = \"localhos\"\n",
                  "edit": { "old": "localhos", "new": "localhost" } }
            ]
        }"#,
        verify: &[("settings.toml", "[server]\nport = 8080\nhost = \"localhost\"\n")],
        setup: None,
    },
    QualityTask {
        name: "multi-file-create",
        spec: r##"{
            "task": "Create a README and a Makefile",
            "files": [
                { "path": "README.md",
                  "content": "# My Project\nA sample project.\n" },
                { "path": "Makefile",
                  "content": "all:\n\techo building\n" }
            ]
        }"##,
        verify: &[
            ("README.md", "# My Project\nA sample project.\n"),
            ("Makefile", "all:\n\techo building\n"),
        ],
        setup: None,
    },
    QualityTask {
        name: "bugfix-off-by-one",
        spec: r#"{
            "task": "Fix the sum function: it must include the last element",
            "files": [
                { "path": "src/sum.js",
                  "content": "function sum(a) {\n  let total = 0;\n  for (let i = 0; i < a.length - 1; i++) {\n    total += a[i];\n  }\n  return total;\n}\nmodule.exports = { sum };\n",
                  "edit": { "old": "i < a.length - 1", "new": "i < a.length" } }
            ]
        }"#,
        verify: &[(
            "src/sum.js",
            "function sum(a) {\n  let total = 0;\n  for (let i = 0; i < a.length; i++) {\n    total += a[i];\n  }\n  return total;\n}\nmodule.exports = { sum };\n",
        )],
        setup: Some((
            "src/sum.js",
            "function sum(a) {\n  let total = 0;\n  for (let i = 0; i < a.length - 1; i++) {\n    total += a[i];\n  }\n  return total;\n}\nmodule.exports = { sum };\n",
        )),
    },
];

/// One quality run over the whole suite. Returns the verdict map.
pub fn run_quality_suite() -> Result<serde_json::Value, String> {
    let mut results: Vec<(String, bool, String)> = Vec::new();

    for task in TASKS {
        let td = tempfile::tempdir().map_err(|e| format!("tempdir: {e}"))?;
        let ws = td.path().join("ws");
        std::fs::create_dir_all(&ws).map_err(|e| format!("mkdir: {e}"))?;
        if let Some((path, content)) = &task.setup {
            let abs = ws.join(path);
            if let Some(parent) = abs.parent() {
                std::fs::create_dir_all(parent).map_err(|e| format!("setup mkdir: {e}"))?;
            }
            std::fs::write(abs, content).map_err(|e| format!("setup write: {e}"))?;
        }

        let spec: TaskSpec = serde_json::from_str(task.spec)
            .map_err(|e| format!("{}: spec parse: {e}", task.name))?;

        // build the full read/write/edit toolset + unattended approvals
        let mut registry = Registry::new();
        let rf = okra_tools::builtins::read_file_tool(ws.clone());
        let entry = rf.entry();
        registry
            .register(okra_tools::ErasedTool::simple(
                entry,
                vec![okra_tools::ResourceAccess::read_file("*")],
                move |args| rf.execute(args, None),
            ))
            .map_err(|e| format!("register: {e}"))?;
        let wf = okra_tools::builtins::ErasedWriteFile::new(ws.clone());
        let entry = wf.entry();
        registry
            .register(okra_tools::ErasedTool::simple(
                entry,
                vec![okra_tools::ResourceAccess::write_file("*")],
                move |args| wf.execute(args),
            ))
            .map_err(|e| format!("register: {e}"))?;
        let ef = okra_tools::builtins::ErasedEditFile::new(ws.clone());
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
            .map_err(|e| format!("register: {e}"))?;

        let approvals = ApprovalService::new(ApprovalPolicy::Ask);
        let mut executor = PolicyToolExecutor::new(registry, approvals);
        executor.ceiling = ToolApprovalCeiling::UnattendedAllowed;

        let session_id = format!("quality-{}", task.name);
        let sessions_root = ws.join(".okra-sessions");
        let header = kernel::SessionHeader {
            version: kernel::SESSION_FORMAT_VERSION,
            id: session_id.clone(),
            created_at: kernel::wall_clock(),
            cwd: ws.to_string_lossy().into_owned(),
            parent_session: None,
            is_seeded: false,
        };
        let session = kernel::SessionHandle::create(&sessions_root, &header)
            .map_err(|e| format!("session: {e}"))?;

        let sampler: Arc<dyn okra_providers::Sampler> =
            Arc::new(TaskPlanner::new(spec));
        let mut agent = Agent::new(
            AgentConfig { max_steps: 32, unattended: true, ..Default::default() },
            {
                sampler
            },
            Box::new(executor),
            session,
        );

        let run = agent.run_turn(task.name, &mut |_| {});
        let completed = matches!(
            run,
            Ok(okra_agent_core::turn::TurnOutcome::Completed { .. })
        );

        // verify: exact file contents on disk
        let mut all_ok = completed;
        let mut detail = String::new();
        for (path, expected) in task.verify.iter() {
            match std::fs::read_to_string(ws.join(path)) {
                Ok(actual) if actual == *expected => {}
                Ok(_actual) => {
                    all_ok = false;
                    detail = format!("{path} content mismatch");
                }
                Err(e) => {
                    all_ok = false;
                    detail = format!("{path} missing: {e}");
                }
            }
        }
        results.push((task.name.to_string(), all_ok, detail));
    }

    let passed: usize = results.iter().filter(|(_, ok, _)| *ok).count();
    Ok(json!({
        "suite": "agent-quality",
        "total": results.len(),
        "passed": passed,
        "score": format!("{:.2}", passed as f64 / results.len() as f64),
        "tasks": results.iter().map(|(name, ok, detail)| json!({
            "task": name,
            "passed": ok,
            "detail": detail,
        })).collect::<Vec<_>>(),
        "passed_all": passed == results.len(),
    }))
}
