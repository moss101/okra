//! M6 mobile-replay export: a real kernel session (created + appended
//! through the single-writer handle) exports to a standalone HTML
//! transcript that renders every surface event, escapes untrusted
//! content, and degrades to a placeholder for empty sessions.

use okra_host::replay_export::export_session_replay;
use okra_kernel as kernel;

fn append_surface(handle: &mut kernel::SessionHandle, ty: &str, text: &str) {
    use kernel::SessionEvent;
    handle
        .append(vec![SessionEvent {
            event_type: ty.to_string(),
            seq: 0,
            time: 1_789_600_000_000.0,
            data: serde_json::json!({ "text": text }),
            ignorable: None,
            surface_op: Some(kernel::SurfaceOp::Append),
            source_event_seqs: None,
        }])
        .unwrap();
}

#[test]
fn kernel_session_exports_to_standalone_replay_html() {
    let td = tempfile::tempdir().unwrap();
    let header = kernel::SessionHeader {
        version: kernel::SESSION_FORMAT_VERSION,
        id: "session-replay-test".into(),
        created_at: 1_789_600_000_000.0,
        cwd: "/tmp".into(),
        parent_session: None,
        is_seeded: false,
    };
    let mut handle = kernel::SessionHandle::create(td.path(), &header).unwrap();
    append_surface(&mut handle, "user/message", "summarize notes.md");
    handle
        .append(vec![kernel::make_log_only_event(
            "tool/call",
            serde_json::json!({ "callId": "c1", "tool": "read_file" }),
            || 1_789_600_000_500.0,
        )])
        .unwrap();
    append_surface(&mut handle, "assistant/message", "Notes say: use <okra> & friends");

    let html = export_session_replay(td.path(), "session-replay-test").unwrap();
    assert!(html.starts_with("<!doctype html>"));
    assert!(html.contains("viewport"), "phone-friendly viewport");
    assert!(html.contains("session-replay-test"));
    assert!(html.contains("summarize notes.md"));
    assert!(html.contains("read_file"), "tool machinery rows render");
    // assistant text is escaped: the angle brackets must not survive raw
    assert!(html.contains("use &lt;okra&gt; &amp; friends"), "{html}");
    assert!(!html.contains("use <okra>"), "unescaped content must not appear");

    // 3 events accounted for in the meta line
    assert!(html.contains("3 events"), "{html}");

    // unknown session → honest error, not empty HTML
    let err = export_session_replay(td.path(), "session-never-was").unwrap_err();
    assert!(err.0.contains("open session"), "{err}");

    // empty session exports a placeholder rather than failing
    let header2 = kernel::SessionHeader {
        id: "session-empty".into(),
        ..header.clone()
    };
    let mut empty = kernel::SessionHandle::create(td.path(), &header2).unwrap();
    empty.append(vec![]).unwrap();
    let html = export_session_replay(td.path(), "session-empty").unwrap();
    assert!(html.contains("no events"));
}
