//! The one-shot pairing state: a code the owner reads out, redeemed once
//! over a remote connection for a real token.
//!
//! Nothing about the code is written to disk, each issue replaces the
//! previous code, a wrong guess burns nothing, and a redeemed code is
//! consumed exactly once — the semantics of the host's `pendingPair`.

use std::sync::Mutex;

use sha2::{Digest, Sha256};

use super::{AuthError, PAIRING_CODE_TTL_MS, hex_encode};

/// Shared by `pair.create` (dispatch) and the admission gate; lives on the
/// host record so every connection sees the same pending code.
pub struct PairingCodes {
    pending: Mutex<Option<Pending>>,
}

struct Pending {
    code: String,
    expires_at_ms: u64,
}

impl PairingCodes {
    /// An empty pairing-code store (codes minted on demand).
    pub fn new() -> Self {
        Self {
            pending: Mutex::new(None),
        }
    }

    /// Issues a fresh 12-hex-character code valid for five minutes,
    /// replacing any previous code. Returns `(code, expiresAt)`.
    pub fn issue(&self, now_ms: u64) -> Result<(String, u64), AuthError> {
        let mut bytes = [0u8; 6];
        getrandom::fill(&mut bytes)
            .map_err(|error| AuthError::PairingEntropy(error.to_string()))?;
        let code = hex_encode(&bytes);
        let expires_at_ms = now_ms.saturating_add(PAIRING_CODE_TTL_MS);
        let Ok(mut pending) = self.pending.lock() else {
            return Err(AuthError::PairingUnavailable);
        };
        *pending = Some(Pending {
            code: code.clone(),
            expires_at_ms,
        });
        Ok((code, expires_at_ms))
    }

    /// Redeems `code` against the pending one, comparing digests so the
    /// comparison itself never leaks which prefix matched. Only a successful
    /// redemption consumes the code; expiry and wrong guesses leave the
    /// pending code in place for the client the owner is about to hand it to.
    pub fn redeem(&self, code: &str, now_ms: u64) -> bool {
        self.redeem_with(code, now_ms, || Ok::<(), std::convert::Infallible>(()))
            .is_some_and(|outcome| outcome.is_ok())
    }

    /// Redeems `code` and, only when it matches, runs `produce` — the step
    /// that turns the redemption into something durable (a minted token).
    /// The code is consumed only when `produce` succeeds: a failed mint
    /// leaves the code in place so the owner can retry instead of having to
    /// issue a new one. `None` means the code did not match or has expired.
    ///
    /// The lock stays held through `produce` and the clear: releasing it
    /// first would let a concurrent `issue()` land between the match and the
    /// consume, and this redemption would wipe the freshly issued code, and
    /// two clients holding the same code could both mint a token.
    pub fn redeem_with<T, E>(
        &self,
        code: &str,
        now_ms: u64,
        produce: impl FnOnce() -> Result<T, E>,
    ) -> Option<Result<T, E>> {
        let Ok(mut pending) = self.pending.lock() else {
            return None;
        };
        let current = pending.as_ref()?;
        if now_ms > current.expires_at_ms {
            return None;
        }
        let submitted = Sha256::digest(code.as_bytes());
        let issued = Sha256::digest(current.code.as_bytes());
        if !super::ct_eq(submitted.as_ref(), issued.as_ref()) {
            return None;
        }
        let outcome = produce();
        if outcome.is_ok() {
            *pending = None;
        }
        Some(outcome)
    }
}
impl Default for PairingCodes {
    fn default() -> Self {
        Self::new()
    }
}
