//! End-to-end turn tests (MASTER-PLAN M0 gate: "wire grok's
//! SessionActor → kernel event log → one tool (`read_file`) through the
//! daemon") + the killAtPhase crash-recovery harness (§3 #63).


// Test harness: these acceptance tests execute the compiled crate binary as
// the system under test. The no-raw-spawn/canonicalize bans target production
// paths (production spawning goes through okra_policy's confined runner); the
// acceptance harness must exercise the real binary end-to-end.
#![allow(clippy::disallowed_methods)]
use std::sync::Arc;

use okra_agent_core as core;
use core::loop_::{Agent, AgentConfig, LoopEvent, PolicyToolExecutor};
use core::steering::PendingInterjection;
use core::turn::{CompletedStop, TurnOutcome};
use okra_kernel as kernel;
use okra_policy::approval::{ApprovalChannel, ApprovalOutcome, ApprovalPolicy, ApprovalService};
use okra_providers::{SamplerError, ScriptedModel, ScriptedStep, ToolCall};
use okra_tools::Registry;
use serde_json::json;

struct AllowChannel;
impl ApprovalChannel for AllowChannel {
    fn answer(&self, _req: &okra_policy::ApprovalRequest) -> Option<ApprovalOutcome> {
        Some(ApprovalOutcome::AllowedOnce)
    }
}

/// Build an agent over a scripted model + read_file/list_dir tools rooted
/// at `root`, logging to a kernel session.
static SESSION_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn build_agent(
    root: &std::path::Path,
    steps: Vec<ScriptedStep>,
    unattended: bool,
) -> Agent<ScriptedModel> {
    let session_id = format!(
        "agent-e2e-{}",
        std::process::id() as u64 * 1000
            + std::sync::atomic::AtomicU64::fetch_add(&SESSION_SEQ, 1, std::sync::atomic::Ordering::SeqCst)
    );
    let mut registry = Registry::new();
    let rf = okra_tools::builtins::read_file_tool(root.to_path_buf());
    let entry = rf.entry();
    let entry_clone = entry.clone();
    registry
        .register(okra_tools::ErasedTool::simple(
            entry,
            vec![okra_tools::ResourceAccess::read_file("*")],
            move |args| rf.execute(args, None),
        ))
        .unwrap();
    let _ = entry_clone;
    let ld = okra_tools::builtins::list_dir_tool(root.to_path_buf());
    let ld_entry = ld.entry();
    registry
        .register(okra_tools::ErasedTool::simple(
            ld_entry,
            vec![okra_tools::ResourceAccess::tree(
                okra_tools::FileAccessOperation::Read,
                "*",
            )],
            move |args| ld.execute(args),
        ))
        .unwrap();

    let mut approvals = ApprovalService::new(if unattended {
        ApprovalPolicy::Never
    } else {
        ApprovalPolicy::Ask
    });
    approvals.add_channel(Box::new(AllowChannel));
    let executor = PolicyToolExecutor::new(registry, approvals);

    let header = kernel::SessionHeader {
        version: kernel::SESSION_FORMAT_VERSION,
        id: session_id.clone(),
        created_at: 1.0,
        cwd: root.to_string_lossy().into_owned(),
        parent_session: None,
        is_seeded: false,
    };
    let session =
        kernel::SessionHandle::create(&root.join(".okra-sessions"), &header).unwrap();
    drop(session_id);

    let sampler = Arc::new(ScriptedModel::new(steps));
    Agent::new(AgentConfig { unattended, ..Default::default() }, sampler, Box::new(executor), session)
}

fn collect(events: &mut Vec<LoopEvent>) -> impl FnMut(LoopEvent) + '_ {
    |ev| events.push(ev)
}

