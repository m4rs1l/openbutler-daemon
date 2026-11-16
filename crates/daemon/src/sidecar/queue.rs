//! Queue admission: two lanes, interactive first, background work
//! limited so it can never hold every slot.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::{Mutex, MutexGuard};

use openbutler_protocol::sidecar::SidecarCall;

use super::Lane;
use super::route::Control;

pub(crate) struct Entry {
    pub(crate) id: String,
    pub(crate) call: SidecarCall,
    pub(crate) control: Arc<Control>,
}

struct QueueState {
    interactive: VecDeque<Entry>,
    background: VecDeque<Entry>,
}

pub(crate) struct Queue {
    state: Mutex<QueueState>,
}

impl Queue {
    pub(crate) fn new() -> Self {
        Self {
            state: Mutex::new(QueueState {
                interactive: VecDeque::new(),
                background: VecDeque::new(),
            }),
        }
    }

    fn lock(&self) -> MutexGuard<'_, QueueState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub(crate) fn push(&self, lane: Lane, entry: Entry) {
        let mut state = self.lock();
        match lane {
            Lane::Interactive => state.interactive.push_back(entry),
            Lane::Background => state.background.push_back(entry),
        }
    }

    /// Next admissible entry: interactive first, then background within
    /// its cap. Callers must hold the writer lock so capacity checks and
    /// slot claims form one critical section.
    pub(crate) fn pop_next(
        &self,
        total: usize,
        background: usize,
        concurrency: usize,
    ) -> Option<Entry> {
        if total >= concurrency {
            return None;
        }
        let mut state = self.lock();
        if let Some(entry) = state.interactive.pop_front() {
            return Some(entry);
        }
        // Background may never take the last slot: at least one stays
        // reserved for interactive work.
        let background_cap = concurrency.saturating_sub(1).max(1);
        if background < background_cap {
            return state.background.pop_front();
        }
        None
    }

    /// Drop a queued entry (cancelled or expired). Returns whether it
    /// was still there.
    pub(crate) fn remove(&self, id: &str) -> bool {
        let mut state = self.lock();
        let before = state.interactive.len() + state.background.len();
        state.interactive.retain(|entry| entry.id != id);
        state.background.retain(|entry| entry.id != id);
        (state.interactive.len() + state.background.len()) < before
    }
}
