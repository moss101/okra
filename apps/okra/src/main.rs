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

mod acp;
mod bench;
mod demo_sampler;
mod quality;
mod serve_tcp;
mod subagent;
mod serve;
mod task;
mod tui_app;
mod workflow_cli;

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
    /// `--minimal`: plain scrollback lines (okra_tui::minimal_line) instead
    /// of inline streaming text — the pager's no-alt-screen sibling.
    minimal: bool,
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
    // `--version` must never be mistaken for a prompt: the dogfood harness
    // probes it, and a whole agent turn per probe is the bug it prevents
    let argv: Vec<String> = std::env::args().skip(1).collect();
    if argv.iter().any(|a| a == "--version" || a == "-V") {
        println!("okra {}", env!("CARGO_PKG_VERSION"));
        std::process::exit(0);
    }
    let mut args = argv.into_iter();
    let mut json = false;
    let mut minimal = false;
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
            "--minimal" => minimal = true,
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
    Ok(Args { json,
        minimal, cwd, max_turns, kill_at_phase, kill_at_boundary, fork_session, worktree, task_spec, sandbox, provider, model, prompt })
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
         --help                  this text
         
         ENV:
         TYPESAFE_API_KEY        enables the semantic wander governor (Jev);
                                 OKRA_SEMANTIC_WATCH=off disables"
    );
}

fn build_registry(cwd: &std::path::Path) -> Registry {
    serve::build_registry(cwd)
}