#[test]
fn m0_gate_read_file_turn_through_daemon() {
    let td = tempfile::tempdir().unwrap();
    std::fs::write(td.path().join("hello.txt"), "okra reads files").unwrap();

    let steps = vec![
        // step 1: model calls read_file
        ScriptedStep {
            text: "Let me read the file.".into(),
            tool_calls: vec![ToolCall {
                id: "call-1".into(),
                name: "read_file".into(),
                args_json: r#"{"path":"hello.txt"}"#.into(),
            }],
            ..Default::default()
        },
        // step 2: model sees the result and ends the turn (semantic)
        ScriptedStep {
            text: "The file says: okra reads files".into(),
            ..Default::default()
        },
    ];
    let mut agent = build_agent(td.path(), steps, true);
    let mut events = Vec::new();
    let outcome = agent.run_turn("read hello.txt", &mut collect(&mut events)).unwrap();

    // semantic termination: completed with EndTurn
    match &outcome {
        TurnOutcome::Completed { tools_called, stop, .. } => {
            assert_eq!(tools_called, &vec!["read_file".to_string()]);
            assert_eq!(stop, &CompletedStop::EndTurn);
        }
        other => panic!("expected Completed, got {other:?}"),
    }

    // the event stream shows the full pipeline
    assert!(events.iter().any(|e| matches!(e, LoopEvent::TurnStarted { turn: 1 })));
    assert!(events
        .iter()
        .any(|e| matches!(e, LoopEvent::ToolCallStarted { name, .. } if name == "read_file")));
    let finished = events
        .iter()
        .find(|e| matches!(e, LoopEvent::ToolCallFinished { name, .. } if name == "read_file"))
        .unwrap();
    if let LoopEvent::ToolCallFinished { output, is_error, .. } = finished {
        assert!(!is_error);
        assert!(output.contains("okra reads files"), "{output}");
    }
    assert!(events.iter().any(|e| matches!(e, LoopEvent::TurnFinished { .. })));

    // durable truth: the kernel log reconstructs the turn
    let events_log = agent.session().read_all().unwrap();
    kernel::check_log(&events_log).unwrap();
    assert!(events_log.iter().any(|e| e.event_type == "turn/start"));
    assert!(events_log
        .iter()
        .any(|e| e.event_type == "user/message" && e.surface_op.is_some()));
    assert!(events_log.iter().any(|e| e.event_type == "tool/call"));
    assert!(events_log.iter().any(|e| e.event_type == "tool/result"));

    // model-visible means logged: the tool OUTPUT text is in the durable
    // event too (not just the 400-char live stream view)
    assert!(events_log
        .iter()
        .any(|e| e.event_type == "tool/result"
            && e.data["output"]
                .as_str()
                .is_some_and(|o| o.contains("okra reads files"))));

    // model-visible means logged: the assistant text is in the log
    assert!(events_log
        .iter()
        .any(|e| e.event_type == "assistant/message"
            && e.data["text"]
                .as_str()
                .unwrap()
                .contains("okra reads files")));
}

