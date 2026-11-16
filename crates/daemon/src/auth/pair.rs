//! The one-shot pairing state: a code the owner reads out, redeemed once
//! over a remote connection for a real token.
//!
//! Nothing about the code is written to disk, each issue replaces the
//! previous code, a wrong guess burns nothing, and a redeemed code is
//! consumed exactly once — the semantics of the host's `pendingPair`.

use std::sync::Mutex;

use sha2::{Digest, Sha256};

use super::{PAIRING_CODE_TTL_MS, hex_encode};

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
    pub fn new() -> Self {
        Self {
            pending: Mutex::new(None),
        }
    }

    /// Issues a fresh 12-hex-character code valid for five minutes,
    /// replacing any previous code. Returns `(code, expiresAt)`.
    pub fn issue(&self, now_ms: u64) -> Result<(String, u64), String> {
        let mut bytes = [0u8; 6];
        getrandom::fill(&mut bytes).map_err(|error| {
            format!("Could not read system entropy for a pairing code: {error}")
        })?;
        let code = hex_encode(&bytes);
        let expires_at_ms = now_ms.saturating_add(PAIRING_CODE_TTL_MS);
        let Ok(mut pending) = self.pending.lock() else {
            return Err("The pairing state is unavailable.".to_owned());
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
        let Ok(pending) = self.pending.lock() else {
            return false;
        };
        let Some(current) = pending.as_ref() else {
            return false;
        };
        if now_ms > current.expires_at_ms {
            return false;
        }
        let submitted = Sha256::digest(code.as_bytes());
        let issued = Sha256::digest(current.code.as_bytes());
        let matches = submitted
            .iter()
            .zip(issued.iter())
            .fold(0u8, |acc, (a, b)| acc | (a ^ b))
            == 0;
        if matches {
            drop(pending);
            if let Ok(mut pending) = self.pending.lock() {
                *pending = None;
            }
        }
        matches
    }
}

impl Default for PairingCodes {
    fn default() -> Self {
        Self::new()
    }
}
