//! `settings.set-listen`: the saved remote-listener preference.
//!
//! Source: `cli-main.ts` L1783-1798 (`listenPreference`): `--listen` is
//! validated *before* anything is written or spawned, and `--no-listen`
//! clears it. The write is the daemon's — backend-owned: the source wrote
//! settings from the CLI process, and this client never touches files.
//!
//! Payload: `{ "listen": "<host:port>" }` saves, `{ "listen": null }`
//! clears. The refusal wording is the listen-target rule's own
//! (`listen_target.rs`), the same rule the daemon binds by, so a saved
//! target can never be one the host would refuse to bind.

use std::path::Path;

use openbutler_protocol::DaemonRequest;
use serde_json::{Value, json};

use crate::dispatch::Host;
use crate::listen_target::parse_listen_target;
use crate::settings::schema::DaemonSettings;

/// Saves or clears `daemon.listen` and answers the stored value.
pub fn handle(host: &Host, data_dir: &Path, request: &DaemonRequest) -> Result<Value, String> {
    let listen = match request.get("listen") {
        Some(Value::Null) => None,
        Some(Value::String(text)) => {
            parse_listen_target(text).map_err(|error| error.to_string())?;
            Some(text.clone())
        }
        // No source sentence exists (the source had no such action): one
        // message covers a missing and a non-string value.
        _ => return Err("listen must be a host:port address or null.".to_owned()),
    };
    let mut loaded = crate::settings::load_settings(data_dir).map_err(|error| error.to_string())?;
    loaded.store.daemon = listen.clone().map(|value| DaemonSettings { listen: value });
    crate::settings::save_settings(data_dir, &mut loaded).map_err(|error| error.to_string())?;
    host.reload_settings(data_dir)
        .map_err(|error| error.to_string())?;
    Ok(json!({ "listen": listen }))
}
