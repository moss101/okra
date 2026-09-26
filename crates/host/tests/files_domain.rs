//! File-management domain end-to-end (MASTER-PLAN §3 #52): one
//! conversation's life — context files attached, a download lands with a
//! full audited lifecycle, open files watched — then the conversation is
//! discarded and nothing of it outlives the thread.

use okra_host::files::{
    AttachmentKind, AttachmentOrigin, AttachmentStore, ChangeKind, DownloadState, DownloadStore,
    FileWatchService,
};

#[test]
fn conversation_lifecycle_downloads_attachments_watches() {
    let td = tempfile::tempdir().unwrap();
    let mut downloads = DownloadStore::open(td.path().join("downloads")).unwrap();
    let mut attachments = AttachmentStore::new();
    let mut watches = FileWatchService::new();

    // --- conversation starts: user attaches context + pastes a snippet
    let report = td.path().join("report.md");
    std::fs::write(&report, "# Report\n").unwrap();
    attachments.add(
        "conv-1",
        AttachmentKind::ContextFile {
            path: report.clone(),
            origin: AttachmentOrigin::UserPick,
        },
    );
    attachments.add(
        "conv-1",
        AttachmentKind::PastedText {
            content: "summarize this".into(),
        },
    );
    assert_eq!(attachments.list("conv-1").len(), 2);

    // --- open-file watch on the attachment
    let watch_id = watches.watch("conv-1", vec![report.clone()]);
    assert_eq!(watches.poll(), Vec::new(), "baseline is quiet");

    // --- a dataset download runs its full lifecycle, bound to conv-1
    let data_path = td.path().join("dataset.csv");
    let dl = downloads.begin(
        "conv-1",
        "https://example.com/dataset.csv",
        &data_path,
        Some(100),
    );
    assert_eq!(downloads.unacknowledged_ids(), vec![dl.id.clone()]);
    downloads.progress(&dl.id, 60).unwrap();
    downloads.complete(&dl.id).unwrap();

    // audit trail is total-ordered
    let kinds: Vec<_> = downloads
        .lifecycle(&dl.id)
        .iter()
        .map(|e| e.kind)
        .collect();
    assert_eq!(
        kinds,
        [
            okra_host::files::LifecycleKind::Started,
            okra_host::files::LifecycleKind::Progress,
            okra_host::files::LifecycleKind::Completed
        ]
    );
    assert_eq!(downloads.get(&dl.id).unwrap().state, DownloadState::Completed);

    // --- the edited download lands on disk → the open-file watch fires
    std::fs::write(&report, "# Report v2\nlonger\n").unwrap();
    let changes = watches.poll();
    assert_eq!(changes.len(), 1, "{changes:?}");
    assert_eq!(changes[0].watch_id, watch_id);
    assert_eq!(changes[0].conversation_id, "conv-1");
    assert_eq!(changes[0].kind, ChangeKind::Modified);

    // --- per-thread download surface filters by conversation
    let (live, history) = downloads.list_for_conversation("conv-1");
    assert!(live.is_empty());
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].id, dl.id);
    let (_, hist_other) = downloads.list_for_conversation("conv-other");
    assert!(hist_other.is_empty());

    // --- UI consumes the download: acknowledge flow
    downloads.acknowledge(std::slice::from_ref(&dl.id));
    assert!(downloads.unacknowledged_ids().is_empty());

    // --- conversation discarded: nothing outlives the thread
    assert_eq!(watches.remove_for_conversation("conv-1"), 1);
    assert_eq!(watches.watch_count(), 0);
    assert_eq!(attachments.clear_conversation("conv-1"), 2);
    assert!(attachments.list("conv-1").is_empty());
    // downloads persist by design — history is the audit record
    assert_eq!(downloads.history().len(), 1);
}

#[test]
fn download_history_survives_daemon_restart() {
    let td = tempfile::tempdir().unwrap();
    let root = td.path().join("downloads");
    {
        let mut d = DownloadStore::open(&root).unwrap();
        let dl = d.begin("c9", "https://x/f.bin", td.path().join("f.bin"), Some(5));
        d.fail(&dl.id, "disk full").unwrap();
    }
    let d = DownloadStore::open(&root).unwrap();
    assert_eq!(d.history().len(), 1);
    assert_eq!(d.history()[0].state, DownloadState::Failed);
    assert_eq!(d.search_history("f.bin").len(), 1);
}