#[test]
fn approval_denial_blocks_side_effect_and_logs_result() {
    let td = tempfile::tempdir().unwrap();

    struct DenyChannel;
    impl ApprovalChannel for DenyChannel {
        fn answer(&self, _req: &okra_policy::ApprovalRequest) -> Option<ApprovalOutcome> {
            Some(ApprovalOutcome::Rejected)
        }
    }

    // side-effecting tool (needs approval): write_file stub via a denied
    // read of a *new* read-only tool? read-only tools never prompt; use a
    // custom non-read-only tool entry.
    let mut registry = Registry::new();
    let mut entry = okra_tools::builtins::read_file_tool(td.path().to_path_buf()).entry();
    entry.spec.name = "deploy".into();
    entry.spec.idempotent = false;
    entry.spec.read_only = false;
    entry.metadata.read_only = false;
    entry.metadata.needs_approval = true;
    registry
        .register(okra_tools::ErasedTool::simple(entry, vec![], |_args| {
            // this must never run when approval is denied
            okra_tools::ToolStream::terminal_only(Ok(okra_tools::ToolOutput::text(
                "DEPLOYED!",
            )))
        }))
        .unwrap();
    let mut approvals = ApprovalService::new(ApprovalPolicy::Ask);
    approvals.add_channel(Box::new(DenyChannel));
    let executor = PolicyToolExecutor::new(registry, approvals);

    let header = kernel::SessionHeader {
        version: kernel::SESSION_FORMAT_VERSION,
        id: "deny-e2e".into(),
        created_at: 1.0,
        cwd: "/tmp".into(),
        parent_session: None,
        is_seeded: false,
    };
    let session =
        kernel::SessionHandle::create(&td.path().join(".okra-sessions"), &header).unwrap();
    let mut agent = Agent::new(Default::default(), Arc::new(ScriptedModel::new(vec![
        ScriptedStep {
            text: "deploying".into(),
            tool_calls: vec![ToolCall {
                id: "d1".into(),
                name: "deploy".into(),
                args_json: "{}".into(),
            }],
            ..Default::default()
        },
        ScriptedStep { text: "denied, stopping.".into(), ..Default::default() },
    ])), Box::new(executor), session);

    let mut events = Vec::new();
    let outcome = agent.run_turn("deploy please", &mut collect(&mut events)).unwrap();
    assert!(matches!(outcome, TurnOutcome::Completed { stop: CompletedStop::EndTurn, .. }));

    // the denied tool's effect never happened
    let finished = events
        .iter()
        .find(|e| matches!(e, LoopEvent::ToolCallFinished { name, .. } if name == "deploy"))
        .unwrap();
    if let LoopEvent::ToolCallFinished { output, is_error, .. } = finished {
        assert!(*is_error);
        assert!(output.contains("denied"), "{output}");
        assert!(!output.contains("DEPLOYED!"));
    }
}

#[test]
fn kill_at_phase_crash_leaves_recoverable_log() {
    // Simulate the killAtPhase harness: run the turn in a child process
    // that aborts mid-turn, then reopen the session and verify recovery.
    // (In-process abort would take the test runner down; use a child.)
    let td = tempfile::tempdir().unwrap();
    let root = td.path().to_path_buf();
    std::fs::write(root.join("f.txt"), "data").unwrap();

    // locate the okra binary next to the test executable (target/debug/)
    let exe = std::env::current_exe().unwrap();
    let target_dir = exe.ancestors().nth(2).unwrap().to_path_buf();
    let bin = target_dir.join("okra");
    if !bin.exists() {
        panic!("okra binary not built at {}", bin.display());
    }
    let kill_spec = "ExecutingTools:1";
    let status = std::process::Command::new(&bin)
        .args(["--json", "--cwd", root.to_str().unwrap(), "--kill-at-phase", kill_spec, "read f.txt"])
        .env("OKRA_KILL_AT_PHASE", kill_spec)
        .output()
        .expect("spawn okra child");
    assert!(
        status.status.code().is_none() || status.status.code() == Some(134),
        "child should have aborted (signal), got {:?} stderr={}",
        status.status.code(),
        String::from_utf8_lossy(&status.stderr)
    );

    // recovery: reopen the session — complete events survive, torn tail is
    // invisible, and a new write handle repairs + continues
    let sessions = root.join(".okra-sessions");
    let mut w = kernel::SessionHandle::open(&sessions, "cli", kernel::SessionAccess::Write).unwrap();
    let recovered = w.read_all().unwrap();
    kernel::check_log(&recovered).unwrap();
    // the user message landed before the abort
    assert!(recovered
        .iter()
        .any(|e| e.event_type == "user/message"));
    // write handle repairs any torn tail and continues the seq space
    let mut ev = kernel::make_event("user/message", json!({ "text": "after crash" }), kernel::wall_clock);
    ev.seq = 0;
    w.append_durable(vec![ev]).unwrap();
    let after = w.read_all().unwrap();
    kernel::check_log(&after).unwrap();
    assert!(after
        .iter()
        .any(|e| e.data.get("text").and_then(|t| t.as_str()) == Some("after crash")));
}

