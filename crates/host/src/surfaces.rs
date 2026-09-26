//! Surface registry (MASTER-PLAN §3 #48 — G4 bookkeeping): the
//! daemon-side registry of attached surfaces. Every surface that talks to
//! the daemon — CLI, TUI, browser (HTTP+SSE), Electron — registers on
//! attach, heartbeats while alive, and is detached (or pruned as stale)
//! on exit. Surfaces query the registry to learn who else is attached
//! and what capabilities they carry.
//!
//! Contracts:
//! - **stable ids**: server-assigned, monotonic (`surf-1`, …);
//! - **heartbeats + staleness**: a surface whose last heartbeat is older
//!   than the stale threshold is reported `stale` and may be pruned;
//! - **kind uniqueness policy**: only one interactive TUI per daemon by
//!   default (a second attach evicts the stale one or is refused when
//!   the incumbent is fresh);
//! - **capability query**: surfaces advertise what they support
//!   (e.g. `sse`, `steer`), so the orchestrator can pick delivery paths.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SurfaceKind {
    Cli,
    Tui,
    Browser,
    Electron,
}

impl SurfaceKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            SurfaceKind::Cli => "cli",
            SurfaceKind::Tui => "tui",
            SurfaceKind::Browser => "browser",
            SurfaceKind::Electron => "electron",
        }
    }
}

/// One attached surface.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SurfaceInfo {
    pub id: String,
    pub kind: SurfaceKind,
    /// Capabilities this surface supports (e.g. `sse`, `steer`, `render`).
    pub capabilities: Vec<String>,
    /// Milliseconds since attach.
    pub attached_ms_ago: u64,
    /// Milliseconds since the last heartbeat.
    pub last_seen_ms_ago: u64,
    pub stale: bool,
    pub detached: bool,
}

struct Entry {
    kind: SurfaceKind,
    capabilities: Vec<String>,
    attached: Instant,
    last_seen: Instant,
    detached: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachDecision {
    Attached,
    /// An existing surface of the same kind was stale and got evicted.
    AttachedEvictingStale,
}

#[derive(Debug, thiserror::Error)]
pub enum SurfaceError {
    #[error("a fresh {kind} surface is already attached ({id}); refusing duplicate")]
    DuplicateInteractive {
        kind: &'static str,
        id: String,
    },
}

/// Default staleness threshold: no heartbeat for 30s → stale.
pub const DEFAULT_STALE_AFTER: Duration = Duration::from_secs(30);

/// The registry. Clock is injectable for deterministic staleness tests.
pub struct SurfaceRegistry {
    now: Instant,
    stale_after: Duration,
    next_id: u64,
    entries: BTreeMap<String, Entry>,
}

impl SurfaceRegistry {
    pub fn new() -> Self {
        SurfaceRegistry {
            now: Instant::now(),
            stale_after: DEFAULT_STALE_AFTER,
            next_id: 0,
            entries: BTreeMap::new(),
        }
    }

    pub fn with_stale_after(mut self, stale_after: Duration) -> Self {
        self.stale_after = stale_after;
        self
    }

    /// Advance the injectable clock (tests).
    pub fn advance(&mut self, d: Duration) {
        self.now += d;
    }

    /// Attach a surface. Interactive kinds (TUI) enforce single-attach:
    /// a fresh incumbent refuses a second attach; a stale incumbent is
    /// evicted.
    pub fn attach(
        &mut self,
        kind: SurfaceKind,
        capabilities: Vec<String>,
    ) -> Result<(String, AttachDecision), SurfaceError> {
        if kind == SurfaceKind::Tui {
            for (id, e) in &self.entries {
                if e.kind == SurfaceKind::Tui && !e.detached {
                    if self.is_stale(e) {
                        let id = id.clone();
                        self.detach(&id);
                        let id = self.attach(kind, capabilities)?;
                        return Ok((id.0, AttachDecision::AttachedEvictingStale));
                    }
                    return Err(SurfaceError::DuplicateInteractive {
                        kind: kind.as_str(),
                        id: id.clone(),
                    });
                }
            }
        }
        self.next_id += 1;
        let id = format!("surf-{}", self.next_id);
        self.entries.insert(
            id.clone(),
            Entry {
                kind,
                capabilities,
                attached: self.now,
                last_seen: self.now,
                detached: false,
            },
        );
        Ok((id, AttachDecision::Attached))
    }

    /// Heartbeat: refresh last-seen.
    pub fn heartbeat(&mut self, id: &str) -> bool {
        match self.entries.get_mut(id) {
            Some(e) if !e.detached => {
                e.last_seen = self.now;
                true
            }
            _ => false,
        }
    }

