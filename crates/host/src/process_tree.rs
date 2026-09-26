//! Process-tree ownership & termination core (MASTER-PLAN §3 #48, from
//! ZCode `services/process/processTree*.ts`): the platform-neutral logic
//! that decides WHICH processes belong to a spawned runtime and WHEN the
//! cleanup budget expires. OS dispatch (signals / taskkill / timers)
//! stays in the runtime; this module makes that side provable.
//!
//! Contracts ported from the TS sources:
//! - **identity, not bare PID**: a process is claimed only when pid AND
//!   startTime match (pgid too, when known). A reused PID is never an
//!   old runtime member — this is the invariant everything else guards;
//! - **no root identity, no ownership** (`captureProcessTreeSnapshot`):
//!   a half snapshot whose root could not be identified must not exist;
//!   delayed reaping along a bare root PID would claim an unrelated tree
//!   after PID reuse. Root must still be present in the final identity
//!   set or the snapshot is refused;
//! - **detached POSIX group merge**: a detached root is its own process
//!   group leader; descendants that reparent during the `ps` scan are
//!   still reachable via PGID === root pid. Ordinary children (pgid ≠
//!   root) must NOT pull in their host process group;
//! - **Windows exited-root window** (`captureExitedRootDescendantsSnap
//!   shot`): Win32_Process keeps ParentProcessId after the parent dies,
//!   but a reused PID would be misclaimed — candidates' creation time
//!   must fall inside the recorded root lifetime [started, exited];
//! - **empty identities ≠ exited** (`identityVerification`): a failed
//!   process-table query yields `unavailable`, which the terminator must
//!   treat as "unknown", not "gone";
//! - **ownership resolution fail-closed ladder**
//!   (`resolveCurrentOwnedIdentities`): child exited → not owned (filter
//!   known only); known root identity no longer current → not owned; no
//!   known root and root discovery disallowed → fail closed permanently;
//!   fresh root contradicts known root → not owned; otherwise merge
//!   known+fresh and re-verify every identity against the CURRENT table
//!   before signalling — a root exiting between two checks is caught by
//!   the final re-check, never by stale booleans;
//! - **cleanup deadline arithmetic** (`processTreeWaiter.ts`): the
//!   waiter's relative budget is force + (taskkill budget when verified
//!   targets exist) + wait-after-force, clamped against the transport's
//!   absolute cleanup deadline; with NO verified targets the observation
//!   floor is 750ms so a slow-but-successful exit is not misreported as
//!   residue, and the bare-PID signal path is never invented.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

/// A process identity — pid alone never proves ownership.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProcessIdentity {
    pub parent_pid: u32,
    pub pid: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process_group_id: Option<u32>,
    /// POSIX: ps `lstart` (darwin appends `|command:` entropy); Linux
    /// refines to `/proc` start ticks; Windows: .NET CreationDate ticks.
    pub start_time: String,
}

/// Which verification backed a snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum IdentityVerification {
    #[default]
    Verified,
    /// The process-table query failed; empty identities ≠ exited tree.
    Unavailable,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProcessTreeSnapshot {
    pub root_pid: u32,
    pub descendant_pids: Vec<u32>,
    pub identities: Vec<ProcessIdentity>,
    #[serde(default)]
    pub identity_verification: IdentityVerification,
}

/// Result of re-resolving which processes are still ours.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OwnershipResolution {
    pub child_still_owned: bool,
    pub current_identities: Vec<ProcessIdentity>,
    pub known_identities: Vec<ProcessIdentity>,
}

// ---------------------------------------------------------------------------
// process-table parsing (processTreeSnapshot.ts)
// ---------------------------------------------------------------------------

fn parse_u32_strict(value: &str) -> Option<u32> {
    value.parse::<u32>().ok().filter(|n| *n > 0)
}

fn parse_u32_non_negative(value: &str) -> Option<u32> {
    value.parse::<u32>().ok()
}