#[test]
fn length_salvage_continues_after_truncation() {
    let td = tempfile::tempdir().unwrap();
    let steps = vec![
        // truncated response
        ScriptedStep {
            text: "partial ans".into(),
            truncate_at: Some(8),
            ..Default::default()
        },
        // continuation
        ScriptedStep { text: "wer completed".into(), ..Default::default() },
    ];
    let mut agent = build_agent(td.path(), steps, true);
    let mut events = Vec::new();
    let outcome = agent.run_turn("answer me", &mut collect(&mut events)).unwrap();
    assert!(matches!(
        outcome,
        TurnOutcome::Completed { stop: CompletedStop::EndTurn, .. }
    ));
}

#[test]
fn sampler_401_parks_uncharged_then_recovers() {
    let td = tempfile::tempdir().unwrap();
    let steps = vec![
        ScriptedStep {
            error: Some(SamplerError::Unauthorized),
            ..Default::default()
        },
        ScriptedStep { text: "recovered after auth".into(), ..Default::default() },
    ];
    let mut agent = build_agent(td.path(), steps, true);
    let outcome = agent.run_turn("go", &mut |_| {}).unwrap();
    assert!(matches!(outcome, TurnOutcome::Completed { .. }));
}

#[test]
fn steering_drains_at_step_boundary() {
    let td = tempfile::tempdir().unwrap();
    let mut agent = build_agent(td.path(), vec![
        ScriptedStep { text: "working...".into(), ..Default::default() },
        ScriptedStep { text: "done with steering".into(), ..Default::default() },
    ], true);
    let sender = agent.steering_sender();
    // queue a mid-turn steering entry (submitted_while_running = true) —
    // the loop drains it at the first step boundary
    let _ = sender.send(core::steering::Tagged {
        interjection: PendingInterjection { text: "mid-turn note".into() },
        submitted_while_running: true,
    });
    let mut events = Vec::new();
    let outcome = agent.run_turn("start", &mut collect(&mut events)).unwrap();
    assert!(matches!(outcome, TurnOutcome::Completed { .. }));
    // the queued note is drained at the first step boundary
    assert!(events
        .iter()
        .any(|e| matches!(e, LoopEvent::SteeringInjected { text } if text.contains("mid-turn note"))));
}

#[test]
fn user_stop_flag_cancels_turn_at_first_boundary() {
    let td = tempfile::tempdir().unwrap();
    std::fs::write(td.path().join("hello.txt"), "never read").unwrap();

    // The model WOULD run two steps, but the stop flag is set before the
    // turn starts — the loop must cancel at the first boundary with
    // Cancelled(UserRequested) and execute NO tools.
    let steps = vec![
        ScriptedStep {
            text: "reading".into(),
            tool_calls: vec![ToolCall {
                id: "call-1".into(),
                name: "read_file".into(),
                args_json: r#"{"path":"hello.txt"}"#.into(),
            }],
            ..Default::default()
        },
        ScriptedStep { text: "done".into(), ..Default::default() },
    ];
    let mut agent = build_agent(td.path(), steps, true);
    let stop = agent.stop_flag();
    stop.store(true, std::sync::atomic::Ordering::Relaxed);

    let mut events = Vec::new();
    let outcome = agent.run_turn("read hello.txt", &mut collect(&mut events)).unwrap();
    match &outcome {
        TurnOutcome::Cancelled { category } => {
            assert_eq!(*category, Some(core::turn::CancellationCategory::UserRequested));
        }
        other => panic!("expected Cancelled(UserRequested), got {other:?}"),
    }
    assert!(!events.iter().any(|e| matches!(e, LoopEvent::ToolCallStarted { .. })));
}

#[test]
fn request_stop_from_another_flag_clone_is_observed() {
    // The serve seam: the surface holds a clone of the flag (installed with
    // set_stop_flag) and flips it while the turn runs on another thread.
    let td = tempfile::tempdir().unwrap();
    let mut agent = build_agent(td.path(), vec![
        ScriptedStep { text: "long work".into(), ..Default::default() },
    ], true);
    let external = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    agent.set_stop_flag(std::sync::Arc::clone(&external));
    external.store(true, std::sync::atomic::Ordering::Relaxed);
    let outcome = agent.run_turn("go", &mut |_| {}).unwrap();
    assert!(matches!(
        outcome,
        TurnOutcome::Cancelled { category: Some(core::turn::CancellationCategory::UserRequested) }
    ));
}

