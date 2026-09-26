//! Tool plane tests: normalize-before-hooks invariant, stream shape, spill
//! budget, and the read_file builtin end-to-end.

use okra_tools as tools;
use serde_json::json;
use okra_tools::{
    apply_output_budget, retain_text, ErasedTool, HookVerdict, IdentityNormalizer, PipelineError,
    Registry, RegistryError, ResourceAccess, RetainedText, TextRetentionStrategy, ToolStream,
    ToolStreamItem,
};

// -- test doubles --

struct LowercasePathNormalizer;
impl tools::ArgumentNormalizer for LowercasePathNormalizer {
    fn normalize(&self, _tool: &str, args: &serde_json::Value) -> Result<serde_json::Value, PipelineError> {
        let mut out = args.clone();
        if let Some(obj) = out.as_object_mut() {
            if let Some(p) = obj.get("path").and_then(|v| v.as_str()) {
                let trimmed = p.trim().to_string();
                obj.insert("path".into(), json!(trimmed));
            }
        }
        Ok(out)
    }
}

struct AccessDeniedHook {
    deny_paths: Vec<String>,
}
impl tools::PreToolUseHook for AccessDeniedHook {
    fn name(&self) -> &str {
        "access-guard"
    }
    fn on_tool_use(&self, _tool: &str, args: &serde_json::Value) -> HookVerdict {
        if let Some(p) = args.get("path").and_then(|v| v.as_str()) {
            if self.deny_paths.iter().any(|d| p.starts_with(d)) {
                return HookVerdict::Deny { reason: format!("{p} is access-denied") };
            }
        }
        HookVerdict::Allow
    }
}

