//! n0028: every continuation turn carries the context head — world state,
//! skill index, memory recall — as a System message BEFORE the first
//! compaction install (previously the head only materialized at install
//! time), and the synthetic copy is never folded back into the session
//! context.

use std::sync::Arc;

use okra_agent_core::loop_::{Agent, PolicyToolExecutor};
use okra_kernel as kernel;
use okra_policy::approval::{ApprovalPolicy, ApprovalService};
use okra_providers::{Role, ScriptedModel, ScriptedStep, ToolCall};
use okra_tools::Registry;

struct AllowChannel;
impl okra_policy::approval::ApprovalChannel for AllowChannel {
    fn answer(
        &self,
        _req: &okra_policy::ApprovalRequest,
    ) -> Option<okra_policy::approval::ApprovalOutcome> {
        Some(okra_policy::approval::ApprovalOutcome::AllowedOnce)
    }
}

fn setup(ws: &std::path::Path) -> std::io::Result<()> {
    std::fs::write(ws.join("f.txt"), "data")?;
    // project-tier memory (recall folds into the head)
    std::fs::create_dir_all(ws.join(".okra"))?;
    std::fs::write(ws.join(".okra").join("okra.memory.md"), "prefer okra crates")?;
    // a skill matching the file the turn touches
    let skills = ws.join(".okra").join("skills");
    std::fs::create_dir_all(&skills)?;
    std::fs::write(
        skills.join("SKILL-txt.md"),
        "---\nname: txt-files\ndescription: text file conventions\nmatch: *.txt\n---\n# Txt\nTrailer lines win.\n",
    )
}

fn build_agent(
    ws: &std::path::Path,
    steps: Vec<ScriptedStep>,
) -> (Agent<ScriptedModel>, Arc<ScriptedModel>) {
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
    let header = kernel::SessionHeader {
        version: kernel::SESSION_FORMAT_VERSION,
        id: format!("cont-head-{}", std::process::id()),
        created_at: 1.0,
        cwd: ws.to_string_lossy().into_owned(),
        parent_session: None,
        is_seeded: false,
    };
    let session = kernel::SessionHandle::create(&ws.join(".okra-sessions"), &header).unwrap();
    let sampler = Arc::new(ScriptedModel::new(steps));
    let agent = Agent::new(Default::default(), Arc::clone(&sampler), Box::new(executor), session);
    (agent, sampler)
}

fn read_step() -> ScriptedStep {
    ScriptedStep {
        tool_calls: vec![ToolCall {
            id: format!("c{}", std::process::id()),
            name: "read_file".into(),
            args_json: r#"{"path":"f.txt"}"#.into(),
        }],
        ..Default::default()
    }
}

#[test]
fn head_seeded_before_first_install_and_skills_activate_by_path() {
    let td = tempfile::tempdir().unwrap();
    let ws = td.path();
    setup(ws).unwrap();

    // turn 1: read + text; turn 2: read + text (ScriptedModel consumes
    // steps sequentially across turns on one agent)
    let (mut agent, sampler) = build_agent(
        ws,
        vec![read_step(), ScriptedStep::default(), read_step(), ScriptedStep::default()],
    );
    let memory = okra_memory::TieredReader::new(td.path().join("home"), ws.to_path_buf());
    let catalog = okra_memory::SkillCatalog::load_dir(&ws.join(".okra").join("skills"));
    let mut ctx = okra_compaction::SessionContext::default();

    let outcome = agent
        .run_turn_continuation(
            &mut ctx,
            &okra_compaction::ScriptedCompactor,
            Some(&memory),
            Some(&catalog),
            "turn one",
            &mut |_| {},
        )
        .unwrap();
    assert!(matches!(
        outcome,
        okra_agent_core::turn::TurnOutcome::Completed { .. }
    ));
    // the tool ran (the note loop below only matters if it did)
    assert!(ctx.noted_paths().contains(&"f.txt".to_string()));

    // ---- turn 1 saw the head BEFORE any install happened ----
    assert_eq!(ctx.installs(), 0, "no compaction ran at this size");
    let head1 = {
        let calls = sampler.requests.lock().unwrap();
        assert_eq!(calls[0].messages[0].role, Role::System, "head leads the model context");
        calls[0].messages[0].text_content()
    };
    let head = &head1;
    assert!(head.contains("<world_state>"), "world state present");
    assert!(head.contains("# Skills"), "L1 skill index present");
    assert!(head.contains("txt-files"), "the skill is indexed");
    assert!(head.contains("<memory_recall>"), "memory recall block present");
    assert!(head.contains("prefer okra crates"), "recall content present");
    // not yet ACTIVE (no path touched before turn 1)
    assert!(!head.contains("ACTIVE"), "no activation before a path is touched");
    // the synthetic head is NOT folded back into the session context
    assert!(
        !ctx.messages().first().is_some_and(|m| m.role == Role::System),
        "synthetic head must not persist into the context"
    );

    // ---- turn 2: chained history + path-conditional activation ----
    let outcome = agent
        .run_turn_continuation(
            &mut ctx,
            &okra_compaction::ScriptedCompactor,
            Some(&memory),
            Some(&catalog),
            "turn two",
            &mut |_| {},
        )
        .unwrap();
    assert!(matches!(
        outcome,
        okra_agent_core::turn::TurnOutcome::Completed { .. }
    ));

    let (head2, texts2): (String, Vec<String>) = {
        let calls = sampler.requests.lock().unwrap();
        // turn 1 = calls 0..2, turn 2 = calls 2..4
        let head2 = calls[2].messages[0].text_content();
        let texts = calls[2].messages.iter().map(|m| m.text_content()).collect();
        (head2, texts)
    };
    assert!(head2.contains("ACTIVE"), "f.txt was noted — the skill activates");
    assert!(head2.contains("txt-files"));
    // history chains: turn 2's context carries turn 1's user message
    assert!(
        texts2.iter().any(|t| t.contains("turn one")),
        "turn 2 sees turn 1's user message"
    );
    assert!(
        texts2.iter().any(|t| t.contains("turn two")),
        "turn 2 sees its own input"
    );
}
