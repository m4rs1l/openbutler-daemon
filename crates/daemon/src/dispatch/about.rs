//! `about.report`: what this machine runs — the program's version, the
//! resolved command shell, the runtime triple and the configuration
//! paths.
//!
//! Sources (read 2026-11-26): `cli-main.ts` `runAbout` (L997-1006) and
//! `configuredShellLabel` (L985-995). The source answered for one Node
//! process; here the host answers for itself — shell resolution included,
//! so the screen can never claim a shell the agent would not run — and
//! the CLI prints the answer.
//!
//! Divergences, recorded in the CLI's surface doc:
//! 1. The source's `Runtime` row was the Node version; this build has no
//!    Node in the path, so the row is the host's os/arch triple.
//! 2. `Runtime packages` (the source listed its supported npm packages)
//!    is the bundled sidecar: the packages ship inside it and this build
//!    has no npm resolution to enumerate them from.
//! 3. Only the paths are named — never a file's contents.

use std::path::Path;

use serde_json::{Value, json};

use super::Host;

/// Builds the answer. `Err` carries a client-visible sentence.
pub fn report(host: &Host, data_dir: &Path) -> Result<Value, String> {
    let configured = host
        .settings()
        .as_ref()
        .and_then(|loaded| loaded.store.command_shell.clone());
    let shell = crate::tools::execute_command::shell::resolve_shell(configured.as_deref())
        .map(|resolved| resolved.label)
        .unwrap_or_else(|error| format!("unavailable ({error})"));
    Ok(json!({
        "version": env!("CARGO_PKG_VERSION"),
        "pid": std::process::id(),
        "shell": shell,
        "runtime": format!("{}/{}", std::env::consts::OS, std::env::consts::ARCH),
        "settingsPath": crate::settings::settings_path(data_dir).display().to_string(),
        "secretsPath": crate::settings::config_path(data_dir).display().to_string(),
        "packages": "the bundled sidecar (AI SDK 7)",
    }))
}
