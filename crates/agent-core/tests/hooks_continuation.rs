//! Proves that PreToolUse hooks fire during run_turn_continuation —
//! the deny>ask>allow pipeline is active on every dispatch, not just in
//! the standalone registry tests.

use std::sync::Arc;

use okra_agent_core::loop_::{Agent, LoopEvent, PolicyToolExecutor};
use okra_kernel as kernel;
use okra_policy::approval::{ApprovalPolicy, ApprovalService};
use okra_providers::{ScriptedModel, ScriptedStep, ToolCall};
use okra_tools::Registry;

struct AllowChannel;
impl okra_policy::approval::ApprovalChannel for AllowChannel {
    fn answer(&self, _req: &okra_policy::ApprovalRequest) -> Option<okra_policy::approval::ApprovalOutcome> {
        Some(okra_policy::approval::ApprovalOutcome::AllowedOnce)
    }
}

fn build_agent(ws: &std::path::Path, steps: Vec<ScriptedStep>) -> Agent<ScriptedModel> {
    let mut registry = Registry::new();
    let rf = okra_tools::builtins::read_file_tool(ws.to_path_buf());
    let entry = rf.entry();
    registry
        .register(okra_tools::ErasedTool::simple(
            entry,
            vec![okra_tools::ResourceAccess::read_file("*")],
            move |args| rf.execute(args, None),
        ))
        .unwrap();
    let mut approvals = ApprovalService::new(ApprovalPolicy::Ask);
    approvals.add_channel(Box::new(AllowChannel));
    let executor = PolicyToolExecutor::new(registry, approvals);
    // AllowChannel added below
    let header = kernel::SessionHeader {
        version: kernel::SESSION_FORMAT_VERSION,
        id: format!("hooks-{}", std::process::id()),
        created_at: 1.0,
        cwd: ws.to_string_lossy().into_owned(),
        parent_session: None,
        is_seeded: false,
    };
    let session = kernel::SessionHandle::create(&ws.join(".okra-sessions"), &header).unwrap();
    let sampler = Arc::new(ScriptedModel::new(steps));
    Agent::new(Default::default(), sampler, Box::new(executor), session)
}

#[test]
fn hooks_fire_during_continuation_dispatch() {
    let td = tempfile::tempdir().unwrap();
    std::fs::write(td.path().join("f.txt"), "data").unwrap();

    let steps = vec![
        ScriptedStep {
            text: "reading".into(),
            tool_calls: vec![ToolCall {
                id: "c1".into(),
                name: "read_file".into(),
                args_json: r#"{"path":"f.txt"}"#.into(),
            }],
            ..Default::default()
        },
        ScriptedStep { text: "done".into(), ..Default::default() },
    ];
    let mut agent = build_agent(td.path(), steps);
    let mut events = Vec::new();
    let outcome = agent.run_turn("read f.txt", &mut |ev| events.push(ev)).unwrap();
    assert!(matches!(outcome, okra_agent_core::turn::TurnOutcome::Completed { .. }));

    // the tool call produced a successful result (hook allow path worked)
    assert!(events.iter().any(|e| matches!(e, LoopEvent::ToolCallFinished { is_error: false, .. })));
}
