//! G2 gate test (MASTER-PLAN §4 M2): the agent-continuation benchmark —
//! 100 turns / 800 file reads with compaction wired into the turn loop —
//! must pass: flat post-compaction context, byte-identical prefixes,
//! validated-only summaries, zero emergency passes.

use std::process::Command;

fn run_bench(extra: &[&str]) -> serde_json::Value {
    let bin = env!("CARGO_BIN_EXE_okra");
    let out = Command::new(bin)
        .args([
            "bench-continuation",
            "--turns",
            "100",
            "--files",
            "8",
            "--reads-per-turn",
            "8",
            "--content-bytes",
            "2048",
            "--limit-tokens",
            "20000",
        ])
        .args(extra)
        .output()
        .expect("spawn okra bench-continuation");
    assert!(
        out.status.success(),
        "benchmark failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let bench_line = stdout
        .lines()
        .find(|l| l.starts_with("BENCH "))
        .expect("BENCH verdict line present");
    serde_json::from_str(bench_line.trim_start_matches("BENCH ")).expect("verdict parses")
}

#[test]
fn g2_agent_continuation_benchmark_100_turns_800_reads() {
    // microcompaction DISABLED: the summary two-pass profile
    let verdict = run_bench(&["--microcompact-at", "off"]);
    assert_eq!(verdict["turns"], 100, "turn count");
    assert_eq!(verdict["reads"], 800, "file read count (100 turns x 8 reads)");
    assert!(
        verdict["compaction_installs"].as_u64().unwrap() >= 20,
        "install count"
    );
    assert_eq!(
        verdict["compaction_installs"], verdict["compaction_prefires"],
        "every install was pre-staged by a prefire"
    );
    assert_eq!(verdict["emergencies"], 0, "prefire must eliminate emergencies");
    assert_eq!(
        verdict["rejected_summaries"], 0,
        "every installed summary was schema-valid"
    );
    assert_eq!(verdict["prefix_bytes_stable"], true, "byte-identical prefixes");
    assert_eq!(verdict["seed_prefix_stable"], true, "stable seed prefix");
    assert_eq!(verdict["passed"], true);
    // #45 hooks + #47 MCP funnel ride the same benchmark (layered profile)
    assert_eq!(
        verdict["mcp_calls"], 100,
        "one use_tool funnel call per turn through the dispatch pipeline"
    );
    let hook_events = verdict["hook_events"].as_u64().unwrap();
    assert!(
        hook_events >= 200,
        "PreToolUse + PostToolUse hooks fire across the run ({hook_events})"
    );
    assert_eq!(verdict["hook_failures"], 0, "hooks never crash the turn");

    // skills (M2): path-conditional activation + progressive disclosure
    assert_eq!(verdict["skills_available"], 2, "two skills in the catalog");
    assert_eq!(
        verdict["skills_activated"], 1,
        "only the bench/*.txt-matching skill activates"
    );
    assert_eq!(
        verdict["skill_index_in_head"], true,
        "L1 skill index folded into the stable head"
    );
    // flat post-compaction context: max usage never ran past the limit
    let max_tokens = verdict["max_context_tokens"].as_u64().unwrap();
    let limit = verdict["limit_tokens"].as_u64().unwrap();
    assert!(
        max_tokens <= limit,
        "context must stay flat: max {max_tokens} vs limit {limit}"
    );
    let final_msgs = verdict["final_context_messages"].as_u64().unwrap();
    assert!(
        final_msgs < 40,
        "post-compaction context is flat: {final_msgs} messages"
    );
}

#[test]
fn g2_m2_blocks_microcompaction_hydration_memory() {
    // default profile: microcompaction layer ON (first line of defense),
    // file-state hydration enabled, tiered memory recall injected
    let verdict = run_bench(&[]);
    assert_eq!(verdict["passed"], true);
    assert_eq!(verdict["turns"], 100);
    assert_eq!(verdict["reads"], 800);

    // #28 microcompaction: bulk evicted, outcomes preserved
    let micro = verdict["microcompactions"].as_u64().unwrap();
    assert!(micro >= 1, "microcompaction ran ({micro})");
    assert!(verdict["evicted_bytes"].as_u64().unwrap() > 0);
    // flatness held anyway
    let max_tokens = verdict["max_context_tokens"].as_u64().unwrap();
    let limit = verdict["limit_tokens"].as_u64().unwrap();
    assert!(max_tokens <= limit, "micro layer kept context flat");

    // #29 hydration: noted files re-read at install, fresh state in head
    let hydrated = verdict["hydrated_files"].as_u64().unwrap();
    assert!(hydrated >= 4, "noted files hydrated at install ({hydrated})");

    // #33 tiered memory recall: injected into the stable head,
    // with the secret from the memory file REDACTED (never leaks)
    assert_eq!(verdict["memory_recall_injected"], true);

    // bounded head churn: one change per world mutation batch, not per turn
    let head_changes = verdict["head_changes"].as_u64().unwrap();
    assert!(
        head_changes <= 10,
        "head churn must be bounded by world mutations, got {head_changes}"
    );
}
