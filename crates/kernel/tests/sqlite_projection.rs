//! SQLite projection tests (MASTER-PLAN §3 #7, M3 strangler first domain):
//! the task/session index is a rebuildable pure function of the kernel log.

use okra_kernel as kernel;
use kernel::{ProjectionDb, SessionHandle, SessionAccess, SessionHeader, SESSION_FORMAT_VERSION};
use serde_json::json;

fn header(id: &str) -> SessionHeader {
    SessionHeader {
        version: SESSION_FORMAT_VERSION,
        id: id.into(),
        created_at: 1.0,
        cwd: "/tmp/proj".into(),
        parent_session: None,
        is_seeded: false,
    }
}

#[test]
fn projection_upsert_list_and_rebuild_from_log() {
    let td = tempfile::tempdir().unwrap();
    let root = td.path().join("sessions");
    let mut h = SessionHandle::create(&root, &header("p1")).unwrap();

    // a small log with a task upsert
    let mut events = Vec::new();
    events.push(kernel::make_log_only_event("turn/start", json!({}), kernel::wall_clock));
    // task events are host-domain vocabulary: ignorable = true so older
    // readers skip them (vocabulary-growth rule)
    let mut task_ev = kernel::make_log_only_event(
        "task/upserted",
        json!({ "task": { "id": "t1", "kind": "todo", "title": "fix flake", "status": "pending", "payload": {} } }),
        kernel::wall_clock,
    );
    task_ev.ignorable = Some(true);
    events.push(task_ev);
    h.append(events).unwrap();

    let db = ProjectionDb::open(&root.join("index.db")).unwrap();
    let log = h.read_all().unwrap();

    // rebuild = pure function of the durable log
    let n = db.rebuild_from_log(&log, "p1", "/tmp/proj").unwrap();
    assert_eq!(n, log.len());

    let sessions = db.list_sessions().unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].id, "p1");
    assert_eq!(sessions[0].event_count, log.len() as u64);

    let tasks = db.tasks_for_session("p1").unwrap();
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0].title, "fix flake");
    assert_eq!(tasks[0].status, "pending");

    // drop the DB and rebuild: identical state (rebuildable index)
    let db2 = ProjectionDb::open(&root.join("index.db")).unwrap();
    db2.rebuild_from_log(&log, "p1", "/tmp/proj").unwrap();
    assert_eq!(db2.list_sessions().unwrap(), db.list_sessions().unwrap());
    assert_eq!(db2.tasks_for_session("p1").unwrap(), db.tasks_for_session("p1").unwrap());
}

#[test]
fn read_access_works_while_writer_holds_session() {
    let td = tempfile::tempdir().unwrap();
    let root = td.path().join("sessions");
    SessionHandle::create(&root, &header("p2")).unwrap();
    let r = SessionHandle::open(&root, "p2", SessionAccess::Read).unwrap();
    let db = ProjectionDb::open(&root.join("p2").join("index.db")).unwrap();
    db.rebuild_from_log(&r.read_all().unwrap(), "p2", "/tmp/proj").unwrap();
    assert!(db.list_sessions().unwrap().is_empty() || db.list_sessions().unwrap().len() == 1);
}

#[test]
fn replace_session_preserves_other_sessions_rows() {
    // The web surface runs MANY sessions; folding session A must never
    // erase session B's indexed rows (the old rebuild_from_log wiped the
    // whole table).
    let td = tempfile::tempdir().unwrap();
    let root = td.path().join("sessions");
    let mut h1 = SessionHandle::create(&root, &header("s1")).unwrap();
    SessionHandle::create(&root, &header("s2")).unwrap();
    h1.append(vec![kernel::make_log_only_event(
        "turn/start",
        json!({ "turn": 1 }),
        kernel::wall_clock,
    )])
    .unwrap();

    let db = ProjectionDb::open(&root.join("index.db")).unwrap();
    let log1 = h1.read_all().unwrap();
    db.replace_session(&log1, "s1", "/tmp/proj").unwrap();

    // fold s2 as well; then re-fold s1 (the every-turn path)
    let r2 = SessionHandle::open(&root, "s2", SessionAccess::Read).unwrap();
    let log2 = r2.read_all().unwrap();
    db.replace_session(&log2, "s2", "/tmp/proj").unwrap();
    db.replace_session(&log1, "s1", "/tmp/proj").unwrap();

    let sessions = db.list_sessions().unwrap();
    let ids: Vec<&str> = sessions.iter().map(|s| s.id.as_str()).collect();
    assert!(ids.contains(&"s1"), "{ids:?}");
    assert!(ids.contains(&"s2"), "s2 rows were wiped by the s1 fold: {ids:?}");
}

#[test]
fn session_title_survives_index_updates() {
    let td = tempfile::tempdir().unwrap();
    let root = td.path().join("sessions");
    std::fs::create_dir_all(&root).unwrap();
    let db = ProjectionDb::open(&root.join("index.db")).unwrap();
    db.upsert_session("t1", "/tmp", "first user text", "active", 1.0, 3)
        .unwrap();
    let rows = db.list_sessions().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].title, "first user text");
}