#[test]
fn normalize_runs_before_hooks_and_bytes_are_frozen() {
    let entry = read_entry();
    // raw args have whitespace the normalizer trims; the hook would deny the
    // DENIED prefix only if normalization ran first (it trims nothing here,
    // so simulate the ordering with a case change).
    struct UppercaseNormalizer;
    impl tools::ArgumentNormalizer for UppercaseNormalizer {
        fn normalize(&self, _t: &str, args: &serde_json::Value) -> Result<serde_json::Value, PipelineError> {
            let mut out = args.clone();
            if let Some(obj) = out.as_object_mut() {
                if let Some(p) = obj.get("path").and_then(|v| v.as_str()) {
                    obj.insert("path".into(), json!(p.to_uppercase()));
                }
            }
            Ok(out)
        }
    }
    struct DenyLowercaseHook;
    impl tools::PreToolUseHook for DenyLowercaseHook {
        fn name(&self) -> &str {
            "case-guard"
        }
        fn on_tool_use(&self, _t: &str, args: &serde_json::Value) -> HookVerdict {
            let p = args["path"].as_str().unwrap_or("");
            if p.chars().any(|c| c.is_lowercase()) {
                HookVerdict::Deny { reason: "saw unnormalized lowercase".into() }
            } else {
                HookVerdict::Allow
            }
        }
    }

    let normalizers: [&dyn tools::ArgumentNormalizer; 1] = [&UppercaseNormalizer];
    let hooks: [&dyn tools::PreToolUseHook; 1] = [&DenyLowercaseHook];
    let approved =
        tools::normalize_before_hooks(&entry, &json!({ "path": "src/main.rs" }), &normalizers, &hooks)
            .expect("normalized args passed the hook");

    // approved bytes carry the NORMALIZED form
    assert_eq!(approved.args_json, r#"{"path":"SRC/MAIN.RS"}"#);
    assert_eq!(approved.approved_by, vec!["case-guard"]);
    // frozen bytes are stable (deep-frozen args)
    assert_eq!(approved.args_json, approved.args().to_string());
}

#[test]
fn hook_deny_carries_reason_and_blocks() {
    let entry = read_entry();
    let normalizers: [&dyn tools::ArgumentNormalizer; 1] = [&IdentityNormalizer];
    let deny = AccessDeniedHook { deny_paths: vec!["secrets/".into()] };
    let hooks: [&dyn tools::PreToolUseHook; 1] = [&deny];
    let err = tools::normalize_before_hooks(
        &entry,
        &json!({ "path": "secrets/key.pem" }),
        &normalizers,
        &hooks,
    )
    .unwrap_err();
    assert!(matches!(err, PipelineError::HookDenied { ref hook, ref reason, .. }
        if hook == "access-guard" && reason.contains("access-denied")));
}

fn read_entry() -> tools::ToolEntry {
    let td = tempfile::tempdir().unwrap();
    std::fs::write(td.path().join("f.txt"), "hello").unwrap();
    // leak the tempdir for the test lifetime
    let path = td.path().to_path_buf();
    std::mem::forget(td);
    tools::builtins::read_file_tool(path).entry()
}

#[test]
fn stream_shape_n_progress_then_one_terminal() {
    let progress = vec![
        okra_tools::ToolProgress::Text { text: "chunk1".into() },
        okra_tools::ToolProgress::Text { text: "chunk2".into() },
    ];
    let s = ToolStream::with_progress(progress, Ok(okra_tools::ToolOutput::text("done")));
    s.validate().unwrap();
    assert_eq!(s.items().len(), 3);
    assert!(matches!(s.items().last(), Some(ToolStreamItem::Terminal(_))));

    // no terminal → protocol violation ("stream_no_terminal", grok dispatch)
    let broken = ToolStream::from_items_unchecked(vec![ToolStreamItem::Progress(
        okra_tools::ToolProgress::Text { text: "dangling".into() },
    )]);
    assert!(broken.validate().is_err());

    // progress after terminal → violation
    let bad_order = ToolStream::from_items_unchecked(vec![
        ToolStreamItem::Terminal(Ok(okra_tools::ToolOutput::text("t"))),
        ToolStreamItem::Progress(okra_tools::ToolProgress::Text { text: "late".into() }),
    ]);
    assert!(bad_order.validate().is_err());
}

#[test]
fn dispatch_validates_stream_and_routes_conflicts() {
    let td = tempfile::tempdir().unwrap();
    std::fs::write(td.path().join("main.rs"), "fn main() {}").unwrap();
    let root = td.path().to_path_buf();

    let mut registry = Registry::new();
    registry.add_normalizer(Box::new(LowercasePathNormalizer));
    registry.add_hook(Box::new(AccessDeniedHook { deny_paths: vec![".git".into()] }));

    let rf = tools::builtins::read_file_tool(root.clone());
    registry
        .register(ErasedTool::simple(rf.entry(), vec![ResourceAccess::read_file("*")], {
            let _root = root.clone();
            move |args| rf.execute(args, None)
        }))
        .unwrap();

    // happy path through normalize → hook → execute
    let stream = registry
        .dispatch("read_file", &json!({ "path": " main.rs " }), &[])
        .expect("dispatch ok");
    let terminal = stream.terminal().unwrap();
    let out = terminal.as_ref().unwrap();
    assert_eq!(out.value["content"], "fn main() {}");

    // hook denial surfaces as Pipeline error
    let err = registry
        .dispatch("read_file", &json!({ "path": ".git/config" }), &[])
        .unwrap_err();
    assert!(matches!(err, RegistryError::Pipeline(PipelineError::HookDenied { .. })));

    // unknown tool
    assert!(matches!(
        registry.dispatch("nope", &json!({}), &[]),
        Err(RegistryError::UnknownTool(_))
    ));

    // stream-protocol violation: a tool returning an empty stream
    let mut broken_entry = read_entry();
    broken_entry.spec.name = "broken".into();
    registry
        .register(ErasedTool::simple(broken_entry, vec![], |_args| {
            ToolStream::from_items_unchecked(vec![])
        }))
        .unwrap();
    let err = registry.dispatch("broken", &json!({}), &[]).unwrap_err();
    assert!(matches!(err, RegistryError::StreamProtocol(_)));
}

#[test]
fn spill_budget_head_tail_with_full_file() {
    let td = tempfile::tempdir().unwrap();
    let store = okra_tools::FsSpillStore::new(td.path().join("spills"));
    let big = "x".repeat(200_000);
    let (inline, spill) = apply_output_budget(
        &store,
        tools::SpillSource::Tool {
            tool_name: "read_file".into(),
            call_id: "c1".into(),
            label: "big.log".into(),
        },
        &big,
        4096,
    );
    assert!(inline.len() < 5000, "inline retained head+tail only");
    assert!(inline.contains("bytes omitted"));
    let spill = spill.expect("over-budget output spills");
    assert_eq!(spill.bytes, 200_000, "full content persisted verbatim");
    let on_disk = std::fs::read_to_string(&spill.locator).unwrap();
    assert_eq!(on_disk.len(), 200_000);

    // small output: no spill
    let (inline2, spill2) =
        apply_output_budget(&store, tools::SpillSource::Tool { tool_name: "t".into(), call_id: "c2".into(), label: "l".into() }, "small", 4096);
    assert_eq!(inline2, "small");
    assert!(spill2.is_none());
}

#[test]
fn retention_cuts_respect_utf8_boundaries() {
    let s = "okrä🦀".repeat(1000); // multi-byte chars
    let RetainedText { text, truncated, omitted_bytes } =
        retain_text(&s, TextRetentionStrategy::HeadTail { head_bytes: 100, tail_bytes: 100 });
    assert!(truncated);
    assert!(omitted_bytes > 0);
    // both halves are valid UTF-8 strings (no replacement chars from cuts)
    assert!(!text.contains('\u{FFFD}'), "cut split a char");
}

#[test]
fn builtin_read_file_confines_to_workspace() {
    let td = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(td.path().join("in.txt"), "inside").unwrap();
    std::fs::write(outside.path().join("out.txt"), "outside").unwrap();
    let tool = tools::builtins::read_file_tool(td.path().to_path_buf());

    // inside works
    let s = tool.execute(&json!({ "path": "in.txt" }), None);
    let out = s.terminal().unwrap().as_ref().unwrap();
    assert_eq!(out.value["content"], "inside");

    // absolute escape rejected
    let s = tool.execute(&json!({ "path": outside.path().join("out.txt") }), None);
    assert!(s.terminal().unwrap().is_err());

    // traversal rejected
    let s = tool.execute(&json!({ "path": "../out.txt" }), None);
    assert!(s.terminal().unwrap().is_err());

    // nonexistent → tool failure, not invalid input
    let s = tool.execute(&json!({ "path": "missing.txt" }), None);
    let err = s.terminal().unwrap().as_ref().unwrap_err();
    assert!(matches!(err, tools::ToolError::ToolFailed { .. }));

    // directory → invalid input
    let s = tool.execute(&json!({ "path": "." }), None);
    assert!(matches!(s.terminal().unwrap(), Err(tools::ToolError::InvalidInput { .. })));
}
