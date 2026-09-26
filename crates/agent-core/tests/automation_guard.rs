//! Automation self-mutation guard end-to-end (MASTER-PLAN §3 #40, from
//! ZCode `automationToolPolicy.ts`): cron-scheduled turns deny Cron* but
//! explicitly allow `offpeak_create`; idle-dispatch turns deny
//! `offpeak_create` only. Denials fire pre-dispatch — before hooks,
//! lattice, or the approval service can run.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use okra_agent_core::loop_::{Agent, AgentConfig, LoopEvent, PolicyToolExecutor, ToolExecutor};
use okra_agent_core::turn::CompletedStop;
use okra_policy::approval::{ApprovalChannel, ApprovalOutcome, ApprovalPolicy, ApprovalService};
use okra_providers::{ScriptedModel, ScriptedStep, ToolCall};
use okra_tools::spec::{RiskLevel, SideEffectScope, ToolEntry, ToolMetadata, ToolSpec};
use okra_tools::{ErasedTool, Registry, ResourceAccess};
use serde_json::json;

/// Approval channel that counts prompts — the guard must deny without
/// ever reaching the approval service.
struct CountingChannel(Arc<AtomicU64>);
impl ApprovalChannel for CountingChannel {
    fn answer(&self, _req: &okra_policy::ApprovalRequest) -> Option<ApprovalOutcome> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Some(ApprovalOutcome::AllowedOnce)
    }
}

fn stub_tool(name: &str) -> ErasedTool {
    let entry = ToolEntry {
        spec: ToolSpec {
            name: name.to_string(),
            namespace: None,
            title: Some(name.to_string()),
            description: format!("{name} stub"),
            arguments_schema: Some(json!({ "type": "object" })),
            kind: Some("automation".into()),
            behavior_version: Some("1".into()),
            idempotent: false,
            read_only: false,
            timeout_ms: None,
            max_concurrency: None,
        },
        metadata: ToolMetadata {
            read_only: false,
            destructive: false,
            concurrent_safe: false,
            needs_approval: true,
            side_effect_scope: SideEffectScope::External,
            risk_level: RiskLevel::Medium,
            timeout_ms: None,
            max_output_bytes: None,
            allowed_in_plan_mode: Some(false),
            stop_turn_on_success: None,
            provider_visible: true,
        },
    };
    let out = format!("{name} ok");
    ErasedTool::simple(
        entry,
        vec![ResourceAccess::file(
            okra_tools::FileAccessOperation::Write,
            "*",
        )],
        move |_| {
            okra_tools::ToolStream::terminal_only(Ok(okra_tools::ToolOutput::text(
                out.clone(),
            )))
        },
    )
}

fn make_executor(turn: okra_agent_core::tasks::TurnDispatch) -> (PolicyToolExecutor, Arc<AtomicU64>) {
    let mut registry = Registry::new();
    for name in ["cron_create", "cron_delete", "offpeak_create", "offpeak_list"] {
        registry.register(stub_tool(name)).unwrap();
    }
    let prompts = Arc::new(AtomicU64::new(0));
    let mut approvals = ApprovalService::new(ApprovalPolicy::Ask);
    approvals.add_channel(Box::new(CountingChannel(Arc::clone(&prompts))));
    let mut executor = PolicyToolExecutor::new(registry, approvals);
    executor.turn_dispatch = turn;
    (executor, prompts)
}

fn call(name: &str) -> ToolCall {
    ToolCall {
        id: format!("c-{name}"),
        name: name.to_string(),
        args_json: "{}".into(),
    }
}

#[test]
fn scheduled_turn_denies_cron_mutation_pre_dispatch() {
    let (mut ex, prompts) = make_executor(okra_agent_core::tasks::TurnDispatch::CronScheduled);
    for tool in ["cron_create", "cron_delete"] {
        let (out, is_error, noop) = ex.execute(&call(tool), &mut |_| {}).unwrap();
        assert!(is_error, "{tool} must be an error on a cron turn");
        assert!(out.contains("automation self-mutation guard"), "{out}");
        assert!(!noop);
    }
    // denied pre-dispatch: no approval prompt, no side-effect tool ran
    assert_eq!(prompts.load(Ordering::SeqCst), 0);
    let counts: Vec<(String, u64)> = ex.execution_counts();
    assert!(counts.iter().all(|(n, c)| *c == 0 || n.starts_with("cron_")));
}

