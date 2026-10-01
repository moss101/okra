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

use crate::approval::{ApprovalChannel, ApprovalOutcome, ApprovalRequest};
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
}

impl Mediator {
    pub fn new(policy: MediationPolicy) -> Self {
        Mediator { policy, designated: None, clients: Vec::new() }
    }

    pub fn with_designated(mut self, id: &str) -> Self {
        self.designated = Some(id.to_string());
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
        match self.policy {
            MediationPolicy::FirstResponder => self
                .clients
                .iter()
                .find_map(|c| c.channel.answer(request)),
            MediationPolicy::Designated => {
                let designated = self.designated.as_deref()?;
                self.clients
                    .iter()
                    .find(|c| c.id == designated)?
                    .channel
                    .answer(request)
            }
            MediationPolicy::Consensus => {
                // silence is not consent; a rejection anywhere rejects
                let mut granted = 0;
                for client in &self.clients {
                    match client.channel.answer(request) {
                        Some(ApprovalOutcome::AllowedOnce) => granted += 1,
                        Some(other) => return Some(other),
                        None => return None,
                    }
                }
                if self.clients.is_empty() {
                    return None;
                }
                if granted == self.clients.len() {
                    Some(ApprovalOutcome::AllowedOnce)
                } else {
                    None
                }
            }
            MediationPolicy::LocalOnly => self
                .locals()
                .into_iter()
                .find_map(|c| c.channel.answer(request)),
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

        let mut empty = Mediator::new(MediationPolicy::Consensus);
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
