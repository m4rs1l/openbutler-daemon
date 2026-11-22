//! Address rules shared by every tailnet-scoped service — the source's
//! `tailnet.ts` (62 lines, read 2026-11-22). The Tailscale range itself,
//! the interface scan that finds this machine's tailnet address, and
//! loopback — the one bind target outside the tailnet that never leaves
//! the machine. Feature-specific join or port policy stays with its
//! feature. Tests inject their own lister and never touch real interfaces.

use std::net::Ipv4Addr;

/// Tailscale assigns addresses from the shared CGNAT range.
/// 100.64.0.0/10 — anything else is not the tailnet.
const TAILNET_V4_BASE: u32 = 0x6440_0000;
const TAILNET_V4_MASK: u32 = 0xffc0_0000;

/// True for an IPv4 address inside 100.64.0.0/10 (`isTailnetAddress`).
///
/// The source converts each part with JS `Number()` before validating, so
/// `100.064.0.1` and ` 100.64.0.1` count — those quirks are kept, not
/// cleaned.
pub fn is_tailnet_address(value: &str) -> bool {
    parse_v4(value).is_some_and(|numbers| {
        let packed = (numbers[0] << 24) | (numbers[1] << 16) | (numbers[2] << 8) | numbers[3];
        (packed & TAILNET_V4_MASK) == TAILNET_V4_BASE
    })
}

/// JS `Number()` over one dotted quad: every part trims, an empty part
/// reads as 0, and each must land as an integer 0..=255 — the shared
/// validation half of `isTailnetAddress` and `isLoopback`. (Hex strings
/// are the one JS-only form: `Number("0x10")` parses there, not here.)
fn parse_v4(value: &str) -> Option<[u32; 4]> {
    let parts: Vec<&str> = value.split('.').collect();
    if parts.len() != 4 {
        return None;
    }
    let mut numbers = [0u32; 4];
    for (index, part) in parts.iter().enumerate() {
        let trimmed = part.trim();
        let parsed: f64 = if trimmed.is_empty() {
            0.0 // JS Number("") and Number("   ") are both 0.
        } else {
            trimmed.parse::<f64>().ok()?
        };
        if !parsed.is_finite() || parsed.fract() != 0.0 || parsed < 0.0 || parsed > 255.0 {
            return None;
        }
        numbers[index] = parsed as u32;
    }
    Some(numbers)
}

/// Address family of one scanned interface (`InterfaceAddress`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddressFamily {
    V4,
    V6,
}

/// One address as the scan reports it; `internal` marks loopback
/// interfaces exactly like node's `networkInterfaces()` flag.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InterfaceAddress {
    pub address: String,
    pub family: AddressFamily,
    pub internal: bool,
}

/// First non-internal IPv4 address in the tailnet range, or `None` when
/// this machine is not on a tailnet (`findTailnetAddress`). The lister is
/// injectable so tests never touch the real interfaces; a failing lister
/// reads as "not on a tailnet", like the source's try/catch.
pub fn find_tailnet_address<F>(lister: F) -> Option<String>
where
    F: FnOnce() -> std::io::Result<Vec<InterfaceAddress>>,
{
    let addresses = match lister() {
        Ok(addresses) => addresses,
        Err(_) => return None,
    };
    for address in addresses {
        if address.family != AddressFamily::V4 || address.internal {
            continue;
        }
        if is_tailnet_address(&address.address) {
            return Some(address.address);
        }
    }
    None
}

/// The production lister: every interface the OS reports, both families,
/// loopback flagged. Errors stay errors — `find_tailnet_address` turns
/// them into "not on a tailnet".
pub fn os_interface_addresses() -> std::io::Result<Vec<InterfaceAddress>> {
    let interfaces = if_addrs::get_if_addrs()?;
    Ok(interfaces
        .into_iter()
        .map(|interface| {
            let (family, address) = match interface.addr {
                if_addrs::IfAddr::V4(v4) => (AddressFamily::V4, v4.ip.to_string()),
                if_addrs::IfAddr::V6(v6) => (AddressFamily::V6, v6.ip.to_string()),
            };
            let internal = match address.parse::<Ipv4Addr>() {
                Ok(v4) => v4.is_loopback(),
                Err(_) => address
                    .parse::<std::net::Ipv6Addr>()
                    .map(|v6| v6.is_loopback())
                    .unwrap_or(false),
            };
            InterfaceAddress {
                address,
                family,
                internal,
            }
        })
        .collect())
}

/// True for a loopback target (`isLoopback`): the whole 127.0.0.0/8
/// range, `::1`, or `localhost` (case-insensitive). A tailnet-scoped
/// service accepts this as its one non-tailnet bind because traffic to it
/// never leaves the machine.
pub fn is_loopback(address: &str) -> bool {
    if address.eq_ignore_ascii_case("localhost") {
        return true;
    }
    if address == "::1" {
        return true;
    }
    parse_v4(address).is_some_and(|numbers| numbers[0] == 127)
}
