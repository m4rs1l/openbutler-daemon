//! Device sessions: what `login` mints and every later connection presents.
//!
//! `sessions.json` holds a salted SHA-256 per session — the same hardening the
//! token store used, so a stolen file yields no usable token — plus the device
//! label, the expiry, and a last-seen stamp that refreshes at most once an
//! hour: verifying a token must not become a disk write on every command.
//!
//! Numbers are stored as epoch milliseconds; the wire carries ISO-8601 strings
//! (see `iso_from_ms`).

use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use base64::Engine;
use openbutler_protocol::constants::MAX_SESSIONS_PER_ACCOUNT;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use super::{AuthError, ct_eq, hex_encode, iso_from_ms, write_private};

/// A session lives thirty days. Fixed, not sliding: a device that keeps
/// working forever is a credential nobody ever revisits.
pub const SESSION_TTL_MS: u64 = 30 * 24 * 60 * 60 * 1_000;

/// How stale `lastSeenAt` may get before a verify rewrites the store.
const LAST_SEEN_REFRESH_MS: u64 = 60 * 60 * 1_000;

/// A freshly issued session: the token's plaintext exists exactly once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssuedSession {
    /// The session id `device list` prints and `device revoke` takes.
    pub id: String,
    /// The token the device presents on every later connection.
    pub token: String,
    /// When the session stops being accepted (epoch milliseconds).
    pub expires_at_ms: u64,
}

/// A session as the owner sees it: no secret, only what identifies the device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSummary {
    /// The session id.
    pub id: String,
    /// The device label the client sent at sign-in.
    pub device: String,
    /// When the session was issued (epoch milliseconds).
    pub created_at_ms: u64,
    /// When it stops being accepted (epoch milliseconds).
    pub expires_at_ms: u64,
    /// Last time the device used it (epoch milliseconds).
    pub last_seen_at_ms: u64,
}

/// The session store. Every method is safe on a fresh data directory.
pub struct SessionStore {
    path: PathBuf,
}

impl SessionStore {
    /// The store lives beside the rest of the data directory.
    pub fn new(data_dir: &Path) -> Self {
        Self {
            path: data_dir.join("sessions.json"),
        }
    }

    /// Mints a session for `device`. Expired records are pruned first, and a
    /// full account is refused rather than evicting a device the owner did not
    /// choose.
    pub fn mint(&self, device: &str, now_ms: u64) -> Result<IssuedSession, AuthError> {
        let mut records = self.read()?;
        records.retain(|record| record.expires_at_ms > now_ms);
        if records.len() >= MAX_SESSIONS_PER_ACCOUNT {
            return Err(AuthError::TooManySessions);
        }

        let id = random_hex(8)?;
        let token = format!(
            "obt_{}",
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(random_bytes(24)?)
        );
        let salt = random_bytes(16)?;
        let hash = hash_token(&salt, &token);
        let expires_at_ms = now_ms.saturating_add(SESSION_TTL_MS);

        records.push(Record {
            id: id.clone(),
            device: device.to_owned(),
            salt,
            hash,
            created_at_ms: now_ms,
            expires_at_ms,
            last_seen_at_ms: now_ms,
        });
        self.write(&records)?;
        Ok(IssuedSession {
            id,
            token,
            expires_at_ms,
        })
    }

    /// Verifies a token and returns its session id. An expired, unknown, or
    /// malformed record is `None`, never an admit.
    pub fn verify(&self, token: &str, now_ms: u64) -> Result<Option<String>, AuthError> {
        if token.is_empty() {
            return Ok(None);
        }
        let mut records = self.read()?;
        let mut found = None;
        for record in &records {
            if record.expires_at_ms <= now_ms {
                continue;
            }
            let candidate = hash_token(&record.salt, token);
            if candidate.len() == record.hash.len() && ct_eq(&candidate, &record.hash) {
                found = Some(record.id.clone());
                break;
            }
        }
        let Some(id) = found else {
            return Ok(None);
        };
        if let Some(record) = records.iter_mut().find(|record| record.id == id)
            && now_ms.saturating_sub(record.last_seen_at_ms) >= LAST_SEEN_REFRESH_MS
        {
            record.last_seen_at_ms = now_ms;
            self.write(&records)?;
        }
        Ok(Some(id))
    }

    /// Every live session, newest expiry last, for `device list`.
    pub fn list(&self, now_ms: u64) -> Result<Vec<SessionSummary>, AuthError> {
        let mut summaries: Vec<SessionSummary> = self
            .read()?
            .into_iter()
            .filter(|record| record.expires_at_ms > now_ms)
            .map(|record| SessionSummary {
                id: record.id,
                device: record.device,
                created_at_ms: record.created_at_ms,
                expires_at_ms: record.expires_at_ms,
                last_seen_at_ms: record.last_seen_at_ms,
            })
            .collect();
        summaries.sort_by_key(|summary| summary.created_at_ms);
        Ok(summaries)
    }

    /// Removes one session (`device revoke`). `true` when the id was there.
    pub fn revoke(&self, id: &str, now_ms: u64) -> Result<bool, AuthError> {
        let records = self.read()?;
        let kept: Vec<Record> = records
            .iter()
            .filter(|record| record.id != id && record.expires_at_ms > now_ms)
            .cloned()
            .collect();
        if kept.len() == records.len() {
            return Ok(false);
        }
        self.write(&kept)?;
        Ok(true)
    }

