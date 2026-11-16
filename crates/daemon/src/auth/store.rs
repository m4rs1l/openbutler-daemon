//! The auth token store: `auth-tokens.json`, salted hashes only.
//!
//! The host keeps a per-token salt and a SHA-256 of salt+token, so a stolen
//! store yields no usable token; the plaintext exists exactly once, in the
//! mint result. Verification is timing-safe and fails closed: a missing
//! store admits nobody, and a broken store is an error the gate turns into
//! a refusal. The file format is byte-compatible with the TypeScript store
//! (`version: 1`, `tokens: [{ id, salt, hash, createdAt }]`).

use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use base64::Engine;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use super::{AuthError, ct_eq, hex_encode, write_private};

/// One minted token: the id lands in the store, the token never does.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Minted {
    pub id: String,
    pub token: String,
}

pub struct TokenStore {
    path: PathBuf,
}

impl TokenStore {
    /// The store lives beside the rest of the data directory.
    pub fn new(data_dir: &Path) -> Self {
        Self {
            path: data_dir.join("auth-tokens.json"),
        }
    }

    /// Mints a token, stores its salted hash, and returns the plaintext once.
    pub fn mint(&self) -> Result<Minted, AuthError> {
        let mut id_bytes = [0u8; 8];
        getrandom::fill(&mut id_bytes).map_err(|error| AuthError::Entropy(error.to_string()))?;
        let mut token_bytes = [0u8; 24];
        getrandom::fill(&mut token_bytes).map_err(|error| AuthError::Entropy(error.to_string()))?;
        let mut salt_bytes = [0u8; 16];
        getrandom::fill(&mut salt_bytes).map_err(|error| AuthError::Entropy(error.to_string()))?;

        let id = hex_encode(&id_bytes);
        let token = format!(
            "obt_{}",
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(token_bytes)
        );
        let salt = hex_encode(&salt_bytes);
        let hash = hex_encode(&hash_token(&salt_bytes, &token));

        let mut tokens = self.read()?.tokens;
        tokens.push(Map::from_iter([
            ("id".to_owned(), Value::String(id.clone())),
            ("salt".to_owned(), Value::String(salt)),
            ("hash".to_owned(), Value::String(hash)),
            ("createdAt".to_owned(), Value::String(creation_stamp())),
        ]));
        self.write(&tokens)?;

        Ok(Minted { id, token })
    }

    /// Timing-safe check against every minted token. An absent store is a
    /// plain `false`; a broken store is an error, never an admit.
    pub fn verify(&self, token: &str) -> Result<bool, AuthError> {
        if token.is_empty() {
            return Ok(false);
        }
        for record in self.read()?.tokens {
            let (Some(salt), Some(expected)) = (
                record
                    .get("salt")
                    .and_then(Value::as_str)
                    .and_then(decode_hex),
                record
                    .get("hash")
                    .and_then(Value::as_str)
                    .and_then(decode_hex),
            ) else {
                continue;
            };
            let candidate = hash_token(&salt, token);
            if candidate.len() == expected.len() && ct_eq(&candidate, &expected) {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Reads and validates the store, matching the TypeScript error texts.
    /// An absent file is an empty store.
    fn read(&self) -> Result<Store, AuthError> {
        let raw = match fs::read_to_string(&self.path) {
            Ok(raw) => raw,
            Err(error) if error.kind() == ErrorKind::NotFound => {
                return Ok(Store { tokens: Vec::new() });
            }
            Err(error) => return Err(AuthError::Io(error)),
        };
        let path = self.path.display().to_string();
        let parsed: Value = serde_json::from_str(&raw)
            .map_err(|_| AuthError::StoreNotJson { path: path.clone() })?;
        let object = parsed
            .as_object()
            .ok_or_else(|| AuthError::StoreNotObject { path: path.clone() })?;
        let tokens = object
            .get("tokens")
            .and_then(Value::as_array)
            .ok_or_else(|| AuthError::StoreMissingTokens { path: path.clone() })?;

        let mut records = Vec::with_capacity(tokens.len());
        for value in tokens {
            let record = value
                .as_object()
                .ok_or_else(|| AuthError::StoreMalformedRecord { path: path.clone() })?;
            for field in ["id", "salt", "hash", "createdAt"] {
                if !record.get(field).is_some_and(Value::is_string) {
                    return Err(AuthError::StoreMalformedRecord { path });
                }
            }
            records.push(record.clone());
        }
        Ok(Store { tokens: records })
    }

    /// Writes beside the store and renames into place: an interrupted write
    /// can only leave the previous store behind, never half of the new one.
    fn write(&self, tokens: &[Map<String, Value>]) -> Result<(), AuthError> {
        let file = Value::Object(Map::from_iter([
            ("version".to_owned(), Value::from(1)),
            (
                "tokens".to_owned(),
                Value::Array(tokens.iter().cloned().map(Value::Object).collect()),
            ),
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

struct Store {
    tokens: Vec<Map<String, Value>>,
}

/// `Buffer.from(salt, 'hex')` semantics: decode, and treat anything
/// undecodable as a mismatch rather than an error.
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

/// SHA-256 over salt bytes then the token's UTF-8, hex encoded — the exact
/// input order the TypeScript `hashToken` uses.
fn hash_token(salt: &[u8], token: &str) -> Vec<u8> {
    let mut hasher = Sha256::new();
    hasher.update(salt);
    hasher.update(token.as_bytes());
    hasher.finalize().to_vec()
}

/// The creation stamp in the TypeScript format: millisecond precision, Z.
fn creation_stamp() -> String {
    let now = time::OffsetDateTime::now_utc();
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        now.year(),
        u8::from(now.month()),
        now.day(),
        now.hour(),
        now.minute(),
        now.second(),
        now.millisecond()
    )
}
