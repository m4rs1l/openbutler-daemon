//! Connection admission: the only frames an unauthenticated remote socket
//! may offer, and the exact wordings the host answers it with.
//!
//! Extracted from the TypeScript host (`daemon-host.ts`,
//! `handleRemoteAuthFrame`, read 2026-11-16). The first frame on a remote
//! connection is either `auth` with a minted token or `pair` with the
//! owner's one-time code; every other frame — a well-formed request
//! included — is refused and the connection closed. This module owns
//! classification and wording only: verification, the token store, and the
//! pairing state live in the daemon, which enforces the gate before
//! dispatch sees anything.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::constants::DAEMON_PROTOCOL_VERSION;

/// Refusal for a first frame that is neither `auth` nor `pair`.
pub const REMOTE_FIRST_FRAME_REFUSED: &str = "This connection is remote and must authenticate first: send { version, id, action: \"auth\", token } as the first frame.";
/// Answer when the store rejects the presented token; the connection closes.
pub const AUTH_FAILED: &str = "Authentication failed: this host does not accept that token.";
/// Answer when the pairing code is wrong, expired, or already consumed.
pub const PAIR_FAILED: &str = "Pairing failed: that code is wrong, expired, or already used.";

/// What a remote socket's first frame turned out to be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FirstFrame<'a> {
    /// A client offering a long-lived token.
    Auth { id: &'a str, token: &'a str },
    /// A client offering the owner's one-time pairing code.
    Pair { id: &'a str, code: &'a str },
    /// Anything else: answered with [`REMOTE_FIRST_FRAME_REFUSED`] and closed.
    Refused { id: &'a str },
}

/// The success payload of an accepted `auth` frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthSuccess {
    pub authenticated: bool,
}

/// The success payload of an accepted `pair` frame: the freshly minted
/// token, which exists in plaintext exactly once.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PairSuccess {
    pub token: String,
}

/// Classifies a parsed first frame exactly like the TypeScript host:
/// `auth` and `pair` require the current protocol version, a non-empty
/// string id, and a string credential; everything else is refused with the
/// id the frame carried, when it carried a string one.
pub fn classify_first_frame(frame: &Value) -> FirstFrame<'_> {
    let record = frame.as_object();
    let id = record
        .and_then(|r| r.get("id"))
        .and_then(Value::as_str)
        .unwrap_or("");
    // JavaScript's `!==` compares numbers by value, so `2.0` and `2` agree
    // here the same way they do on the wire's origin side.
    let well_formed = record.is_some_and(|r| {
        r.get("version").and_then(Value::as_f64) == Some(f64::from(DAEMON_PROTOCOL_VERSION))
            && !id.is_empty()
    });
    if well_formed {
        let action = record.and_then(|r| r.get("action")).and_then(Value::as_str);
        if action == Some("auth")
            && let Some(token) = record.and_then(|r| r.get("token")).and_then(Value::as_str)
        {
            return FirstFrame::Auth { id, token };
        }
        if action == Some("pair")
            && let Some(code) = record.and_then(|r| r.get("code")).and_then(Value::as_str)
        {
            return FirstFrame::Pair { id, code };
        }
    }
    FirstFrame::Refused { id }
}
