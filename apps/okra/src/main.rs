//! okra — the headless CLI (MASTER-PLAN §3 #56, M1 flag set).
//!
//! ```text
//! okra --json [--cwd DIR] [--max-turns N] [--kill-at-phase PHASE:N] "prompt"
//! ```
//!
//! `--json` emits one NDJSON LoopEvent per line. The M0 build ships the
//! offline demo sampler (an agent that really plans read_file/list_dir calls
//! through the full policy + kernel pipeline); real model providers wire in
//! behind the same Sampler seam at M1.

mod bench;
mod demo_sampler;
mod quality;
mod serve_tcp;
mod subagent;
mod serve;
mod task;

use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;

use okra_agent_core::loop_::{Agent, AgentConfig, LoopEvent, PolicyToolExecutor};
use okra_kernel as kernel;
use okra_policy::SelfConfinement;
use okra_policy::approval::{ApprovalPolicy, ApprovalService};
use okra_providers::Sampler;
use okra_tools::Registry;

struct Args {
    json: bool,
    cwd: PathBuf,
    max_turns: usize,
    kill_at_phase: Option<String>,
    kill_at_boundary: Option<String>,
    fork_session: bool,
    worktree: Option<PathBuf>,
    task_spec: Option<PathBuf>,
    sandbox: Option<String>,
    provider: Option<String>,
    model: Option<String>,
    prompt: String,
}

fn parse_args() -> Result<Args, String> {
    let mut args = std::env::args().skip(1);
    let mut json = false;
    let mut cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let mut max_turns = 32usize;
    let mut kill_at_phase = None;
    let mut kill_at_boundary = None;
    let mut fork_session = false;
    let mut worktree = None;
    let mut task_spec: Option<PathBuf> = None;
    let mut sandbox: Option<String> = None;
    let mut provider: Option<String> = None;
    let mut model: Option<String> = None;
    let mut prompt: Option<String> = None;

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--json" => json = true,
            "--cwd" => {
                cwd = PathBuf::from(args.next().ok_or("--cwd needs a value")?);
            }
            "--max-turns" => {
                max_turns = args
                    .next()
                    .ok_or("--max-turns needs a value")?
                    .parse()
                    .map_err(|_| "--max-turns expects a number")?;
            }
            "--kill-at-phase" => {
                kill_at_phase = Some(args.next().ok_or("--kill-at-phase needs PHASE:N")?);
            }
            "--kill-at-boundary" => {
                kill_at_boundary = Some(args.next().ok_or("--kill-at-boundary needs NAME:N")?);
            }
            "--task" => {
                task_spec = Some(PathBuf::from(args.next().ok_or("--task needs a spec path")?));
            }
            "--sandbox" => {
                sandbox = Some(args.next().ok_or("--sandbox needs read-only|workspace-write|strict|off")?);
            }
            "--provider" => {
                provider = Some(args.next().ok_or("--provider needs openai")?);
            }
            "--model" => {
                model = Some(args.next().ok_or("--model needs a model name")?);
            }
            "--fork-session" => fork_session = true,
            "--worktree" => {
                worktree = Some(PathBuf::from(args.next().ok_or("--worktree needs a path")?));
            }
            "--help" | "-h" => {
                print_help();
                std::process::exit(0);
            }
            other if prompt.is_none() => prompt = Some(other.to_string()),
            other => return Err(format!("unexpected argument: {other}")),
        }
    }
    let prompt = if task_spec.is_some() {
        prompt.unwrap_or_else(|| "execute the task spec".to_string())
    } else {
        prompt.ok_or("missing prompt (try: okra --json \"read hello.txt\")")?
    };
    Ok(Args { json, cwd, max_turns, kill_at_phase, kill_at_boundary, fork_session, worktree, task_spec, sandbox, provider, model, prompt })
}

