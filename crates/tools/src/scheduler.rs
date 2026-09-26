//! ToolAccesses conflict semantics + sync scheduler — port of kimi
//! `packages/agent-core-v2/src/tool/toolContract.ts` (`ToolAccesses`,
//! `conflict`) and `agent/toolExecutor/toolScheduler.ts`, re-expressed for a
//! synchronous runtime (decision N0001).
//!
//! Conflict rule (toolContract.ts:184-215):
//! - `all` conflicts with everything;
//! - otherwise only when **at least one side writes** AND the paths overlap
//!   (equal, or a recursive access's subtree contains the other path).
//!
//! Reads and searches run concurrently; a write serializes against anything
//! overlapping.

use std::collections::HashMap;
use std::collections::VecDeque;
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex};
use serde::{Deserialize, Serialize};

/// `ToolFileAccessOperation` (toolContract.ts:113).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileAccessOperation {
    Read,
    Write,
    Readwrite,
    Search,
}

impl FileAccessOperation {
    /// `fileOperationWrites` (toolContract.ts:201-209).
    pub fn writes(self) -> bool {
        matches!(self, FileAccessOperation::Write | FileAccessOperation::Readwrite)
    }
}

/// `ToolResourceAccess` (toolContract.ts:126-132).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ResourceAccess {
    File {
        operation: FileAccessOperation,
        path: String,
        #[serde(default)]
        recursive: bool,
    },
    All,
}

impl ResourceAccess {
    pub fn file(operation: FileAccessOperation, path: impl Into<String>) -> Self {
        ResourceAccess::File { operation, path: path.into(), recursive: false }
    }
    pub fn tree(operation: FileAccessOperation, path: impl Into<String>) -> Self {
        ResourceAccess::File { operation, path: path.into(), recursive: true }
    }
    pub fn read_file(path: impl Into<String>) -> Self {
        Self::file(FileAccessOperation::Read, path)
    }
    pub fn write_file(path: impl Into<String>) -> Self {
        Self::file(FileAccessOperation::Write, path)
    }
}

/// `ToolAccesses` — the declared footprint of one tool call.
pub type ToolAccesses = Vec<ResourceAccess>;

/// `normalizePath` (toolContract.ts:216-219): backslashes → slashes, collapse
/// repeats, case-fold (TS-side cross-platform semantics kept for fidelity).
pub fn normalize_path(path: &str) -> String {
    let mut normalized = path.replace('\\', "/");
    while normalized.contains("//") {
        normalized = normalized.replace("//", "/");
    }
    normalized.to_lowercase()
}

/// `fileAccessesOverlap` (toolContract.ts:168-180).
fn accesses_overlap(left: &ResourceAccess, right: &ResourceAccess) -> bool {
    let (
        ResourceAccess::File { path: lp, recursive: lr, .. },
        ResourceAccess::File { path: rp, recursive: rr, .. },
    ) = (left, right)
    else {
        return true; // All vs anything overlaps; callers checked All earlier
    };
    let l = normalize_path(lp);
    let r = normalize_path(rp);
    if l == r {
        return true;
    }
    let lprefix = if l.ends_with('/') { l.clone() } else { format!("{l}/") };
    let rprefix = if r.ends_with('/') { r.clone() } else { format!("{r}/") };
    (*lr && r.starts_with(&lprefix)) || (*rr && l.starts_with(&rprefix))
}

/// `resourceAccessesConflict` (toolContract.ts:186-193): conflict only when
/// at least one operation writes AND the accesses overlap.
pub fn resource_accesses_conflict(left: &ResourceAccess, right: &ResourceAccess) -> bool {
    if matches!(left, ResourceAccess::All) || matches!(right, ResourceAccess::All) {
        return true;
    }
    let (
        ResourceAccess::File { operation: lo, .. },
        ResourceAccess::File { operation: ro, .. },
    ) = (left, right)
    else {
        return true;
    };
    if !(lo.writes() || ro.writes()) {
        return false;
    }
    accesses_overlap(left, right)
}

/// `ToolAccesses.conflict` (toolContract.ts:184): any conflicting pair.
pub fn accesses_conflict(left: &[ResourceAccess], right: &[ResourceAccess]) -> bool {
    left.iter().any(|l| right.iter().any(|r| resource_accesses_conflict(l, r)))
}

