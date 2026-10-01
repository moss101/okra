//! `okra workflow run SCRIPT` — the M3 workflow engine over the real
//! agent: every Rhai `step(name, input)` executes as a FULL okra turn
//! (own kernel session, own policy ceiling, own continuation context) in
//! a child process; budgets + the run journal are enforced by the engine
//! (crates/workflow, MASTER-PLAN §3 #41).
//!
//! A step is a process, not a function call, on purpose: the child gets
//! its own sandbox/argv confinement and its own crash domain, and the
//! workflow survives a step that dies. This module is the app's sanctioned
//! spawn site (same pattern as host git.rs — the day-1 ban targets tool
//! paths, and this IS the deliberate seam).

use std::path::PathBuf;

use okra_workflow::engine::{run_workflow, WorkflowHost};
use okra_workflow::{RunBudgets, RunJournal, RunStatus};
use rhai::Dynamic;

/// Step host: re-exec the okra binary per step (`okra --cwd DIR --json
/// PROMPT`, optional `--provider/--model` passthrough) and return the
/// assistant text from the NDJSON stream.
pub struct TurnStepHost {
    exe: PathBuf,
    cwd: PathBuf,
    provider: Option<String>,
    model: Option<String>,
}

impl TurnStepHost {
    pub fn new(cwd: PathBuf, provider: Option<String>, model: Option<String>) -> Self {
        TurnStepHost {
            exe: std::env::current_exe().unwrap_or_else(|_| PathBuf::from("okra")),
            cwd,
            provider,
            model,
        }
    }
}

impl WorkflowHost for TurnStepHost {
    #[allow(clippy::disallowed_methods)] // sanctioned site: the step IS a full okra turn
    fn step(&mut self, name: &str, input: Dynamic) -> Result<Dynamic, String> {
        let prompt = input.to_string();
        if prompt.trim().is_empty() {
            return Err(format!("step `{name}`: empty prompt"));
        }
        let mut cmd = std::process::Command::new(&self.exe);
        cmd.arg("--cwd").arg(&self.cwd).arg("--json").arg(&prompt);
        if let Some(provider) = &self.provider {
            cmd.arg("--provider").arg(provider);
        }
        if let Some(model) = &self.model {
            cmd.arg("--model").arg(model);
        }
        let output = cmd.output().map_err(|e| format!("spawn step turn: {e}"))?;
        if !output.status.success() {
            return Err(format!(
                "step turn exited {}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        // NDJSON LoopEvents carry tag "event" (snake_case); the assistant
        // text is the concatenation of text_delta payloads
        let mut text = String::new();
        for line in String::from_utf8_lossy(&output.stdout).lines() {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(line)
                && v["event"] == "text_delta"
                && let Some(delta) = v["text"].as_str()
            {
                text.push_str(delta);
            }
        }
        if text.trim().is_empty() {
            return Err(format!("step `{name}`: turn produced no assistant text"));
        }
        Ok(text.into())
    }
}

pub fn run_workflow_cli(
    script_path: &std::path::Path,
    cwd: PathBuf,
    provider: Option<String>,
    model: Option<String>,
    budgets: RunBudgets,
) -> Result<i32, String> {
    let script = std::fs::read_to_string(script_path)
        .map_err(|e| format!("read {}: {e}", script_path.display()))?;
    let journal = RunJournal::new(cwd.join(".okra").join("workflows"));
    let run_id = format!(
        "{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0)
    );
    let run = run_workflow(
        &script,
        TurnStepHost::new(cwd, provider, model),
        &journal,
        &run_id,
        &budgets,
        None,
    );
    let summary = serde_json::json!({
        "runId": run.run_id,
        "status": run.status,
        "stepsBudget": run.budgets.max_steps,
        "error": run.error,
    });
    println!("WORKFLOW {}", serde_json::to_string(&summary).unwrap_or_default());
    Ok(match run.status {
        RunStatus::Completed => 0,
        RunStatus::Cancelled => 130,
        _ => 1,
    })
}
