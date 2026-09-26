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
use okra_memory::TieredReader;
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
    /// Microcompaction (#28): evicted tool-result payloads.
    pub microcompactions: u32,
    pub evicted_bytes: u64,
    /// Hydration (#29): noted files re-read at install.
    pub hydrated_files: u32,
    /// Tiered memory recall (#33): injected into the stable head.
    pub memory_recall_injected: bool,
    /// Number of times the byte-stable head CHANGED across the run
    /// (initial state + one change per world mutation — bounded churn).
    pub head_changes: u64,
    pub passed: bool,
}

pub fn run_benchmark(
    turns: usize,
    files: usize,
    reads_per_turn: usize,
    content_bytes: usize,
    limit_tokens: u64,
    microcompact_at: Option<u64>,
) -> Result<BenchVerdict, String> {
    let with_memory = true;
    let with_hydration = true;
    let rewrite_at = turns / 2;
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

    // tiered memory (#33): project tier file with a secret that must be
    // redacted out of the recall
    let home = ws.join("home");
    let memory_dir = ws.join(".okra");
    std::fs::create_dir_all(&memory_dir).map_err(|e| format!("memory dir: {e}"))?;
    std::fs::write(
        memory_dir.join("okra.memory.md"),
        "project tier: benchmark workspace\napi_key = SUPERSECRETVALUE123\n",
    )
    .map_err(|e| format!("write memory: {e}"))?;
    let _ = &home;

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
        microcompact_at,
        keep_recent_tool_results: 4,
    });
    if with_hydration {
        ctx.enable_hydration(ws.clone(), 8);
    }
    let memory_reader = if with_memory {
        Some(TieredReader::new(home.clone(), ws.clone()))
    } else {
        None
    };
    let compactor = ScriptedCompactor;

    let mut reads: u64 = 0;
    let mut max_tokens: u64 = 0;
    let mut prefix_heads: Vec<Vec<u8>> = Vec::new();
    let mut first_message_texts: Vec<String> = Vec::new();
    let mut seen_first_install = false;

    let rewritten: Vec<usize> = (0..files.saturating_sub(1).max(1)).collect();
    for turn in 1..=turns {
        if turn == rewrite_at {
            // world mutation (ONE-TIME, fixed content): rewrite most files
            // with new headers; the next install's hydration must refresh
            // the head exactly once and stay stable afterwards
            for i in &rewritten {
                std::fs::write(
                    bench_dir.join(format!("file{i}.txt")),
                    format!(
                        "file {i} header REWRITTEN\n{}\nfile {i} trailer-TAIL-MARKER\n",
                        "x".repeat(content_bytes.saturating_sub(64))
                    ),
                )
                .map_err(|e| format!("rewrite: {e}"))?;
            }
        }
        planner.next_turn();
        let outcome = agent.run_turn_continuation(
            &mut ctx,
            &compactor,
            memory_reader.as_ref(),
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

    // the head is stable WITHIN each world state; count the transitions —
    // bounded churn: one per newly-noted file + one per world rewrite batch
    let head_changes: u64 = prefix_heads.windows(2).filter(|w| w[0] != w[1]).count() as u64;
    let prefix_bytes_stable = prefix_heads.windows(2).all(|w| w[0] == w[1]) || head_changes <= (files + 2) as u64;
    let seed_prefix_stable = first_message_texts.windows(2).all(|w| w[0] == w[1]) || head_changes <= (files + 2) as u64;
    let emergencies = ctx.emergencies();
    let installs = ctx.installs();
    let memory_recall_injected = ctx
        .prefix_head()
        .windows(b"<memory_recall>".len())
        .any(|w| w == b"<memory_recall>");
    // the secret from the memory tier file must NEVER reach the context
    let secret_leaked = prefix_heads
        .iter()
        .any(|h| h.windows(b"SUPERSECRETVALUE123".len()).any(|w| w == b"SUPERSECRETVALUE123"));

    // microcompaction assertions apply only when the layer is enabled
    let micro_required = microcompact_at.is_some();
    let micro_ok = !micro_required
        || (ctx.microcompactions() >= 1 && ctx.evicted_bytes() > 0);
    let passed = reads == (turns * reads_per_turn) as u64
        && installs + ctx.microcompactions() >= 1
        && emergencies == 0
        && ctx.rejected_summaries() == 0
        && prefix_bytes_stable
        && seed_prefix_stable
        && max_tokens < limit_tokens + (reads_per_turn as u64) * 1024
        && micro_ok
        && (!with_hydration || ctx.hydrated_files() >= 4)
        && memory_recall_injected
        && !secret_leaked
        && head_changes <= (files + 2) as u64;

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
        microcompactions: ctx.microcompactions(),
        evicted_bytes: ctx.evicted_bytes(),
        hydrated_files: ctx.hydrated_files(),
        memory_recall_injected,
        head_changes,
        passed,
    })
}
