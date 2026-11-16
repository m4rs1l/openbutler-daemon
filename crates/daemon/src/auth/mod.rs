//! Remote admission state: the token store and the one-shot pairing codes.
//!
//! Ported from the TypeScript `auth-tokens.ts` and the host's `pendingPair`
//! state (read 2026-11-16). The wire contract for the frames that reach
//! these lives in `openbutler_protocol::auth`; this module is the daemon
//! side that actually says yes or no.

pub mod pair;
pub mod store;

use std::time::{SystemTime, UNIX_EPOCH};

pub use pair::PairingCodes;
pub use store::{Minted, TokenStore};

/// The remote admission deadline: a socket that sends nothing is closed, so
/// an open port never holds an unauthenticated connection alive.
/// (TypeScript `REMOTE_AUTH_TIMEOUT_MS = 10_000`.)
pub const REMOTE_AUTH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Pairing codes live five minutes. (`PAIRING_CODE_TTL_MS = 5 * 60_000`.)
pub(crate) const PAIRING_CODE_TTL_MS: u64 = 5 * 60_000;

/// Epoch milliseconds — the clock `pair.create` and the gate share.
pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_millis() as u64)
        .unwrap_or(0)
}

/// Lowercase hex, the encoding every secret id, salt, and code uses.
pub(crate) fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}