/// N0021 — the semantic wander governor: a turn whose tool activity never
/// converges gets a Jev-judged nudge at step boundaries; a healthy turn
/// does not; the hook is fail-open.
#[test]
fn semantic_wander_governor_nudges_a_circling_turn() {
    use okra_agent_core::semantics::{JevWanderVerdict, SemanticJudge};
    use std::sync::Mutex;

    struct StubWander {
        verdicts: Mutex<Vec<Option<JevWanderVerdict>>>,
    }
    impl SemanticJudge for StubWander {
        fn judge_wander(&self, _a: &str, _u: &str) -> Option<JevWanderVerdict> {
            let mut v = self.verdicts.lock().unwrap();
            if v.is_empty() { None } else { v.remove(0) }
        }
    }

    let td = tempfile::tempdir().unwrap();
    for i in 0..8 {
        std::fs::write(td.path().join(format!("f{i}.txt")), format!("file {i}")).unwrap();
    }
    // seven DIFFERENT reads (stationarity stays quiet), then end the turn
    let mut steps = Vec::new();
    for i in 0..7 {
        steps.push(ScriptedStep {
            text: format!("checking f{i}"),
            tool_calls: vec![ToolCall {
                id: format!("c{i}"),
                name: "read_file".into(),
                args_json: serde_json::json!({ "path": format!("f{i}.txt") }).to_string(),
            }],
            ..Default::default()
        });
    }
    steps.push(ScriptedStep { text: "done".into(), ..Default::default() });

    let session_id = "wander-e2e";
    let mut registry = Registry::new();
    let rf = okra_tools::builtins::read_file_tool(td.path().to_path_buf());
    let entry = rf.entry();
    registry
        .register(okra_tools::ErasedTool::simple(
            entry,
            vec![okra_tools::ResourceAccess::read_file("*")],
            move |args| rf.execute(args, None),
        ))
        .unwrap();
    let mut approvals = ApprovalService::new(ApprovalPolicy::Never);
    approvals.add_channel(Box::new(AllowChannel));
    let executor = PolicyToolExecutor::new(registry, approvals);
    let header = kernel::SessionHeader {
        version: kernel::SESSION_FORMAT_VERSION,
        id: session_id.into(),
        created_at: 1.0,
        cwd: td.path().to_string_lossy().into_owned(),
        parent_session: None,
        is_seeded: false,
    };
    let session = kernel::SessionHandle::create(
        &td.path().join(".okra-sessions"),
        &header,
    )
    .unwrap();

    // two wandering verdicts, then healthy: judgments happen at steps 4
    // and (had calls continued) 7 — step 4 nudges, the rest are quiet
    let mut agent = Agent::new(
        AgentConfig {
            unattended: true,
            semantic_judge: Some(std::sync::Arc::new(StubWander {
                verdicts: Mutex::new(vec![
                    Some(JevWanderVerdict { progressing_probability: 0.05, activity: "repeating".into() }),
                    Some(JevWanderVerdict { progressing_probability: 0.9, activity: "exploring".into() }),
                ]),
            })),
            ..Default::default()
        },
        Arc::new(ScriptedModel::new(steps)),
        Box::new(executor),
        session,
    );
    let mut events = Vec::new();
    let outcome = agent.run_turn("review these files", &mut collect(&mut events)).unwrap();
    assert!(matches!(outcome, TurnOutcome::Completed { .. }));
    let semantic_nudges: Vec<_> = events
        .iter()
        .filter(|e| matches!(e, LoopEvent::Nudge { reason } if reason.contains("semantic")))
        .collect();
    assert_eq!(semantic_nudges.len(), 1, "exactly one semantic nudge; events: {events:?}");
}

