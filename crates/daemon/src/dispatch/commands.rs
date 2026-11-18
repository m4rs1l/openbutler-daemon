//! The slash-command registry's daemon half: `session.command` carries
//! a name, never an effect.
//!
//! Source: `commands/registry.ts` `daemonCommandAction` (L94) — the
//! single name-to-effect mapping; nothing else in the codebase is
//! allowed to restate it. Three names reach the daemon (`/new`,
//! `/compact`, `/stop`); every other name — surface effects like
//! `/help` or unknown text — is refused with `Unknown command.` The
//! surface vocabulary (menus, descriptions) stays with the surfaces
//! that render it.

use openbutler_protocol::DaemonRequest;
use openbutler_protocol::payload::require_string;

/// The action a `session.command` request re-dispatches as, or the
/// refusal. Resolution happens before the switch, so the target arm
/// reads the same payload the command arrived with.
pub fn daemon_command_action(request: &DaemonRequest) -> Result<&'static str, String> {
    let name =
        require_string(request.get("name"), "name", 64).map_err(|error| error.to_string())?;
    match name.as_str() {
        "/new" => Ok("session.new"),
        "/compact" => Ok("session.compact"),
        "/stop" => Ok("session.stop"),
        _ => Err("Unknown command.".to_owned()),
    }
}
