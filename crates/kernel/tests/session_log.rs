//! Crash-recovery + single-writer test suite (MASTER-PLAN day 11-15:
//! "crash-recovery test suite green").

use okra_kernel as kernel;
use kernel::{
    fold_surface, make_event, make_log_only_event, make_replace_event, scan_log,
    SessionAccess, SessionHandle, SessionHeader, SurfaceOp, LOG_FILENAME,
};
use serde_json::json;

fn header(id: &str) -> SessionHeader {
    SessionHeader {
        version: kernel::SESSION_FORMAT_VERSION,
        id: id.to_string(),
        created_at: 1_000.0,
        cwd: "/tmp/proj".into(),
        parent_session: None,
        is_seeded: false,
    }
}

fn root() -> (tempfile::TempDir, std::path::PathBuf) {
    let td = tempfile::tempdir().unwrap();
    let p = td.path().to_path_buf();
    (td, p)
}

fn user_msg(text: &str) -> kernel::SessionEvent {
    make_event("user/message", json!({ "text": text }), kernel::wall_clock)
}

#[test]
fn create_append_reopen_read_back_contiguous() {
    let (_td, root) = root();
    {
        let mut h = SessionHandle::create(&root, &header("s1")).unwrap();
        h.append(vec![user_msg("hello"), user_msg("world")]).unwrap();
        assert_eq!(h.next_seq(), 2);
    }
    let h = SessionHandle::open(&root, "s1", SessionAccess::Read).unwrap();
    let events = h.read_all().unwrap();
    assert_eq!(events.len(), 2);
    assert_eq!(events[0].seq, 0);
    assert_eq!(events[1].seq, 1);
    assert_eq!(events[1].data["text"], "world");
    kernel::check_log(&events).unwrap();
}

#[test]
fn single_writer_second_write_handle_rejected() {
    let (_td, root) = root();
    let _w1 = SessionHandle::create(&root, &header("s2")).unwrap();
    // in-process: second writer refused
    let err = SessionHandle::open(&root, "s2", SessionAccess::Write).unwrap_err();
    assert!(matches!(err, kernel::HandleError::AlreadyOwned));
    // readers work fine next to the writer
    let r = SessionHandle::open(&root, "s2", SessionAccess::Read).unwrap();
    assert_eq!(r.read_all().unwrap().len(), 0);
}

#[test]
fn cross_process_lease_rejected_while_held() {
    let (_td, root) = root();
    {
        let mut w = SessionHandle::create(&root, &header("s3")).unwrap();
        w.append(vec![user_msg("x")]).unwrap();
        // simulate another process: raw flock claim on the same dir
        let err = kernel::claim_write_lease(&root.join("s3")).unwrap_err();
        assert!(matches!(err, kernel::StorageError::AlreadyOwned));
    }
    // after the handle (and its lease) drop, a claim succeeds
    assert!(kernel::claim_write_lease(&root.join("s3")).is_ok());
}

#[test]
fn torn_tail_is_invisible_to_readers_and_truncated_on_first_append() {
    let (_td, root) = root();
    {
        let mut w = SessionHandle::create(&root, &header("s4")).unwrap();
        w.append_durable(vec![user_msg("complete one"), user_msg("complete two")]).unwrap();
    }
    // simulate a crash mid-append: partial JSON, no trailing newline
    let log = root.join("s4").join(LOG_FILENAME);
    {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new().append(true).open(&log).unwrap();
        write!(f, "{{\"type\":\"user/messa").unwrap();
    }

    // reader: torn tail never returned
    let r = SessionHandle::open(&root, "s4", SessionAccess::Read).unwrap();
    let events = r.read_all().unwrap();
    assert_eq!(events.len(), 2, "torn tail must be invisible");

    // writer: truncates torn bytes before its first append
    let mut w = SessionHandle::open(&root, "s4", SessionAccess::Write).unwrap();
    assert!(w.torn_tail_pending());
    w.append(vec![user_msg("after crash")]).unwrap();
    assert!(!w.torn_tail_pending());

    let r = SessionHandle::open(&root, "s4", SessionAccess::Read).unwrap();
    let events = r.read_all().unwrap();
    assert_eq!(events.len(), 3);
    assert_eq!(events[2].seq, 2);
    assert_eq!(events[2].data["text"], "after crash");
    kernel::check_log(&events).unwrap();
}

