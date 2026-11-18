//! The supervisor: idle kill, draining, force kill.
//!
//! States: `Ready` while work may be admitted; idle past the window with
//! zero in-flight calls kills immediately; with calls still running the
//! door drains — and gives up at the drain maximum, killing the process
//! even if a call never ends. New work is refused once not-Ready.

use std::time::Duration;

use tokio::sync::watch;

use super::{SidecarDoor, now_ms};

/// What the door currently is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// The door process's lifecycle state (ready/draining/dead).
pub enum DoorState {
    Ready,
    Draining,
    Dead,
}

/// How often the supervisor looks at the clocks. Timers are sampled, not
/// exact; 50ms keeps the state machine cheap and the tests honest.
const TICK: Duration = Duration::from_millis(50);

pub(crate) async fn run(door: SidecarDoor, mut state: watch::Receiver<DoorState>) {
    let idle_ms = u64::try_from(door.inner.config.idle_kill.as_millis()).unwrap_or(u64::MAX);
    let drain_ms = u64::try_from(door.inner.config.drain_max.as_millis()).unwrap_or(u64::MAX);
    let mut drain_deadline_ms: u64 = 0;

    loop {
        tokio::select! {
            changed = state.changed() => {
                if changed.is_err() {
                    return; // the door handle is gone
                }
                if *state.borrow() == DoorState::Dead {
                    return;
                }
                continue;
            }
            _ = tokio::time::sleep(TICK) => {}
        }
        if *state.borrow() == DoorState::Dead {
            return;
        }

        let now = now_ms();
        let last = door
            .inner
            .last_used_ms
            .load(std::sync::atomic::Ordering::Acquire);
        if now.saturating_sub(last) < idle_ms {
            continue; // not idle yet
        }
        // Copy before matching: the watch borrow must not live across an await.
        let current = *state.borrow();
        match current {
            DoorState::Ready => {
                if door
                    .inner
                    .inflight
                    .load(std::sync::atomic::Ordering::Acquire)
                    == 0
                {
                    door.kill_child().await;
                    return;
                }
                drain_deadline_ms = now + drain_ms;
                door.inner.state.send_replace(DoorState::Draining);
            }
            DoorState::Draining => {
                let inflight = door
                    .inner
                    .inflight
                    .load(std::sync::atomic::Ordering::Acquire);
                if inflight == 0 || now >= drain_deadline_ms {
                    door.kill_child().await;
                    return;
                }
            }
            DoorState::Dead => return,
        }
    }
}
