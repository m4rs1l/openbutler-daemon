//! `admin.create`: the one admin account, created on the host machine only.
//!
//! The source had no account: a remote client proved itself with a token the
//! owner minted by hand on the host. The account replaces that bootstrap, and
//! its creation stays a host-local action — a port whose first frame could
//! create the account would be a takeover window.
//!
//! The password arrives in the payload because the masked prompt runs where the
//! terminal is (the same contract `keys.add` follows). It is never echoed back:
//! the answer carries the host's own line and nothing secret.

use std::path::Path;

use openbutler_protocol::DaemonRequest;
use openbutler_protocol::constants::{MAX_PASSWORD_LENGTH, MAX_USERNAME_LENGTH};
use serde_json::{Value, json};

use super::Host;
use crate::auth::AdminAccount;
use crate::shape::screen_line;

/// The account is created on the host machine only.
pub const REMOTE_REFUSAL: &str = "The admin account is created on the host machine only.";

/// Creates the account and answers with the host's own sentence.
pub fn create(_host: &Host, data_dir: &Path, request: &DaemonRequest) -> Result<Value, String> {
    let username = super::require(request.get("username"), "username", MAX_USERNAME_LENGTH * 4)?;
    let password = super::require(request.get("password"), "password", MAX_PASSWORD_LENGTH * 4)?;
    let summary = AdminAccount::new(data_dir)
        .create(&username, &password)
        .map_err(|error| error.to_string())?;
    let sentence = format!(
        "Admin account {} created. Sign in from a client with: openbutler connect <host:port> {}",
        summary.username, summary.username
    );
    Ok(json!({
        "username": summary.username,
        "createdAt": summary.created_at,
        "lines": [screen_line("success", sentence)],
    }))
}