#[test]
fn mid_file_corrupt_line_is_skipped_and_quarantined_once() {
    let (_td, root) = root();
    {
        let mut w = SessionHandle::create(&root, &header("s5")).unwrap();
        w.append_durable(vec![user_msg("a"), user_msg("b"), user_msg("c")]).unwrap();
    }
    // corrupt the middle line in place (garbage but valid line + newline)
    let log = root.join("s5").join(LOG_FILENAME);
    let raw = std::fs::read(&log).unwrap();
    let text = String::from_utf8(raw).unwrap();
    let mut lines: Vec<String> = text.lines().map(String::from).collect();
    assert_eq!(lines.len(), 3);
    lines[1] = "{not json".to_string();
    std::fs::write(&log, lines.join("\n") + "\n").unwrap();

    let r = SessionHandle::open(&root, "s5", SessionAccess::Read).unwrap();
    assert_eq!(r.read_all().unwrap().len(), 2, "corrupt line skipped");
    assert_eq!(r.corrupt_lines(), 1);
    assert!(!root.join("s5").join(format!("{LOG_FILENAME}.corrupt")).exists(),
        "read-only load must not mutate the session");

    // write path: quarantine copy lands before the first append
    let mut w = SessionHandle::open(&root, "s5", SessionAccess::Write).unwrap();
    assert_eq!(w.corrupt_lines(), 1);
    w.append(vec![user_msg("d")]).unwrap();
    let corrupt = root.join("s5").join(format!("{LOG_FILENAME}.corrupt"));
    assert!(corrupt.exists(), "quarantine copy must exist");
    let qraw = std::fs::read_to_string(&corrupt).unwrap();
    assert_eq!(qraw.lines().count(), 3, "quarantine preserves the raw file");

    let r = SessionHandle::open(&root, "s5", SessionAccess::Read).unwrap();
    let events = r.read_all().unwrap();
    assert_eq!(events.len(), 3);
    assert_eq!(events[2].seq, 3, "seq space stays contiguous over skipped lines");
}

#[test]
fn grok_torn_tail_heal_keeps_torn_record_its_own_corrupt_line() {
    // When a write handle heals a torn tail via append_batch (grok path:
    // prepend "\n" rather than truncate), the torn record stays a single
    // corrupt line. We exercise the storage-layer function directly.
    let (_td, root) = root();
    let dir = root.join("s6");
    std::fs::create_dir_all(&dir).unwrap();
    let log = kernel::JsonlLog::new(dir.clone());
    // seq assignment is the single writer's job; the storage layer takes
    // events as-is, so seqs are explicit here.
    let mut ev = user_msg("one");
    ev.seq = 0;
    log.append_batch(std::slice::from_ref(&ev), kernel::AppendDurability::Durable).unwrap();
    // tear it
    {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new().append(true).open(log.log_path()).unwrap();
        write!(f, "{{\"torn\":").unwrap();
    }
    let mut two = user_msg("two");
    two.seq = 1;
    log.append_batch(&[two], kernel::AppendDurability::Durable).unwrap();

    let scan = scan_log(&log.log_path()).unwrap();
    assert_eq!(scan.events.len(), 2, "torn record is a corrupt line, not fatal");
    assert_eq!(scan.skipped, 1, "the terminated torn record is skipped");
    assert!(!scan.torn_tail, "heal terminated the torn record");
    assert_eq!(scan.next_seq(), 2, "next seq follows the highest logged seq");
}

#[test]
fn non_contiguous_append_rejected() {
    let (_td, root) = root();
    let mut w = SessionHandle::create(&root, &header("s7")).unwrap();
    let mut ev = user_msg("seq stolen");
    ev.seq = 5;
    let err = w.append(vec![ev]).unwrap_err();
    assert!(matches!(err, kernel::HandleError::NonContiguous { .. }));
}

#[test]
fn read_handle_append_rejected() {
    let (_td, root) = root();
    SessionHandle::create(&root, &header("s8")).unwrap();
    let mut r = SessionHandle::open(&root, "s8", SessionAccess::Read).unwrap();
    assert!(matches!(
        r.append(vec![user_msg("nope")]).unwrap_err(),
        kernel::HandleError::ReadOnly
    ));
}

// ---- surface ops ----

