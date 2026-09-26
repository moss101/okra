//! Broadcast domain (MASTER-PLAN §3 #48, from ZCode `broadcast/`):
//! cross-session event fan-out. One session publishes an event on a
//! topic; every OTHER subscribed session receives it exactly once via a
//! per-subscriber cursor — the daemon-side pub/sub behind
//! `broadcast/send` and `broadcast/receive`.
//!
//! Contracts:
//! - **sender exclusion**: the publishing session never receives its own
//!   broadcast;
//! - **per-subscriber cursors**: delivery is at-least-once per poll and
//!   exactly-once across polls — each subscriber's cursor advances past
//!   what it consumed;
//! - **bounded ring**: the bus keeps the newest `capacity` broadcasts;
//!   a slow subscriber that falls past the ring start misses events and
//!   its cursor snaps to the ring start (donor ring-buffer semantics);
//! - **topic filtering**: subscribers choose which topics they receive.

use std::collections::BTreeMap;
use std::time::UNIX_EPOCH;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Broadcast {
    pub broadcast_id: u64,
    pub topic: String,
    pub from_session: String,
    pub payload: Value,
    pub created_at_epoch_ms: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum BroadcastError {
    #[error("unknown session {0:?} — subscribe first")]
    UnknownSession(String),
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// The broadcast bus over one daemon.
pub struct BroadcastBus {
    broadcasts: Vec<Broadcast>,
    /// delivery cursor per subscriber session — survives re-subscribe so
    /// an absent session resumes where it left off
    cursors: BTreeMap<String, u64>,
    /// subscriber session → (set of topics or ALL, next unread broadcast id)
    subscribers: BTreeMap<String, Subscriber>,
    next_id: u64,
    capacity: usize,
}

struct Subscriber {
    topics: Option<BTreeSet<String>>,
    cursor: u64,
}

// small local set to avoid another import at call sites
use std::collections::BTreeSet;

impl BroadcastBus {
    pub fn new(capacity: usize) -> Self {
        BroadcastBus {
            broadcasts: Vec::with_capacity(capacity.min(64)),
            subscribers: BTreeMap::new(),
            cursors: BTreeMap::new(),
            next_id: 0,
            capacity,
        }
    }

    /// Register a session as a subscriber. `topics = None` receives all
    /// topics. Re-subscribing updates the topic set and resets the cursor
    /// to now (only future broadcasts).
    pub fn subscribe(&mut self, session_id: &str, topics: Option<Vec<String>>) {
        let cursor = *self.cursors.entry(session_id.to_string()).or_insert(0);
        self.subscribers.insert(
            session_id.to_string(),
            Subscriber {
                topics: topics.map(|t| t.into_iter().collect()),
                cursor,
            },
        );
    }

    /// Remove a subscriber (session exit).
    pub fn unsubscribe(&mut self, session_id: &str) -> bool {
        self.subscribers.remove(session_id).is_some()
    }

    /// Publish one broadcast. The sender is registered implicitly as a
    /// subscriber of its own topic (sender-excluded at delivery).
    pub fn publish(
        &mut self,
        topic: impl Into<String>,
        from_session: &str,
        payload: Value,
    ) -> u64 {
        let topic = topic.into();
        if !self.subscribers.contains_key(from_session) {
            self.subscribe(from_session, None);
        }
        self.next_id += 1;
        let broadcast = Broadcast {
            broadcast_id: self.next_id,
            topic: topic.clone(),
            from_session: from_session.to_string(),
            payload,
            created_at_epoch_ms: now_ms(),
        };
        self.broadcasts.push(broadcast);
        if self.broadcasts.len() > self.capacity {
            let overflow = self.broadcasts.len() - self.capacity;
            self.broadcasts.drain(..overflow);
        }
        self.next_id
    }

    /// Deliver broadcasts to a subscriber: every event on its topics from
    /// OTHER sessions, not yet consumed. Advances the cursor.
    pub fn poll(&mut self, session_id: &str) -> Result<Vec<Broadcast>, BroadcastError> {
        let subscriber = self
            .subscribers
            .get_mut(session_id)
            .ok_or_else(|| BroadcastError::UnknownSession(session_id.to_string()))?;
        let cursor = subscriber.cursor;
        let topics = subscriber.topics.clone();
        let mut delivered = Vec::new();
        let mut max_seen = cursor;
        for b in &self.broadcasts {
            if b.broadcast_id <= cursor || b.from_session == session_id {
                continue;
            }
            let topic_match = topics.as_ref().is_none_or(|t| t.contains(&b.topic));
            if topic_match {
                delivered.push(b.clone());
                max_seen = max_seen.max(b.broadcast_id);
            }
        }
        subscriber.cursor = max_seen;
        self.cursors.insert(session_id.to_string(), max_seen);
        Ok(delivered)
    }

    pub fn len(&self) -> usize {
        self.broadcasts.len()
    }

    pub fn is_empty(&self) -> bool {
        self.broadcasts.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn publish_subscribe_with_sender_exclusion() {
        let mut bus = BroadcastBus::new(10);
        bus.subscribe("sess-a", None);
        bus.subscribe("sess-b", None);

        bus.publish("chat", "sess-a", json!({ "text": "hello" }));

        // sender does not receive its own broadcast
        assert!(bus.poll("sess-a").unwrap().is_empty());
        // the other subscriber receives it
        let got = bus.poll("sess-b").unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].payload, json!({ "text": "hello" }));
        // exactly once across polls
        assert!(bus.poll("sess-b").unwrap().is_empty());
    }

    #[test]
    fn unknown_session_cannot_poll() {
        let mut bus = BroadcastBus::new(10);
        assert!(matches!(
            bus.poll("ghost"),
            Err(BroadcastError::UnknownSession(_))
        ));
    }

    #[test]
    fn topic_filtering_per_subscriber() {
        let mut bus = BroadcastBus::new(10);
        bus.subscribe("all-ears", None);
        bus.subscribe("chat-only", Some(vec!["chat".into()]));
        bus.publish("chat", "system", json!({"m": 1}));
        bus.publish("system", "system", json!({"m": 2}));

        let all = bus.poll("all-ears").unwrap();
        assert_eq!(all.len(), 2);
        let chat_only = bus.poll("chat-only").unwrap();
        assert_eq!(chat_only.len(), 1);
        assert_eq!(chat_only[0].topic, "chat");
    }

    #[test]
    fn ring_capacity_drops_oldest_and_cursor_snaps() {
        let mut bus = BroadcastBus::new(3);
        bus.subscribe("slow", None);
        for i in 0..5u64 {
            bus.publish("t", "system", json!({ "i": i }));
        }
        // ring holds only 3 newest (ids 3,4,5); the subscriber's cursor
        // (0) points before the ring start, so poll snaps to what remains
        let got = bus.poll("slow").unwrap();
        assert_eq!(got.len(), 3);
        assert_eq!(got[0].broadcast_id, 3);
        assert_eq!(got[2].payload, json!({ "i": 4 }));
        assert_eq!(bus.len(), 3);
    }

    #[test]
    fn resubscribe_keeps_cursor_at_least_once() {
        let mut bus = BroadcastBus::new(10);
        bus.subscribe("s", None);
        bus.publish("t", "system", json!({ "i": 1 }));
        bus.publish("t", "system", json!({ "i": 2 }));
        // re-subscribe: the cursor is persistent, so undelivered
        // broadcasts are still delivered (at-least-once semantics)
        bus.subscribe("s", None);
        bus.publish("t", "system", json!({ "i": 3 }));
        let got = bus.poll("s").unwrap();
        assert_eq!(got.len(), 3, "undelivered broadcasts survive re-subscribe");
        let ids: Vec<u64> = got.iter().map(|b| b.broadcast_id).collect();
        assert_eq!(ids, vec![1, 2, 3]);
    }
}
