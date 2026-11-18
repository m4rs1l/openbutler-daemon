//! Fan-out: who receives a session's published events, and how one
//! published state reaches them.
//!
//! Source: legacy `attachments` map + `writeEvent` (daemon-host.ts L1461,
//! L4026), read 2026-11-17. The sink trait keeps `dispatch` free of any
//! `server` dependency — the connection implements the trait with its
//! bounded write queue, exactly the budget the response path already uses.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use openbutler_protocol::DAEMON_PROTOCOL_VERSION;
use openbutler_protocol::runtime::RuntimeViewData;

use crate::agent::events::{PublishSnapshot, make_session_event};

/// One connection's write path, as fan-out sees it. `false` means the
/// frame was not queued (budget or closed pipe) and the subscription is
/// dropped — the source's `socket.destroy` for an output frame over the
/// limit.
pub trait EventSink: Send + Sync {
    fn try_send(&self, frame: Vec<u8>) -> bool;
}

pub type SharedSink = Arc<dyn EventSink>;

/// Subscriptions keyed by a host-minted id; a connection unsubscribes on
/// drop, a failed send unsubscribes immediately.
#[derive(Default)]
/// The roster of session subscriptions a publish walks.
pub struct Fanout {
    next_id: AtomicU64,
    subscribers: RwLock<HashMap<u64, (String, SharedSink)>>,
}

impl Fanout {
    /// An empty roster (host-minted ids start here).
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers `sink` for `session_id` events; the id is the handle the
    /// owning connection drops with.
    pub fn subscribe(&self, session_id: &str, sink: SharedSink) -> u64 {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        self.subscribers
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(id, (session_id.to_owned(), sink));
        id
    }

    /// Removes a subscriber; a rejected sink is dropped on publish.
    pub fn unsubscribe(&self, id: u64) {
        self.subscribers
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&id);
    }

    /// Builds the event between two states and queues it for every
    /// subscriber of `session_id`. Returns how many sinks received it.
    /// A sink that refuses a frame is unsubscribed (over-budget output
    /// destroys the socket in the source); a session with no subscribers
    /// costs nothing.
    pub fn publish(
        &self,
        session_id: &str,
        previous: &PublishSnapshot,
        next: &PublishSnapshot,
        runtime: &RuntimeViewData,
    ) -> usize {
        let event = make_session_event(previous, next, runtime, DAEMON_PROTOCOL_VERSION);
        let Ok(mut frame) = serde_json::to_vec(&event) else {
            return 0;
        };
        frame.push(b'\n');

        let mut delivered = 0;
        let mut failed = Vec::new();
        {
            let subscribers = self
                .subscribers
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            for (id, (subscribed_session, sink)) in subscribers.iter() {
                if subscribed_session != session_id {
                    continue;
                }
                if sink.try_send(frame.clone()) {
                    delivered += 1;
                } else {
                    failed.push(*id);
                }
            }
        }
        if !failed.is_empty() {
            let mut subscribers = self
                .subscribers
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            for id in failed {
                subscribers.remove(&id);
            }
        }
        delivered
    }
}
