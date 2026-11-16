//! Call routing and termination: where a sidecar event goes, and how a
//! call ends — clean finish, timeout, cancellation, or process death.
//!
//! Invariants: the routes map is the single delivery authority (whoever
//! removes a route delivers its terminal outcome), the slot release
//! happens exactly once per sent call, and `sent`/`aborted` are only
//! mutated under the writer lock so pump, cancel and expiry can never
//! disagree about whether a call went out.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use openbutler_protocol::sidecar::{SidecarCommand, SidecarEvent};
use tokio::sync::mpsc;

use super::{DoorError, DoorState, Lane, SidecarDoor, now_ms};

/// One registered call: its event channel plus lifecycle flags.
pub(crate) struct Route {
    pub(crate) tx: mpsc::Sender<Result<SidecarEvent, DoorError>>,
    pub(crate) control: Arc<Control>,
}

pub(crate) struct Control {
    pub(crate) lane: Lane,
    sent: AtomicBool,
    aborted: AtomicBool,
    released: AtomicBool,
    last_event_ms: AtomicU64,
}

impl Control {
    pub(crate) fn new(lane: Lane) -> Arc<Self> {
        Arc::new(Self {
            lane,
            sent: AtomicBool::new(false),
            aborted: AtomicBool::new(false),
            released: AtomicBool::new(false),
            last_event_ms: AtomicU64::new(0),
        })
    }

    pub(crate) fn is_sent(&self) -> bool {
        self.sent.load(Ordering::Acquire)
    }

    pub(crate) fn is_aborted(&self) -> bool {
        self.aborted.load(Ordering::Acquire)
    }

    /// Under the writer lock: clear the call for sending.
    pub(crate) fn mark_sent(&self) {
        self.sent.store(true, Ordering::Release);
    }

    /// Under the writer lock: forbid sending forever (cancelled or
    /// expired while queued).
    pub(crate) fn mark_aborted(&self) {
        self.aborted.store(true, Ordering::Release);
    }

    pub(crate) fn note_event(&self) {
        self.last_event_ms.store(now_ms(), Ordering::Release);
    }

    pub(crate) fn last_event_ms(&self) -> u64 {
        self.last_event_ms.load(Ordering::Acquire)
    }

    /// True exactly once, so the slot counters drop exactly once.
    pub(crate) fn try_release(&self) -> bool {
        self.released
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }
}

pub(crate) fn event_id(event: &SidecarEvent) -> Option<&str> {
    match event {
        SidecarEvent::Ready(_) => None,
        SidecarEvent::TextDelta { id, .. }
        | SidecarEvent::ReasoningDelta { id, .. }
        | SidecarEvent::ToolCall { id, .. }
        | SidecarEvent::Usage { id, .. }
        | SidecarEvent::Finish { id, .. }
        | SidecarEvent::Error { id, .. } => Some(id),
    }
}

impl SidecarDoor {
    pub(crate) async fn has_route(&self, id: &str) -> bool {
        self.inner.routes.lock().await.contains_key(id)
    }

    /// Deliver one event to its call. `finish` and `error` events close
    /// the stream afterwards — a structured provider error ends the call
    /// without becoming a door error.
    pub(crate) async fn route_event(&self, event: SidecarEvent) {
        let Some(id) = event_id(&event).map(str::to_owned) else {
            return;
        };
        let (tx, control) = {
            let routes = self.inner.routes.lock().await;
            let Some(route) = routes.get(id.as_str()) else {
                return; // late event after cancel or termination: drop it
            };
            (route.tx.clone(), route.control.clone())
        };
        control.note_event();
        let closes = matches!(
            event,
            SidecarEvent::Finish { .. } | SidecarEvent::Error { .. }
        );
        let _ = tx.send(Ok(event)).await;
        if closes {
            self.finish_call(&id).await;
        }
    }

    /// A clean end: close the channel (no error) and free the slot.
    pub(crate) async fn finish_call(&self, id: &str) {
        let route = { self.inner.routes.lock().await.remove(id) };
        if let Some(route) = route {
            drop(route.tx);
            self.release(&route.control).await;
        }
    }

    /// A failed end: deliver the terminal error, then free the slot.
    pub(crate) async fn fail_call(&self, id: &str, error: DoorError) {
        let route = { self.inner.routes.lock().await.remove(id) };
        if let Some(route) = route {
            let _ = route.tx.send(Err(error)).await;
            drop(route.tx);
            self.release(&route.control).await;
        }
    }

