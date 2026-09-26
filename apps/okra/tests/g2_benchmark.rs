//! G2 gate test (MASTER-PLAN §4 M2): the agent-continuation benchmark —
//! 100 turns / 800 file reads with compaction wired into the turn loop —
//! must pass: flat post-compaction context, byte-identical prefixes,
//! validated-only summaries, zero emergency passes.

use std::process::Command;

#[test]
fn g2_agent_continuation_benchmark_100_turns_800_reads() {
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
    let verdict: serde_json::Value =
        serde_json::from_str(bench_line.trim_start_matches("BENCH ")).expect("verdict parses");

    assert_eq!(verdict["turns"], 100, "turn count");
    assert_eq!(verdict["reads"], 800, "file read count (100 turns x 8 reads)");
    assert_eq!(verdict["compaction_installs"], 20, "install count");
    assert_eq!(verdict["compaction_prefires"], 20, "prefire count (pass 1)");
    assert_eq!(verdict["emergencies"], 0, "prefire must eliminate emergencies");
    assert_eq!(
        verdict["rejected_summaries"], 0,
        "every installed summary was schema-valid"
    );
    assert_eq!(verdict["prefix_bytes_stable"], true, "byte-identical prefixes");
    assert_eq!(verdict["seed_prefix_stable"], true, "stable seed prefix");
    assert_eq!(verdict["passed"], true);
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