/// Parse `ps -axo pid=,ppid=,pgid=,lstart=,command=` output. On darwin
/// the full command is appended to start_time as extra reuse entropy
/// (second-granularity lstart alone is too weak); Linux later overrides
/// start_time with /proc start ticks.
pub fn parse_posix_process_list(stdout: &str, include_command: bool) -> Vec<ProcessIdentity> {
    let mut identities = Vec::new();
    for line in stdout.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        if fields.len() < 8 {
            continue;
        }
        let Some(pid) = parse_u32_strict(fields[0]) else {
            continue;
        };
        let Some(parent_pid) = parse_u32_non_negative(fields[1]) else {
            continue;
        };
        let Some(process_group_id) = parse_u32_strict(fields[2]) else {
            continue;
        };
        let lstart = fields[3..8].join(" ");
        let start_time = if include_command {
            format!("{}|command:{}", lstart, fields[8..].join(" "))
        } else {
            lstart
        };
        if start_time.is_empty() {
            continue;
        }
        identities.push(ProcessIdentity {
            parent_pid,
            pid,
            process_group_id: Some(process_group_id),
            start_time,
        });
    }
    identities
}

/// Parse Windows `"{pid} {ppid} {CreationDate.Ticks}"` rows.
pub fn parse_windows_process_list(stdout: &str) -> Vec<ProcessIdentity> {
    let mut identities = Vec::new();
    for line in stdout.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        let [pid_text, parent_text, start_text] = fields.as_slice() else {
            continue;
        };
        let (Some(pid), Some(parent_pid)) =
            (parse_u32_strict(pid_text), parse_u32_non_negative(parent_text))
        else {
            continue;
        };
        if start_text.is_empty() {
            continue;
        }
        identities.push(ProcessIdentity {
            parent_pid,
            pid,
            process_group_id: None,
            start_time: start_text.to_string(),
        });
    }
    identities
}

const DOTNET_UNIX_EPOCH_TICKS: u64 = 621_355_968_000_000_000;
const TICKS_PER_MILLISECOND: u64 = 10_000;

/// .NET `DateTime.Ticks` (year 0001 epoch) → unix epoch ms.
pub fn parse_windows_creation_time_ms(start_time: &str) -> Option<u64> {
    let ticks = start_time.parse::<u64>().ok()?;
    let millis = ticks.saturating_sub(DOTNET_UNIX_EPOCH_TICKS) / TICKS_PER_MILLISECOND;
    Some(millis)
}

/// Descendants of `root_pid` by parent chain; the seen-set also protects
/// against cycles in hostile table data.
pub fn collect_descendant_identities(
    root_pid: u32,
    identities: &[ProcessIdentity],
) -> Vec<ProcessIdentity> {
    let mut children_by_parent: BTreeMap<u32, Vec<&ProcessIdentity>> = BTreeMap::new();
    for identity in identities {
        children_by_parent
            .entry(identity.parent_pid)
            .or_default()
            .push(identity);
    }
    let mut descendants = Vec::new();
    let mut seen: BTreeSet<u32> = BTreeSet::from([root_pid]);
    let mut queue = std::collections::VecDeque::from([root_pid]);
    while let Some(pid) = queue.pop_front() {
        for child in children_by_parent.get(&pid).into_iter().flatten() {
            if seen.contains(&child.pid) {
                continue;
            }
            seen.insert(child.pid);
            descendants.push((*child).clone());
            queue.push_back(child.pid);
        }
    }
    descendants
}