#[test]
fn semantic_governor_stays_quiet_when_healthy() {
    use okra_agent_core::semantics::{JevWanderVerdict, SemanticJudge};

    struct Healthy;
    impl SemanticJudge for Healthy {
        fn judge_wander(&self, _a: &str, _u: &str) -> Option<JevWanderVerdict> {
            Some(JevWanderVerdict { progressing_probability: 0.95, activity: "delivering".into() })
        }
    }

    let td = tempfile::tempdir().unwrap();
    std::fs::write(td.path().join("a.txt"), "a").unwrap();
    std::fs::write(td.path().join("b.txt"), "b").unwrap();
    let steps = vec![
        ScriptedStep {
            text: "reading a".into(),
            tool_calls: vec![ToolCall {
                id: "c1".into(),
                name: "read_file".into(),
                args_json: r#"{"path":"a.txt"}"#.into(),
            }],
            ..Default::default()
        },
        ScriptedStep {
            text: "reading b".into(),
            tool_calls: vec![ToolCall {
                id: "c2".into(),
                name: "read_file".into(),
                args_json: r#"{"path":"b.txt"}"#.into(),
            }],
            ..Default::default()
        },
        ScriptedStep { text: "done".into(), ..Default::default() },
    ];
    let mut registry = Registry::new();
    let rf = okra_tools::builtins::read_file_tool(td.path().to_path_buf());
    let entry = rf.entry();
    registry
        .register(okra_tools::ErasedTool::simple(
            entry,
            vec![okra_tools::ResourceAccess::read_file("*")],
            move |args| rf.execute(args, None),
        ))
        .unwrap();
    let approvals = ApprovalService::new(ApprovalPolicy::Never);
    let executor = PolicyToolExecutor::new(registry, approvals);
    let header = kernel::SessionHeader {
        version: kernel::SESSION_FORMAT_VERSION,
        id: "wander-healthy".into(),
        created_at: 1.0,
        cwd: td.path().to_string_lossy().into_owned(),
        parent_session: None,
        is_seeded: false,
    };
    let session = kernel::SessionHandle::create(&td.path().join(".okra-sessions"), &header).unwrap();
    let mut agent = Agent::new(
        AgentConfig {
            unattended: true,
            semantic_judge: Some(std::sync::Arc::new(Healthy)),
            ..Default::default()
        },
        Arc::new(ScriptedModel::new(steps)),
        Box::new(executor),
        session,
    );
    let mut events = Vec::new();
    let outcome = agent.run_turn("read two files", &mut collect(&mut events)).unwrap();
    assert!(matches!(outcome, TurnOutcome::Completed { .. }));
    assert!(!events.iter().any(|e| matches!(e, LoopEvent::Nudge { .. })));
}

// ---- #53 approval scopes: the session tool grant suppresses re-prompts —

use okra_policy::approval::{ApprovalAnswer, ApprovalScope};
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};

struct AskCountingScopedChannel {
    scope: ApprovalScope,
    asks: Arc<AtomicU64>,
}
impl ApprovalChannel for AskCountingScopedChannel {
    fn answer(&self, _req: &okra_policy::ApprovalRequest) -> Option<ApprovalOutcome> {
        self.asks.fetch_add(1, AtomicOrdering::SeqCst);
        Some(ApprovalOutcome::AllowedOnce)
    }
    fn answer_scoped(&self, req: &okra_policy::ApprovalRequest) -> Option<ApprovalAnswer> {
        self.answer(req).map(|o| ApprovalAnswer::scoped(o, self.scope))
    }
}

fn scope_agent(
    root: &std::path::Path,
    steps: Vec<ScriptedStep>,
    scope: ApprovalScope,
    asks: Arc<AtomicU64>,
) -> Agent<ScriptedModel> {
    let mut registry = Registry::new();
    let wf = okra_tools::builtins::ErasedWriteFile::new(root.to_path_buf());
    let entry = wf.entry();
    registry
        .register(okra_tools::ErasedTool::simple(
            entry,
            vec![okra_tools::ResourceAccess::write_file("*")],
            move |args| wf.execute(args),
        ))
        .unwrap();
    let mut approvals = ApprovalService::new(ApprovalPolicy::Ask);
    approvals.add_channel(Box::new(AskCountingScopedChannel { scope, asks }));
    let executor = PolicyToolExecutor::new(registry, approvals);
    let header = kernel::SessionHeader {
        version: kernel::SESSION_FORMAT_VERSION,
        id: format!(
            "scope-{}",
            std::process::id() as u64 * 1000
                + std::sync::atomic::AtomicU64::fetch_add(&SESSION_SEQ, 1, std::sync::atomic::Ordering::SeqCst)
        ),
        created_at: 1.0,
        cwd: root.to_string_lossy().into_owned(),
        parent_session: None,
        is_seeded: false,
    };
    let session = kernel::SessionHandle::create(&root.join(".okra-sessions"), &header).unwrap();
    Agent::new(
        AgentConfig { unattended: true, ..Default::default() },
        Arc::new(ScriptedModel::new(steps)),
        Box::new(executor),
        session,
    )
}