#[test]
fn surface_fold_append_and_replace() {
    let events = vec![
        user_msg("v1"),
        make_log_only_event("turn/start", json!({}), kernel::wall_clock),
        make_replace_event(
            "user/message",
            json!({ "text": "v2 (edited)" }),
            vec![0],
            0,
            0,
            kernel::wall_clock,
        ),
    ];
    // validate before folding (seqs assigned as they would be on append)
    for (seq, ev) in events.iter().enumerate() {
        let mut ev = ev.clone();
        ev.seq = seq as u64;
        kernel::validate_event(&ev, &kernel::CORE_EVENT_TYPES).unwrap();
    }
    let mut assigned = events.clone();
    for (i, ev) in assigned.iter_mut().enumerate() {
        ev.seq = i as u64;
    }
    let folded = fold_surface(&assigned).unwrap();
    assert_eq!(folded.nodes.len(), 1, "replace collapses append+replace into one node");
    assert_eq!(folded.nodes[0].event.data["text"], "v2 (edited)");
    assert_eq!(folded.replacements.len(), 1);
    assert_eq!(folded.replacements[0].1, vec![0]);
    // projected messages exclude log-only events
    assert!(folded.projected_messages().iter().all(|e| e.surface_op.is_some()));
}

#[test]
fn tool_result_replace_must_rewrite_exactly_one_node() {
    let mut events = vec![
        make_event("tool/call", json!({ "callId": "c1" }), kernel::wall_clock),
        make_event("tool/result", json!({ "callId": "c1", "out": "big" }), kernel::wall_clock),
        make_event("tool/result", json!({ "callId": "c1", "out": "spilled" }), kernel::wall_clock),
    ];
    for (i, ev) in events.iter_mut().enumerate() {
        ev.seq = i as u64;
    }
    // replace of a RANGE by tool/result is invalid (exactly one node rule)
    let bad = make_replace_event(
        "tool/result",
        json!({ "callId": "c1", "out": "x" }),
        vec![1, 2],
        1,
        2,
        kernel::wall_clock,
    );
    let mut bad = bad;
    bad.seq = 3;
    events.push(bad);
    assert!(matches!(
        fold_surface(&events),
        Err(kernel::SurfaceError::ToolResultRewriteNotSingle { .. })
    ));
}

#[test]
fn surface_op_required_on_surface_events_only() {
    let mut ev = user_msg("x");
    ev.surface_op = None;
    assert!(kernel::validate_event(&ev, &kernel::CORE_EVENT_TYPES).is_err());

    let mut log_only = make_log_only_event("turn/start", json!({}), kernel::wall_clock);
    log_only.surface_op = Some(SurfaceOp::Append);
    assert!(kernel::validate_event(&log_only, &kernel::CORE_EVENT_TYPES).is_err());

    // unknown type without ignorable marker is refused (vocabulary rule)
    let mut unknown = make_log_only_event("brand/new-kind", json!({}), kernel::wall_clock);
    unknown.seq = 1;
    assert!(matches!(
        kernel::validate_event(&unknown, &kernel::CORE_EVENT_TYPES),
        Err(kernel::EventError::UnignorableUnknownType(_))
    ));
    unknown.ignorable = Some(true);
    assert!(kernel::validate_event(&unknown, &kernel::CORE_EVENT_TYPES).is_ok());
}

#[test]
fn invariant_catches_tool_pairing_and_turn_nesting() {
    let mk = |ty: &str, data: serde_json::Value, seq: u64| -> kernel::SessionEvent {
        let mut e = make_log_only_event(ty, data, kernel::wall_clock);
        e.seq = seq;
        e
    };
    // orphan result
    let log = vec![mk("tool/result", json!({ "callId": "x" }), 0)];
    assert!(kernel::check_log(&log).is_err());
    // turn end inside open step
    let log = vec![
        mk("turn/start", json!({}), 0),
        mk("step/start", json!({}), 1),
        mk("turn/end", json!({}), 2),
    ];
    assert!(matches!(
        kernel::check_log(&log),
        Err(kernel::InvariantError::TurnEndInsideStep { .. })
    ));
    // balanced log passes
    let log = vec![
        mk("turn/start", json!({}), 0),
        mk("step/start", json!({}), 1),
        mk("tool/call", json!({ "callId": "a" }), 2),
        mk("tool/result", json!({ "callId": "a" }), 3),
        mk("step/end", json!({}), 4),
        mk("turn/end", json!({}), 5),
    ];
    assert_eq!(kernel::check_log(&log), Ok(()));
}
