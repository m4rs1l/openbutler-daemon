//! The admin account: one username, one argon2id verifier, no plaintext.
//!
//! `admin.json` beside the rest of the data directory. The stored value is a
//! PHC string (`$argon2id$v=19$m=19456,t=2,p=1$<salt>$<output>`), so the record
//! carries its own salt and parameters: raising the cost later does not lock
//! anyone out, because verification reads the parameters from the record it is
//! checking.
//!
//! Cost: argon2's defaults — 19 MiB, two passes, one lane, the OWASP baseline.
//! That is deliberately slow (tens of milliseconds per verification), which is
//! why the sign-in path also carries a throttle.
//!
//! Fail closed: a missing file means no account, and a file that cannot be read
//! means no account either. A broken store must never be read as "configured
//! with no password".

use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use argon2::Argon2;
use argon2::password_hash::phc::PasswordHash;
use argon2::password_hash::{PasswordHasher, PasswordVerifier};
use openbutler_protocol::constants::MAX_USERNAME_LENGTH;
use openbutler_protocol::envelope::js_length;
use serde_json::{Map, Value};

use super::creation_stamp;
use super::{AuthError, write_private};

/// The shortest password the host accepts, in JS string length units.
pub const MIN_PASSWORD_LENGTH: usize = 8;

/// The public face of the account: what the host may print about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountSummary {
    /// The account's username.
    pub username: String,
    /// When the account was created (millisecond, `Z`).
    pub created_at: String,
}

/// The admin account store. Every method is safe to call on a fresh data
/// directory: the answers are "no account", never a panic.
pub struct AdminAccount {
    path: PathBuf,
}

impl AdminAccount {
    /// The store lives beside the rest of the data directory.
    pub fn new(data_dir: &Path) -> Self {
        Self {
            path: data_dir.join("admin.json"),
        }
    }

    /// Whether a usable account exists. A missing or broken file is `false`,
    /// so nothing that binds a door on this answer can open on a broken store.
    pub fn is_configured(&self) -> bool {
        self.read().is_ok()
    }

    /// The account's public summary, or [`AuthError::NoAccount`].
    pub fn summary(&self) -> Result<AccountSummary, AuthError> {
        let record = self.read()?;
        Ok(AccountSummary {
            username: record.username,
            created_at: record.created_at,
        })
    }

    /// Creates the account. Refuses an existing one (change it with
    /// [`Self::change_password`]) and a broken store (fix or delete it), so a
    /// typo in `admin.json` can never be silently replaced.
    pub fn create(&self, username: &str, password: &str) -> Result<AccountSummary, AuthError> {
        let username = username.trim();
        if !(1..=MAX_USERNAME_LENGTH).contains(&js_length(username)) {
            return Err(AuthError::AccountUsernameInvalid);
        }
        if js_length(password) < MIN_PASSWORD_LENGTH {
            return Err(AuthError::PasswordTooShort);
        }
        match self.read() {
            Ok(_) => return Err(AuthError::AccountExists),
            Err(AuthError::NoAccount) => {}
            Err(error) => return Err(error),
        }
        let hash = hash_password(password)?;
        let created_at = creation_stamp();
        self.write(&Record {
            username: username.to_owned(),
            hash,
            created_at: created_at.clone(),
            updated_at: created_at.clone(),
        })?;
        Ok(AccountSummary {
            username: username.to_owned(),
            created_at,
        })
    }

    /// Whether `username` + `password` is this account.
    ///
    /// The verification always runs, even when the username does not match:
    /// an unknown username must cost exactly what a known one costs, or the
    /// answer's latency becomes a username oracle.
    pub fn verify(&self, username: &str, password: &str) -> Result<bool, AuthError> {
        let record = self.read()?;
        let parsed =
            PasswordHash::new(&record.hash).map_err(|error| AuthError::Hash(error.to_string()))?;
        let matched = Argon2::default()
            .verify_password(password.as_bytes(), &parsed)
            .is_ok();
        Ok(matched && record.username == username)
    }

    /// Changes the password, requiring the current one. Sessions are not
    /// revoked here: that is `device revoke`'s job, one device at a time.
    pub fn change_password(&self, current: &str, next: &str) -> Result<(), AuthError> {
        let record = self.read()?;
        if js_length(next) < MIN_PASSWORD_LENGTH {
            return Err(AuthError::PasswordTooShort);
        }
        let parsed =
            PasswordHash::new(&record.hash).map_err(|error| AuthError::Hash(error.to_string()))?;
        if Argon2::default()
            .verify_password(current.as_bytes(), &parsed)
            .is_err()
        {
            return Err(AuthError::WrongPassword);
        }
        self.write(&Record {
            hash: hash_password(next)?,
            updated_at: creation_stamp(),
            ..record
        })
    }

    /// Reads and validates the record. A missing file is [`AuthError::NoAccount`];
    /// a file that does not describe an account is a named error, never an
    /// empty one.
    fn read(&self) -> Result<Record, AuthError> {
        let raw = match fs::read_to_string(&self.path) {
            Ok(raw) => raw,
            Err(error) if error.kind() == ErrorKind::NotFound => return Err(AuthError::NoAccount),
            Err(error) => return Err(AuthError::Io(error)),
        };
        let path = self.path.display().to_string();
        let parsed: Value = serde_json::from_str(&raw)
            .map_err(|_| AuthError::AccountNotJson { path: path.clone() })?;
        let object = parsed
            .as_object()
            .ok_or_else(|| AuthError::AccountNotObject { path: path.clone() })?;
        let field = |name: &'static str| -> Result<String, AuthError> {
            object
                .get(name)
                .and_then(Value::as_str)
                .map(str::to_owned)
                .ok_or(AuthError::AccountMissingField {
                    path: path.clone(),
                    field: name,
                })
        };
        let record = Record {
            username: field("username")?,
            hash: field("hash")?,
            created_at: field("createdAt")?,
            updated_at: field("updatedAt")?,
        };
        if record.username.is_empty() || record.hash.is_empty() {
            return Err(AuthError::AccountMalformed { path });
        }
        Ok(record)
    }

    /// Writes beside the record and renames into place, so an interrupted
    /// write leaves the previous account behind rather than half of a new one.
    fn write(&self, record: &Record) -> Result<(), AuthError> {
        let file = Value::Object(Map::from_iter([
            ("version".to_owned(), Value::from(1)),
            (
                "username".to_owned(),
                Value::String(record.username.clone()),
            ),
            ("hash".to_owned(), Value::String(record.hash.clone())),
            (
                "createdAt".to_owned(),
                Value::String(record.created_at.clone()),
            ),
            (
                "updatedAt".to_owned(),
                Value::String(record.updated_at.clone()),
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

/// One account record as the file holds it.
struct Record {
    username: String,
    hash: String,
    created_at: String,
    updated_at: String,
}

/// Hashes with argon2's defaults; the PHC string carries salt and parameters.
fn hash_password(password: &str) -> Result<String, AuthError> {
    Argon2::default()
        .hash_password(password.as_bytes())
        .map(|hash| hash.to_string())
        .map_err(|error| AuthError::Hash(error.to_string()))
}
