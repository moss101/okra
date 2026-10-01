//! The Rhai workflow engine (MASTER-PLAN §3 #41; grok `xai-workflow`
//! analog, contract-faithful rather than vendored per decision N0004 —
//! the extraction was rejected for the same dependency-web reasons as the
//! sandbox).
//!
//! Contract:
//! - A workflow script is Rhai defining `fn run()` (or `fn main()`).
//! - The ONLY host effect is `step(name, input)` — every step goes through
//!   the [`WorkflowHost`], which owns execution (subagent run, agent turn,
//!   anything) and its own policy. The engine never touches the world.
//! - Every step appends to the run journal (`step/started`,
//!   `step/finished`) before and after the effect — durable truth first,
//!   the same rule as the kernel log.
//! - Budgets are enforced per step AND per Rhai statement (`on_progress`):
//!   steps, wall clock, and the stop flag abort the run as `cancelled`
//!   (a budget stop is a cancellation with a named breach, never a
//!   failure — the same honesty rule as backstopped turns).
//! - A failing step fails the run unless the script catches it itself
//!   (Rhai `try/catch`); the engine does not retry.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use rhai::{Dynamic, Engine, EvalAltResult, Scope};

use crate::{check_budgets, BudgetBreach, JournalEntry, RunBudgets, RunJournalSink, RunStatus, WorkflowRun};

/// Step execution seam. `step` receives the script's name + input and
/// returns the step's output; an Err fails the run.
pub trait WorkflowHost {
    fn step(&mut self, name: &str, input: Dynamic) -> Result<Dynamic, String>;
}

/// Why a run ended early. Budget stops and user stops are CANCELLATIONS
/// (honest, reported), not failures. The marker strings cross the Rhai
/// error boundary — the only place the engine is stringly typed, kept to
/// these three closed forms.
enum Abort {
    Cancelled(Option<BudgetBreach>),
}

impl Abort {
    fn marker(&self) -> String {
        match self {
            Abort::Cancelled(Some(BudgetBreach::Steps { used, max })) => {
                format!("OKRA_BUDGET:steps:{used}:{max}")
            }
            Abort::Cancelled(Some(BudgetBreach::FanOut { requested, max })) => {
                format!("OKRA_BUDGET:fan_out:{requested}:{max}")
            }
            Abort::Cancelled(Some(BudgetBreach::WallClock { elapsed_ms, max_ms })) => {
                format!("OKRA_BUDGET:wall_clock:{elapsed_ms}:{max_ms}")
            }
            Abort::Cancelled(None) => "OKRA_CANCEL".to_string(),
        }
    }
    /// Stable human form for the run's `error` field.
    fn reason(&self) -> String {
        match self {
            Abort::Cancelled(Some(BudgetBreach::Steps { used, max })) => {
                format!("budget breach: steps (used {used} > max {max})")
            }
            Abort::Cancelled(Some(BudgetBreach::FanOut { requested, max })) => {
                format!("budget breach: fan-out (requested {requested} > max {max})")
            }
            Abort::Cancelled(Some(BudgetBreach::WallClock { elapsed_ms, max_ms })) => {
                format!("budget breach: wall clock (elapsed {elapsed_ms}ms > max {max_ms}ms)")
            }
            Abort::Cancelled(None) => "stopped".to_string(),
        }
    }
    fn from_marker(text: &str) -> Option<Abort> {
        if text == "OKRA_CANCEL" {
            return Some(Abort::Cancelled(None));
        }
        let rest = text.strip_prefix("OKRA_BUDGET:")?;
        let parts: Vec<&str> = rest.split(':').collect();
        match parts.as_slice() {
            ["steps", used, max] => Some(Abort::Cancelled(Some(BudgetBreach::Steps {
                used: used.parse().ok()?,
                max: max.parse().ok()?,
            }))),
            ["fan_out", requested, max] => Some(Abort::Cancelled(Some(BudgetBreach::FanOut {
                requested: requested.parse().ok()?,
                max: max.parse().ok()?,
            }))),
            ["wall_clock", elapsed_ms, max_ms] => {
                Some(Abort::Cancelled(Some(BudgetBreach::WallClock {
                    elapsed_ms: elapsed_ms.parse().ok()?,
                    max_ms: max_ms.parse().ok()?,
                })))
            }
            _ => None,
        }
    }
    fn into_rhai(self) -> Box<EvalAltResult> {
        Box::new(EvalAltResult::ErrorRuntime(self.marker().into(), rhai::Position::NONE))
    }
}

