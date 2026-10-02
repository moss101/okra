//! Multi-client permission mediation (MASTER-PLAN §3 #26; qwen
//! `MultiClientPermissionMediator`): who answers a permission prompt
//! when several clients are attached to one daemon.
//!
//! Fail-closed rules, per policy:
//! - `first_responder` — the first channel that answers wins (the
//!   implicit behavior of a shared bridge; made explicit and testable).
//! - `designated` — ONLY the named client's channel is consulted. A
//!   missing designation answers nothing → the ask falls through to the
//!   next channel in the service waterfall (unavailable → deny).
//! - `consensus` — every attached channel must answer `allowed-once`
//!   for the grant; a single rejection rejects; a single silence is NOT
//!   consent (none → the waterfall continues, fail-closed).
//! - `local_only` — remote clients never see the ask; locals answer
//!   first-responder among themselves.
//!
//! Mediation composes with the ApprovalService waterfall: the mediator
//! IS one channel; `None` (no verdict) hands the ask onward.

use crate::approval::{ApprovalAnswer, ApprovalChannel, ApprovalOutcome, ApprovalRequest, ApprovalScope};
use crate::lattice::MediationPolicy;

/// Where a client runs — `local` surfaces (loopback UIs on the same
/// machine) may hold different trust than remote-attached editors.
pub struct Client {
    pub id: String,
    pub local: bool,
    pub channel: Box<dyn ApprovalChannel>,
}

pub struct Mediator {
    pub policy: MediationPolicy,
    /// The `designated` policy's chosen client id.
    pub designated: Option<String>,
    pub clients: Vec<Client>,
    /// Live-attachment probe (n0042): answers whether a client id is
    /// currently attached. When present and the DESIGNATED client is not
    /// attached, the ask answers NOTHING — the designation names a real
    /// surface, not a hope. `Send + Sync` so a daemon can consult its
    /// surface registry from the turn thread.
    pub attached_probe: Option<AttachedProbe>,
}

/// Live-attachment probe: is a client id currently attached?
pub type AttachedProbe = std::sync::Arc<dyn Fn(&str) -> bool + Send + Sync>;

impl Mediator {
    pub fn new(policy: MediationPolicy) -> Self {
        Mediator { policy, designated: None, clients: Vec::new(), attached_probe: None }
    }

    pub fn with_designated(mut self, id: &str) -> Self {
        self.designated = Some(id.to_string());
        self
    }

    pub fn with_attached_probe(mut self, probe: AttachedProbe) -> Self {
        self.attached_probe = Some(probe);
        self
    }

    pub fn add_client(&mut self, id: &str, local: bool, channel: Box<dyn ApprovalChannel>) {
        self.clients.push(Client { id: id.to_string(), local, channel });
    }

    fn locals(&self) -> Vec<&Client> {
        self.clients.iter().filter(|c| c.local).collect()
    }
}

impl ApprovalChannel for Mediator {
    fn answer(&self, request: &ApprovalRequest) -> Option<ApprovalOutcome> {
        self.answer_scoped(request).map(|a| a.outcome)
    }

