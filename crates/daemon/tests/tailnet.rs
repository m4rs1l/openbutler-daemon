//! Tailnet address rules — fail-before pins for the team slice's T3a
//! (legacy `tailnet.ts`, 62 lines, read 2026-11-22). Truth tables first;
//! the port follows. The lister is injectable so these never touch the
//! host's real interfaces.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use openbutler_daemon::tailnet::{
    AddressFamily, InterfaceAddress, find_tailnet_address, is_loopback, is_tailnet_address,
    os_interface_addresses,
};

fn addr(address: &str, family: AddressFamily, internal: bool) -> InterfaceAddress {
    InterfaceAddress {
        address: address.to_owned(),
        family,
        internal,
    }
}

#[test]
fn tailnet_range_is_the_shared_cgnat_slash_ten() {
    // 100.64.0.0/10 — the Tailscale block itself.
    assert!(is_tailnet_address("100.64.0.1"));
    assert!(is_tailnet_address("100.64.0.0"));
    assert!(is_tailnet_address("100.127.255.255"));
    // Just outside, both directions.
    assert!(!is_tailnet_address("100.128.0.1"));
    assert!(!is_tailnet_address("100.63.255.255"));
    // Everything else.
    assert!(!is_tailnet_address("10.0.0.1"));
    assert!(!is_tailnet_address("192.168.1.1"));
    assert!(!is_tailnet_address("0.0.0.0"));
}

#[test]
fn tailnet_membership_rejects_every_malformed_shape() {
    assert!(!is_tailnet_address(""));
    assert!(!is_tailnet_address("100.64.0"));
    assert!(!is_tailnet_address("100.64.0.1.2"));
    assert!(!is_tailnet_address("a.b.c.d"));
    assert!(!is_tailnet_address("256.64.0.1"));
    assert!(!is_tailnet_address("-1.64.0.1"));
}

#[test]
fn tailnet_membership_keeps_the_source_number_coercions() {
    // JS `Number("064")` is 64 and `Number(" 100")` is 100 — the source
    // converts per part before validating, so these count. Documented,
    // source-faithful quirks of `isTailnetAddress`, not accidental ones.
    assert!(is_tailnet_address("100.064.0.1"));
    assert!(is_tailnet_address(" 100.64.0.1"));
    assert!(is_tailnet_address("100.64.0.1 "));
    // JS Number("") is 0, so a trailing empty part is 100.64.0.0.
    assert!(is_tailnet_address("100.64.0."));
}

#[test]
fn loopback_covers_localhost_the_whole_127_block_and_ip6_loopback() {
    assert!(is_loopback("localhost"));
    assert!(is_loopback("LOCALHOST"));
    assert!(is_loopback("::1"));
    assert!(is_loopback("127.0.0.1"));
    assert!(is_loopback("127.1.2.3"));
    assert!(!is_loopback("128.1.1.1"));
    assert!(!is_loopback("10.0.0.1"));
    assert!(!is_loopback("::2"));
    assert!(!is_loopback("1.2.3"));
    assert!(!is_loopback(""));
    // All four parts validate in the source, and Number() trims: a NaN
    // tail refuses even when the first part is 127.
    assert!(!is_loopback("127.0.0.x"));
    assert!(is_loopback(" 127.0.0.1"));
}

#[test]
fn first_non_internal_v4_address_in_range_wins() {
    let found = find_tailnet_address(|| {
        Ok(vec![
            addr("192.168.1.10", AddressFamily::V4, false),
            addr("100.64.0.7", AddressFamily::V4, false),
            addr("100.64.0.8", AddressFamily::V4, false),
        ])
    });
    assert_eq!(found.as_deref(), Some("100.64.0.7"));
}

#[test]
fn internal_and_ip6_addresses_are_skipped() {
    // Loopback interfaces (node's `internal: true`) and IPv6 never count,
    // even when the v6 address would be tempting text.
    let found = find_tailnet_address(|| {
        Ok(vec![
            addr("100.64.0.9", AddressFamily::V4, true),
            addr("fd7a:115c:a1e0::1", AddressFamily::V6, false),
            addr("100.64.0.10", AddressFamily::V4, false),
        ])
    });
    assert_eq!(found.as_deref(), Some("100.64.0.10"));
    let none = find_tailnet_address(|| Ok(vec![addr("100.64.0.9", AddressFamily::V4, true)]));
    assert_eq!(none, None);
}

#[test]
fn a_failing_lister_reads_as_absent_tailnet() {
    // Source: `try { interfaces = lister(); } catch { return null; }`.
    let found = find_tailnet_address(|| Err(std::io::Error::other("no interfaces")));
    assert_eq!(found, None);
}

#[test]
fn the_os_lister_returns_addresses_without_panic() {
    // Production wiring through if-addrs; the machine may or may not be on
    // a tailnet — the contract is only that it does not fail hard.
    let addresses = os_interface_addresses().expect("interface scan succeeds");
    for entry in &addresses {
        assert!(!entry.address.is_empty());
        // Every returned string must parse as one of the two families.
        assert!(
            entry.address.parse::<std::net::Ipv4Addr>().is_ok()
                || entry.address.parse::<std::net::Ipv6Addr>().is_ok(),
            "not an ip literal: {}",
            entry.address
        );
    }
}
