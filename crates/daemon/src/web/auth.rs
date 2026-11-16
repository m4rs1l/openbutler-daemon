//! Browser credentials: the one-shot pairing link and the persistent
//! session cookie.
//!
//! Pairing is two steps with two authorities: the server mints a link the
//! owner opens once (five minutes, single use), and the successful visit
//! sets a cookie that persists across restarts so the owner's devices are
//! never silently unpaired.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

use base64::Engine;

use crate::auth::{ct_eq, write_private};

/// The cookie that marks a browser as paired, and the pre-rename name
/// still accepted so existing browsers stay paired.
pub const SESSION_COOKIE: &str = "openbutler_web";
pub const LEGACY_SESSION_COOKIE: &str = "ranite_web";

/// 30 days, in seconds — the cookie's `Max-Age`.
pub const SESSION_TTL_SECONDS: u64 = 30 * 24 * 60 * 60;

/// The pairing link lives five minutes. (TypeScript `PAIR_TTL_MS`.)
const PAIR_TTL_MS: u64 = 5 * 60 * 1_000;

/// The one-shot browser pairing token: minted when the server starts,
/// consumed by exactly one successful visit.
pub struct PairToken {
    token: String,
    expires_at_ms: u64,
    used: AtomicBool,
}

impl PairToken {
    pub fn mint(now_ms: u64) -> std::io::Result<Self> {
        let mut bytes = [0u8; 32];
        getrandom::fill(&mut bytes).map_err(std::io::Error::other)?;
        Ok(Self {
            token: base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes),
            expires_at_ms: now_ms.saturating_add(PAIR_TTL_MS),
            used: AtomicBool::new(false),
        })
    }

    pub fn token(&self) -> &str {
        &self.token
    }

    /// The link's verdict: matching, inside the window, and claimed — the
    /// single use is won atomically, so two browsers racing the same link
    /// get exactly one success.
    pub fn try_consume(&self, supplied: &str, now_ms: u64) -> bool {
        if self.used.load(Ordering::Acquire) {
            return false;
        }
        if now_ms >= self.expires_at_ms {
            return false;
        }
        if !ct_eq(supplied.as_bytes(), self.token.as_bytes()) {
            return false;
        }
        self.used
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }
}

/// The persistent browser session token: loaded from the data dir when one
/// exists, minted and stored otherwise. A read-only data directory still
/// works — it just forgets pairings on restart, exactly like the legacy
/// server.
pub fn persistent_session_token(data_dir: &Path) -> std::io::Result<String> {
    let file = data_dir.join("web-session-token");
    if let Ok(existing) = std::fs::read_to_string(&file) {
        let existing = existing.trim();
        if existing.len() >= 32 {
            return Ok(existing.to_owned());
        }
    }
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(std::io::Error::other)?;
    let token = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);
    if let Some(parent) = file.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = write_private(&file, token.as_bytes());
    Ok(token)
}

/// The paired-browser cookie from a `Cookie` header. The token is base64url
/// — every character survives `encodeURIComponent` unchanged — so the raw
/// value is what the legacy decoder would have produced; no decode needed.
pub fn session_cookie(header_value: &str) -> Option<&str> {
    header_value.split(';').map(str::trim).find_map(|pair| {
        let (name, value) = pair.split_once('=')?;
        (name == SESSION_COOKIE || name == LEGACY_SESSION_COOKIE).then_some(value)
    })
}

/// Same-origin gate: an `Origin` header, when present, must name the `Host`
/// it was sent to. Comparing against the request's own Host covers every
/// way in — localhost, a LAN address, a tailnet name — without listing
/// them, and still refuses a page served from somewhere else. No Origin:
/// non-browser clients are allowed through to the cookie check.
pub fn origin_allowed(origin: Option<&str>, host: Option<&str>) -> bool {
    let Some(origin) = origin else {
        return true;
    };
    let Some(host) = host else {
        return false;
    };
    let Some(rest) = origin.split_once("://").map(|(_, rest)| rest) else {
        return false;
    };
    let authority = rest.split('/').next().unwrap_or("");
    if authority.is_empty() {
        return false;
    }
    // `new URL(origin).host` lowercases the host; browsers never send a
    // default port in Origin, so the lowercased authority is the faithful
    // comparison against the raw Host header.
    authority.to_ascii_lowercase() == host
}
