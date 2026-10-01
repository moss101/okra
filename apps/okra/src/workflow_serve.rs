//! The workflow→wire link (n0041): a live workflow run projects into the
//! ported `workflowRuns` protocol state (crates/protocol/workflow_runs,
//! N0032) and emits CHANGE-ONLY deltas via `diff_workflow_runs_state` —
//! the exact producer the TS contract was ported for. Deltas broadcast to
//! every attached surface (NDJSON + SSE) as `v4/workflowRuns` frames
//! carrying serialized `ConversationDelta`s; applying them with the
//! protocol's own reducer reconstructs the run state byte-faithfully.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use okra_protocol::{diff_workflow_runs_state, ConversationDelta, WorkflowRunNode, WorkflowRunState, WorkflowRunsState};
use okra_workflow::engine::run_workflow;
use okra_workflow::{JournalEntry, RunBudgets, RunJournal, TeeJournal};

use crate::workflow_cli::TurnStepHost;

/// Folds engine journal entries into the protocol state; each `fold`
/// returns the deltas that express the change (empty when nothing
/// material happened).
pub struct WorkflowProjector {
    state: WorkflowRunsState,
    run_id: String,
}

impl WorkflowProjector {
    pub fn new(run_id: &str) -> Self {
        WorkflowProjector {
            state: WorkflowRunsState { revision: 0, runs: Vec::new() },
            run_id: run_id.to_string(),
        }
    }

    /// Header pairs EXCLUDING runId/status — those live in the typed
    /// fields (the port's invariant: apply/diff route them out of patches,
    /// and canonical emission derives them from WORKFLOW_RUN_KEYS).
    fn header(&self, nodes_used: u64, last_seq: u64) -> Vec<(String, serde_json::Value)> {
        vec![
            ("usage".to_string(), serde_json::json!({ "spentTokens": 0, "nodesUsed": nodes_used })),
            ("lastEventSequence".to_string(), serde_json::json!(last_seq)),
        ]
    }

    fn node(step: u64, name: &str, phase: &str, extra: Option<(&str, &str)>) -> WorkflowRunNode {
        let mut fields = serde_json::Map::new();
        fields.insert("kind".into(), serde_json::json!("step"));
        fields.insert("phase".into(), serde_json::json!(phase));
        fields.insert("name".into(), serde_json::json!(name));
        if let Some((k, v)) = extra {
            fields.insert(k.to_string(), serde_json::json!(v));
        }
        WorkflowRunNode { site_id: "engine".into(), ordinal: step, extra: fields }
    }

    fn run_at(&self, status: &str, nodes: Vec<WorkflowRunNode>, last_seq: u64) -> WorkflowRunState {
        WorkflowRunState {
            run_id: self.run_id.clone(),
            status: status.to_string(),
            header: self.header(nodes.len() as u64, last_seq),
            actors: Vec::new(),
            nodes,
        }
    }

    fn current(&self) -> (String, Vec<WorkflowRunNode>, u64) {
        match self.state.runs.first() {
            Some(run) => {
                let nodes = run.nodes.clone();
                let seq = run
                    .header_value("lastEventSequence")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0);
                (run.status.clone(), nodes, seq)
            }
            None => ("pending".into(), Vec::new(), 0),
        }
    }

    /// Advance the state for one journal entry; the returned deltas are
    /// what the wire should carry.
    pub fn fold(&mut self, entry: &JournalEntry) -> Vec<ConversationDelta> {
        let seq = entry.seq;
        let next = match entry.event.as_str() {
            "run/started" => self.run_at("running", Vec::new(), seq),
            "step/started" => {
                let (status, mut nodes, _) = self.current();
                let step = entry.data["step"].as_u64().unwrap_or(nodes.len() as u64 + 1);
                let name = entry.data["name"].as_str().unwrap_or("step");
                nodes.retain(|n| n.ordinal != step);
                nodes.push(Self::node(step, name, "running", None));
                self.run_at(&status, nodes, seq)
            }
            "step/finished" => {
                let (status, mut nodes, _) = self.current();
                let step = entry.data["step"].as_u64().unwrap_or(nodes.len() as u64);
                let name = entry.data["name"].as_str().unwrap_or("step");
                let ok = entry.data["ok"].as_bool().unwrap_or(false);
                let extra = if ok { None } else { Some(("error", entry.data["error"].as_str().unwrap_or("failed"))) };
                nodes.retain(|n| n.ordinal != step);
                nodes.push(Self::node(step, name, if ok { "completed" } else { "failed" }, extra));
                self.run_at(&status, nodes, seq)
            }
            "run/completed" => {
                let (_, nodes, _) = self.current();
                self.run_at("completed", nodes, seq)
            }
            "run/failed" => {
                let (_, nodes, _) = self.current();
                self.run_at("failed", nodes, seq)
            }
            "run/cancelled" => {
                let (_, nodes, _) = self.current();
                self.run_at("cancelled", nodes, seq)
            }
            // engine/ready + others carry no material state change
            _ => return Vec::new(),
        };
        let prior = std::mem::replace(&mut self.state, WorkflowRunsState { revision: 0, runs: Vec::new() });
        self.state = WorkflowRunsState { revision: prior.revision + 1, runs: vec![next] };
        diff_workflow_runs_state(Some(&prior), &self.state)
    }
}

/// Run one workflow over the daemon: validate first (errors return the
/// report), then engine + tee journal + live projection, deltas broadcast
/// as they happen. Returns the accepted run id (the caller responds
/// immediately; the terminal delta carries the final status).
pub fn spawn_workflow_run(
    state: &Arc<crate::serve_tcp::TcpServeState>,
    script: &str,
    max_steps: u32,
    provider: Option<(String, String)>,
) -> Result<String, okra_workflow::validate::ValidationReport> {
    let report = okra_workflow::validate::validate(script);
    if report.has_errors() {
        return Err(report);
    }
    let run_id = format!("wf-{}", crate::serve::uuid_v4());
    let state2 = Arc::clone(state);
    let script = script.to_string();
    let run_id_thread = run_id.clone();
    std::thread::spawn(move || {
        let b = Arc::clone(&state2);
        let broadcast = Arc::new(move |m: &str, p: serde_json::Value| {
            let v = serde_json::json!({ "method": m, "params": p });
            let line = serde_json::to_vec(&v).unwrap_or_default();
            b.broadcast_bytes(&line);
        });
        let cwd = state2.cwd.clone();
        let journal = RunJournal::new(cwd.join(".okra").join("workflows"));
        let projector = Rc::new(RefCell::new(WorkflowProjector::new(&run_id_thread)));
        let topic = format!("workflow/{run_id_thread}");
        let sink_projector = Rc::clone(&projector);
        let sink_broadcast = Arc::clone(&broadcast);
        let sink_topic = topic.clone();
        let tee = TeeJournal::new(
            journal,
            Rc::new(move |entry: &JournalEntry| {
                let deltas = sink_projector.borrow_mut().fold(entry);
                if !deltas.is_empty() {
                    sink_broadcast(
                        "v4/workflowRuns",
                        serde_json::json!({ "topic": sink_topic, "deltas": deltas }),
                    );
                }
            }),
        );
                let (host_provider, host_model) = match provider {
            Some((p, m)) => (Some(p), Some(m)),
            None => (None, None),
        };
        let host = TurnStepHost::new(cwd, host_provider, host_model);
        let budgets = RunBudgets { max_steps, ..Default::default() };
        let _ = run_workflow(&script, host, Rc::new(tee), &run_id_thread, &budgets, None);
    });
    Ok(run_id)
}