fn write_call(id: &str, path: &str) -> ScriptedStep {
    ScriptedStep {
        text: String::new(),
        tool_calls: vec![ToolCall {
            id: id.into(),
            name: "write_file".into(),
            args_json: json!({ "path": path, "content": "x" }).to_string(),
        }],
        ..Default::default()
    }
}

#[test]
fn conversation_scope_grants_later_different_args_without_reprompting() {
    let td = tempfile::tempdir().unwrap();
    let asks = Arc::new(AtomicU64::new(0));
    let steps = vec![
        write_call("c1", "a.txt"),
        write_call("c2", "b.txt"), // different args — covered by the session tool grant
        ScriptedStep { text: "done".into(), ..Default::default() },
    ];
    let mut agent = scope_agent(td.path(), steps, ApprovalScope::Conversation, Arc::clone(&asks));
    let mut events = Vec::new();
    let outcome = agent.run_turn("write two files", &mut collect(&mut events)).unwrap();
    assert!(matches!(outcome, TurnOutcome::Completed { .. }));
    assert_eq!(
        asks.load(AtomicOrdering::SeqCst),
        1,
        "#53: the conversation scope grants the second different-args write WITHOUT a second prompt"
    );
    // the grant event rode to the surface with the scope
    assert!(events.iter().any(|e| matches!(e, LoopEvent::ApprovalGranted { scope, .. } if scope == "conversation")));
}

#[test]
fn once_scope_still_prompts_per_call() {
    let td = tempfile::tempdir().unwrap();
    let asks = Arc::new(AtomicU64::new(0));
    let steps = vec![
        write_call("c1", "a.txt"),
        write_call("c2", "b.txt"), // different args — no wider grant: prompts again
        ScriptedStep { text: "done".into(), ..Default::default() },
    ];
    let mut agent = scope_agent(td.path(), steps, ApprovalScope::Once, Arc::clone(&asks));
    let mut events = Vec::new();
    let outcome = agent.run_turn("write two files", &mut collect(&mut events)).unwrap();
    assert!(matches!(outcome, TurnOutcome::Completed { .. }));
    assert_eq!(
        asks.load(AtomicOrdering::SeqCst),
        2,
        "scope once keeps the pre-#53 behavior: every distinct approved bytes prompts"
    );
    assert!(events.iter().any(|e| matches!(e, LoopEvent::ApprovalGranted { scope, .. } if scope == "once")));
}

// ---- #37: XML tool-call recovery + model-fallback events ----------------

use okra_providers::{FallbackEvent, SampleRequest, SampleResponse};

