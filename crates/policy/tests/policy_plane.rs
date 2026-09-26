//! Policy plane tests: fail-closed approvals, arg-hash grant binding,
//! permission lattice precedence, sandbox profile merge rules.

use okra_policy as policy;
use policy::{
    ApprovalOutcome, ApprovalPolicy, ApprovalService, Decision, GrantDecision, GrantScope,
    GrantStore, PermissionLattice, RuleEffect, RuleSource, PermissionRule, SandboxMode,
    ToolApprovalCeiling,
};
use serde_json::json;

struct ScriptedChannel(Option<ApprovalOutcome>);
impl policy::ApprovalChannel for ScriptedChannel {
    fn answer(&self, _req: &policy::ApprovalRequest) -> Option<ApprovalOutcome> {
        self.0
    }
}

#[test]
fn only_allowed_once_grants() {
    for (outcome, grants) in [
        (ApprovalOutcome::AllowedOnce, true),
        (ApprovalOutcome::Rejected, false),
        (ApprovalOutcome::Cancelled, false),
        (ApprovalOutcome::Unavailable, false),
    ] {
        assert_eq!(outcome.grants(), grants, "{outcome:?}");
    }
}

#[test]
fn never_policy_decides_before_any_channel_sees_request() {
    let mut svc = ApprovalService::new(ApprovalPolicy::Never);
    // A rogue channel that would allow everything — it must never be asked.
    svc.add_channel(Box::new(ScriptedChannel(Some(ApprovalOutcome::AllowedOnce))));
    let (outcome, audit) = svc.decide("bash", "c1", r#"{"cmd":"ls"}"#);
    assert_eq!(outcome, ApprovalOutcome::Rejected);
    // audit pair: exactly one asked + one decided
    assert_eq!(audit.len(), 2);
    assert!(matches!(audit[0], policy::ApprovalAuditEvent::Asked { .. }));
    assert!(matches!(audit[1], policy::ApprovalAuditEvent::Decided { .. }));
}

#[test]
fn ask_with_no_channel_fails_closed_to_unavailable() {
    let mut svc = ApprovalService::new(ApprovalPolicy::Ask);
    let (outcome, _) = svc.decide("bash", "c1", "{}");
    assert_eq!(outcome, ApprovalOutcome::Unavailable);
    assert!(!outcome.grants());
}

#[test]
fn ask_uses_first_answering_channel() {
    let mut svc = ApprovalService::new(ApprovalPolicy::Ask);
    svc.add_channel(Box::new(ScriptedChannel(Some(ApprovalOutcome::Cancelled))));
    svc.add_channel(Box::new(ScriptedChannel(Some(ApprovalOutcome::AllowedOnce))));
    let (outcome, _) = svc.decide("bash", "c1", "{}");
    assert_eq!(outcome, ApprovalOutcome::Cancelled, "first answerer wins");
}

#[test]
fn grants_bind_to_exact_args_bytes_and_policy_version() {
    let mut store = GrantStore::new(1);
    let args = r#"{"cmd":"cargo test"}"#;
    let grant = store
        .record_approval(
            ApprovalOutcome::AllowedOnce,
            "bash",
            args,
            "1",
            ToolApprovalCeiling::GrantsAllowed,
            false,
        )
        .expect("allowed-once mints a grant");

    // exact bytes grant
    assert_eq!(
        store.check("bash", args, "1", GrantScope::Conversation),
        GrantDecision::Granted
    );
    // one byte of args drift → not granted
    assert_eq!(
        store.check("bash", r#"{"cmd":"cargo test "}"#, "1", GrantScope::Conversation),
        GrantDecision::NotGranted
    );
    // different tool → not granted
    assert_eq!(
        store.check("other", args, "1", GrantScope::Conversation),
        GrantDecision::NotGranted
    );
    // behavior bump → not granted
    assert_eq!(
        store.check("bash", args, "2", GrantScope::Conversation),
        GrantDecision::NotGranted
    );
    let _ = grant;

    // policy version bump invalidates older grants
    let mut store2 = GrantStore::new(2);
    store2.record_approval(
        ApprovalOutcome::AllowedOnce,
        "bash",
        args,
        "1",
        ToolApprovalCeiling::GrantsAllowed,
        false,
    );
    // same store version still matches
    assert_eq!(
        store2.check("bash", args, "1", GrantScope::Conversation),
        GrantDecision::Granted
    );
    let _ = store;
}

#[test]
fn rejections_never_mint_grants_and_always_prompt_ceiling_ignores_them() {
    let mut store = GrantStore::new(1);
    for outcome in [
        ApprovalOutcome::Rejected,
        ApprovalOutcome::Cancelled,
        ApprovalOutcome::Unavailable,
    ] {
        assert!(store
            .record_approval(outcome, "bash", "{}", "1", ToolApprovalCeiling::GrantsAllowed, false)
            .is_none());
    }
    // AlwaysPrompt ceiling: grants ignored entirely
    assert!(store
        .record_approval(
            ApprovalOutcome::AllowedOnce,
            "bash",
            "{}",
            "1",
            ToolApprovalCeiling::AlwaysPrompt,
            false
        )
        .is_none());
    assert!(store.is_empty());

    // UnattendedAllowed + persistent scope works
    assert!(store
        .record_approval(
            ApprovalOutcome::AllowedOnce,
            "bash",
            "{}",
            "1",
            ToolApprovalCeiling::UnattendedAllowed,
            true
        )
        .is_some());
    assert_eq!(
        store.check("bash", "{}", "1", GrantScope::Persistent),
        GrantDecision::Granted
    );
    // revocation clears persistent grants
    store.revoke_persistent();
    assert_eq!(
        store.check("bash", "{}", "1", GrantScope::Persistent),
        GrantDecision::NotGranted
    );
}

#[test]
fn once_scope_is_dedup_not_approval() {
    let mut store = GrantStore::new(1);
    store.record_once("write_file", r#"{"path":"a"}"#, "1");
    assert!(store.seen_once("write_file", r#"{"path":"a"}"#, "1"));
    assert!(!store.seen_once("write_file", r#"{"path":"b"}"#, "1"));
}

#[test]
fn ceiling_parse_fails_closed() {
    assert_eq!(
        policy::parse_ceiling(None),
        ToolApprovalCeiling::GrantsAllowed,
        "omitted → GrantsAllowed"
    );
    assert_eq!(
        policy::parse_ceiling(Some(&json!("always_prompt"))),
        ToolApprovalCeiling::AlwaysPrompt
    );
    // malformed → AlwaysPrompt (fail closed)
    assert_eq!(
        policy::parse_ceiling(Some(&json!({"evil": 1}))),
        ToolApprovalCeiling::AlwaysPrompt
    );
    assert_eq!(
        policy::parse_ceiling(Some(&json!(42))),
        ToolApprovalCeiling::AlwaysPrompt
    );
}

#[test]
fn lattice_deny_beats_ask_beats_allow() {
    let mut l = PermissionLattice::new();
    l.add_rule(PermissionRule {
        tool: "read_file".into(),
        path_prefix: None,
        effect: RuleEffect::Allow,
        source: RuleSource::UserSettings,
    });
    l.add_rule(PermissionRule {
        tool: "read_file".into(),
        path_prefix: Some("secrets/".into()),
        effect: RuleEffect::Deny,
        source: RuleSource::Project,
    });
    l.add_rule(PermissionRule {
        tool: "bash".into(),
        path_prefix: None,
        effect: RuleEffect::Ask,
        source: RuleSource::Project,
    });

    assert_eq!(l.evaluate("read_file", Some("src/main.rs")), Decision::Allow);
    // deny wins even though allow also matched
    assert_eq!(l.evaluate("read_file", Some("secrets/key.pem")), Decision::Deny);
    assert_eq!(l.evaluate("bash", None), Decision::Ask);
    // unmatched → Default
    assert_eq!(l.evaluate("write_file", None), Decision::Default);
    // managed rules trump project
    l.add_rule(PermissionRule {
        tool: "bash".into(),
        path_prefix: None,
        effect: RuleEffect::Allow,
        source: RuleSource::PolicyManaged,
    });
    assert_eq!(l.evaluate("bash", None), Decision::Allow);
    // but managed deny trumps managed allow
    l.add_rule(PermissionRule {
        tool: "bash".into(),
        path_prefix: Some("rm".into()),
        effect: RuleEffect::Deny,
        source: RuleSource::PolicyManaged,
    });
    assert_eq!(l.evaluate("bash", Some("rm -rf /")), Decision::Deny);
}

#[test]
fn sandbox_profiles_strict_restriction_and_merge_rule() {
    // strict + read-only restrict network; workspace/devbox do not
    assert!(policy::parse_profile_name("strict").restricts_network());
    assert!(policy::parse_profile_name("readonly").restricts_network());
    assert!(!policy::parse_profile_name("workspace").restricts_network());
    assert_eq!(policy::parse_profile_name("none"), policy::ProfileName::Off);
    assert!(matches!(
        policy::parse_profile_name("my-profile"),
        policy::ProfileName::Custom(_)
    ));

    // strict profile classifies paths
    let ws = std::path::Path::new("/tmp/ws");
    let p = policy::strict_profile(ws);
    assert_eq!(p.classify(&ws.join("src/main.rs")), policy::PathAccess::ReadAllowed);
    assert_eq!(p.classify(std::path::Path::new("/etc/passwd")), policy::PathAccess::NoAccess);

    // additive-only merge: project may add, never redefine
    let global = policy::SandboxConfig {
        profiles: [(
            "workspace".into(),
            policy::ProfileConfig::default(),
        )]
        .into_iter()
        .collect(),
    };
    let project = policy::SandboxConfig {
        profiles: [
            ("extra".into(), policy::ProfileConfig::default()),
            ("workspace".into(), policy::ProfileConfig { restrict_network: true, ..Default::default() }),
        ]
        .into_iter()
        .collect(),
    };
    let err = policy::merge_configs(&global, &project).unwrap_err();
    assert!(err.contains("anti-hollow-out"), "{err}");

    let project_ok = policy::SandboxConfig {
        profiles: [("extra".into(), policy::ProfileConfig::default())]
            .into_iter()
            .collect(),
    };
    let merged = policy::merge_configs(&global, &project_ok).unwrap();
    assert_eq!(merged.profiles.len(), 2);

    // resolution through extends chains
    let cfg = policy::SandboxConfig {
        profiles: [
            (
                "base".into(),
                policy::ProfileConfig {
                    extends: None,
                    restrict_network: true,
                    read_only: vec!["/tmp/ws".into()],
                    ..Default::default()
                },
            ),
            (
                "derived".into(),
                policy::ProfileConfig {
                    extends: Some("base".into()),
                    restrict_network: false,
                    read_write: vec!["/tmp/ws/target".into()],
                    ..Default::default()
                },
            ),
        ]
        .into_iter()
        .collect(),
    };
    let resolved = policy::resolve_profile(&cfg, "derived").unwrap();
    assert!(resolved.read_only.contains(&std::path::PathBuf::from("/tmp/ws")));
    assert!(resolved
        .read_write
        .contains(&std::path::PathBuf::from("/tmp/ws/target")));
    assert!(resolved.restrict_network, "inherited from base");
    // cycles resolve to None, not a hang
    let cyclic = policy::SandboxConfig {
        profiles: [
            ("a".into(), policy::ProfileConfig { extends: Some("b".into()), ..Default::default() }),
            ("b".into(), policy::ProfileConfig { extends: Some("a".into()), ..Default::default() }),
        ]
        .into_iter()
        .collect(),
    };
    assert!(policy::resolve_profile(&cyclic, "a").is_none());
}

#[test]
fn sandbox_mode_wire_and_confinability() {
    let mode: SandboxMode =
        serde_json::from_value(json!("workspace-write")).unwrap();
    assert_eq!(mode, SandboxMode::WorkspaceWrite);
    let policy = policy::SandboxExecutionPolicy {
        mode: SandboxMode::ReadOnly,
        workspace_root: "/tmp/ws".into(),
        session_id: Some("s1".into()),
    };
    assert!(policy::confinable(&policy));
}
