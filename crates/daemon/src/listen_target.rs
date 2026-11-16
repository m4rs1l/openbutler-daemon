//! The `--listen` target rule: one parser for the flag and for a saved
//! `daemon.listen` preference, so the preference and the flag it came from
//! cannot drift apart.
//!
//! Source: `listen-target.ts` + `tailnet.ts` (read 2026-11-16).

/// A parsed listen target: an IPv4 literal the daemon may bind and a port.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListenTarget {
    pub host: String,
    pub port: u16,
}

/// Refusals of the listen-target rule, worded exactly as the contract.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
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
    if !is_loopback_v4(host) && !is_tailnet_v4(host) {
        return Err(ListenTargetError::NotTailnet {
            host: host.to_owned(),
        });
    }
    Ok(ListenTarget {
        host: host.to_owned(),
        port: port as u16,
    })
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

fn octets(value: &str) -> Option<[u8; 4]> {
    let mut out = [0u8; 4];
    for (index, group) in value.split('.').enumerate() {
        out[index] = group.parse().ok()?;
    }
    Some(out)
}

/// The whole 127.0.0.0/8 range is loopback: the one bind target outside the
/// tailnet that never leaves the machine.
fn is_loopback_v4(value: &str) -> bool {
    octets(value).is_some_and(|parts| parts[0] == 127)
}

/// True for an IPv4 address inside 100.64.0.0/10 (the shared CGNAT range
/// Tailscale assigns from). Anything else is not the tailnet.
fn is_tailnet_v4(value: &str) -> bool {
    const TAILNET_V4_BASE: u32 = 0x6440_0000; // 100.64.0.0
    const TAILNET_V4_MASK: u32 = 0xffc0_0000; // /10
    let Some(parts) = octets(value) else {
        return false;
    };
    let packed = u32::from_be_bytes(parts);
    (packed & TAILNET_V4_MASK) == TAILNET_V4_BASE
}
