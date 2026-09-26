//! Steering — port of grok `xai-interjection-core` + shell interjection
//! handling (`interjection.rs:332`, `:40-44`, `:87-96`).
//!
//! - Arrivals during a RUNNING turn are buffered; the turn loop drains them
//!   at step boundaries and injects as user content.
//! - An interjection arriving while idle (or after the final drain) becomes
//!   a front-of-queue fallback prompt (id prefix `interject-fallback-`).

use okra_providers::Message;
use std::sync::mpsc::{self, Receiver, Sender};

#[derive(Debug, Clone, PartialEq)]
pub struct Tagged {
    pub interjection: PendingInterjection,
    pub submitted_while_running: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PendingInterjection {
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SteeringRoute {
    /// Buffer into the running turn (drained at the next step boundary).
    Buffered,
    /// No turn running: front-of-queue fallback prompt.
    FallbackPrompt,
}

/// The steering inbox: an mpsc queue plus a "turn running" flag the loop
/// toggles. `drain` returns buffered interjections in arrival order.
pub struct SteeringInbox {
    rx: Receiver<Tagged>,
    tx: Sender<Tagged>,
    turn_running: bool,
}

impl Default for SteeringInbox {
    fn default() -> Self {
        Self::new()
    }
}

impl SteeringInbox {
    pub fn new() -> Self {
        let (tx, rx) = mpsc::channel();
        SteeringInbox { rx, tx, turn_running: false }
    }

    pub fn sender(&self) -> Sender<Tagged> {
        self.tx.clone()
    }

    /// Submit from any thread. Routing is decided at submit time and
    /// recorded on the entry (grok: buffered only if a turn is actually
    /// running; idle arrivals become front-of-queue fallback prompts).
    pub fn submit(&self, text: impl Into<String>) -> SteeringRoute {
        let tagged = Tagged {
            interjection: PendingInterjection { text: text.into() },
            submitted_while_running: self.turn_running,
        };
        let route = if tagged.submitted_while_running {
            SteeringRoute::Buffered
        } else {
            SteeringRoute::FallbackPrompt
        };
        // senders never block (unbounded channel)
        let _ = self.tx.send(tagged);
        route
    }

    pub fn set_turn_running(&mut self, running: bool) {
        self.turn_running = running;
    }

    /// Non-blocking drain of steering submitted WHILE a turn runs. Idle
    /// submissions stay queued for `take_fallback_prompts` (the loop calls
    /// this at step boundaries).
    pub fn drain(&self) -> Vec<PendingInterjection> {
        let mut out = Vec::new();
        let mut requeue = Vec::new();
        while let Ok(tagged) = self.rx.try_recv() {
            if tagged.submitted_while_running {
                out.push(tagged.interjection);
            } else {
                requeue.push(tagged);
            }
        }
        for r in requeue {
            let _ = self.tx.send(r);
        }
        out
    }

    /// Idle arrivals that became front-of-queue prompts (grok
    /// `interject-fallback-` turns). Called by the scheduler when idle.
    pub fn take_fallback_prompts(&self) -> Vec<PendingInterjection> {
        let mut out = Vec::new();
        while let Ok(tagged) = self.rx.try_recv() {
            if !tagged.submitted_while_running {
                out.push(tagged.interjection);
            }
        }
        out
    }
}

/// Format drained interjections as a synthetic user message (grok
/// `drain_formatted`, buffer.rs): the text wraps as user content the model
/// sees at the step boundary.
pub fn format_as_user_message(interjections: &[PendingInterjection]) -> Option<Message> {
    if interjections.is_empty() {
        return None;
    }
    let joined = interjections
        .iter()
        .map(|i| i.text.clone())
        .collect::<Vec<_>>()
        .join("\n");
    Some(Message::user(joined))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buffered_while_running_fallback_when_idle() {
        let mut inbox = SteeringInbox::new();
        assert_eq!(inbox.submit("hello?"), SteeringRoute::FallbackPrompt);
        inbox.set_turn_running(true);
        assert_eq!(inbox.submit("stop, wrong file"), SteeringRoute::Buffered);
        // drain inside the running turn sees ONLY the steering submission;
        // the idle one stays a fallback prompt
        let drained = inbox.drain();
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].text, "stop, wrong file");
        inbox.set_turn_running(false);
        assert_eq!(inbox.submit("and one more"), SteeringRoute::FallbackPrompt);
        // idle arrivals become front-of-queue prompts (interject-fallback)
        let fallbacks = inbox.take_fallback_prompts();
        assert_eq!(
            fallbacks.iter().map(|i| i.text.as_str()).collect::<Vec<_>>(),
            vec!["hello?", "and one more"]
        );
    }

    #[test]
    fn drain_formats_one_user_message() {
        let mut inbox = SteeringInbox::new();
        inbox.set_turn_running(true);
        inbox.submit("a");
        inbox.submit("b");
        let msg = format_as_user_message(&inbox.drain()).unwrap();
        assert_eq!(msg.text_content(), "a\nb");
        assert!(format_as_user_message(&[]).is_none());
    }
}