// ---- the scheduler ----

struct QueuedTask {
    id: u64,
    accesses: ToolAccesses,
    job: Box<dyn FnOnce() + Send>,
}

struct SchedulerState {
    next_id: u64,
    /// Accesses of currently running tasks, by task id.
    active: HashMap<u64, ToolAccesses>,
    queued: VecDeque<QueuedTask>,
}

/// kimi `ToolScheduler`, synchronous: `add` starts the task immediately when
/// nothing it conflicts with is active **or queued before it** (FIFO
/// fairness, `isBlocked`); otherwise it queues and is started by whichever
/// finishing task frees it (`startQueuedTasks`).
#[derive(Clone)]
pub struct ToolScheduler {
    state: Arc<Mutex<SchedulerState>>,
    cv: Arc<Condvar>,
}

impl Default for ToolScheduler {
    fn default() -> Self {
        Self::new()
    }
}

impl ToolScheduler {
    pub fn new() -> Self {
        ToolScheduler {
            state: Arc::new(Mutex::new(SchedulerState {
                next_id: 0,
                active: HashMap::new(),
                queued: VecDeque::new(),
            })),
            cv: Arc::new(Condvar::new()),
        }
    }

    /// Submit a call; completion arrives on the returned receiver. The task
    /// runs on its own thread once started.
    pub fn add<T: Send + 'static>(
        &self,
        accesses: ToolAccesses,
        job: impl FnOnce() -> T + Send + 'static,
    ) -> mpsc::Receiver<T> {
        let (tx, rx) = mpsc::channel();
        let done_tx = tx.clone();

        let mut state = self.state.lock().unwrap();
        let id = state.next_id;
        state.next_id += 1;
        let task = QueuedTask {
            id,
            accesses,
            job: Box::new(move || {
                let value = job();
                let _ = tx.send(value);
                let _ = done_tx; // keep the sender alive until the job ran
            }),
        };

        let blocked_by_active = state
            .active
            .values()
            .any(|a| accesses_conflict(&task.accesses, a));
        let blocked_by_queued = state
            .queued
            .iter()
            .any(|t| accesses_conflict(&task.accesses, &t.accesses));
        if blocked_by_active || blocked_by_queued {
            state.queued.push_back(task);
        } else {
            self.spawn(&mut state, task);
        }
        drop(state);
        rx
    }

    /// Start a task on its own thread. Caller holds the lock.
    fn spawn(&self, state: &mut SchedulerState, task: QueuedTask) {
        state.active.insert(task.id, task.accesses.clone());
        let id = task.id;
        let job = task.job;
        let state_arc = Arc::clone(&self.state);
        let cv = Arc::clone(&self.cv);
        std::thread::spawn(move || {
            job();
            finish(state_arc, cv, id);
        });
    }

    /// Wait until every submitted task (including queued ones) finishes.
    pub fn join(&self) {
        let mut state = self.state.lock().unwrap();
        while !state.active.is_empty() || !state.queued.is_empty() {
            state = self.cv.wait(state).unwrap();
        }
    }

    /// Snapshot of active + queued counts (for tests and status lines).
    pub fn load(&self) -> (usize, usize) {
        let state = self.state.lock().unwrap();
        (state.active.len(), state.queued.len())
    }
}

