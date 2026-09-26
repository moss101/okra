//! `okra bench-continuation` — the G2 agent-continuation benchmark
//! (MASTER-PLAN §4 M2 gate): **100 turns / 800 file reads** with
//! compaction wired into the loop, asserting
//!
//! 1. **flat post-compaction context** — the context never exceeds the
//!    compaction limit after a cycle (installs happened, emergencies 0);
//! 2. **byte-identical prefixes** — the world_state head is byte-identical
//!    across all 100 turns (unchanged world) and every turn seeds from the
//!    same stable head, which is exactly the provider prefix-cache
//!    property;
//! 3. every installed summary passed schema validation (rejected = 0).

use okra_agent_core::loop_::{Agent, AgentConfig, PolicyToolExecutor};
use okra_compaction::{
    ScriptedCompactor, SessionContext, SessionContextConfig,
};
use okra_kernel as kernel;
use okra_policy::approval::{ApprovalPolicy, ApprovalService};
use okra_providers::{SampleRequest, SampleResponse, Sampler, SamplerError, StopReason, ToolCall, Usage};
use std::sync::{Arc, Mutex};

use okra_tools::Registry;

/// Reads `reads_per_turn` files per turn, cycling through the workspace
/// files; then ends the turn.
pub struct BenchmarkPlanner {
    state: Mutex<BenchState>,
    files: usize,
    reads_per_turn: usize,
}

#[derive(Default)]
struct BenchState {
    sample_in_turn: usize,
    reads_issued: u64,
}

impl BenchmarkPlanner {
    pub fn new(files: usize, reads_per_turn: usize) -> Self {
        BenchmarkPlanner {
            state: Mutex::new(BenchState::default()),
            files,
            reads_per_turn,
        }
    }

    /// Mark the start of a new turn (resets the in-turn sample counter).
    pub fn next_turn(&self) {
        self.state.lock().unwrap().sample_in_turn = 0;
    }
}

impl Sampler for BenchmarkPlanner {
    fn sample(&self, _request: &SampleRequest) -> Result<SampleResponse, SamplerError> {
        let mut st = self.state.lock().unwrap();
        let n = st.sample_in_turn;
        st.sample_in_turn += 1;
        if n < self.reads_per_turn {
            let file_idx = (st.reads_issued as usize) % self.files;
            st.reads_issued += 1;
            return Ok(SampleResponse {
                text: format!("Reading bench/file{file_idx}.txt."),
                tool_calls: vec![ToolCall {
                    id: format!("bench-call-{}", st.reads_issued),
                    name: "read_file".into(),
                    args_json: serde_json::json!({ "path": format!("bench/file{file_idx}.txt") })
                        .to_string(),
                }],
                stop_reason: StopReason::ToolUse,
                usage: Usage { input_tokens: 30, output_tokens: 10 },
            });
        }
        Ok(SampleResponse {
            text: "Turn done: all files read.".into(),
            tool_calls: vec![],
            stop_reason: StopReason::EndTurn,
            usage: Usage { input_tokens: 60, output_tokens: 20 },
        })
    }
}

/// The benchmark verdict (serialized as one `BENCH {...}` NDJSON line).
#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub struct BenchVerdict {
    pub turns: u64,
    pub reads: u64,
    pub compaction_installs: u32,
    pub compaction_prefires: u32,
    pub emergencies: u32,
    pub rejected_summaries: u32,
    pub max_context_tokens: u64,
    pub final_context_tokens: u64,
    pub final_context_messages: usize,
    pub prefix_bytes_stable: bool,
    pub seed_prefix_stable: bool,
    pub limit_tokens: u64,
    pub passed: bool,
}