#[test]
fn scheduled_turn_allows_offpeak_create_but_idle_turn_denies_it() {
    // cron turn: OffPeakCreate explicitly allowed (ZCode's separated lists)
    let (mut ex, prompts) = make_executor(okra_agent_core::tasks::TurnDispatch::CronScheduled);
    let (out, is_error, _) = ex.execute(&call("offpeak_create"), &mut |_| {}).unwrap();
    assert!(!is_error, "offpeak_create must run on a cron turn: {out}");
    assert_eq!(prompts.load(Ordering::SeqCst), 1, "needs_approval path ran");

    // idle turn: OffPeakCreate denied — no recursive idle spawns
    let (mut ex, prompts) = make_executor(okra_agent_core::tasks::TurnDispatch::IdleDispatch);
    let (out, is_error, _) = ex.execute(&call("offpeak_create"), &mut |_| {}).unwrap();
    assert!(is_error);
    assert!(out.contains("automation self-mutation guard"), "{out}");
    assert_eq!(prompts.load(Ordering::SeqCst), 0);
    // read-only offpeak_list stays allowed on an idle turn
    let (out, is_error, _) = ex.execute(&call("offpeak_list"), &mut |_| {}).unwrap();
    assert!(!is_error, "{out}");
}

#[test]
fn ordinary_turn_allows_everything() {
    let (mut ex, prompts) = make_executor(okra_agent_core::tasks::TurnDispatch::Ordinary);
    for tool in ["cron_create", "cron_delete", "offpeak_create", "offpeak_list"] {
        let (out, is_error, _) = ex.execute(&call(tool), &mut |_| {}).unwrap();
        assert!(!is_error, "{tool} must run on an ordinary turn: {out}");
    }
    assert_eq!(prompts.load(Ordering::SeqCst), 4);
}

#[test]
fn guard_deny_flows_through_agent_turn_as_tool_result() {
    let td = tempfile::tempdir().unwrap();
    let (executor, _prompts) =
        make_executor(okra_agent_core::tasks::TurnDispatch::CronScheduled);
    let header = okra_kernel::SessionHeader {
        version: okra_kernel::SESSION_FORMAT_VERSION,
        id: format!("guard-e2e-{}", std::process::id()),
        created_at: 1.0,
        cwd: td.path().to_string_lossy().into_owned(),
        parent_session: None,
        is_seeded: false,
    };
    let session = okra_kernel::SessionHandle::create(&td.path().join(".okra-sessions"), &header)
        .unwrap();
    let sampler = Arc::new(ScriptedModel::new(vec![
        ScriptedStep {
            text: "Rescheduling myself.".into(),
            tool_calls: vec![call("cron_create")],
            ..Default::default()
        },
        ScriptedStep {
            text: "Understood, I cannot.".into(),
            ..Default::default()
        },
    ]));
    let mut agent = Agent::new(
        AgentConfig {
            unattended: true,
            ..Default::default()
        },
        sampler,
        Box::new(executor),
        session,
    );
    let mut events = Vec::new();
    let outcome = agent.run_turn("wake up", &mut |ev| events.push(ev)).unwrap();
    match &outcome {
        okra_agent_core::turn::TurnOutcome::Completed { stop, .. } => {
            assert_eq!(stop, &CompletedStop::EndTurn);
        }
        other => panic!("expected Completed, got {other:?}"),
    }
    let finished = events.iter().any(|ev| matches!(
        ev,
        LoopEvent::ToolCallFinished { name, is_error, output, .. }
            if name == "cron_create" && *is_error && output.contains("automation self-mutation guard")
    ));
    assert!(finished, "the model must see the guard denial: {events:?}");
}
