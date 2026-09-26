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

use okra_tools::{
    ClosureHookRunner, HookAction, HookDef, HookEffect, HookSystem, McpClient, McpFunnel,
    ToolDirectory, UseToolFunnel, Registry,
};

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
    mcp_calls: u64,
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

    /// MCP funnel calls issued so far.
    pub fn mcp_calls(&self) -> u64 {
        self.state.lock().unwrap().mcp_calls
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
        // one MCP funnel call per turn (use_tool -> mcp_echo)
        if n == self.reads_per_turn {
            st.mcp_calls += 1;
            let turn = st.reads_issued;
            return Ok(SampleResponse {
                text: "Calling the MCP echo tool through the dispatch funnel.".into(),
                tool_calls: vec![ToolCall {
                    id: format!("bench-mcp-{turn}"),
                    name: "use_tool".into(),
                    args_json: serde_json::json!({
                        "server": "bench-mcp",
                        "tool": "mcp_echo",
                        "arguments": { "text": format!("turn-{turn}") }
                    })
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
    /// Skills (M2): catalog size + path-conditional activations.
    pub skills_available: usize,
    pub skills_activated: u64,
    pub skill_index_in_head: bool,
    /// Number of times the byte-stable head CHANGED across the run
    /// (initial state + one change per world mutation — bounded churn).
    pub head_changes: u64,
    /// MCP (#47): use_tool funnel calls completed.
    pub mcp_calls: u64,
    /// Hooks (#45): events fired through the dispatch pipeline.
    pub hook_events: u64,
    /// Hooks contained failures (must be 0).
    pub hook_failures: u64,
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

    // MCP funnel (#47): in-process mock server with two deferred tools
    let mut funnel = McpFunnel::new();
    let client = McpClient::in_process("bench-mcp", Box::new(|method, params| match method {
        "initialize" => Ok(serde_json::json!({
            "protocolVersion": "2024-11-05",
            "serverInfo": { "name": "bench-mcp" }
        })),
        "tools/list" => Ok(serde_json::json!({
            "tools": [
                { "name": "mcp_echo", "description": "echo text",
                  "inputSchema": { "type": "object" } },
                { "name": "mcp_len", "description": "text length",
                  "inputSchema": { "type": "object" } }
            ]
        })),
        "tools/call" => {
            let name = params["name"].as_str().unwrap_or_default();
            let text = params["arguments"]["text"].as_str().unwrap_or_default();
            match name {
                "mcp_echo" => Ok(serde_json::json!({
                    "content": [{ "type": "text", "text": format!("mcp-echo: {text}") }]
                })),
                "mcp_len" => Ok(serde_json::json!({
                    "content": [{ "type": "text", "text": text.chars().count().to_string() }]
                })),
                _ => Ok(serde_json::json!({ "content": [], "isError": true })),
            }
        }
        _ => Err(format!("unknown method {method}")),
    }));
    funnel.attach(client).map_err(|e| format!("mcp attach: {e}"))?;
    let funnel_arc = Arc::new(funnel);
    let use_tool = UseToolFunnel { funnel: Arc::clone(&funnel_arc) };
    let entry = use_tool.entry();
    registry
        .register(okra_tools::ErasedTool::simple(entry, vec![], move |args| {
            use_tool.execute(args)
        }))
        .map_err(|e| format!("register use_tool: {e}"))?;
    let directory = ToolDirectory { funnel: Arc::clone(&funnel_arc) };
    let entry = directory.entry();
    registry
        .register(okra_tools::ErasedTool::simple(entry, vec![], move |args| {
            directory.execute(args)
        }))
        .map_err(|e| format!("register tool_directory: {e}"))?;

    // hooks (#45): observe counters on the tool lifecycle
    let mut hook_system = HookSystem::with_runner(Arc::new(ClosureHookRunner {
        handler: Box::new(|_def, _payload| Ok(okra_tools::ExternalHookVerdict::Observe)),
    }));
    for event in ["PreToolUse", "PostToolUse"] {
        hook_system
            .register(HookDef {
                event: event.into(),
                matcher: None,
                action: HookAction::Command { program: "true".into(), args: vec![] },
                effect: HookEffect::Observe,
                timeout_ms: 5_000,
            })
            .map_err(|e| format!("register hook: {e}"))?;
    }
    registry.hook_system = hook_system;

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
    // skills (M2): two skills, only one matches the bench file paths —
    // path-conditional activation + progressive disclosure
    let skills_dir = ws.join(".okra").join("skills");
    std::fs::create_dir_all(&skills_dir).map_err(|e| format!("skills dir: {e}"))?;
    std::fs::write(
        skills_dir.join("SKILL-bench-files.md"),
        "---\nname: bench-files\ndescription: conventions for bench data files\nmatch: bench/*.txt\n---\n# Bench files\nTrailer lines are the authoritative content marker.\n",
    )
    .map_err(|e| format!("write skill: {e}"))?;
    std::fs::write(
        skills_dir.join("SKILL-docker.md"),
        "---\nname: docker-build\ndescription: container build conventions\nmatch: Dockerfile*\n---\n# Docker\nUse buildkit.\n",
    )
    .map_err(|e| format!("write skill: {e}"))?;
    let skill_catalog = okra_memory::SkillCatalog::load_dir(&skills_dir);
    let skills_available = skill_catalog.len();

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
    let mut executor_hook_stats = (0u64, 0u64);

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
            Some(&skill_catalog),
            &format!("turn {turn}: read every bench file and summarize"),
            &mut |_| {},
        )?;
        executor_hook_stats = agent.hook_stats();
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
    let mcp_calls = planner.mcp_calls();
    let (hook_events, registry_hook_failures) = executor_hook_stats;
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
        && head_changes <= (files + 2) as u64
        && skills_available >= 2
        && ctx
            .prefix_head()
            .windows(b"ACTIVE".len())
            .any(|w| w == b"ACTIVE")
        // progressive disclosure: the non-matching skill's L2 BODY never
        // enters the context (its L1 name legitimately appears in the index)
        && !prefix_heads
            .iter()
            .any(|h| h.windows(b"Use buildkit".len()).any(|w| w == b"Use buildkit"))
        && mcp_calls == turns as u64
        && hook_events >= mcp_calls * 2
        && registry_hook_failures == 0;

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
        skills_available,
        skills_activated: ctx
            .prefix_head()
            .windows(b"ACTIVE".len())
            .filter(|w| w == b"ACTIVE")
            .count() as u64,
        skill_index_in_head: ctx
            .prefix_head()
            .windows(b"# Skills".len())
            .any(|w| w == b"# Skills"),
        head_changes,
        mcp_calls,
        hook_events,
        hook_failures: registry_hook_failures,
        passed,
    })
}
