//! Event bus — port of qwen `packages/acp-bridge/eventBus` semantics
//! (MASTER-PLAN §3 #8): ring replay, Last-Event-ID reconnect, slow-client
//! eviction.
//!
//! - Every event gets a monotonically increasing id; a ring buffer retains
//!   the last N (protocol cap: `subscriberBufferMaxOps = 500`,
//!   zcode-protocol-v4 core.ts).
//! - Reconnect with `Last-Event-ID: <id>` replays everything after that id
//!   from the ring (deterministic redelivery — the same guarantee the v4
//!   recovery path gives via the delivery-profile filter).
//! - A client whose unbounded backlog exceeds the byte/ops caps is EVICTED
//!   (disconnected) rather than silently dropping events: the client must
//!   reconnect with Last-Event-ID and replay.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

use okra_protocol::ProtocolV4Limits;

/// One bus event. `payload` is the NDJSON-ready value; `topic` partitions
/// streams.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BusEvent {
    pub id: u64,
    pub topic: String,
    pub payload: serde_json::Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubscribeError {
    /// The requested Last-Event-ID is older than the ring retains: the
    /// client needs a snapshot resync, not a replay.
    EventIdTooOld { oldest_retained: Option<u64> },
}

/// A client's delivery buffer with byte/ops caps.
struct ClientBuffer {
    events: Vec<BusEvent>,
    bytes: usize,
}

impl ClientBuffer {
    fn push(&mut self, ev: BusEvent) -> Result<(), ()> {
        let size = serde_json::to_vec(&ev.payload).map(|v| v.len()).unwrap_or(64);
        if self.events.len() + 1 > ProtocolV4Limits::SUBSCRIBER_BUFFER_MAX_OPS
            || self.bytes + size > ProtocolV4Limits::SUBSCRIBER_BUFFER_MAX_BYTES
        {
            return Err(()); // slow client: evict
        }
        self.events.push(ev);
        self.bytes += size;
        Ok(())
    }
}

struct BusState {
    clients: HashMap<u64, ClientBuffer>,
    evicted: Vec<u64>,
}

/// The bus. Single-process M0 shape (threads attach via handles).
pub struct EventBus {
    next_id: AtomicU64,
    /// Global ring for reconnect replay, capped at EVENT_RETENTION_PER_SESSION.
    ring: Mutex<VecDeque<BusEvent>>,
    state: Mutex<BusState>,
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new()
    }
}

impl EventBus {
    pub fn new() -> Self {
        EventBus {
            next_id: AtomicU64::new(1),
            ring: Mutex::new(VecDeque::new()),
            state: Mutex::new(BusState { clients: HashMap::new(), evicted: Vec::new() }),
        }
    }

    /// Publish to a topic; returns the assigned id. The event is appended to
    /// every attached client's buffer (subject to their caps) and the ring.
    pub fn publish(&self, topic: &str, payload: serde_json::Value) -> u64 {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let ev = BusEvent { id, topic: topic.to_string(), payload };

        {
            let mut ring = self.ring.lock().unwrap();
            ring.push_back(ev.clone());
            let retain = ProtocolV4Limits::EVENT_RETENTION_PER_SESSION;
            while ring.len() > retain {
                ring.pop_front();
            }
        }

        let mut state = self.state.lock().unwrap();
        let mut to_evict = Vec::new();
        for (client_id, buf) in state.clients.iter_mut() {
            if buf.push(ev.clone()).is_err() {
                to_evict.push(*client_id);
            }
        }
        for id in to_evict {
            state.clients.remove(&id);
            state.evicted.push(id);
        }
        id
    }

    /// Attach a new client; returns its handle id.
    pub fn attach(&self) -> u64 {
        let mut state = self.state.lock().unwrap();
        let id = state.clients.len() as u64 + 1 + state.evicted.len() as u64;
        state.clients.insert(id, ClientBuffer { events: Vec::new(), bytes: 0 });
        id
    }

    /// Deliver buffered events for a client and drain them.
    pub fn poll(&self, client: u64, max: usize) -> Vec<BusEvent> {
        let mut state = self.state.lock().unwrap();
        match state.clients.get_mut(&client) {
            Some(buf) => {
                let take = buf.events.len().min(max);
                buf.events.drain(..take).collect()
            }
            None => Vec::new(),
        }
    }

    pub fn is_evicted(&self, client: u64) -> bool {
        self.state.lock().unwrap().evicted.contains(&client)
    }

    /// Reconnect replay: events after `last_event_id` from the ring
    /// (deterministic redelivery). `EventIdTooOld` forces snapshot resync.
    pub fn replay_after(&self, last_event_id: u64) -> Result<Vec<BusEvent>, SubscribeError> {
        let ring = self.ring.lock().unwrap();
        if let Some(oldest) = ring.front().map(|e| e.id)
            && last_event_id + 1 < oldest {
                return Err(SubscribeError::EventIdTooOld { oldest_retained: Some(oldest) });
            }
        Ok(ring.iter().filter(|e| e.id > last_event_id).cloned().collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn publish_ids_increase_and_clients_receive() {
        let bus = EventBus::new();
        let client = bus.attach();
        let id1 = bus.publish("conv/s1", json!({ "op": "row.appended" }));
        let id2 = bus.publish("conv/s1", json!({ "op": "row.delta" }));
        assert_eq!(id2, id1 + 1);
        let events = bus.poll(client, 10);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].id, id1);
        assert!(bus.poll(client, 10).is_empty(), "drained");
    }

    #[test]
    fn replay_after_last_event_id_is_deterministic() {
        let bus = EventBus::new();
        let mut ids = Vec::new();
        for i in 0..5 {
            ids.push(bus.publish("t", json!(i)));
        }
        let replay = bus.replay_after(ids[1]).unwrap();
        assert_eq!(replay.len(), 3);
        assert_eq!(replay[0].id, ids[2]);
        assert_eq!(replay[2].id, ids[4]);
    }

    #[test]
    fn slow_client_is_evicted_not_dropped() {
        let bus = EventBus::new();
        let slow = bus.attach();
        let fast = bus.attach();
        // the "fast" client polls every 100 publishes; the slow one never
        // polls and eventually exceeds its buffer cap
        let total_events = ProtocolV4Limits::SUBSCRIBER_BUFFER_MAX_OPS + 10;
        let mut received = 0usize;
        for i in 0..total_events {
            bus.publish("t", json!({ "i": i }));
            if i % 100 == 99 {
                received += bus.poll(fast, 500).len();
            }
        }
        assert!(bus.is_evicted(slow), "slow client evicted, not silently dropped");
        assert!(!bus.is_evicted(fast), "a client that polls keeps receiving");
        received += bus.poll(fast, 500).len();
        assert_eq!(received, total_events, "no event is lost for the live client");
    }
}
