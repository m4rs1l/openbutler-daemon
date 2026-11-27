//! The sign-in lock: five failures in a row hold the door for a minute.
//!
//! In memory only, deliberately: the lock is not a secret, and a host restart
//! is the owner's own act — which also makes a restart the recovery path if the
//! owner locks themselves out. The password hash is expensive by design, so
//! this lock is the second line: it stops an attacker from turning the sign-in
//! path into a CPU heater.

use std::sync::Mutex;

/// Consecutive failures that arm the lock.
pub const MAX_SIGN_IN_FAILURES: u32 = 5;
/// How long the lock holds, in milliseconds (the fixed sentence says a minute).
pub const SIGN_IN_LOCK_MS: u64 = 60_000;

/// The gate's state: how many failures in a row, and until when it is shut.
#[derive(Debug, Default, Clone, Copy)]
struct State {
    failures: u32,
    locked_until_ms: u64,
}

/// One gate per host, shared by every connection.
#[derive(Debug, Default)]
pub struct SignInGate {
    state: Mutex<State>,
}

impl SignInGate {
    /// A fresh gate, open.
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether sign-in is shut at `now_ms`.
    pub fn is_locked(&self, now_ms: u64) -> bool {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .locked_until_ms
            > now_ms
    }

    /// Counts one failed attempt, arming the lock on the last allowed one.
    /// The counter resets when it arms, so the next window starts clean.
    pub fn record_failure(&self, now_ms: u64) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.failures = state.failures.saturating_add(1);
        if state.failures >= MAX_SIGN_IN_FAILURES {
            state.failures = 0;
            state.locked_until_ms = now_ms.saturating_add(SIGN_IN_LOCK_MS);
        }
    }

    /// One accepted sign-in clears both the counter and any armed lock.
    pub fn record_success(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *state = State::default();
    }
}