fn print_help() {
    println!(
        "okra — one Rust daemon, any surface (M0 headless build)\n\
         \n\
         USAGE:\n  \
         okra [flags] \"prompt\"\n\
         \n\
         FLAGS:\n  \
         --json                  NDJSON event stream on stdout\n  \
         --cwd DIR               workspace root (default: .)\n  \
         --max-turns N           step budget for the turn (default 32)\n  \
         --kill-at-phase P:N     fault injection: abort at phase P on occurrence N\n  \
         --kill-at-boundary B:N  abort after durable boundary B (G1 matrix)\n  \
         --sandbox MODE          kernel-confine this process: read-only|strict|workspace-write\n  \
         --provider openai       real network model provider (OKRA_API_KEY/OPENAI_API_KEY)\n  \
         --model NAME            model name for the provider\n  \
         --task SPEC.json        run a scripted multi-file coding task\n  \
         --fork-session          fork instead of reusing the last session\n  \
         --worktree PATH         run against an isolated worktree\n  \
         --help                  this text"
    );
}

fn build_registry(cwd: &std::path::Path) -> Registry {
    let mut registry = Registry::new();
    let rf = okra_tools::builtins::read_file_tool(cwd.to_path_buf());
    let entry = rf.entry();
    registry
        .register(okra_tools::ErasedTool::simple(
            entry,
            vec![okra_tools::ResourceAccess::read_file("*")],
            move |args| rf.execute(args, None),
        ))
        .expect("read_file registers once");
    let ld = okra_tools::builtins::list_dir_tool(cwd.to_path_buf());
    let entry = ld.entry();
    registry
        .register(okra_tools::ErasedTool::simple(
            entry,
            vec![okra_tools::ResourceAccess::tree(
                okra_tools::FileAccessOperation::Read,
                "*",
            )],
            move |args| ld.execute(args),
        ))
        .expect("list_dir registers once");
    let wf = okra_tools::builtins::ErasedWriteFile::new(cwd.to_path_buf());
    let entry = wf.entry();
    registry
        .register(okra_tools::ErasedTool::simple(
            entry,
            vec![okra_tools::ResourceAccess::write_file("*")],
            move |args| wf.execute(args),
        ))
        .expect("write_file registers once");
    let ef = okra_tools::builtins::ErasedEditFile::new(cwd.to_path_buf());
    let entry = ef.entry();
    registry
        .register(okra_tools::ErasedTool::simple(
            entry,
            vec![okra_tools::ResourceAccess::file(
                okra_tools::FileAccessOperation::Readwrite,
                "*",
            )],
            move |args| ef.execute(args),
        ))
        .expect("edit_file registers once");
    registry
}