    /// Explicit detach (clean client exit).
    pub fn detach(&mut self, id: &str) -> bool {
        match self.entries.get_mut(id) {
            Some(e) if !e.detached => {
                e.detached = true;
                true
            }
            _ => false,
        }
    }

    fn is_stale(&self, e: &Entry) -> bool {
        !e.detached && self.now.duration_since(e.last_seen) > self.stale_after
    }

    /// All live (non-detached) surfaces, newest attach first; stale ones
    /// are flagged.
    pub fn list(&self) -> Vec<SurfaceInfo> {
        self.entries
            .iter()
            .filter(|(_, e)| !e.detached)
            .map(|(id, e)| SurfaceInfo {
                id: id.clone(),
                kind: e.kind,
                capabilities: e.capabilities.clone(),
                attached_ms_ago: self.now.duration_since(e.attached).as_millis() as u64,
                last_seen_ms_ago: self.now.duration_since(e.last_seen).as_millis() as u64,
                stale: self.is_stale(e),
                detached: false,
            })
            .collect()
    }

    /// Capability query: ids of live surfaces supporting `capability`.
    pub fn surfaces_with(&self, capability: &str) -> Vec<String> {
        self.entries
            .iter()
            .filter(|(_, e)| {
                !e.detached && e.capabilities.iter().any(|c| c == capability)
            })
            .map(|(id, _)| id.clone())
            .collect()
    }
}

impl Default for SurfaceRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registry() -> SurfaceRegistry {
        SurfaceRegistry::new().with_stale_after(Duration::from_secs(30))
    }

    #[test]
    fn attach_heartbeat_detach_lifecycle() {
        let mut r = registry();
        let (id, decision) = r
            .attach(SurfaceKind::Tui, vec!["render".into()])
            .unwrap();
        assert_eq!(decision, AttachDecision::Attached);
        assert_eq!(id, "surf-1");

        assert!(r.heartbeat(&id));
        assert!(!r.heartbeat("surf-unknown"));

        let list = r.list();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].kind, SurfaceKind::Tui);
        assert!(!list[0].stale);

        assert!(r.detach(&id));
        assert!(!r.detach(&id), "idempotent detach");
        assert!(r.list().is_empty());
    }

    #[test]
    fn interactive_tui_enforces_single_attach() {
        let mut r = registry();
        r.attach(SurfaceKind::Tui, vec![]).unwrap();
        // fresh incumbent refuses
        assert!(matches!(
            r.attach(SurfaceKind::Tui, vec![]),
            Err(SurfaceError::DuplicateInteractive { .. })
        ));
        // non-interactive kinds are not constrained
        r.attach(SurfaceKind::Browser, vec!["sse".into()]).unwrap();
        r.attach(SurfaceKind::Cli, vec![]).unwrap();
        assert_eq!(r.list().len(), 3);

        // a stale incumbent gets evicted
        r.advance(Duration::from_secs(31));
        let (id, decision) = r.attach(SurfaceKind::Tui, vec![]).unwrap();
        assert_eq!(decision, AttachDecision::AttachedEvictingStale);
        assert_ne!(id, "surf-1", "a NEW tui surface replaces the stale one");
        let live = r.list();
        assert_eq!(live.len(), 3, "browser+cli remain plus the new tui");
        assert!(live.iter().all(|s| s.id != "surf-1"), "stale tui evicted: {live:?}");
    }

    #[test]
    fn staleness_is_flagged_by_the_clock() {
        let mut r = registry();
        r.attach(SurfaceKind::Browser, vec!["sse".into()]).unwrap();
        r.advance(Duration::from_secs(10));
        assert!(!r.list()[0].stale, "inside threshold");
        r.advance(Duration::from_secs(21));
        assert!(r.list()[0].stale, "31s without heartbeat");
        // heartbeat revives
        let id = r.list()[0].id.clone();
        r.heartbeat(&id);
        assert!(!r.list()[0].stale);
    }

    #[test]
    fn capability_query_filters_live_surfaces() {
        let mut r = registry();
        r.attach(SurfaceKind::Browser, vec!["sse".into(), "steer".into()])
            .unwrap();
        r.attach(SurfaceKind::Cli, vec!["steer".into()]).unwrap();
        let (sse_only, _) = r.attach(SurfaceKind::Electron, vec!["render".into()]).unwrap();
        r.detach(&sse_only);

        assert_eq!(r.surfaces_with("sse"), vec!["surf-1".to_string()]);
        assert_eq!(
            r.surfaces_with("steer"),
            vec!["surf-1".to_string(), "surf-2".to_string()]
        );
        assert!(r.surfaces_with("render").is_empty(), "detached excluded");
    }
}