    /// Reads the store; a missing file is an empty store, a broken one is a
    /// named error (the caller refuses rather than admitting).
    fn read(&self) -> Result<Vec<Record>, AuthError> {
        let raw = match fs::read_to_string(&self.path) {
            Ok(raw) => raw,
            Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(AuthError::Io(error)),
        };
        let path = self.path.display().to_string();
        let parsed: Value = serde_json::from_str(&raw)
            .map_err(|_| AuthError::SessionStoreNotJson { path: path.clone() })?;
        let object = parsed
            .as_object()
            .ok_or_else(|| AuthError::SessionStoreNotObject { path: path.clone() })?;
        let sessions = object
            .get("sessions")
            .and_then(Value::as_array)
            .ok_or_else(|| AuthError::SessionStoreMalformed { path: path.clone() })?;

        let mut records = Vec::with_capacity(sessions.len());
        for value in sessions {
            let record = value
                .as_object()
                .ok_or_else(|| AuthError::SessionStoreMalformed { path: path.clone() })?;
            let text = |name: &str| record.get(name).and_then(Value::as_str);
            let number = |name: &str| record.get(name).and_then(Value::as_u64);
            let (Some(id), Some(device), Some(salt), Some(hash)) =
                (text("id"), text("device"), text("salt"), text("hash"))
            else {
                return Err(AuthError::SessionStoreMalformed { path });
            };
            let (Some(created_at_ms), Some(expires_at_ms), Some(last_seen_at_ms)) = (
                number("createdAtMs"),
                number("expiresAtMs"),
                number("lastSeenAtMs"),
            ) else {
                return Err(AuthError::SessionStoreMalformed { path });
            };
            records.push(Record {
                id: id.to_owned(),
                device: device.to_owned(),
                salt: decode_hex(salt)
                    .ok_or_else(|| AuthError::SessionStoreMalformed { path: path.clone() })?,
                hash: decode_hex(hash)
                    .ok_or_else(|| AuthError::SessionStoreMalformed { path: path.clone() })?,
                created_at_ms,
                expires_at_ms,
                last_seen_at_ms,
            });
        }
        Ok(records)
    }

    /// Writes beside the store and renames into place: an interrupted write
    /// leaves the previous store behind, never half of a new one.
    fn write(&self, records: &[Record]) -> Result<(), AuthError> {
        let sessions: Vec<Value> = records
            .iter()
            .map(|record| {
                let mut object = Map::new();
                object.insert("id".to_owned(), Value::String(record.id.clone()));
                object.insert("device".to_owned(), Value::String(record.device.clone()));
                object.insert("salt".to_owned(), Value::String(hex_encode(&record.salt)));
                object.insert("hash".to_owned(), Value::String(hex_encode(&record.hash)));
                object.insert("createdAtMs".to_owned(), Value::from(record.created_at_ms));
                object.insert("expiresAtMs".to_owned(), Value::from(record.expires_at_ms));
                object.insert(
                    "lastSeenAtMs".to_owned(),
                    Value::from(record.last_seen_at_ms),
                );
                Value::Object(object)
            })
            .collect();
        let file = Value::Object(Map::from_iter([
            ("version".to_owned(), Value::from(1)),
            ("sessions".to_owned(), Value::Array(sessions)),
        ]));
        let mut body = serde_json::to_string_pretty(&file)
            .map_err(|error| AuthError::Io(std::io::Error::other(error.to_string())))?;
        body.push('\n');

        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        let tmp = self.path.with_file_name(format!(
            "{}.{}.tmp",
            self.path.file_name().unwrap_or_default().to_string_lossy(),
            std::process::id()
        ));
        write_private(&tmp, body.as_bytes())?;
        fs::rename(&tmp, &self.path).map_err(|error| {
            let _ = fs::remove_file(&tmp);
            AuthError::Io(error)
        })?;
        Ok(())
    }
}

/// One session record as the file holds it.
#[derive(Clone)]
struct Record {
    id: String,
    device: String,
    salt: Vec<u8>,
    hash: Vec<u8>,
    created_at_ms: u64,
    expires_at_ms: u64,
    last_seen_at_ms: u64,
}

/// The ISO stamp of a session's expiry, for the wire.
pub fn expires_at_iso(issued: &IssuedSession) -> String {
    iso_from_ms(issued.expires_at_ms)
}

fn random_bytes(len: usize) -> Result<Vec<u8>, AuthError> {
    let mut bytes = vec![0u8; len];
    getrandom::fill(&mut bytes).map_err(|error| AuthError::Entropy(error.to_string()))?;
    Ok(bytes)
}

fn random_hex(len: usize) -> Result<String, AuthError> {
    Ok(hex_encode(&random_bytes(len)?))
}

/// SHA-256 over salt bytes then the token's UTF-8 — the same construction the
/// token store uses.
fn hash_token(salt: &[u8], token: &str) -> Vec<u8> {
    let mut hasher = Sha256::new();
    hasher.update(salt);
    hasher.update(token.as_bytes());
    hasher.finalize().to_vec()
}

/// `Buffer.from(salt, 'hex')` semantics: anything undecodable is a malformed
/// record, not a mismatch.
fn decode_hex(text: &str) -> Option<Vec<u8>> {
    let bytes = text.as_bytes();
    if !bytes.len().is_multiple_of(2) {
        return None;
    }
    bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| {
            let high = (pair[0] as char).to_digit(16)?;
            let low = (pair[1] as char).to_digit(16)?;
            Some(((high << 4) | low) as u8)
        })
        .collect()
}