fn main() {
    // `okra bench-continuation [--turns N --files K --reads-per-turn R]`: G2
    let argv: Vec<String> = std::env::args().skip(1).collect();
    if argv.first().map(String::as_str) == Some("bench-continuation") {
        let mut turns = 100usize;
        let mut files = 8usize;
        let mut reads_per_turn = 8usize;
        let mut content_bytes = 2048usize;
        let mut limit_tokens = 20_000u64;
        let mut microcompact_at: Option<u64> = Some(limit_tokens / 3);
        let mut provider: Option<String> = None;
        let mut model: Option<String> = None;
        let mut i = 1;
        while i < argv.len() {
            let next = |i: &mut usize| -> String {
                *i += 1;
                argv.get(*i).cloned().unwrap_or_default()
            };
            match argv[i].as_str() {
                "--turns" => turns = next(&mut i).parse().unwrap_or(turns),
                "--files" => files = next(&mut i).parse().unwrap_or(files),
                "--reads-per-turn" => reads_per_turn = next(&mut i).parse().unwrap_or(reads_per_turn),
                "--content-bytes" => content_bytes = next(&mut i).parse().unwrap_or(content_bytes),
                "--limit-tokens" => limit_tokens = next(&mut i).parse().unwrap_or(limit_tokens),
                "--microcompact-at" => {
                    let v = next(&mut i);
                    microcompact_at = if v == "off" { None } else { v.parse().ok() };
                }
                "--provider" => provider = Some(next(&mut i)),
                "--model" => model = Some(next(&mut i)),
                other => {
                    eprintln!("error: unknown bench flag {other}");
                    std::process::exit(2);
                }
            }
            i += 1;
        }
        match bench::run_benchmark(&bench::BenchParams {
            turns,
            files,
            reads_per_turn,
            content_bytes,
            limit_tokens,
            microcompact_at,
            provider,
            model,
        }) {
            Ok(v) => {
                let passed = v.passed;
                println!("BENCH {}", serde_json::to_string(&v).unwrap_or_default());
                std::process::exit(if passed { 0 } else { 1 });
            }
            Err(e) => {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
        }
    }

    // `okra bench-quality`: agent-quality benchmark suite (#65)
    let argv: Vec<String> = std::env::args().skip(1).collect();
    if argv.first().map(String::as_str) == Some("bench-quality") {
        match quality::run_quality_suite() {
            Ok(v) => {
                let passed_all = v["passed_all"] == serde_json::Value::Bool(true);
                println!("QUALITY {}", serde_json::to_string(&v).unwrap_or_default());
                std::process::exit(if passed_all { 0 } else { 1 });
            }
            Err(e) => {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
        }
    }

    // `okra run-subagent --grant DIR --task SPEC.json`: G5 kernel-isolated
    if argv.first().map(String::as_str) == Some("run-subagent") {
        let mut grant = None;
        let mut spec = None;
        let mut i = 1;
        while i < argv.len() {
            match argv[i].as_str() {
                "--grant" => { i += 1; grant = Some(PathBuf::from(argv.get(i).cloned().unwrap_or_default())); }
                "--task" => { i += 1; spec = Some(PathBuf::from(argv.get(i).cloned().unwrap_or_default())); }
                other => { eprintln!("error: unknown run-subagent flag {other}"); std::process::exit(2); }
            }
            i += 1;
        }
        let (grant, spec) = match (grant, spec) {
            (Some(g), Some(s)) => (g, s),
            _ => { eprintln!("error: run-subagent needs --grant DIR --task SPEC.json"); std::process::exit(2); }
        };
        match crate::subagent::run_subagent(&grant, &spec) {
            Ok(v) => {
                let ok = v["passed"] == serde_json::Value::Bool(true);
                println!("SUBAGENT {}", serde_json::to_string(&v).unwrap_or_default());
                std::process::exit(if ok { 0 } else { 1 });
            }
            Err(e) => {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
        }
    }

    // `okra subagent-launch`: G5 full loop — launcher (git worktree grant)
    // → confined run-subagent child → collect_work on the branch
    if argv.first().map(String::as_str) == Some("subagent-launch") {
        let mut repo = None;
        let mut name = String::from("subagent-task");
        let mut worktree = None;
        let mut task = String::from("execute the task");
        let mut spec = None;
        let mut parent_session = None;
        let mut i = 1;
        while i < argv.len() {
            match argv[i].as_str() {
                "--repo" => { i += 1; repo = Some(PathBuf::from(argv.get(i).cloned().unwrap_or_default())); }
                "--name" => { i += 1; name = argv.get(i).cloned().unwrap_or_default(); }
                "--worktree" => { i += 1; worktree = Some(PathBuf::from(argv.get(i).cloned().unwrap_or_default())); }
                "--task" => { i += 1; task = argv.get(i).cloned().unwrap_or_default(); }
                "--task-spec" => { i += 1; spec = Some(PathBuf::from(argv.get(i).cloned().unwrap_or_default())); }
                "--parent-session" => { i += 1; parent_session = Some(argv.get(i).cloned().unwrap_or_default()); }
                other => { eprintln!("error: unknown subagent-launch flag {other}"); std::process::exit(2); }
            }
            i += 1;
        }
        let (repo, worktree, spec) = match (repo, worktree, spec) {
            (Some(r), Some(w), Some(s)) => (r, w, s),
            _ => { eprintln!("error: subagent-launch needs --repo DIR --worktree DIR --task-spec SPEC.json [--name N] [--task TEXT]"); std::process::exit(2); }
        };
        let role = okra_host::subagent::RoleScope {
            readable: vec!["README.md".into()],
            writable: vec![".".into()],
        };
        match crate::subagent::orchestrate_subagent(&repo, &name, &worktree, role, &task, &spec, parent_session.as_deref()) {
            Ok(v) => {
                let ok = v["passed"] == serde_json::Value::Bool(true);
                println!("ORCHESTRATION {}", serde_json::to_string(&v).unwrap_or_default());
                std::process::exit(if ok { 0 } else { 1 });
            }
            Err(e) => {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
        }
    }

    // `okra sessions [--cwd DIR]`: M3 strangler — task/session index query
    // (SQLite projection behind the kernel SessionHandle)
    if argv.first().map(String::as_str) == Some("sessions") {
        let mut cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let mut i = 1;
        while i < argv.len() {
            if argv[i] == "--cwd" {
                i += 1;
                cwd = PathBuf::from(argv.get(i).cloned().unwrap_or_default());
            }
            i += 1;
        }
        let db_path = cwd.join(".okra-sessions").join("index.db");
        let db = kernel::ProjectionDb::open(&db_path).unwrap_or_else(|e| {
            eprintln!("error: cannot open index: {e}");
            std::process::exit(1);
        });
        match db.list_sessions() {
            Ok(rows) => {
                if rows.is_empty() {
                    println!("no sessions indexed");
                }
                for r in rows {
                    println!("{:<40} {:<8} events={} {}", r.id, r.status, r.event_count, r.workspace);
                }
                std::process::exit(0);
            }
            Err(e) => {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
        }
    }


    // `okra serve --tcp ADDR --cwd DIR`: G4 multi-surface daemon
    if argv.first().map(String::as_str) == Some("serve")
        && argv.iter().any(|a| a == "--tcp")
    {
        let mut tcp_cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let addr = String::from("127.0.0.1:0");
        let mut i = 1;
        while i < argv.len() {
            match argv[i].as_str() {
                "--tcp" => {}
                "--cwd" => {
                    i += 1;
                    tcp_cwd = PathBuf::from(argv.get(i).cloned().unwrap_or_default());
                }
                _ => {}
            }
            i += 1;
        }
        // loopback-only posture
        let host_part = addr.rsplit_once(':').map(|(h, _)| h).unwrap_or(&addr).to_string();
        if !host_part.starts_with("127.0.0.1") && !host_part.starts_with("localhost") && !host_part.starts_with("[::1]") {
            eprintln!("error: --tcp binds must be loopback (got {addr})");
            std::process::exit(2);
        }
        let listener = std::net::TcpListener::bind(&addr).unwrap_or_else(|e| {
            eprintln!("error: cannot bind {addr}: {e}");
            std::process::exit(1);
        });
        let bound = listener.local_addr().unwrap();
        eprintln!("[serve-tcp] multi-surface daemon on {bound} (loopback only)");
        let sessions_dir = tcp_cwd.join(".okra-sessions");
        let state = std::sync::Arc::new(serve_tcp::TcpServeState::new(
            tcp_cwd.clone(),
            sessions_dir,
        ));
        serve_tcp::serve_tcp(state, listener);
    }

    // `okra serve --stdio --cwd DIR [--sessions DIR]`: G0 daemon mode
    let argv: Vec<String> = std::env::args().skip(1).collect();
    if argv.first().map(String::as_str) == Some("serve") {
        let mut cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let mut sessions_dir = None;
        let mut i = 1;
        while i < argv.len() {
            match argv[i].as_str() {
                "--stdio" => {}
                "--cwd" => {
                    i += 1;
                    cwd = PathBuf::from(argv.get(i).cloned().unwrap_or_default());
                }
                "--sessions" => {
                    i += 1;
                    sessions_dir = Some(PathBuf::from(argv.get(i).cloned().unwrap_or_default()));
                }
                other => {
                    eprintln!("error: unknown serve flag {other}");
                    std::process::exit(2);
                }
            }
            i += 1;
        }
        let sessions_dir = sessions_dir.unwrap_or_else(|| cwd.join(".okra-sessions"));
        serve::serve_stdio(cwd, sessions_dir);
    }

    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("error: {e}\n");
            print_help();
            std::process::exit(2);
        }
    };

    if let Some(spec) = &args.kill_at_phase {
        // set_var is unsafe in edition 2024 (process-global mutation); safe
        // here: single-threaded init before any threads spawn.
        unsafe { std::env::set_var("OKRA_KILL_AT_PHASE", spec) };
    }
    if let Some(spec) = &args.kill_at_boundary {
        unsafe { std::env::set_var("OKRA_KILL_AT_BOUNDARY", spec) };
    }

    if !args.cwd.is_dir() {
        eprintln!("error: --cwd {:?} is not a directory", args.cwd);
        std::process::exit(2);
    }
    // --worktree: M3 lands real worktrees (grok worktree crate); M0 records
    // the flag and refuses a missing path instead of silently ignoring it.
    if let Some(wt) = &args.worktree
        && !wt.is_dir() {
            eprintln!("error: --worktree {:?} is not a directory (worktree isolation lands in M3)", wt);
            std::process::exit(2);
        }

    // ---- agent assembly (the daemon core, in-process for M0) ----
    let registry = build_registry(&args.cwd);
    let approvals = ApprovalService::new(ApprovalPolicy::Ask);
    let mut executor = PolicyToolExecutor::new(registry, approvals);
    // Headless/CI runs unattended: the UnattendedAllowed ceiling lets the
    // executor honour yolo for side-effecting tools (arg-hash grants still
    // record every approval).
    executor.ceiling = okra_policy::ToolApprovalCeiling::UnattendedAllowed;
    // The interactive demo planner remains available for prompt mode; task
    // mode (--task) replaces it with the spec-driven TaskPlanner below.
    #[allow(unused_variables)]
    let demo_sampler = demo_sampler::DemoPlanner::new(args.cwd.clone());

    // kernel session: <cwd>/.okra-sessions/cli
    let sessions_root = args.cwd.join(".okra-sessions");
    let session_id = if args.fork_session {
        format!(
            "cli-fork-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or_default()
        )
    } else {
        "cli".to_string()
    };
    let header = kernel::SessionHeader {
        version: kernel::SESSION_FORMAT_VERSION,
        id: session_id.clone(),
        created_at: kernel::wall_clock(),
        cwd: args.cwd.to_string_lossy().into_owned(),
        parent_session: if args.fork_session { Some("cli".into()) } else { None },
        is_seeded: false,
    };
    let mut session = match kernel::SessionHandle::open(&sessions_root, &session_id, kernel::SessionAccess::Write) {
        Ok(h) => h,
        Err(kernel::HandleError::NotFound(_)) => {
            kernel::SessionHandle::create(&sessions_root, &header)
                .unwrap_or_else(|e| {
                    eprintln!("error: cannot create session: {e}");
                    std::process::exit(1);
                })
        }
        Err(e) => {
            eprintln!("error: cannot open session: {e}");
            std::process::exit(1);
        }
    };

    // G1 recovery: repair an interrupted turn BEFORE the next one runs —
    // synthesized closers close the seq space; no interrupted tool call can
    // be re-executed (its outcome is already logged as unknown).
    {
        let events = session
            .read_all()
            .map_err(|e| format!("read session: {e}"));
        match events {
            Ok(events) if kernel::needs_repair(&events) => {
                let closers = kernel::interrupted_turn_closers(&events);
                if let Err(e) = kernel::validate_closers(&closers) {
                    eprintln!("error: repair: {e}");
                    std::process::exit(1);
                }
                if let Err(e) = session.append_durable(closers) {
                    eprintln!("error: append repair: {e}");
                    std::process::exit(1);
                }
            }
            Err(e) => {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
            _ => {}
        }
    }

    // Kernel-enforced self-confinement (N0006): irreversible; the process
    // physically cannot leave the workspace (or touch the network under
    // read-only/strict) from this point on.
    let extra_writable: Vec<PathBuf> = vec![sessions_root.clone()];
    if let Some(mode_str) = &args.sandbox
        && mode_str != "off" {
            let mode = match mode_str.as_str() {
                "read-only" | "readonly" => okra_policy::SandboxMode::ReadOnly,
                "strict" => okra_policy::SandboxMode::ReadOnly,
                "workspace-write" => okra_policy::SandboxMode::WorkspaceWrite,
                other => {
                    eprintln!("error: unknown --sandbox mode {other}");
                    std::process::exit(2);
                }
            };
            let policy = okra_policy::SandboxExecutionPolicy {
                mode,
                workspace_root: args.cwd.clone(),
                session_id: Some(session_id.clone()),
            };
            // network follows the mode: blocked under read-only/strict,
            // allowed under workspace-write (the model provider needs it)
            let backend = okra_policy::NonoSandboxBackend::new();
            match backend.apply_to_self(&policy, &extra_writable) {
                Ok(report) => {
                    eprintln!(
                        "[sandbox] enforcement={:?} platform={} network_blocked={} workspace={}",
                        report.enforcement, report.platform, report.network_blocked,
                        report.workspace.display()
                    );
                }
                Err(e) => {
                    eprintln!("error: sandbox apply failed: {e}");
                    std::process::exit(1);
                }
            }
        }
    drop(extra_writable);

    // sampler: task spec planner (G1) or interactive demo planner (G0)
    #[allow(unused_variables)] // sampler unused when only serve paths built
    let sampler: Arc<dyn Sampler> = if let Some(provider_kind) = &args.provider {
        if provider_kind != "openai" {
            eprintln!("error: unknown provider {provider_kind} (supported: openai)");
            std::process::exit(2);
        }
        let model = args.model.clone().unwrap_or_else(|| "gpt-4o-mini".to_string());
        let provider = okra_providers::OpenAiProvider::from_env(model).unwrap_or_else(|| {
            eprintln!("error: set OKRA_API_KEY (or OPENAI_API_KEY) to use --provider openai");
            std::process::exit(2);
        });
        Arc::new(provider)
    } else {
        match &args.task_spec {
            Some(spec_path) => {
                let spec = task::TaskSpec::load(spec_path).unwrap_or_else(|e| {
                    eprintln!("error: {e}");
                    std::process::exit(2);
                });
                Arc::new(task::TaskPlanner::new(spec))
            }
            None => Arc::new(demo_sampler),
        }
    };
    let config = AgentConfig { max_steps: args.max_turns, unattended: true, ..Default::default() };
    let mut agent = Agent::new(config, sampler, Box::new(executor), session);

    // ---- run the turn, streaming NDJSON ----
    let stdout = std::io::stdout();
    let mut sink = stdout.lock();
    let write_event = |ev: &LoopEvent, sink: &mut dyn Write, json: bool| {
        if json {
            let line = serde_json::to_string(ev).unwrap_or_default();
            let _ = writeln!(sink, "{line}");
        } else {
            match ev {
                LoopEvent::TextDelta { text } => {
                    let _ = write!(sink, "{text}");
                }
                LoopEvent::ToolCallFinished { name, output, .. } => {
                    let _ = writeln!(sink, "[{name}] {output}");
                }
                LoopEvent::TurnFinished { .. } => {
                    let _ = writeln!(sink);
                }
                _ => {}
            }
        }
    };

    let result = agent.run_turn(&args.prompt, &mut |ev| write_event(&ev, &mut sink, args.json));
    let _ = sink.flush();

    // M3 strangler: fold the session into the SQLite task/session index
    // (derived, rebuildable from the kernel log — the durable truth).
    drop(agent); // release the write handle before reopening
    {
        let db_path = sessions_root.join("index.db");
        if let Ok(db) = kernel::ProjectionDb::open(&db_path)
            && let Ok(reader) = kernel::SessionHandle::open(
                &sessions_root,
                &session_id,
                kernel::SessionAccess::Read,
            )
            && let Ok(events) = reader.read_all()
        {
            let _ = db.rebuild_from_log(&events, &session_id, &args.cwd.to_string_lossy());
        }
    }

    match result {
        Ok(outcome) => {
            use okra_agent_core::turn::TurnOutcome;
            if !args.json {
                match outcome {
                    TurnOutcome::Completed { .. } => {}
                    other => println!("turn outcome: {other:?}"),
                }
            } else {
                let kind = match outcome {
                    TurnOutcome::Completed { .. } => "completed",
                    TurnOutcome::Cancelled { .. } => "cancelled",
                    TurnOutcome::MaxTurnsReached { .. } => "max_turns",
                    TurnOutcome::StationarityEnded => "stationarity_ended",
                };
                println!("{{\"event\":\"exit\",\"outcome\":\"{kind}\"}}");
            }
        }
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    }
}