/// Capture the full owned tree (`captureProcessTreeSnapshot`).
///
/// `root_is_group_leader` models the POSIX detached-spawn case (the
/// runtime owns the root's process group). Returns `None` when the root
/// cannot be identified in the CURRENT table — a half snapshot would let
/// delayed reaping claim an unrelated tree after PID reuse — or when the
/// root vanished from the refined identity set.
pub fn capture_process_tree_snapshot(
    root_pid: u32,
    process_list: &[ProcessIdentity],
    root_is_group_leader: bool,
) -> Option<ProcessTreeSnapshot> {
    let root_identity = process_list.iter().find(|i| i.pid == root_pid)?;
    let mut identities: BTreeMap<u32, ProcessIdentity> = BTreeMap::new();
    for descendant in collect_descendant_identities(root_pid, process_list) {
        identities.insert(descendant.pid, descendant);
    }
    // POSIX detached root: merge remaining group members reachable by
    // PGID === root pid ONLY (an ordinary child's host group must not be
    // absorbed).
    if root_is_group_leader && root_identity.process_group_id == Some(root_pid) {
        for identity in process_list
            .iter()
            .filter(|i| i.process_group_id == Some(root_pid))
        {
            identities.insert(identity.pid, identity.clone());
        }
    }
    identities.insert(root_pid, root_identity.clone());
    if !identities.contains_key(&root_pid) {
        return None;
    }
    let descendant_pids = identities
        .keys()
        .copied()
        .filter(|pid| *pid != root_pid)
        .collect();
    Some(ProcessTreeSnapshot {
        root_pid,
        descendant_pids,
        identities: identities.into_values().collect(),
        identity_verification: IdentityVerification::Verified,
    })
}

/// Capture members of an owned POSIX process group
/// (`captureProcessGroupSnapshot`): valid pgid > 0, POSIX-only (callers
/// pass `false` on Windows).
pub fn capture_process_group_snapshot(
    process_group_id: u32,
    process_list: &[ProcessIdentity],
    posix: bool,
) -> Option<ProcessTreeSnapshot> {
    if !posix || process_group_id == 0 {
        return None;
    }
    let identities: Vec<ProcessIdentity> = process_list
        .iter()
        .filter(|i| i.process_group_id == Some(process_group_id))
        .cloned()
        .collect();
    if identities.is_empty() {
        return None;
    }
    let descendant_pids = identities
        .iter()
        .map(|i| i.pid)
        .filter(|pid| *pid != process_group_id)
        .collect();
    Some(ProcessTreeSnapshot {
        root_pid: process_group_id,
        descendant_pids,
        identities,
        identity_verification: IdentityVerification::Verified,
    })
}

/// Windows-only: descendants of an EXITED root. ParentProcessId survives
/// the root, but a reused PID must never be claimed — candidates' creation
/// time must fall inside the recorded root lifetime
/// (`captureExitedRootDescendantsSnapshot`).
pub fn capture_exited_root_descendants_snapshot(
    root_pid: u32,
    started_at_ms: u64,
    exited_at_ms: u64,
    process_list: &[ProcessIdentity],
    windows: bool,
) -> Option<ProcessTreeSnapshot> {
    if !windows || root_pid == 0 {
        return None;
    }
    let candidates: Vec<ProcessIdentity> = process_list
        .iter()
        .filter(|identity| {
            parse_windows_creation_time_ms(&identity.start_time)
                .map(|created| created >= started_at_ms && created <= exited_at_ms)
                .unwrap_or(false)
        })
        .cloned()
        .collect();
    let identities = collect_descendant_identities(root_pid, &candidates);
    if identities.is_empty() {
        return None;
    }
    Some(ProcessTreeSnapshot {
        root_pid,
        descendant_pids: identities.iter().map(|i| i.pid).collect(),
        identities,
        identity_verification: IdentityVerification::Verified,
    })
}

/// Re-verify tracked identities against the CURRENT table
/// (`filterCurrentProcessIdentities`): same pid, SAME start_time, and
/// pgid must still match when the known identity recorded one.
pub fn filter_current_identities(
    known: &[ProcessIdentity],
    current_list: &[ProcessIdentity],
) -> Vec<ProcessIdentity> {
    let tracked: BTreeSet<u32> = known.iter().map(|i| i.pid).collect();
    let current_by_pid: BTreeMap<u32, &ProcessIdentity> = current_list
        .iter()
        .filter(|i| tracked.contains(&i.pid))
        .map(|i| (i.pid, i))
        .collect();
    known
        .iter()
        .filter(|identity| {
            current_by_pid.get(&identity.pid).is_some_and(|current| {
                current.start_time == identity.start_time
                    && identity
                        .process_group_id
                        .is_none_or(|pgid| current.process_group_id == Some(pgid))
            })
        })
        .cloned()
        .collect()
}