    /// Fail every live call — process death and protocol violations both
    /// end the door for everyone.
    pub(crate) async fn fail_all(&self, error: DoorError) {
        let routes: Vec<Route> = {
            let mut guard = self.inner.routes.lock().await;
            guard.drain().map(|(_, route)| route).collect()
        };
        for route in routes {
            let _ = route.tx.send(Err(error.clone())).await;
            drop(route.tx);
            self.release(&route.control).await;
        }
    }

    pub(crate) async fn on_child_eof(&self) {
        self.inner.state.send_replace(DoorState::Dead);
        self.fail_all(DoorError::ProcessExited).await;
    }

    pub(crate) async fn on_protocol_violation(&self, error: DoorError) {
        // Dead first so no pump admits anything while we tear down.
        self.inner.state.send_replace(DoorState::Dead);
        self.fail_all(error).await;
        let mut writes = self.inner.writes.lock().await;
        let _ = writes.kill().await;
    }

    /// Cancel one call: never-sent calls are forbidden from ever going
    /// out; sent calls get the cancel command and free their slot.
    pub(crate) async fn cancel_call(&self, id: &str) {
        let route = { self.inner.routes.lock().await.remove(id) };
        self.inner.queue.remove(id);
        let Some(route) = route else {
            return; // already terminal
        };
        let control = route.control.clone();
        drop(route.tx);
        let was_sent = {
            let mut writes = self.inner.writes.lock().await;
            if control.is_sent() {
                if let Ok(line) =
                    serde_json::to_string(&SidecarCommand::Cancel { id: id.to_owned() })
                {
                    let _ = writes.write_line(&line).await;
                }
                true
            } else {
                control.mark_aborted();
                false
            }
        };
        if was_sent {
            self.release(&control).await;
        }
    }

    /// Queue expiry: only a call that never went out can expire, and
    /// expiry forbids it from going out from here on.
    pub(crate) async fn expire_unsent(&self, id: &str, control: &Control) {
        let expired = {
            // The guard is the critical section; it is never touched.
            let _writes = self.inner.writes.lock().await;
            if control.is_sent() || control.is_aborted() {
                false
            } else {
                control.mark_aborted();
                true
            }
        };
        if !expired {
            return;
        }
        let route = { self.inner.routes.lock().await.remove(id) };
        self.inner.queue.remove(id);
        if let Some(route) = route {
            let _ = route.tx.send(Err(DoorError::QueueTimeout)).await;
            drop(route.tx);
        }
    }

    /// The event watchdog: the first-event window starts at send, then
    /// the quiet window runs per event. A call nobody routes anymore has
    /// ended, and the watchdog exits.
    pub(crate) async fn watch_events(
        &self,
        id: String,
        control: Arc<Control>,
        first: Duration,
        inter: Duration,
    ) {
        tokio::time::sleep(first).await;
        if !self.has_route(&id).await {
            return;
        }
        if control.last_event_ms() == 0 {
            self.fail_call(&id, DoorError::FirstEventTimeout).await;
            return;
        }
        let inter_ms = u64::try_from(inter.as_millis()).unwrap_or(u64::MAX);
        loop {
            tokio::time::sleep(inter).await;
            if !self.has_route(&id).await {
                return;
            }
            let last = control.last_event_ms();
            if last == 0 || now_ms().saturating_sub(last) >= inter_ms {
                self.fail_call(&id, DoorError::InterEventTimeout).await;
                return;
            }
        }
    }

    /// End the process and mark the door dead (supervisor paths).
    pub(crate) async fn kill_child(&self) {
        let mut writes = self.inner.writes.lock().await;
        let _ = writes.kill().await;
        drop(writes);
        self.inner.state.send_replace(DoorState::Dead);
    }

    /// Free a sent call's slot exactly once and look for queued work.
    async fn release(&self, control: &Control) {
        if !control.is_sent() || !control.try_release() {
            return;
        }
        if control.lane == Lane::Background {
            let previous = self
                .inner
                .background_inflight
                .fetch_sub(1, Ordering::AcqRel);
            debug_assert!(previous > 0, "background inflight underflow");
        }
        let previous = self.inner.inflight.fetch_sub(1, Ordering::AcqRel);
        debug_assert!(previous > 0, "inflight underflow");
        // Wake the admission loop — never pump from here: that would make
        // release → pump → fail_call → release an async cycle.
        self.inner.admit.notify_one();
    }
}
