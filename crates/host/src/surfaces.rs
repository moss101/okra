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
    #[error("unknown surface {id}")]
    UnknownSurface {
        id: String,
    },
}

/// Default staleness threshold: no heartbeat for 30s → stale.
pub const DEFAULT_STALE_AFTER: Duration = Duration::from_secs(30);

/// Leadership expires when the leader misses renewals for this long.
pub const DEFAULT_LEADER_HOLD: Duration = Duration::from_secs(10);

/// Outcome of a roster/claim: one leader per daemon (the surface that
/// drives turns); everyone else attaches as a follower of the live leader.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", rename_all_fields = "camelCase")]
pub enum LeaderDecision {
    BecameLeader { term: u64 },
    Follower { leader_surface: String, term: u64 },
}

/// The registry. Clock is injectable for deterministic staleness tests.
pub struct SurfaceRegistry {
    now: Instant,
    stale_after: Duration,
    leader_hold: Duration,
    next_id: u64,
    entries: BTreeMap<String, Entry>,
    /// Live leadership: (surface id, term, last renewal). A new claim
    /// succeeds when this is None, detached, or expired — every change
    /// bumps the term so followers can detect a leader change.
    leader: Option<(String, u64, Instant)>,
    next_term: u64,
}

impl SurfaceRegistry {
    pub fn new() -> Self {
        SurfaceRegistry {
            now: Instant::now(),
            stale_after: DEFAULT_STALE_AFTER,
            leader_hold: DEFAULT_LEADER_HOLD,
            next_id: 0,
            entries: BTreeMap::new(),
            leader: None,
            next_term: 0,
        }
    }

    pub fn with_stale_after(mut self, stale_after: Duration) -> Self {
        self.stale_after = stale_after;
        self
    }

    pub fn with_leader_hold(mut self, leader_hold: Duration) -> Self {
        self.leader_hold = leader_hold;
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

    /// Explicit detach (clean client exit). A leader detaching releases
    /// leadership — the next roster/claim elects a new leader (term+1).
    pub fn detach(&mut self, id: &str) -> bool {
        match self.entries.get_mut(id) {
            Some(e) if !e.detached => {
                e.detached = true;
                if let Some((leader_id, _, _)) = &self.leader
                    && leader_id == id
                {
                    self.leader = None;
                }
                true
            }
            _ => false,
        }
    }

    fn leader_live(&self) -> Option<(String, u64, Instant)> {
        match &self.leader {
            Some((id, term, renewed)) => {
                let surface_live = self.entries.get(id).map(|e| !e.detached).unwrap_or(false);
                let fresh = self.now.duration_since(*renewed) <= self.leader_hold;
                if surface_live && fresh {
                    Some((id.clone(), *term, *renewed))
                } else {
                    None
                }
            }
            None => None,
        }
    }

    /// Roster claim: the caller leads if the daemon has no live leader
    /// (none yet, leader detached, or leadership expired without renewal);
    /// otherwise the caller attaches as a follower of that leader.
    pub fn claim_leader(&mut self, surface_id: &str) -> Result<LeaderDecision, SurfaceError> {
        if !self.entries.contains_key(surface_id) {
            return Err(SurfaceError::UnknownSurface {
                id: surface_id.to_string(),
            });
        }
        if let Some((id, term, renewed)) = self.leader_live() {
            let _ = renewed;
            let leader_id = id;
            let term = term;
            if leader_id == surface_id {
                // re-claiming the crown you already hold renews it
                self.leader = Some((leader_id, term, self.now));
                return Ok(LeaderDecision::BecameLeader { term });
            }
            return Ok(LeaderDecision::Follower {
                leader_surface: leader_id,
                term,
            });
        }
        self.next_term += 1;
        let term = self.next_term;
        self.leader = Some((surface_id.to_string(), term, self.now));
        Ok(LeaderDecision::BecameLeader { term })
    }

    /// Leadership renewal (a leader's heartbeat). A leader that stops
    /// renewing for `leader_hold` forfeits to the next claim.
    pub fn renew_leader(&mut self, surface_id: &str) -> bool {
        match &self.leader {
            Some((id, term, _)) if id == surface_id => {
                self.leader = Some((id.clone(), *term, self.now));
                true
            }
            _ => false,
        }
    }

    /// The live leader (id, term), if leadership is held and fresh.
    pub fn leader(&self) -> Option<(String, u64)> {
        self.leader_live()
            .map(|(id, term, _)| (id, term))
    }

    /// Explicit release without detaching the surface.
    pub fn release_leader(&mut self, surface_id: &str) -> bool {
        match &self.leader {
            Some((id, _, _)) if id == surface_id => {
                self.leader = None;
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

    #[test]
    fn leadership_one_leader_followers_and_term_bumps() {
        let mut r = registry().with_leader_hold(Duration::from_secs(10));
        let (a, _) = r.attach(SurfaceKind::Tui, vec![]).unwrap();
        let (b, _) = r.attach(SurfaceKind::Browser, vec!["sse".into()]).unwrap();

        // first claim leads
        assert_eq!(
            r.claim_leader(&a).unwrap(),
            LeaderDecision::BecameLeader { term: 1 }
        );
        assert_eq!(r.leader(), Some((a.clone(), 1)));

        // a second surface follows the live leader
        assert_eq!(
            r.claim_leader(&b).unwrap(),
            LeaderDecision::Follower {
                leader_surface: a.clone(),
                term: 1
            }
        );

        // re-claiming while leading just renews (same term)
        assert_eq!(r.claim_leader(&a).unwrap(), LeaderDecision::BecameLeader { term: 1 });

        // expiry: the leader stops renewing → next claim takes over, term bumps
        r.advance(Duration::from_secs(11));
        assert!(r.leader().is_none(), "expired leadership invisible");
        assert_eq!(
            r.claim_leader(&b).unwrap(),
            LeaderDecision::BecameLeader { term: 2 }
        );

        // renewal keeps a fresh leader in the crown
        let _ = r.renew_leader(&b);
        r.advance(Duration::from_secs(5));
        assert_eq!(r.leader(), Some((b.clone(), 2)), "renewed within hold");
        r.advance(Duration::from_secs(6));
        assert!(r.leader().is_none(), "11s without renewal forfeits");

        // explicit release (surface stays attached) frees the crown;
        // the reclaim itself was a term-consuming leadership change
        let _ = r.claim_leader(&a).unwrap(); // term 3
        assert!(r.release_leader(&a));
        assert!(r.leader().is_none());
        assert_eq!(
            r.claim_leader(&b).unwrap(),
            LeaderDecision::BecameLeader { term: 4 }
        );

        // a leader DETACHING releases leadership
        r.detach(&b);
        assert!(r.leader().is_none());
        assert_eq!(
            r.claim_leader(&a).unwrap(),
            LeaderDecision::BecameLeader { term: 5 }
        );

        // unknown surfaces cannot claim
        assert!(matches!(
            r.claim_leader("surf-999"),
            Err(SurfaceError::UnknownSurface { .. })
        ));
    }
}
