//! The host-wide connection roster: who a host notification reaches.
//!
//! Source: the legacy `sockets` set and the `stop` action's notification
//! loop (daemon-host.ts L3200-3210, read 2026-11-24). The roster is the set
//! of admitted connections — a remote socket only enters it after the
//! admission plane (this build gates at accept time, so every connection
//! reaching dispatch is already admitted, exactly the authenticated set the
//! source iterates). A broadcast skips the requester: it hears the stop
//! through its own response. A sink that refuses its frame leaves the roster
//! immediately, the fan-out contract.

use std::collections::HashMap;
use std::sync::RwLock;
use std::sync::atomic::{AtomicU64, Ordering};

use openbutler_protocol::{DAEMON_PROTOCOL_VERSION, envelope::DaemonEvent};

use crate::dispatch::fanout::SharedSink;

/// The roster of live connections, keyed by a host-minted id the owning
/// connection drops with.
#[derive(Default)]
/// Live connections a host notification reaches.
pub struct Clients {
    next_id: AtomicU64,
    sinks: RwLock<HashMap<u64, SharedSink>>,
}

impl Clients {
    /// An empty roster.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adds one admitted connection; the id is the handle its guard
    /// unregisters with.
    pub fn register(&self, sink: SharedSink) -> u64 {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        self.sinks
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(id, sink);
        id
    }

    /// Removes a connection (its guard runs on drop).
    pub fn unregister(&self, id: u64) {
        self.sinks
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&id);
    }

    /// Queues one `notification` frame to every connection except the
    /// requester, answering how many sinks took it. A refusing sink is
    /// dropped from the roster on the spot.
    pub fn broadcast_notification(
        &self,
        title: &str,
        message: &str,
        except: Option<&SharedSink>,
    ) -> usize {
        let event = DaemonEvent::Notification {
            version: DAEMON_PROTOCOL_VERSION,
            title: title.to_owned(),
            message: message.to_owned(),
        };
        let Ok(mut frame) = serde_json::to_vec(&event) else {
            return 0;
        };
        frame.push(b'\n');

        let mut delivered = 0;
        let mut failed = Vec::new();
        {
            let sinks = self
                .sinks
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            for (id, sink) in sinks.iter() {
                if let Some(skip) = except
                    && std::sync::Arc::ptr_eq(sink, skip)
                {
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
            let mut sinks = self
                .sinks
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            for id in failed {
                sinks.remove(&id);
            }
        }
        delivered
    }
}
