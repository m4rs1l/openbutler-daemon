//! Remote admission state: the token store and the one-shot pairing codes.
//!
//! Ported from the TypeScript `auth-tokens.ts` and the host's `pendingPair`
//! state (read 2026-11-16). The wire contract for the frames that reach
//! these lives in `openbutler_protocol::auth`; this module is the daemon
//! side that actually says yes or no.

pub mod account;
pub mod pair;
pub mod sessions;
pub mod store;
pub mod throttle;

use std::fs;
use std::path::Path;

pub use account::{AccountSummary, AdminAccount, MIN_PASSWORD_LENGTH};
pub use pair::PairingCodes;
pub use sessions::{IssuedSession, SessionStore, SessionSummary};
pub use store::{Minted, TokenStore, TokenSummary};
pub use throttle::SignInGate;

/// Everything a credential operation can fail with. The messages are the
/// fixed wordings, verbatim — they are part of the wire contract.
#[derive(Debug, thiserror::Error)]
/// Authentication failure (missing, expired, or unknown credential).
pub enum AuthError {
    /// An I/O failure underneath (the message is the OS's own).
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// The store exists but is not JSON.
    #[error(
        "The auth token store at {path} is not valid JSON. Fix or delete it to mint a new token."
    )]
    StoreNotJson { path: String },
    /// The store parses but is not a mapping.
    #[error("The auth token store at {path} must be a JSON object.")]
    StoreNotObject { path: String },
    /// The mapping has no `tokens` array.
    #[error("The auth token store at {path} needs a \"tokens\" array.")]
    StoreMissingTokens { path: String },
    /// A record inside `tokens` is missing a required string field.
    #[error("The auth token store at {path} has a malformed token record.")]
    StoreMalformedRecord { path: String },
    /// There is no admin account yet.
    #[error(
        "This host has no admin account yet. Create one on the host machine with: openbutler admin create"
    )]
    NoAccount,
    /// An admin account already exists.
    #[error(
        "An admin account already exists. Change its password on the host with: openbutler admin passwd"
    )]
    AccountExists,
    /// The account file exists but is not JSON.
    #[error("The admin account at {path} is not valid JSON. Fix or delete it to create a new one.")]
    AccountNotJson { path: String },
    /// The account file parses but is not a mapping.
    #[error("The admin account at {path} must be a JSON object.")]
    AccountNotObject { path: String },
    /// A required field is missing from the account file.
    #[error("The admin account at {path} is missing its {field} field.")]
    AccountMissingField { path: String, field: &'static str },
    /// The account file parses but does not describe an account.
    #[error("The admin account at {path} has an empty username or hash.")]
    AccountMalformed { path: String },
    /// The username is empty or over the cap.
    #[error("The username needs 1-64 characters.")]
    AccountUsernameInvalid,
    /// The password is under the floor.
    #[error("The password needs at least 8 characters.")]
    PasswordTooShort,
    /// The current password of `admin passwd` did not match.
    #[error("That is not the current admin password.")]
    WrongPassword,
    /// The password hasher refused (a broken stored record, entropy, or
    /// parameters it cannot read).
    #[error("The admin password hasher refused: {0}")]
    Hash(String),
    /// The account already holds the maximum number of devices.
    #[error("{}", openbutler_protocol::auth::TOO_MANY_DEVICES)]
    TooManySessions,
    /// The session store is not JSON.
    #[error("The session store at {path} is not valid JSON. Fix or delete it to sign in again.")]
    SessionStoreNotJson { path: String },
    /// The session store parses but is not a mapping.
    #[error("The session store at {path} must be a JSON object.")]
    SessionStoreNotObject { path: String },
    /// A session record is missing a field or cannot be decoded.
    #[error("The session store at {path} has a malformed session record.")]
    SessionStoreMalformed { path: String },
    /// The OS would not provide entropy for a stored credential.
    #[error("Could not read system entropy for a credential: {0}")]
    Entropy(String),
    /// The OS would not provide entropy for a pairing code.
    #[error("Could not read system entropy for a pairing code: {0}")]
    PairingEntropy(String),
    /// The pairing state lock is unavailable (a holder panicked).
    #[error("The pairing state is unavailable.")]
    PairingUnavailable,
}

/// The remote admission deadline: a socket that sends nothing is closed, so
/// an open port never holds an unauthenticated connection alive.
/// (TypeScript `REMOTE_AUTH_TIMEOUT_MS = 10_000`.)
pub const REMOTE_AUTH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// Pairing codes live five minutes. (`PAIRING_CODE_TTL_MS = 5 * 60_000`.)
pub(crate) const PAIRING_CODE_TTL_MS: u64 = 5 * 60_000;

/// Epoch milliseconds — the clock `pair.create` and the gate share
/// (the daemon-wide reading lives in `ledger::now_ms`).
pub(crate) fn now_ms() -> u64 {
    crate::ledger::now_ms().max(0) as u64
}

/// The wire stamp of an epoch-millisecond value: the TypeScript format
/// (`YYYY-MM-DDTHH:MM:SS.mmmZ`). One formatter for every credential stamp, so
/// the account and the sessions cannot drift into two shapes.
pub(crate) fn iso_from_ms(ms: u64) -> String {
    let stamp = time::OffsetDateTime::from_unix_timestamp_nanos(i128::from(ms) * 1_000_000)
        .unwrap_or(time::OffsetDateTime::UNIX_EPOCH);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        stamp.year(),
        u8::from(stamp.month()),
        stamp.day(),
        stamp.hour(),
        stamp.minute(),
        stamp.second(),
        stamp.millisecond()
    )
}

/// The current time in the same shape.
pub(crate) fn creation_stamp() -> String {
    iso_from_ms(now_ms())
}

/// Lowercase hex, the encoding every secret id, salt, and code uses.
pub(crate) fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// Equal-length, content-oblivious comparison. Every credential check in
/// the daemon (stored tokens, pairing codes, browser cookies) funnels
/// through it, so there is exactly one constant-time routine.
pub(crate) fn ct_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    left.iter()
        .zip(right.iter())
        .fold(0u8, |acc, (a, b)| acc | (a ^ b))
        == 0
}

/// Writes a credential file 0600 where the platform has the concept
/// (Unix); Windows keeps the default ACL like the rest of the data
/// directory.
pub(crate) fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;

    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(bytes)
}
