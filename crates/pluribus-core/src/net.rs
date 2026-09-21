//! Destination address policy shared by every network transport.
//!
//! Resolution belongs to the host crate that owns the connection; the
//! judgement about a resolved address belongs here so HTTP and TLS streams
//! cannot drift apart on what counts as reachable.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// Whether an address is on the public internet.
///
/// Loopback, private, link-local, multicast, and the documentation and
/// benchmarking ranges are not. A grant must opt in explicitly to reach them.
#[must_use]
pub fn is_public_address(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => is_public_v4(address),
        IpAddr::V6(address) => is_public_v6(address),
    }
}

fn is_public_v4(address: Ipv4Addr) -> bool {
    let [a, b, c, _] = address.octets();
    !(address.is_private()
        || address.is_loopback()
        || address.is_link_local()
        || address.is_multicast()
        || address.is_unspecified()
        || address == Ipv4Addr::BROADCAST
        || a == 0
        || a >= 240
        || (a == 100 && (64..=127).contains(&b))
        || (a == 192 && b == 0 && c == 0)
        || (a == 192 && b == 0 && c == 2)
        || (a == 198 && (b == 18 || b == 19))
        || (a == 198 && b == 51 && c == 100)
        || (a == 203 && b == 0 && c == 113))
}

fn is_public_v6(address: Ipv6Addr) -> bool {
    let segments = address.segments();
    if let Some(mapped) = address.to_ipv4_mapped() {
        return is_public_v4(mapped);
    }
    !(address.is_loopback()
        || address.is_unspecified()
        || address.is_multicast()
        || (segments[0] & 0xfe00) == 0xfc00
        || (segments[0] & 0xffc0) == 0xfe80
        || (segments[0] == 0x2001 && segments[1] == 0x0db8))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn public(literal: &str) -> bool {
        is_public_address(literal.parse().unwrap())
    }

    #[test]
    fn routable_addresses_are_public() {
        assert!(public("17.253.144.10"));
        assert!(public("2606:4700:4700::1111"));
    }

    #[test]
    fn local_and_reserved_addresses_are_not() {
        for literal in [
            "127.0.0.1",
            "10.0.0.1",
            "192.168.1.1",
            "172.16.0.1",
            "169.254.169.254",
            "100.64.0.1",
            "203.0.113.1",
            "::1",
            "fd00::1",
            "fe80::1",
            "::ffff:127.0.0.1",
        ] {
            assert!(!public(literal), "{literal} must not be public");
        }
    }
}