    /// The scoped path (#53): the scope rides on the answer wherever the
    /// policy has ONE answerer (first-responder, designated, local-only
    /// first local). Consensus keeps its unanimous-outcome rule; the scope
    /// of a consensus allow is the TIGHTEST scope any client claimed — a
    /// widening requires unanimity, a narrowing never blocks the grant.
    fn answer_scoped(&self, request: &ApprovalRequest) -> Option<ApprovalAnswer> {
        match self.policy {
            MediationPolicy::FirstResponder => self
                .clients
                .iter()
                .find_map(|c| c.channel.answer_scoped(request)),
            MediationPolicy::Designated => {
                let designated = self.designated.as_deref()?;
                // the designation must name a LIVE surface: absent → the
                // ask falls through the waterfall (unavailable → deny),
                // never silently answered by someone else
                if let Some(probe) = &self.attached_probe
                    && !probe(designated)
                {
                    return None;
                }
                self.clients
                    .iter()
                    .find(|c| c.id == designated)?
                    .channel
                    .answer_scoped(request)
            }
            MediationPolicy::Consensus => {
                // silence is not consent; a rejection anywhere rejects
                let mut granted = 0;
                let mut tightest = ApprovalScope::Always;
                for client in &self.clients {
                    match client.channel.answer_scoped(request) {
                        Some(a) if a.outcome == ApprovalOutcome::AllowedOnce => {
                            granted += 1;
                            if (a.scope as u8) < (tightest as u8) {
                                tightest = a.scope;
                            }
                        }
                        Some(other) => return Some(ApprovalAnswer::new(other.outcome)),
                        None => return None,
                    }
                }
                if self.clients.is_empty() {
                    return None;
                }
                if granted == self.clients.len() {
                    Some(ApprovalAnswer::scoped(ApprovalOutcome::AllowedOnce, tightest))
                } else {
                    None
                }
            }
            MediationPolicy::LocalOnly => self
                .locals()
                .into_iter()
                .find_map(|c| c.channel.answer_scoped(request)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::approval::ApprovalOutcome as O;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    struct Scripted {
        answer: Option<O>,
        asked: AtomicUsize,
    }
    impl Scripted {
        fn allow() -> Box<Self> {
            Box::new(Scripted { answer: Some(O::AllowedOnce), asked: AtomicUsize::new(0) })
        }
        fn deny() -> Box<Self> {
            Box::new(Scripted { answer: Some(O::Rejected), asked: AtomicUsize::new(0) })
        }
        fn silent() -> Box<Self> {
            Box::new(Scripted { answer: None, asked: AtomicUsize::new(0) })
        }
    }
    impl ApprovalChannel for Scripted {
        fn answer(&self, _req: &ApprovalRequest) -> Option<O> {
            self.asked.fetch_add(1, Ordering::SeqCst);
            self.answer
        }
    }

    fn req() -> ApprovalRequest {
        ApprovalRequest {
            id: "test-approval".to_string(),
            tool_name: "write_file".into(),
            call_id: "c1".into(),
            args_json: "{}".into(),
            reason: None,
        }
    }

    #[test]
    fn first_responder_first_answer_wins_and_later_clients_not_asked() {
        let first = Scripted::allow();
        let second = Scripted::deny();
        let mut m = Mediator::new(MediationPolicy::FirstResponder);
        m.add_client("a", true, first);
        m.add_client("b", false, second);
        assert_eq!(m.answer(&req()), Some(O::AllowedOnce));
        // the loser was never consulted — first RESPONSE wins, in order
    }

    #[test]
    fn designated_only_the_named_client_answers() {
        let outsider = Scripted::allow();
        let mut m = Mediator::new(MediationPolicy::Designated).with_designated("editor");
        m.add_client("workbench", true, Scripted::deny());
        m.add_client("editor", false, outsider);
        // the designated client is silent → no verdict (fail-closed)
        let mut m2 = Mediator::new(MediationPolicy::Designated).with_designated("editor");
        m2.add_client("workbench", true, Scripted::allow());
        m2.add_client("editor", false, Scripted::silent());
        assert_eq!(m2.answer(&req()), None, "missing designation never falls back to another client");
    }

    #[test]
    fn designated_with_an_absent_surface_answers_nothing_even_with_the_bridge_attached() {
        // the probe says the designated surface is NOT live → no verdict,
        // even though the bridge channel would happily answer
        let mut m = Mediator::new(MediationPolicy::Designated)
            .with_designated("workbench")
            .with_attached_probe(Arc::new(|id| id != "workbench"));
        m.add_client("workbench", true, Scripted::allow());
        assert_eq!(m.answer(&req()), None);

        // probe says live → the bridge answers
        let mut live = Mediator::new(MediationPolicy::Designated)
            .with_designated("workbench")
            .with_attached_probe(Arc::new(|id| id == "workbench"));
        live.add_client("workbench", true, Scripted::allow());
        assert_eq!(live.answer(&req()), Some(O::AllowedOnce));

        // no probe configured: the static behavior stands (back-compat)
        let mut noprobe = Mediator::new(MediationPolicy::Designated).with_designated("workbench");
        noprobe.add_client("workbench", true, Scripted::allow());
        assert_eq!(noprobe.answer(&req()), Some(O::AllowedOnce));
    }

    #[test]
    fn designated_without_a_name_answers_nothing() {
        let mut m = Mediator::new(MediationPolicy::Designated);
        m.add_client("a", true, Scripted::allow());
        assert_eq!(m.answer(&req()), None);
    }

    #[test]
    fn consensus_requires_every_client() {
        let mut all_yes = Mediator::new(MediationPolicy::Consensus);
        all_yes.add_client("a", true, Scripted::allow());
        all_yes.add_client("b", false, Scripted::allow());
        assert_eq!(all_yes.answer(&req()), Some(O::AllowedOnce));

        let mut one_silent = Mediator::new(MediationPolicy::Consensus);
        one_silent.add_client("a", true, Scripted::allow());
        one_silent.add_client("b", false, Scripted::silent());
        assert_eq!(one_silent.answer(&req()), None, "silence is not consent");

        let mut one_no = Mediator::new(MediationPolicy::Consensus);
        one_no.add_client("a", true, Scripted::allow());
        one_no.add_client("b", false, Scripted::deny());
        assert_eq!(one_no.answer(&req()), Some(O::Rejected), "one rejection rejects");

        let empty = Mediator::new(MediationPolicy::Consensus);
        assert_eq!(empty.answer(&req()), None, "no clients attached: no verdict");
    }

    #[test]
    fn local_only_never_asks_remotes() {
        let remote = Arc::new(AtomicUsize::new(0));
        let remote2 = Arc::clone(&remote);
        let remote_channel = FixedAsk::new(Some(O::AllowedOnce), remote2);
        let mut m = Mediator::new(MediationPolicy::LocalOnly);
        m.add_client("remote-editor", false, Box::new(remote_channel));
        m.add_client("workbench", true, Scripted::deny());
        assert_eq!(m.answer(&req()), Some(O::Rejected));
        assert_eq!(remote.load(Ordering::SeqCst), 0, "the remote was never asked");
    }

    struct FixedAsk {
        answer: Option<O>,
        asked: Arc<AtomicUsize>,
    }
    impl FixedAsk {
        fn new(answer: Option<O>, asked: Arc<AtomicUsize>) -> Self {
            FixedAsk { answer, asked }
        }
    }
    impl ApprovalChannel for FixedAsk {
        fn answer(&self, _req: &ApprovalRequest) -> Option<O> {
            self.asked.fetch_add(1, Ordering::SeqCst);
            self.answer
        }
    }
}