struct RunState {
    seq: u64,
    steps: u32,
}

fn journal_entry(journal: &dyn RunJournalSink, state: &Rc<RefCell<RunState>>, run_id: &str, event: &str, data: serde_json::Value) {
    let entry = {
        let mut st = state.borrow_mut();
        st.seq += 1;
        JournalEntry {
            seq: st.seq,
            run_id: run_id.to_string(),
            event: event.to_string(),
            at_ms: crate::now_ms(),
            data,
        }
    };
    let _ = journal.append(&entry);
}

/// Run one workflow script to a terminal status, journaling as it goes.
/// The sequential engine keeps exactly one step in flight, so the fan-out
/// budget is enforced at 1 (the seam is live; a parallel engine raises it
/// without touching the contract).
pub fn run_workflow<H: WorkflowHost + 'static>(
    script: &str,
    host: H,
    journal: std::rc::Rc<dyn RunJournalSink>,
    run_id: &str,
    budgets: &RunBudgets,
    stop: Option<&Arc<AtomicBool>>,
) -> WorkflowRun {
    let started = Instant::now();
    let state = Rc::new(RefCell::new(RunState { seq: 0, steps: 0 }));
    let host = Rc::new(RefCell::new(host));
    let run_id = run_id.to_string();
    let budgets = *budgets;
    let stop = stop.cloned();

    let mut run = WorkflowRun {
        run_id: run_id.clone(),
        status: RunStatus::Running,
        budgets,
        error: None,
    };
    journal_entry(&*journal, &state, &run_id, "run/started", serde_json::json!({
        "budgets": budgets,
    }));

    let mut engine = Engine::new();
    // our budgets own termination (wall clock rides every statement via
    // on_progress), so the engine's own operations cap is disabled rather
    // than racing ours to a different verdict
    engine.set_max_operations(0);
    engine.set_max_call_levels(64);

    // wall-clock + stop checks ride every statement — budgets must hold
    // inside pure-Rhai loops too, not only between steps. rhai's
    // on_progress early-exits by returning Some(value); the reason rides
    // the shared `aborted` flag (single-threaded engine, no race).
    let aborted = Rc::new(RefCell::new(None::<Abort>));
    {
        let stop = stop.clone();
        let state = Rc::clone(&state);
        let journal = Rc::clone(&journal);
        let run_id = run_id.clone();
        let aborted = Rc::clone(&aborted);
        engine.on_progress(move |_ops| {
            let abort = if let Some(flag) = stop.as_ref()
                && flag.load(Ordering::Relaxed)
            {
                Some(Abort::Cancelled(None))
            } else if let Some(BudgetBreach::WallClock { .. }) =
                check_budgets(&budgets, 0, 1, started.elapsed().as_millis() as u64)
            {
                Some(Abort::Cancelled(Some(BudgetBreach::WallClock {
                    elapsed_ms: started.elapsed().as_millis() as u64,
                    max_ms: budgets.max_wall_clock_ms,
                })))
            } else {
                None
            };
            if let Some(abort) = abort {
                *aborted.borrow_mut() = Some(abort);
                return Some(Dynamic::UNIT);
            }
            let _ = (&state, &journal, &run_id);
            None
        });
    }

    {
        let state = Rc::clone(&state);
        let host = Rc::clone(&host);
        let journal = Rc::clone(&journal);
        let run_id = run_id.clone();
        let aborted = Rc::clone(&aborted);
        engine.register_fn("step", move |name: &str, input: Dynamic| {
            let (steps_now, breach) = {
                let mut st = state.borrow_mut();
                st.steps += 1;
                let steps = st.steps;
                (steps, check_budgets(&budgets, steps, 1, started.elapsed().as_millis() as u64))
            };
            if let Some(flag) = stop.as_ref()
                && flag.load(Ordering::Relaxed)
            {
                *aborted.borrow_mut() = Some(Abort::Cancelled(None));
                return Err(Abort::Cancelled(None).into_rhai());
            }
            if let Some(breach) = breach {
                *aborted.borrow_mut() = Some(Abort::Cancelled(Some(breach)));
                return Err(Abort::Cancelled(Some(breach)).into_rhai());
            }
            journal_entry(&*journal, &state, &run_id, "step/started", serde_json::json!({
                "name": name,
                "step": steps_now,
            }));
            match host.borrow_mut().step(name, input) {
                Ok(value) => {
                    let preview = truncate_2048(&value.to_string());
                    journal_entry(&*journal, &state, &run_id, "step/finished", serde_json::json!({
                        "name": name,
                        "step": steps_now,
                        "ok": true,
                        "output": preview,
                    }));
                    Ok(value)
                }
                Err(e) => {
                    journal_entry(&*journal, &state, &run_id, "step/finished", serde_json::json!({
                        "name": name,
                        "step": steps_now,
                        "ok": false,
                        "error": truncate_2048(&e),
                    }));
                    Err(format!("step `{name}` failed: {e}").into())
                }
            }
        });
    }

    let ast = match engine.compile(script) {
        Ok(ast) => ast,
        Err(e) => {
            run.status = RunStatus::Failed;
            run.error = Some(truncate_2048(&e.to_string()));
            journal_entry(&*journal, &state, &run_id, "run/failed", serde_json::json!({
                "stage": "compile",
                "error": run.error,
            }));
            return run;
        }
    };

    let entry = if ast.iter_functions().any(|f| f.name == "run") { "run" } else { "main" };
    let mut scope = Scope::new();
    let result = engine.call_fn::<Dynamic>(&mut scope, &ast, entry, ());

    let elapsed = started.elapsed().as_millis() as u64;
    // the abort flag wins over the returned value: a script's try/catch
    // may swallow the step-level error, but it cannot un-flag the abort
    // (and the next step/statement re-flags it anyway)
    let early_abort = aborted.borrow_mut().take();
    match (result, early_abort) {
        (_, Some(abort)) => {
            run.status = RunStatus::Cancelled;
            run.error = Some(abort.reason());
            journal_entry(&*journal, &state, &run_id, "run/cancelled", serde_json::json!({
                "steps": state.borrow().steps,
                "elapsedMs": elapsed,
                "reason": run.error,
            }));
        }
        (Ok(value), None) => {
            run.status = RunStatus::Completed;
            journal_entry(&*journal, &state, &run_id, "run/completed", serde_json::json!({
                "steps": state.borrow().steps,
                "elapsedMs": elapsed,
                "result": truncate_2048(&value.to_string()),
            }));
        }
        (Err(err), None) => {
            let text = err.to_string();
            if let Some(abort) = Abort::from_marker(&text) {
                run.status = RunStatus::Cancelled;
                run.error = Some(abort.reason());
                journal_entry(&*journal, &state, &run_id, "run/cancelled", serde_json::json!({
                    "steps": state.borrow().steps,
                    "elapsedMs": elapsed,
                    "reason": run.error,
                }));
            } else {
                run.status = RunStatus::Failed;
                run.error = Some(truncate_2048(&text));
                journal_entry(&*journal, &state, &run_id, "run/failed", serde_json::json!({
                    "steps": state.borrow().steps,
                    "elapsedMs": elapsed,
                    "error": run.error,
                }));
            }
        }
    }
    run
}