/// Write key/secret bytes with owner-only permissions on unix (best-effort
/// 0600; on non-unix the plain write is the platform's best available).
fn write_private_0600(path: &std::path::Path, bytes: &[u8]) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    #[cfg(unix)]
    {
        use std::io::Write as _;
        use std::os::unix::fs::OpenOptionsExt as _;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)
            .unwrap_or_else(|e| {
                eprintln!("error: write {}: {e}", path.display());
                std::process::exit(1);
            });
        f.write_all(bytes).unwrap_or_else(|e| {
            eprintln!("error: write {}: {e}", path.display());
            std::process::exit(1);
        });
    }
    #[cfg(not(unix))]
    std::fs::write(path, bytes).unwrap_or_else(|e| {
        eprintln!("error: write {}: {e}", path.display());
        std::process::exit(1);
    });
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

    // `okra tui [--cwd DIR] [--provider openai --model M]`: the M4 pager —
    // ratatui block scrollback over real turns (keys: Enter send, PgUp/PgDn
    // scroll, Ctrl-C stop/quit)
    if argv.first().map(String::as_str) == Some("tui") {
        let mut provider = None;
        let mut model = None;
        let mut cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let mut i = 1;
        while i < argv.len() {
            match argv[i].as_str() {
                "--cwd" => { i += 1; cwd = argv.get(i).cloned().map(PathBuf::from).unwrap_or(cwd); }
                "--provider" => { i += 1; provider = argv.get(i).cloned(); }
                "--model" => { i += 1; model = argv.get(i).cloned(); }
                other => { eprintln!("error: unknown tui flag {other}"); std::process::exit(2); }
            }
            i += 1;
        }
        if let Err(e) = tui_app::run_tui(cwd, provider, model) {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
        std::process::exit(0);
    }

    // `okra workflow run SCRIPT.rhai [--provider openai --model M]
    //  [--max-steps N]`: M3 Rhai engine — each script step is a full
    // agent turn in a child process (crates/workflow owns budgets + journal)
    if argv.first().map(String::as_str) == Some("workflow") {
        let mut script = None;
        let mut provider = None;
        let mut model = None;
        let mut max_steps: u32 = 256;
        let mut i = 1;
        while i < argv.len() {
            match argv[i].as_str() {
                "run" => {}
                "--script" => { i += 1; script = argv.get(i).cloned(); }
                other if other.ends_with(".rhai") => { script = Some(other.to_string()); }
                "--provider" => { i += 1; provider = argv.get(i).cloned(); }
                "--model" => { i += 1; model = argv.get(i).cloned(); }
                "--max-steps" => {
                    i += 1;
                    max_steps = argv.get(i).and_then(|v| v.parse().ok()).unwrap_or(256);
                }
                other => { eprintln!("error: unknown workflow flag {other}"); std::process::exit(2); }
            }
            i += 1;
        }
        let Some(script) = script else {
            eprintln!("error: workflow needs a .rhai script (`okra workflow run SCRIPT.rhai`)");
            std::process::exit(2);
        };
        let budgets = okra_workflow::RunBudgets { max_steps, ..Default::default() };
        match workflow_cli::run_workflow_cli(
            std::path::Path::new(&script),
            std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")),
            provider,
            model,
            budgets,
        ) {
            Ok(code) => std::process::exit(code),
            Err(e) => { eprintln!("error: {e}"); std::process::exit(1); }
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


    // `okra pin-sign --policy PATH (--key HEX | --key-file PATH) [-o OUT]`:
    // the admin-side counterpart of pin-status — mint the signed envelope
    // an enterprise trust model distributes. `--generate-key KEYFILE`
    // provisions a fresh signing seed instead (raw 32 bytes, 0600) and
    // prints the public key a trust file must list.
    if argv.first().map(String::as_str) == Some("pin-sign") {
        let mut policy: Option<PathBuf> = None;
        let mut key_hex: Option<String> = None;
        let mut key_file: Option<PathBuf> = None;
        let mut generate_key: Option<PathBuf> = None;
        let mut out: Option<PathBuf> = None;
        let mut i = 1;
        while i < argv.len() {
            match argv[i].as_str() {
                "--policy" => {
                    i += 1;
                    policy = Some(PathBuf::from(argv.get(i).cloned().unwrap_or_default()));
                }
                "--key" => {
                    i += 1;
                    key_hex = Some(argv.get(i).cloned().unwrap_or_default());
                }
                "--key-file" => {
                    i += 1;
                    key_file = Some(PathBuf::from(argv.get(i).cloned().unwrap_or_default()));
                }
                "--generate-key" => {
                    i += 1;
                    generate_key = Some(PathBuf::from(argv.get(i).cloned().unwrap_or_default()));
                }
                "-o" | "--output" => {
                    i += 1;
                    out = Some(PathBuf::from(argv.get(i).cloned().unwrap_or_default()));
                }
                other => {
                    eprintln!("error: unknown pin-sign flag {other}");
                    std::process::exit(2);
                }
            }
            i += 1;
        }

        if let Some(path) = generate_key {
            let seed = okra_host::plugins::signing::generate_seed()
                .unwrap_or_else(|e| {
                    eprintln!("error: CSPRNG unavailable: {e}");
                    std::process::exit(1);
                });
            write_private_0600(&path, &seed);
            println!(
                "{}",
                serde_json::json!({
                    "keyFile": path.to_string_lossy(),
                    "publicKey": okra_host::managed_policy::public_key_hex(seed),
                })
            );
            std::process::exit(0);
        }

        let policy = policy.unwrap_or_else(|| {
            eprintln!("error: pin-sign requires --policy PATH");
            std::process::exit(2);
        });
        let seed = if let Some(hex) = key_hex {
            okra_host::managed_policy::parse_signing_seed(hex.as_bytes())
                .unwrap_or_else(|e| {
                    eprintln!("error: --key: {e}");
                    std::process::exit(2);
                })
        } else if let Some(path) = key_file {
            let raw = std::fs::read(&path).unwrap_or_else(|e| {
                eprintln!("error: key file {}: {e}", path.display());
                std::process::exit(2);
            });
            okra_host::managed_policy::parse_signing_seed(&raw).unwrap_or_else(|e| {
                eprintln!("error: {}: {e}", path.display());
                std::process::exit(2);
            })
        } else {
            eprintln!("error: pin-sign requires --key HEX or --key-file PATH");
            std::process::exit(2);
        };
        let payload = std::fs::read_to_string(&policy).unwrap_or_else(|e| {
            eprintln!("error: policy {}: {e}", policy.display());
            std::process::exit(2);
        });
        let envelope = okra_host::managed_policy::sign_policy_envelope(seed, &payload)
            .unwrap_or_else(|e| {
                eprintln!("error: {e}");
                std::process::exit(1);
            });
        let rendered = serde_json::to_vec_pretty(&envelope).unwrap();
        match out {
            Some(path) => {
                if let Some(parent) = path.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                std::fs::write(&path, &rendered).unwrap_or_else(|e| {
                    eprintln!("error: write {}: {e}", path.display());
                    std::process::exit(1);
                });
            }
            None => {
                use std::io::Write as _;
                std::io::stdout().write_all(&rendered).unwrap();
                println!();
            }
        }
        std::process::exit(0);
    }

    // `okra pin-status [--pin PATH]`: operator view of the managed policy
    // pin — which bytes are in force, from where, under which state.
    if argv.first().map(String::as_str) == Some("pin-status") {
        let mut pin_path: Option<PathBuf> = None;
        let mut trust_file: Option<PathBuf> = None;
        let mut i = 1;
        while i < argv.len() {
            match argv[i].as_str() {
                "--pin" => {
                    i += 1;
                    pin_path = Some(PathBuf::from(argv.get(i).cloned().unwrap_or_default()));
                }
                "--trust-file" => {
                    i += 1;
                    trust_file = Some(PathBuf::from(argv.get(i).cloned().unwrap_or_default()));
                }
                other => {
                    eprintln!("error: unknown pin-status flag {other}");
                    std::process::exit(2);
                }
            }
            i += 1;
        }
        let trusted_signers = trust_file
            .as_deref()
            .and_then(okra_host::load_trusted_signers);
        let default_path = okra_host::fsutil::home_dir()
            .unwrap_or_else(|| PathBuf::from("/"))
            .join(".okra")
            .join("managed-policy.json");
        let path = pin_path.unwrap_or(default_path);
        let pin = okra_host::load_managed_pin_verified(&path, trusted_signers.as_deref());
        let prov = pin.provenance.clone();
        println!(
            "{}",
            serde_json::json!({
                "state": pin.state,
                "path": path.to_string_lossy(),
                "sha256": prov.as_ref().map(|p| p.sha256.clone()),
                "source": prov.as_ref().map(|p| p.source.clone()),
                "sandboxCeiling": pin.sandbox_ceiling().map(|c| serde_json::to_value(c).unwrap_or_default()),
                "approvalMustAsk": if pin.approval_must_ask() { Some(true) } else { None },
                "signatureVerified": prov.as_ref().and_then(|p| p.signature_verified),
                // trust is three-valued: Some(only when a trust list is
                // provisioned AND the verified signer is on it); no trust
                // file = UNKNOWN, never silently "trusted"
                "signerTrusted": pin
                    .provenance
                    .as_ref()
                    .and_then(|prov| {
                        prov.signature_verified
                            .map(|verified| (verified, prov.signer.clone()))
                    })
                    .and_then(|(verified, signer)| {
                        trusted_signers.as_ref().map(|list| {
                            verified
                                && signer.as_deref().is_some_and(|s| {
                                    list.iter().any(|t| t.eq_ignore_ascii_case(s))
                                })
                        })
                    }),
                "diagnostics": pin.diagnostics,
            })
        );
        std::process::exit(0);
    }

    // `okra export-replay SESSION_ID [--cwd DIR] [--sessions DIR] -o OUT.html`:
    // render a kernel session log as a standalone mobile-friendly HTML replay
    if argv.first().map(String::as_str) == Some("export-replay") {
        let mut cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let mut sessions_dir: Option<PathBuf> = None;
        let mut out: Option<PathBuf> = None;
        let mut session_id: Option<String> = None;
        let mut i = 1;
        while i < argv.len() {
            match argv[i].as_str() {
                "--cwd" => {
                    i += 1;
                    cwd = PathBuf::from(argv.get(i).cloned().unwrap_or_default());
                }
                "--sessions" => {
                    i += 1;
                    sessions_dir = Some(PathBuf::from(argv.get(i).cloned().unwrap_or_default()));
                }
                "-o" | "--output" => {
                    i += 1;
                    out = Some(PathBuf::from(argv.get(i).cloned().unwrap_or_default()));
                }
                other if session_id.is_none() && !other.starts_with('-') => {
                    session_id = Some(other.to_string());
                }
                other => {
                    eprintln!("error: unknown export-replay flag {other}");
                    std::process::exit(2);
                }
            }
            i += 1;
        }
        let Some(session_id) = session_id else {
            eprintln!("error: usage: okra export-replay <session-id> [-o OUT.html] [--cwd DIR] [--sessions DIR]");
            std::process::exit(2);
        };
        let sessions_dir =
            sessions_dir.unwrap_or_else(|| cwd.join(".okra-sessions"));
        let html = okra_host::export_session_replay(&sessions_dir, &session_id)
            .unwrap_or_else(|e| {
                eprintln!("error: {e}");
                std::process::exit(1);
            });
        match out {
            Some(path) => {
                std::fs::write(&path, &html).unwrap_or_else(|e| {
                    eprintln!("error: write {}: {e}", path.display());
                    std::process::exit(1);
                });
                println!("wrote {} ({} bytes)", path.display(), html.len());
            }
            None => print!("{html}"),
        }
        std::process::exit(0);
    }

    // `okra serve --tcp ADDR --cwd DIR [--provider openai] [--model NAME]`:
    // G4 multi-surface daemon + the workbench web UI at GET /
    if argv.first().map(String::as_str) == Some("serve")
        && argv.iter().any(|a| a == "--tcp")
    {
        let mut tcp_cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let addr = String::from("127.0.0.1:0");
        let mut provider: Option<String> = None;
        let mut model: Option<String> = None;
        let mut i = 1;
        while i < argv.len() {
            match argv[i].as_str() {
                "--tcp" => {}
                "--cwd" => {
                    i += 1;
                    tcp_cwd = PathBuf::from(argv.get(i).cloned().unwrap_or_default());
                }
                "--provider" => {
                    i += 1;
                    provider = Some(argv.get(i).cloned().unwrap_or_default());
                }
                "--model" => {
                    i += 1;
                    model = Some(argv.get(i).cloned().unwrap_or_default());
                }
                _ => {}
            }
            i += 1;
        }
        if let Some(p) = &provider
            && p != "openai" {
                eprintln!("error: unknown provider {p} (supported: openai)");
                std::process::exit(2);
            }
        // the daemon refuses to start a provider the managed pin denies —
        // enforcement at the same gate the CLI prompt path uses
        let pin = okra_host::managed_policy::runtime_pin();
        if let Some(p) = &provider
            && !pin.provider_allowed(p)
        {
            eprintln!("error: provider \"{p}\" is denied by the managed policy pin");
            std::process::exit(1);
        }
        let model_name = model.unwrap_or_else(|| "gpt-4o-mini".to_string());
        let (factory, label) = match &provider {
            Some(_) => {
                let f = serve::openai_sampler_factory(model_name.clone())
                    .unwrap_or_else(|e| {
                        eprintln!("error: {e}");
                        std::process::exit(2);
                    });
                (f, format!("openai/{model_name}"))
            }
            None => (serve::demo_sampler_factory(tcp_cwd.clone()), "demo".to_string()),
        };
        if !tcp_cwd.is_dir() {
            eprintln!("error: --cwd {:?} is not a directory", tcp_cwd);
            std::process::exit(2);
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
        eprintln!("[serve-tcp] multi-surface daemon on {bound} (loopback only) sampler={label}");
        eprintln!("[serve-tcp] workbench UI: http://{bound}/");
        let sessions_dir = tcp_cwd.join(".okra-sessions");
        let state = std::sync::Arc::new(serve_tcp::TcpServeState::new(
            tcp_cwd.clone(),
            sessions_dir,
            factory,
            label,
        ));
        serve_tcp::serve_tcp(state, listener);
    }

    // `okra serve --acp --cwd DIR [--sessions DIR]`: ACP agent over stdio
    // (G4 remainder: the editor seam — Zed et al. drive the same daemon)
    let argv: Vec<String> = std::env::args().skip(1).collect();
    if argv.first().map(String::as_str) == Some("serve")
        && argv.iter().any(|a| a == "--acp")
    {
        let mut acp_cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        let mut acp_sessions = None;
        let mut i = 1;
        while i < argv.len() {
            match argv[i].as_str() {
                "--acp" => {}
                "--cwd" => {
                    i += 1;
                    acp_cwd = PathBuf::from(argv.get(i).cloned().unwrap_or_default());
                }
                "--sessions" => {
                    i += 1;
                    acp_sessions = Some(PathBuf::from(argv.get(i).cloned().unwrap_or_default()));
                }
                other => {
                    eprintln!("error: unknown serve flag {other}");
                    std::process::exit(2);
                }
            }
            i += 1;
        }
        let acp_sessions = acp_sessions.unwrap_or_else(|| acp_cwd.join(".okra-sessions"));
        acp::serve_acp(acp_cwd, acp_sessions);
    }

    // `okra serve --stdio --cwd DIR [--sessions DIR]`: G0 daemon mode
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

    let mut args = match parse_args() {
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

    // Managed policy pin (enterprise): ENFORCED, not just reportable — a
    // pin at the managed path clamps the settings this run started with
    // before any model call or tool runs. NotConfigured → zero effect.
    let pin = okra_host::managed_policy::runtime_pin();
    if let okra_host::managed_policy::PinState::FailClosed { reason } = &pin.state {
        eprintln!("[pin] fail-closed: {reason} — managed dimensions at their most restrictive");
    }
    if let Some(p) = &args.provider
        && !pin.provider_allowed(p)
    {
        eprintln!(
            "error: provider \"{p}\" is denied by the managed policy pin (source: {})",
            pin.provenance.as_ref().map(|pr| pr.source.as_str()).unwrap_or("managed")
        );
        std::process::exit(1);
    }
    {
        let (mt, clamped) = pin.clamp_max_turns(args.max_turns as u32);
        if clamped {
            eprintln!("[pin] max-turns clamped to {mt}");
        }
        args.max_turns = mt as usize;
    }

    // Kernel-enforced self-confinement (N0006): irreversible; the process
    // physically cannot leave the workspace (or touch the network under
    // read-only/strict) from this point on.
    let extra_writable: Vec<PathBuf> = vec![sessions_root.clone()];
    let mut sandbox_mode: Option<okra_policy::SandboxMode> = match args.sandbox.as_deref() {
        None | Some("off") => None,
        Some("read-only" | "readonly" | "strict") => Some(okra_policy::SandboxMode::ReadOnly),
        Some("workspace-write") => Some(okra_policy::SandboxMode::WorkspaceWrite),
        Some(other) => {
            eprintln!("error: unknown --sandbox mode {other}");
            std::process::exit(2);
        }
    };
    // pin sandbox ceiling: a requested mode above the ceiling clamps down,
    // and "off" under a ceiling turns confinement ON at the ceiling —
    // unconstrained is more permissive than any ceiling allows.
    if let Some(ceiling) = pin.sandbox_ceiling() {
        let ceiling_mode = match ceiling {
            okra_host::managed_policy::SandboxCeiling::ReadOnly => {
                Some(okra_policy::SandboxMode::ReadOnly)
            }
            okra_host::managed_policy::SandboxCeiling::WorkspaceWrite => {
                Some(okra_policy::SandboxMode::WorkspaceWrite)
            }
            // the kernel's strongest mode (workspace-write) is already at
            // or under this ceiling — nothing to clamp
            okra_host::managed_policy::SandboxCeiling::DangerFullAccess => None,
        };
        if let Some(cm) = ceiling_mode {
            let exceeds = match sandbox_mode {
                None => true,
                Some(m) => m == okra_policy::SandboxMode::WorkspaceWrite && cm == okra_policy::SandboxMode::ReadOnly,
            };
            if exceeds {
                eprintln!("[pin] sandbox clamped to {cm:?} by the managed policy pin");
                sandbox_mode = Some(cm);
            }
        }
    }
    if let Some(mode) = sandbox_mode {
        {
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
    let config = AgentConfig {
        max_steps: args.max_turns,
        unattended: true,
        semantic_judge: okra_agent_core::semantics::from_env_config(),
        ..Default::default()
    };
    let mut agent = Agent::new(config, sampler, Box::new(executor), session);

    // ---- run the turn, streaming NDJSON ----
    let stdout = std::io::stdout();
    let mut sink = stdout.lock();
    let write_event = |ev: &LoopEvent, sink: &mut dyn Write, json: bool, minimal: bool| {
        if json {
            let line = serde_json::to_string(ev).unwrap_or_default();
            let _ = writeln!(sink, "{line}");
        } else if minimal {
            // scrollback mode (#55): one line per event via the pager's
            // plain writer — terminal-friendly, pipe-friendly
            if let Ok(v) = serde_json::to_value(ev)
                && let Some(name) = v["event"].as_str()
                && let Some(line) = okra_tui::minimal_line(name, &v)
            {
                let _ = writeln!(sink, "{line}");
            }
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

    // n0028: CLI turns are continuations too — tiered memory recall, the
    // project skill catalog (path-conditional activation + progressive
    // disclosure), and the world-state head reach the model on every CLI
    // turn, not only in the benchmark harness. Fail-open: absent dirs are
    // simply empty.
    let home = okra_host::fsutil::home_dir().unwrap_or_else(|| args.cwd.clone());
    let memory_reader = okra_memory::TieredReader::new(home, args.cwd.clone());
    let skill_catalog =
        okra_memory::SkillCatalog::load_dir(&args.cwd.join(".okra").join("skills"));
    let mut continuation_ctx = okra_compaction::SessionContext::default();
    let result = agent.run_turn_continuation(
        &mut continuation_ctx,
        &okra_compaction::ScriptedCompactor,
        Some(&memory_reader),
        Some(&skill_catalog),
        &args.prompt,
        &mut |ev| write_event(&ev, &mut sink, args.json, args.minimal),
    );
    let _ = sink.flush();

    // M3 strangler: fold the session into the SQLite task/session index
    // (derived, rebuildable from the kernel log — the durable truth).
    // Per-session replace: other sessions' indexed rows survive.
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
            let _ = db.replace_session(&events, &session_id, &args.cwd.to_string_lossy());
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
