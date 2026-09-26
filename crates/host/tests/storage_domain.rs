//! Storage domain tests (MASTER-PLAN §3 #48, session/task SQLite
//! beyond the kernel mirror): incremental per-session sync without cross
//! wipes, cross-session queries + lifecycle, and the storage catalog.

use okra_host::storage::StorageService;
use okra_kernel as kernel;
use okra_kernel::{SessionHandle, SessionHeader};
use std::path::Path;

fn make_session(sessions_dir: &Path, id: &str, tasks: &[(&str, &str)]) {
    let header = SessionHeader {
        version: kernel::SESSION_FORMAT_VERSION,
        id: id.to_string(),
        created_at: 1.0,
        cwd: "/work".into(),
        parent_session: None,
        is_seeded: false,
    };
    let mut handle = SessionHandle::create(sessions_dir, &header).unwrap();
    for (task_id, title) in tasks {
        // task events are host-domain vocabulary: log-only + ignorable so
        // older readers skip them (vocabulary-growth rule)
        let mut event = kernel::make_log_only_event(
            "task/upserted",
            serde_json::json!({
                "task": {
                    "id": task_id,
                    "kind": "todo",
                    "title": title,
                    "status": "pending",
                    "payload": {}
                }
            }),
            kernel::wall_clock,
        );
        event.ignorable = Some(true);
        handle.append(vec![event]).unwrap();
    }
}

#[test]
fn sync_all_indexes_sessions_without_cross_wipe() {
    let td = tempfile::tempdir().unwrap();
    let sessions_dir = td.path().join(".okra/sessions");
    std::fs::create_dir_all(&sessions_dir).unwrap();
    make_session(&sessions_dir, "sync-a", &[("t1", "first task")]);
    make_session(&sessions_dir, "sync-b", &[("t2", "second task")]);

    let svc = StorageService::open(td.path()).unwrap();
    let indexed = svc.sync_all().unwrap();
    assert_eq!(indexed.len(), 2);
    assert_eq!(svc.list_sessions().unwrap().len(), 2);

    // re-sync ONE session: the other's rows survive (incremental, not
    // the whole-table rebuild)
    make_session(&sessions_dir, "sync-a", &[("t1b", "reindexed task")]);
    svc.sync_session("sync-a").unwrap();
    assert_eq!(svc.list_sessions().unwrap().len(), 2, "no cross wipe");
    let tasks_a = svc.tasks_for_session("sync-a").unwrap();
    assert_eq!(tasks_a.len(), 2, "both appended task events indexed");
    assert!(tasks_a.iter().any(|t| t.id == "t1b"));
    // sess-b's task untouched
    assert_eq!(svc.tasks_for_session("sync-b").unwrap()[0].id, "t2");
}

#[test]
fn queries_filters_and_lifecycle() {
    let td = tempfile::tempdir().unwrap();
    let sessions_dir = td.path().join(".okra/sessions");
    std::fs::create_dir_all(&sessions_dir).unwrap();
    make_session(&sessions_dir, "q-a", &[("t1", "fix flake")]);
    make_session(&sessions_dir, "q-b", &[("t2", "write tests")]);

    let svc = StorageService::open(td.path()).unwrap();
    svc.sync_all().unwrap();

    // workspace filter + search (sync tags the REAL workspace path)
    let ws = td.path().to_string_lossy().into_owned();
    assert_eq!(svc.sessions_in_workspace(&ws).unwrap().len(), 2);
    assert_eq!(svc.sessions_in_workspace("/elsewhere").unwrap().len(), 0);
    svc.set_session_status("q-b", "active").unwrap();
    // title search after setting a searchable title
    svc.search_sessions("nothing-matches").unwrap();

    // task filters across sessions
    let pending = svc.tasks_by_filter(Some("pending"), None).unwrap();
    assert_eq!(pending.len(), 2);
    assert!(svc.tasks_by_filter(Some("completed"), None).unwrap().is_empty());
    let todos = svc.tasks_by_filter(None, Some("todo")).unwrap();
    assert_eq!(todos.len(), 2);

    // status counts
    let counts = svc.task_status_counts("q-a").unwrap();
    assert_eq!(counts, vec![("pending".to_string(), 1)]);

    // archive + forget
    svc.set_session_status("q-a", "archived").unwrap();
    let row = svc
        .list_sessions()
        .unwrap()
        .into_iter()
        .find(|s| s.id == "q-a")
        .unwrap();
    assert_eq!(row.status, "archived");
    svc.forget_session("q-a").unwrap();
    assert_eq!(svc.list_sessions().unwrap().len(), 1);
    assert!(svc.tasks_for_session("q-a").unwrap().is_empty());
}

#[test]
fn catalog_reports_locations() {
    let td = tempfile::tempdir().unwrap();
    let svc = StorageService::open(td.path()).unwrap();
    let catalog = svc.catalog();
    let ids: Vec<&str> = catalog.iter().map(|e| e.id.as_str()).collect();
    assert_eq!(ids, vec!["projection-db", "kernel-sessions", "workspace"]);
    let db_entry = catalog.iter().find(|e| e.id == "projection-db").unwrap();
    assert!(db_entry.exists, "db created on open");
    assert!(db_entry.size_bytes > 0);
}