fn same_process_identity(left: &ProcessIdentity, right: &ProcessIdentity) -> bool {
    left.pid == right.pid
        && left.start_time == right.start_time
        && left.process_group_id == right.process_group_id
}

fn merge_identities(groups: &[&[ProcessIdentity]]) -> Vec<ProcessIdentity> {
    let mut by_pid: BTreeMap<u32, ProcessIdentity> = BTreeMap::new();
    for group in groups {
        for identity in *group {
            by_pid.insert(identity.pid, identity.clone());
        }
    }
    by_pid.into_values().collect()
}

// ---------------------------------------------------------------------------
// ownership resolution (processTreeOwnership.ts)
// ---------------------------------------------------------------------------

/// Inputs for one ownership resolution pass. The runtime supplies the
/// child's exit state and a fresh tree capture (when the child is still
/// alive) alongside the current process table.
pub struct OwnershipInputs<'a> {
    pub root_pid: u32,
    pub child_has_exited: bool,
    pub known_identities: &'a [ProcessIdentity],
    /// Fresh capture taken while the child was alive (`None` when the
    /// snapshot could not be taken or the child had exited).
    pub fresh_identities: Option<&'a [ProcessIdentity]>,
    pub current_list: &'a [ProcessIdentity],
    /// Whether discovering the root identity from a bare pid is allowed
    /// (only the very first pass after spawn).
    pub allow_root_discovery: bool,
}

/// Resolve which processes are still owned
/// (`resolveCurrentOwnedIdentities`), fail-closed at every rung:
/// exited child → known-only; root identity stale → not owned; no known
/// root without discovery allowance → permanently nothing; fresh root
/// contradicting the known root → not owned; otherwise merge + re-verify
/// everything against the current table, and only a final root hit
/// counts as owned.
pub fn resolve_current_owned_identities(inputs: OwnershipInputs<'_>) -> OwnershipResolution {
    let known_root = inputs
        .known_identities
        .iter()
        .find(|i| i.pid == inputs.root_pid)
        .cloned();

    if inputs.child_has_exited {
        return OwnershipResolution {
            child_still_owned: false,
            current_identities: filter_current_identities(inputs.known_identities, inputs.current_list),
            known_identities: inputs.known_identities.to_vec(),
        };
    }

    if let Some(known_root) = &known_root {
        let root_still_current =
            !filter_current_identities(std::slice::from_ref(known_root), inputs.current_list).is_empty();
        if !root_still_current {
            return OwnershipResolution {
                child_still_owned: false,
                current_identities: filter_current_identities(inputs.known_identities, inputs.current_list),
                known_identities: inputs.known_identities.to_vec(),
            };
        }
    }

    if known_root.is_none() && !inputs.allow_root_discovery {
        // the original PID may have been reused between passes; claiming
        // along the bare pid now would adopt an unrelated tree — fail
        // closed for this reap forever
        return OwnershipResolution::default();
    }

    let fresh = inputs.fresh_identities.unwrap_or(&[]);
    let fresh_root = fresh.iter().find(|i| i.pid == inputs.root_pid).cloned();
    if known_root.is_none() && fresh_root.is_none() {
        return OwnershipResolution::default();
    }
    if let (Some(known_root), Some(fresh_root)) = (&known_root, &fresh_root)
        && !same_process_identity(known_root, fresh_root)
    {
        return OwnershipResolution {
            child_still_owned: false,
            current_identities: filter_current_identities(inputs.known_identities, inputs.current_list),
            known_identities: inputs.known_identities.to_vec(),
        };
    }

    let merged = if fresh_root.is_some() {
        merge_identities(&[inputs.known_identities, fresh])
    } else {
        inputs.known_identities.to_vec()
    };
    let current_identities = filter_current_identities(&merged, inputs.current_list);
    OwnershipResolution {
        // the root may have exited between the earlier check and the fresh
        // capture — only the FINAL re-verified set may be signalled
        child_still_owned: current_identities.iter().any(|i| i.pid == inputs.root_pid),
        known_identities: merged,
        current_identities,
    }
}

