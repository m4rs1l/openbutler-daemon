//! The `--listen` target rule: one parser for the flag and for a saved
//! `daemon.listen` preference, so the preference and the flag it came from
//! cannot drift apart.
//!
//! Source: `listen-target.ts` (read 2026-11-16); the address rules it
//! reuses are `tailnet.ts`'s port in `crate::tailnet` — one authority for
//! both callers.

use crate::tailnet::{is_loopback, is_tailnet_address};

/// A parsed listen target: an IPv4 literal the daemon may bind and a port.
#[derive(Debug, Clone, PartialEq, Eq)]
/// The parsed `--listen` / `daemon.listen` target.
pub struct ListenTarget {
    pub host: String,
    pub port: u16,
}

/// Refusals of the listen-target rule, worded exactly as the contract.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
/// Why a listen target was refused (shape, range, or host).
pub enum ListenTargetError {
    #[error("--listen needs an IPv4 address and port, e.g. 100.64.0.5:45280 (got \"{spec}\").")]
    NotAnIpv4Literal { spec: String },
    #[error("Listen port {port} is out of range 1-65535.")]
    PortOutOfRange { port: u32 },
    #[error(
        "{host} is not a tailnet address. The daemon binds the tailnet or loopback only; 0.0.0.0 and public addresses are refused."
    )]
    NotTailnet { host: String },
}

/// Parse `--listen`: an IPv4 literal the daemon may bind (tailnet or
/// loopback) plus a port in 1-65535. The same rule the settings layer
/// validates a saved `daemon.listen` through, because a remote listener
/// without a tailnet boundary would expose every session to whatever can
/// route to the machine.
pub fn parse_listen_target(spec: &str) -> Result<ListenTarget, ListenTargetError> {
    let trimmed = spec.trim();
    let Some((host, port_text)) = trimmed.split_once(':') else {
        return Err(ListenTargetError::NotAnIpv4Literal {
            spec: spec.to_owned(),
        });
    };
    if !is_ipv4_literal(host)
        || port_text.is_empty()
        || port_text.len() > 5
        || !port_text.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err(ListenTargetError::NotAnIpv4Literal {
            spec: spec.to_owned(),
        });
    }
    let port: u32 = port_text
        .parse()
        .map_err(|_| ListenTargetError::NotAnIpv4Literal {
            spec: spec.to_owned(),
        })?;
    if !(1..=65535).contains(&port) {
        return Err(ListenTargetError::PortOutOfRange { port });
    }
    if !is_loopback(host) && !is_tailnet_address(host) {
        return Err(ListenTargetError::NotTailnet {
            host: host.to_owned(),
        });
    }
    Ok(ListenTarget {
        host: host.to_owned(),
        port: port as u16,
    })
}

/// The listener the host binds: an explicit flag wins, `suppress` refuses
/// the saved preference for this run, otherwise the saved `daemon.listen`
/// applies (source: `daemonRunOptions` — the spawn carries the settings
/// value, so a saved target binds without repeating the flag).
pub fn effective_listen(flag: Option<&str>, suppress: bool, saved: Option<&str>) -> Option<String> {
    match (flag, suppress) {
        (Some(spec), _) => Some(spec.to_owned()),
        (None, true) => None,
        (None, false) => saved.map(str::to_owned),
    }
}

/// Four groups of one-to-three digits — the shape, not the range: an octet
/// above 255 parses as a literal here and is refused by the address rules.
fn is_ipv4_literal(value: &str) -> bool {
    let groups: Vec<&str> = value.split('.').collect();
    groups.len() == 4
        && groups.iter().all(|group| {
            (1..=3).contains(&group.len()) && group.bytes().all(|byte| byte.is_ascii_digit())
        })
}