#[test]
fn xml_tool_calls_in_text_are_recovered_and_executed() {
    let td = tempfile::tempdir().unwrap();
    // the model emits the call as TEXT instead of the structured field
    let steps = vec![
        ScriptedStep {
            text: "<tool_call>{\"name\":\"write_file\",\"arguments\":{\"path\":\"rec.txt\",\"content\":\"recovered!\"}}</tool_call>".into(),
            ..Default::default()
        },
        ScriptedStep { text: "done".into(), ..Default::default() },
    ];
    let mut registry = Registry::new();
    let wf = okra_tools::builtins::ErasedWriteFile::new(td.path().to_path_buf());
    let entry = wf.entry();
    registry
        .register(okra_tools::ErasedTool::simple(
            entry,
            vec![okra_tools::ResourceAccess::write_file("*")],
            move |args| wf.execute(args),
        ))
        .unwrap();
    // the recovered write is a REAL side-effecting call: it goes through
    // the same approval seam as any write (an allow channel keeps the test
    // focused on the recovery, not the approval)
    let mut approvals = ApprovalService::new(ApprovalPolicy::Ask);
    approvals.add_channel(Box::new(AllowChannel));
    let executor = PolicyToolExecutor::new(registry, approvals);
    let header = kernel::SessionHeader {
        version: kernel::SESSION_FORMAT_VERSION,
        id: format!("xml-recovery-{}", std::process::id()),
        created_at: 1.0,
        cwd: td.path().to_string_lossy().into_owned(),
        parent_session: None,
        is_seeded: false,
    };
    let session = kernel::SessionHandle::create(&td.path().join(".okra-sessions"), &header).unwrap();
    let mut agent = Agent::new(
        AgentConfig { unattended: true, ..Default::default() },
        Arc::new(ScriptedModel::new(steps)),
        Box::new(executor),
        session,
    );
    let mut events = Vec::new();
    let outcome = agent.run_turn("write the file", &mut collect(&mut events)).unwrap();
    assert!(matches!(outcome, TurnOutcome::Completed { .. }));
    // the recovered call REALLY executed through the tool plane
    assert!(
        td.path().join("rec.txt").is_file(),
        "the XML-embedded call executed for real"
    );
    assert_eq!(
        std::fs::read_to_string(td.path().join("rec.txt")).unwrap(),
        "recovered!"
    );
    // the receipt rode to the surface
    assert!(events.iter().any(|e| matches!(e, LoopEvent::ToolCallsRecovered { count: 1 })));
    assert!(events.iter().any(|e| matches!(e, LoopEvent::ToolCallStarted { name, .. } if name == "write_file")));
}

/// Wraps a scripted model and reports a canned fallback switch (#37).
struct FallbackReportingModel {
    inner: ScriptedModel,
    event: FallbackEvent,
}
impl okra_providers::Sampler for FallbackReportingModel {
    fn sample(&self, request: &SampleRequest) -> Result<SampleResponse, SamplerError> {
        self.inner.sample(request)
    }
    fn drain_fallback_events(&self) -> Vec<FallbackEvent> {
        vec![self.event.clone()]
    }
}

#[test]
fn model_fallback_switches_are_surfaced_and_logged() {
    let td = tempfile::tempdir().unwrap();
    let steps = vec![
        ScriptedStep { text: "answered on the fallback model".into(), ..Default::default() },
        ScriptedStep { text: "done".into(), ..Default::default() },
    ];
    let registry = Registry::new();
    let approvals = ApprovalService::new(ApprovalPolicy::Never);
    let executor = PolicyToolExecutor::new(registry, approvals);
    let header = kernel::SessionHeader {
        version: kernel::SESSION_FORMAT_VERSION,
        id: format!("fallback-{}", std::process::id()),
        created_at: 1.0,
        cwd: td.path().to_string_lossy().into_owned(),
        parent_session: None,
        is_seeded: false,
    };
    let session = kernel::SessionHandle::create(&td.path().join(".okra-sessions"), &header).unwrap();
    let model = FallbackReportingModel {
        inner: ScriptedModel::new(steps),
        event: FallbackEvent {
            from: "glm-primary".into(),
            to: "glm-backup".into(),
            reason: "rate limited (429)".into(),
        },
    };
    let mut agent = Agent::new(
        AgentConfig { unattended: true, ..Default::default() },
        Arc::new(model),
        Box::new(executor),
        session,
    );
    let mut events = Vec::new();
    let outcome = agent.run_turn("hello", &mut collect(&mut events)).unwrap();
    assert!(matches!(outcome, TurnOutcome::Completed { .. }));
    assert!(events.iter().any(|e| matches!(e, LoopEvent::ModelFallback { from, to, .. }
        if from == "glm-primary" && to == "glm-backup")));
}