/// Display cap matching the workflow-runs wire budget (2048).
fn truncate_2048(text: &str) -> String {
    if text.len() <= 2048 {
        return text.to_string();
    }
    let mut cut = 2048;
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}…", &text[..cut])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::RunJournal;

    struct EchoHost;
    impl WorkflowHost for EchoHost {
        fn step(&mut self, name: &str, input: Dynamic) -> Result<Dynamic, String> {
            Ok(format!("{name}:{}", input).into())
        }
    }

    struct FailingHost;
    impl WorkflowHost for FailingHost {
        fn step(&mut self, _name: &str, _input: Dynamic) -> Result<Dynamic, String> {
            Err("boom".into())
        }
    }

    #[test]
    fn runs_the_entry_function_and_journals_every_step() {
        let td = tempfile::tempdir().unwrap();
        let journal = RunJournal::new(td.path().join("j"));
        let script = r#"
            fn run() {
                let a = step("first", 1);
                let b = step("second", a);
                b
            }
        "#;
        let run = run_workflow(script, EchoHost, Rc::new(journal.clone()), "r1", &RunBudgets::default(), None);
        assert_eq!(run.status, RunStatus::Completed);
        let entries = journal.read("r1").unwrap();
        let events: Vec<&str> = entries.iter().map(|e| e.event.as_str()).collect();
        assert!(events.contains(&"run/started"));
        assert!(events.contains(&"step/started"));
        assert!(events.contains(&"step/finished"));
        assert_eq!(events.last().copied(), Some("run/completed"));
        let completed = entries.last().cloned().unwrap();
        assert_eq!(completed.data["steps"], serde_json::json!(2));
        assert_eq!(completed.data["result"], serde_json::json!("second:first:1"));
    }

    #[test]
    fn a_failing_step_fails_the_run() {
        let td = tempfile::tempdir().unwrap();
        let journal = RunJournal::new(td.path().join("j"));
        let script = r#"fn run() { step("explode", 0) }"#;
        let run = run_workflow(script, FailingHost, Rc::new(journal.clone()), "r2", &RunBudgets::default(), None);
        assert_eq!(run.status, RunStatus::Failed);
        assert!(run.error.unwrap().contains("explode"));
        let entries = journal.read("r2").unwrap();
        assert_eq!(entries.last().unwrap().event, "run/failed");
    }

    #[test]
    fn step_budget_cancels_not_fails() {
        let td = tempfile::tempdir().unwrap();
        let journal = RunJournal::new(td.path().join("j"));
        let script = r#"fn run() { for i in 0..10 { step("s" + i, i); } }"#;
        let budgets = RunBudgets { max_steps: 3, ..Default::default() };
        let run = run_workflow(script, EchoHost, Rc::new(journal.clone()), "r3", &budgets, None);
        assert_eq!(run.status, RunStatus::Cancelled, "{:?}", run.error);
        assert!(run.error.unwrap().contains("steps"));
        let entries = journal.read("r3").unwrap();
        assert_eq!(entries.last().unwrap().event, "run/cancelled");
    }

    #[test]
    fn wall_clock_cancels_inside_pure_loops() {
        let td = tempfile::tempdir().unwrap();
        let journal = RunJournal::new(td.path().join("j"));
        // no steps at all — the on_progress wall-clock check is the only
        // thing standing between this script and forever
        let script = r#"fn run() { let x = 0; while x >= 0 { x = x + 1; } 0 }"#;
        let budgets = RunBudgets { max_wall_clock_ms: 300, max_steps: 256, max_fan_out: 16 };
        let run = run_workflow(script, EchoHost, Rc::new(journal.clone()), "r4", &budgets, None);
        assert_eq!(run.status, RunStatus::Cancelled, "{:?}", run.error);
        assert!(run.error.unwrap().contains("wall clock"));
    }

    #[test]
    fn stop_flag_cancels() {
        let td = tempfile::tempdir().unwrap();
        let journal = RunJournal::new(td.path().join("j"));
        let flag = Arc::new(AtomicBool::new(false));
        let flag2 = Arc::clone(&flag);
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(50));
            flag2.store(true, Ordering::Relaxed);
        });
        let script = r#"fn run() { let x = 0; while true { x = x + 1; } 0 }"#;
        let run = run_workflow(script, EchoHost, Rc::new(journal.clone()), "r5", &RunBudgets::default(), Some(&flag));
        assert_eq!(run.status, RunStatus::Cancelled);
        assert_eq!(run.error.as_deref(), Some("stopped"));
    }

    #[test]
    fn script_can_catch_a_failing_step_itself() {
        let td = tempfile::tempdir().unwrap();
        let journal = RunJournal::new(td.path().join("j"));
        let script = r#"
            fn run() {
                let recovered = "kept";
                try {
                    step("explode", 0);
                } catch (err) {
                    recovered = "caught";
                }
                recovered
            }
        "#;
        let run = run_workflow(script, FailingHost, Rc::new(journal.clone()), "r6", &RunBudgets::default(), None);
        assert_eq!(run.status, RunStatus::Completed, "{:?}", run.error);
        let entries = journal.read("r6").unwrap();
        let completed = entries.last().cloned().unwrap();
        assert_eq!(completed.data["result"], serde_json::json!("caught"));
    }
}
