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

mod demo_sampler;
mod serve;

use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;

use okra_agent_core::loop_::{Agent, AgentConfig, LoopEvent, PolicyToolExecutor};
use okra_kernel as kernel;
use okra_policy::approval::{ApprovalPolicy, ApprovalService};
use okra_tools::Registry;

struct Args {
    json: bool,
    cwd: PathBuf,
    max_turns: usize,
    kill_at_phase: Option<String>,
    fork_session: bool,
    worktree: Option<PathBuf>,
    prompt: String,
}

fn parse_args() -> Result<Args, String> {
    let mut args = std::env::args().skip(1);
    let mut json = false;
    let mut cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let mut max_turns = 8usize;
    let mut kill_at_phase = None;
    let mut fork_session = false;
    let mut worktree = None;
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
    let prompt = prompt.ok_or("missing prompt (try: okra --json \"read hello.txt\")")?;
    Ok(Args { json, cwd, max_turns, kill_at_phase, fork_session, worktree, prompt })
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
         --max-turns N           step budget for the turn (default 8)\n  \
         --kill-at-phase P:N     fault injection: abort at phase P on occurrence N\n  \
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
    registry
}

fn main() {
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
    let approvals = ApprovalService::new(ApprovalPolicy::Never);
    let executor = PolicyToolExecutor::new(registry, approvals);
    let sampler = demo_sampler::DemoPlanner::new(args.cwd.clone());

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
    let session = match kernel::SessionHandle::open(&sessions_root, &session_id, kernel::SessionAccess::Write) {
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

    let config = AgentConfig { max_steps: args.max_turns, unattended: true, ..Default::default() };
    let mut agent = Agent::new(config, Arc::new(sampler), Box::new(executor), session);

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