// ---------------------------------------------------------------------------
// cleanup deadline arithmetic (processTreeWaiter.ts)
// ---------------------------------------------------------------------------

pub const DEFAULT_FORCE_AFTER_MS: u64 = 2_000;
pub const DEFAULT_WAIT_AFTER_FORCE_MS: u64 = 250;
pub const DEFAULT_WINDOWS_TASKKILL_TIMEOUT_MS: u64 = 2_000;
/// Floor for the target-less observation path: a slow-but-real exit must
/// not be misreported as persistent residue, and no bare-PID signal may
/// be invented to shorten it.
pub const WINDOWS_LATE_EXIT_OBSERVATION_MS: u64 = 750;

/// Compute the waiter's cleanup deadline in ms from its start
/// (`processTreeWaiter.ts` deadline block).
///
/// - `force_after_ms`: graceful budget (`None` → 2000);
/// - `has_verified_targets`: taskkill budget only exists when verified
///   signal targets are known — with none, both taskkill flights would
///   be empty and reserving their budget just wastes the Host phase;
/// - `transport_deadline_at_ms`: absolute epoch-ms bound covering the
///   whole cleanup chain; clamps the relative budget.
pub fn windows_cleanup_deadline_ms(
    waiter_started_at_ms: u64,
    force_after_ms: Option<u64>,
    has_verified_targets: bool,
    wait_after_force_ms: Option<u64>,
    transport_deadline_at_ms: Option<u64>,
) -> u64 {
    let force_after = force_after_ms.unwrap_or(DEFAULT_FORCE_AFTER_MS);
    let wait_after_force = wait_after_force_ms.unwrap_or(DEFAULT_WAIT_AFTER_FORCE_MS);
    let taskkill_budget = if has_verified_targets {
        DEFAULT_WINDOWS_TASKKILL_TIMEOUT_MS.max(1)
    } else {
        0
    };
    let relative = force_after + taskkill_budget + wait_after_force;
    let transport_remaining = transport_deadline_at_ms
        .map(|deadline| deadline.saturating_sub(waiter_started_at_ms))
        .unwrap_or(relative);
    // without verified targets, at least observe WINDOWS_LATE_EXIT_OBSERVATION_MS
    let observation_floor = WINDOWS_LATE_EXIT_OBSERVATION_MS.max(relative);
    if !has_verified_targets && transport_deadline_at_ms.is_some() {
        observation_floor.min(transport_remaining)
    } else {
        relative.min(transport_remaining)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ident(pid: u32, parent: u32, pgid: Option<u32>, start: &str) -> ProcessIdentity {
        ProcessIdentity {
            parent_pid: parent,
            pid,
            process_group_id: pgid,
            start_time: start.into(),
        }
    }

    #[test]
    fn posix_parsing_keeps_lstart_and_darwin_command_entropy() {
        let linux = parse_posix_process_list(
            "  42 1 42 Mon Sep 28 09:15:00 2026 /usr/bin/node server.js\n  bad-line\n",
            false,
        );
        assert_eq!(linux.len(), 1);
        assert_eq!(linux[0].pid, 42);
        assert_eq!(linux[0].parent_pid, 1);
        assert_eq!(linux[0].process_group_id, Some(42));
        assert_eq!(linux[0].start_time, "Mon Sep 28 09:15:00 2026");

        let darwin = parse_posix_process_list(
            "  42 1 42 Mon Sep 28 09:15:00 2026 /usr/bin/node server.js\n",
            true,
        );
        assert_eq!(
            darwin[0].start_time,
            "Mon Sep 28 09:15:00 2026|command:/usr/bin/node server.js"
        );
    }

    #[test]
    fn windows_parsing_and_tick_conversion() {
        let rows = parse_windows_process_list("11 4 133911360000000000\nbad\n12 4 noticks\n");
        // TS keeps rows with an unparsable start_time; the creation-window
        // filter drops them later (parse_windows_creation_time_ms -> None)
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].pid, 11);
        assert_eq!(rows[0].process_group_id, None);
        assert_eq!(rows[0].start_time, "133911360000000000");
        assert_eq!(rows[1].pid, 12);
        assert_eq!(rows[1].start_time, "noticks");
        // round-trip: build .NET ticks from a known unix-ms and parse back
        // (2026-09-27T00:00:00Z = 1790467200000 ms)
        let unix_ms: u64 = 1_790_467_200_000;
        let ticks = (DOTNET_UNIX_EPOCH_TICKS + unix_ms * TICKS_PER_MILLISECOND).to_string();
        assert_eq!(parse_windows_creation_time_ms(&ticks), Some(unix_ms));
        assert_eq!(parse_windows_creation_time_ms("noticks"), None);
    }

    #[test]
    fn descendants_follow_parent_chain_and_resist_cycles() {
        let list = vec![
            ident(1, 0, Some(1), "a"),
            ident(10, 1, Some(1), "b"),
            ident(11, 10, Some(1), "c"),
            ident(20, 7, Some(7), "d"),
        ];
        let ids = collect_descendant_identities(1, &list);
        let pids: Vec<u32> = ids.iter().map(|i| i.pid).collect();
        assert_eq!(pids, [10, 11]);
        // hostile cycle: 2 → 3 → 2 must terminate
        let cyclic = vec![ident(2, 3, None, "x"), ident(3, 2, None, "y")];
        assert!(collect_descendant_identities(2, &cyclic).len() <= 2);
    }

    #[test]
    fn snapshot_refuses_rootless_tables_but_merges_owned_groups() {
        let list = vec![
            ident(1, 0, Some(1), "a"),
            ident(10, 1, Some(10), "b"), // detached root, own group leader
            ident(11, 10, Some(10), "c"),
            ident(99, 777, Some(10), "z"), // orphan: parent gone, group 10 keeps it
            ident(50, 10, Some(1), "h"),   // chain child living in the HOST group
        ];
        let snap =
            capture_process_tree_snapshot(10, &list, true).expect("root 10 is identified");
        assert_eq!(snap.root_pid, 10);
        assert!(snap.identities.iter().any(|i| i.pid == 99)); // orphan claimed via pgid
        assert!(snap.identities.iter().any(|i| i.pid == 50)); // chain child; its host group is irrelevant
        let pids: BTreeSet<u32> = snap.descendant_pids.into_iter().collect();
        assert_eq!(pids, BTreeSet::from([11, 50, 99]));

        // ordinary (non-leader) root: pgid merge must not fire
        let plain = capture_process_tree_snapshot(50, &list, false).unwrap();
        assert_eq!(plain.descendant_pids, Vec::<u32>::new());

        // root missing → NO snapshot at all (no bare-pid claims)
        assert!(capture_process_tree_snapshot(777, &list, true).is_none());
    }

    #[test]
    fn group_snapshot_is_posix_only_and_positive() {
        let list = vec![ident(10, 1, Some(10), "a"), ident(11, 10, Some(10), "b")];
        let snap = capture_process_group_snapshot(10, &list, true).unwrap();
        assert_eq!(snap.root_pid, 10);
        assert_eq!(snap.descendant_pids, [11]);
        assert!(capture_process_group_snapshot(0, &list, true).is_none());
        assert!(capture_process_group_snapshot(10, &list, false).is_none());
    }

    #[test]
    fn exited_root_candidates_are_windowed_by_creation_time() {
        // started 1000, exited 2000; child created 1500 → claimed;
        // 500 (before root) and 2500 (after exit, a REUSED pid) → refused
        let mk = |pid: u32, parent: u32, created_ms: u64| ProcessIdentity {
            parent_pid: parent,
            pid,
            process_group_id: None,
            start_time: (DOTNET_UNIX_EPOCH_TICKS + created_ms * TICKS_PER_MILLISECOND).to_string(),
        };
        let list = vec![
            mk(10, 1, 1_000),  // the exited root itself
            mk(11, 10, 1_500), // real descendant
            mk(12, 10, 500),   // too old
            mk(13, 10, 2_500), // pid reuse after root exit
        ];
        let snap = capture_exited_root_descendants_snapshot(10, 1_000, 2_000, &list, true).unwrap();
        assert_eq!(snap.descendant_pids, [11]);
        // non-Windows and invalid roots never snapshot
        assert!(capture_exited_root_descendants_snapshot(10, 1_000, 2_000, &list, false).is_none());
        assert!(capture_exited_root_descendants_snapshot(0, 1_000, 2_000, &list, true).is_none());
    }

    #[test]
    fn identity_filter_requires_same_start_time_and_group() {
        let known = vec![
            ident(10, 1, Some(10), "t1"),
            ident(11, 10, None, "t2"), // pgid unknown at spawn time
            ident(12, 10, Some(12), "t3"),
            ident(13, 10, Some(13), "t4"),
        ];
        let current = vec![
            ident(10, 1, Some(10), "t1"),   // same identity: kept
            ident(11, 10, Some(11), "t2"),  // pgid now known + equal: kept
            ident(12, 10, Some(99), "t3"),  // pgid changed: dropped
            ident(13, 10, Some(13), "t9"),  // start_time changed (reuse): dropped
            ident(14, 10, Some(14), "t14"), // never tracked: ignored
        ];
        let kept = filter_current_identities(&known, &current);
        let pids: Vec<u32> = kept.iter().map(|i| i.pid).collect();
        assert_eq!(pids, [10, 11]);
    }

    #[test]
    fn ownership_fails_closed_on_every_stale_rung() {
        let known = vec![ident(10, 1, Some(10), "t1"), ident(11, 10, Some(10), "t2")];
        let fresh = vec![
            ident(10, 1, Some(10), "t1"),
            ident(11, 10, Some(10), "t2"),
            ident(12, 10, Some(10), "t3"),
        ];
        let current: Vec<ProcessIdentity> = fresh.clone();

        // child exited: not owned, known identities still filtered
        let res = resolve_current_owned_identities(OwnershipInputs {
            root_pid: 10,
            child_has_exited: true,
            known_identities: &known,
            fresh_identities: Some(&fresh),
            current_list: &current,
            allow_root_discovery: true,
        });
        assert!(!res.child_still_owned);
        assert_eq!(res.current_identities.len(), 2);

        // known root start_time no longer current (PID reused): not owned
        let reused = vec![ident(10, 1, Some(10), "OTHER"), ident(11, 10, Some(10), "t2")];
        let res = resolve_current_owned_identities(OwnershipInputs {
            root_pid: 10,
            child_has_exited: false,
            known_identities: &known,
            fresh_identities: Some(&fresh),
            current_list: &reused,
            allow_root_discovery: true,
        });
        assert!(!res.child_still_owned); // root identity stale → never signalled
        assert_eq!(res.current_identities.len(), 1); // descendant 11 remains owned

        // no known root + discovery disallowed → permanent fail closed
        let res = resolve_current_owned_identities(OwnershipInputs {
            root_pid: 10,
            child_has_exited: false,
            known_identities: &[],
            fresh_identities: Some(&fresh),
            current_list: &current,
            allow_root_discovery: false,
        });
        assert_eq!(res, OwnershipResolution::default());

        // fresh root contradicts known root → not owned (but known set kept)
        let diverged = vec![ident(10, 1, Some(10), "t1-again"), ident(11, 10, Some(10), "t2")];
        let res = resolve_current_owned_identities(OwnershipInputs {
            root_pid: 10,
            child_has_exited: false,
            known_identities: &known,
            fresh_identities: Some(&diverged),
            current_list: &current,
            allow_root_discovery: true,
        });
        assert!(!res.child_still_owned);
        assert_eq!(res.known_identities, known);
    }

    #[test]
    fn ownership_discovers_root_only_and_reverifies_at_the_end() {
        // first pass: no known identities, discovery allowed → fresh root claims tree
        let fresh = vec![ident(10, 1, Some(10), "t1"), ident(11, 10, Some(10), "t2")];
        let current = fresh.clone();
        let res = resolve_current_owned_identities(OwnershipInputs {
            root_pid: 10,
            child_has_exited: false,
            known_identities: &[],
            fresh_identities: Some(&fresh),
            current_list: &current,
            allow_root_discovery: true,
        });
        assert!(res.child_still_owned);
        assert_eq!(res.current_identities.len(), 2);

        // root vanished from the CURRENT table between checks: the final
        // re-verification must not report ownership from the fresh capture
        let current_gone = vec![ident(11, 10, Some(10), "t2")];
        let res = resolve_current_owned_identities(OwnershipInputs {
            root_pid: 10,
            child_has_exited: false,
            known_identities: &fresh,
            fresh_identities: Some(&fresh),
            current_list: &current_gone,
            allow_root_discovery: true,
        });
        assert!(!res.child_still_owned);
        assert_eq!(res.current_identities.len(), 1); // 11 still ours
        assert_eq!(res.known_identities.len(), 2); // merged set retained
    }

    #[test]
    fn cleanup_deadline_matches_waiter_arithmetic() {
        let start = 10_000;
        // verified targets: force(2000) + taskkill(2000) + wait(250)
        assert_eq!(
            windows_cleanup_deadline_ms(start, None, true, None, None),
            4_250
        );
        // no verified targets: no taskkill budget
        assert_eq!(
            windows_cleanup_deadline_ms(start, None, false, None, None),
            2_250
        );
        // transport deadline clamps the relative budget
        assert_eq!(
            windows_cleanup_deadline_ms(start, None, true, None, Some(10_000 + 1_500)),
            1_500
        );
        // transport already expired → 0
        assert_eq!(
            windows_cleanup_deadline_ms(start, None, true, None, Some(9_999)),
            0
        );
        // target-less observation floor: relative (2250) is above the 750
        // floor, and the transport clamp applies — floor.min(remaining)
        assert_eq!(
            windows_cleanup_deadline_ms(start, None, false, None, Some(10_000 + 500)),
            500
        );
        // with NO transport deadline the relative budget applies as-is
        // (TS: min(relative, relative)) — the 750ms floor only exists to
        // keep the target-less observation path alive inside an absolute
        // transport deadline
        assert_eq!(
            windows_cleanup_deadline_ms(start, Some(0), false, Some(0), None),
            0
        );
        assert_eq!(
            windows_cleanup_deadline_ms(
                start,
                Some(0),
                false,
                Some(0),
                Some(start + WINDOWS_LATE_EXIT_OBSERVATION_MS)
            ),
            WINDOWS_LATE_EXIT_OBSERVATION_MS
        );
    }

    #[test]
    fn identities_serialize_with_zcode_wire_fields() {
        let identity = ident(10, 1, Some(10), "t1");
        let json = serde_json::to_value(&identity).unwrap();
        assert_eq!(json["pid"], 10);
        assert_eq!(json["parentPid"], 1);
        assert_eq!(json["processGroupId"], 10);
        assert_eq!(json["startTime"], "t1");

        let snap = ProcessTreeSnapshot {
            identity_verification: IdentityVerification::Unavailable,
            ..Default::default()
        };
        let json = serde_json::to_value(&snap).unwrap();
        assert_eq!(json["identityVerification"], "unavailable");
        // a failed table query must be distinguishable from "tree exited"
        assert_ne!(snap.identity_verification, IdentityVerification::Verified);
    }
}