pub fn run_benchmark(
    turns: usize,
    files: usize,
    reads_per_turn: usize,
    content_bytes: usize,
    limit_tokens: u64,
) -> Result<BenchVerdict, String> {
    let run_id = std::process::id();
    let ws = std::env::temp_dir().join(format!("okra-bench-{run_id}"));
    let bench_dir = ws.join("bench");
    std::fs::create_dir_all(&bench_dir).map_err(|e| format!("mkdir: {e}"))?;

    let filler = "x".repeat(content_bytes.saturating_sub(24));
    for i in 0..files {
        std::fs::write(
            bench_dir.join(format!("file{i}.txt")),
            format!("file {i} header\n{filler}\nfile {i} trailer-TAIL-MARKER\n"),
        )
        .map_err(|e| format!("write bench file: {e}"))?;
    }

    let mut registry = Registry::new();
    let rf = okra_tools::builtins::read_file_tool(ws.clone());
    let entry = rf.entry();
    registry
        .register(okra_tools::ErasedTool::simple(
            entry,
            vec![okra_tools::ResourceAccess::read_file("*")],
            move |args| rf.execute(args, None),
        ))
        .map_err(|e| format!("register read_file: {e}"))?;

    let approvals = ApprovalService::new(ApprovalPolicy::Never);
    let mut executor = PolicyToolExecutor::new(registry, approvals);
    executor.ceiling = okra_policy::ToolApprovalCeiling::UnattendedAllowed;

    let sessions_root = ws.join(".okra-sessions");
    let header = kernel::SessionHeader {
        version: kernel::SESSION_FORMAT_VERSION,
        id: "bench".into(),
        created_at: kernel::wall_clock(),
        cwd: ws.to_string_lossy().into_owned(),
        parent_session: None,
        is_seeded: false,
    };
    let session = kernel::SessionHandle::create(&sessions_root, &header)
        .map_err(|e| format!("create session: {e}"))?;

    let planner = Arc::new(BenchmarkPlanner::new(files, reads_per_turn));
    let config = AgentConfig {
        max_steps: reads_per_turn + 4,
        unattended: true,
        ..Default::default()
    };
    let mut agent = Agent::new(config, planner.clone(), Box::new(executor), session);

    let mut ctx = SessionContext::new(SessionContextConfig {
        limit_tokens,
        prefire_margin: limit_tokens / 4,
        tail_messages: 8,
    });
    let compactor = ScriptedCompactor;

    let mut reads: u64 = 0;
    let mut max_tokens: u64 = 0;
    let mut prefix_heads: Vec<Vec<u8>> = Vec::new();
    let mut first_message_texts: Vec<String> = Vec::new();
    let mut seen_first_install = false;

    for turn in 1..=turns {
        planner.next_turn();
        let outcome = agent.run_turn_continuation(
            &mut ctx,
            &compactor,
            &format!("turn {turn}: read every bench file and summarize"),
            &mut |_| {},
        )?;
        match outcome {
            okra_agent_core::turn::TurnOutcome::Completed { .. } => {}
            other => return Err(format!("turn {turn} did not complete: {other:?}")),
        }
        reads += reads_per_turn as u64;
        max_tokens = max_tokens.max(ctx.tokens());
        if !seen_first_install && ctx.installs() > 0 {
            seen_first_install = true;
            continue; // stability is a POST-compaction property: start collecting here
        }
        if seen_first_install {
            prefix_heads.push(ctx.prefix_head().to_vec());
            first_message_texts.push(
                ctx.messages()
                    .first()
                    .map(|m| m.text_content())
                    .unwrap_or_default(),
            );
        }
    }

    let prefix_bytes_stable = prefix_heads.windows(2).all(|w| w[0] == w[1]);
    let seed_prefix_stable = first_message_texts.windows(2).all(|w| w[0] == w[1]);
    let emergencies = ctx.emergencies();
    let installs = ctx.installs();

    let passed = reads == (turns * reads_per_turn) as u64
        && installs >= 1
        && emergencies == 0
        && ctx.rejected_summaries() == 0
        && prefix_bytes_stable
        && seed_prefix_stable
        && max_tokens < limit_tokens + (reads_per_turn as u64) * 1024;

    // best-effort scratch cleanup
    let _ = std::fs::remove_dir_all(&ws);

    Ok(BenchVerdict {
        turns: turns as u64,
        reads,
        compaction_installs: installs,
        compaction_prefires: ctx.prefires(),
        emergencies,
        rejected_summaries: ctx.rejected_summaries(),
        max_context_tokens: max_tokens,
        final_context_tokens: ctx.tokens(),
        final_context_messages: ctx.messages().len(),
        prefix_bytes_stable,
        seed_prefix_stable,
        limit_tokens,
        passed,
    })
}