/// Task completion: drop the active slot, then `startQueuedTasks`
/// (kimi scheduler.ts:86-98) — still-queued tasks keep their order; a queued
/// task starts unless it conflicts with an active task or a queued task
/// still ahead of it.
fn finish(state: Arc<Mutex<SchedulerState>>, cv: Arc<Condvar>, finished_id: u64) {
    let mut st = state.lock().unwrap();
    st.active.remove(&finished_id);
    let mut still_queued = VecDeque::new();
    while let Some(task) = st.queued.pop_front() {
        let blocked_by_active = st
            .active
            .values()
            .any(|a| accesses_conflict(&task.accesses, a));
        let blocked_by_queued = still_queued
            .iter()
            .any(|t: &QueuedTask| accesses_conflict(&task.accesses, &t.accesses));
        if blocked_by_active || blocked_by_queued {
            still_queued.push_back(task);
        } else {
            let QueuedTask { id, accesses, job } = task;
            st.active.insert(id, accesses.clone());
            let s2 = Arc::clone(&state);
            let c2 = Arc::clone(&cv);
            std::thread::spawn(move || {
                job();
                finish(s2, c2, id);
            });
        }
    }
    st.queued = still_queued;
    cv.notify_all();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn conflict_rules_match_kimi_semantics() {
        // read vs read: parallel
        assert!(!accesses_conflict(
            &[ResourceAccess::read_file("a.txt")],
            &[ResourceAccess::read_file("a.txt")]
        ));
        // write vs read same file: conflict
        assert!(accesses_conflict(
            &[ResourceAccess::write_file("a.txt")],
            &[ResourceAccess::read_file("a.txt")]
        ));
        // write vs read different files: parallel
        assert!(!accesses_conflict(
            &[ResourceAccess::write_file("a.txt")],
            &[ResourceAccess::read_file("b.txt")]
        ));
        // recursive write contains a deep read: conflict
        assert!(accesses_conflict(
            &[ResourceAccess::tree(FileAccessOperation::Write, "src")],
            &[ResourceAccess::read_file("src/deep/nested/x.rs")]
        ));
        // recursive read does NOT block a write elsewhere
        assert!(!accesses_conflict(
            &[ResourceAccess::tree(FileAccessOperation::Read, "docs")],
            &[ResourceAccess::write_file("src/main.rs")]
        ));
        // all conflicts with anything
        assert!(accesses_conflict(
            &[ResourceAccess::All],
            &[ResourceAccess::read_file("whatever")]
        ));
        // search never conflicts with read
        assert!(!accesses_conflict(
            &[ResourceAccess::tree(FileAccessOperation::Search, "src")],
            &[ResourceAccess::tree(FileAccessOperation::Read, "src")]
        ));
        // windows separators + case-fold normalization
        assert!(accesses_conflict(
            &[ResourceAccess::write_file("SRC\\Main.rs")],
            &[ResourceAccess::read_file("src/main.rs")]
        ));
    }

    #[test]
    fn reads_run_parallel_writes_serialize() {
        static CONCURRENT: AtomicUsize = AtomicUsize::new(0);
        static PEAK: AtomicUsize = AtomicUsize::new(0);
        let sched = ToolScheduler::new();
        let mut rxs = Vec::new();
        for i in 0..6 {
            let accesses = if i < 4 {
                vec![ResourceAccess::read_file(format!("f{i}.txt"))]
            } else {
                vec![ResourceAccess::write_file("shared.txt")]
            };
            rxs.push(sched.add(accesses, move || {
                let now = CONCURRENT.fetch_add(1, Ordering::SeqCst) + 1;
                PEAK.fetch_max(now, Ordering::SeqCst);
                std::thread::sleep(std::time::Duration::from_millis(30));
                CONCURRENT.fetch_sub(1, Ordering::SeqCst);
                i
            }));
        }
        sched.join();
        for (i, rx) in rxs.into_iter().enumerate() {
            assert_eq!(rx.recv().unwrap(), i);
        }
        // 4 reads overlap freely; the two writes never overlapped with
        // anything (peak during a write is 1).
        assert!(PEAK.load(Ordering::SeqCst) >= 4);
    }

    #[test]
    fn fifo_write_queued_behind_write_runs_after() {
        let sched = ToolScheduler::new();
        let order = Arc::new(Mutex::new(Vec::new()));
        // occupy a reader on the same file first (blocks the first write)
        let order2 = Arc::clone(&order);
        let rx_read = sched.add(
            vec![ResourceAccess::read_file("f.txt")],
            move || {
                std::thread::sleep(std::time::Duration::from_millis(40));
                order2.lock().unwrap().push("read");
            },
        );
        let order3 = Arc::clone(&order);
        let rx_write = sched.add(
            vec![ResourceAccess::write_file("f.txt")],
            move || {
                order3.lock().unwrap().push("write");
            },
        );
        rx_read.recv().unwrap();
        rx_write.recv().unwrap();
        sched.join();
        assert_eq!(*order.lock().unwrap(), vec!["read", "write"]);
    }
}
