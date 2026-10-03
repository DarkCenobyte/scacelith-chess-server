//! Rate-limit keys of client addresses, as the former server built them: an IPv4 address (IPv4-
//! mapped IPv6 included) is its own key, an IPv6 address is reduced to its /64 ([`ip_key`]: one
//! customer network) or its /48 ([`prefix_key`]: one site), and what is not an address is kept as
//! it is.

use std::net::{Ipv4Addr, Ipv6Addr};

use crate::security::encoding::js_trim;

/// Canonical form of an address: brackets and zone ids dropped, IPv4-mapped IPv6 (`::ffff:1.2.3.4`)
/// turned into IPv4, IPv6 lower-cased (not otherwise rewritten). `None` for anything that is not
/// an IP address.
pub fn normalize_address(ip: &str) -> Option<String> {
    let mut a = js_trim(ip);
    if a.len() >= 2 && a.starts_with('[') && a.ends_with(']') {
        a = &a[1..a.len() - 1];
    }
    if let Some(pct) = a.find('%') {
        a = &a[..pct];
    }
    let lower = a.to_ascii_lowercase();
    let a = match lower.strip_prefix("::ffff:") {
        Some(v4) if lower.contains('.') => v4,
        _ => lower.as_str(),
    };
    if a.parse::<Ipv4Addr>().is_ok() || a.parse::<Ipv6Addr>().is_ok() { Some(a.to_string()) } else { None }
}

/// The address group of `ip`: an IPv4 address itself, or the /64 (`v6_prefix` 64) or /48 (48) of
/// an IPv6 address, written `2001:db8:1:2::/64` (groups in lower-case hex without leading
/// zeros). `None` when `ip` is not an address.
fn group_key(ip: &str, v6_prefix: u8) -> Option<String> {
    let a = normalize_address(ip)?;
    let Ok(v6) = a.parse::<Ipv6Addr>() else {
        return Some(a);
    };
    let s = v6.segments();
    Some(if v6_prefix == 48 {
        format!("{:x}:{:x}:{:x}::/48", s[0], s[1], s[2])
    } else {
        format!("{:x}:{:x}:{:x}:{:x}::/64", s[0], s[1], s[2], s[3])
    })
}

/// The rate-limit key of a client: its IPv4 address, or the /64 of its IPv6 address; anything
/// that is not an address is kept as is (`""` stays `""`).
pub fn ip_key(ip: &str) -> String {
    group_key(ip, 64).unwrap_or_else(|| ip.to_string())
}

/// The wider source of a client: its IPv4 address, or the /48 of its IPv6 address (65536 /64
/// networks, often one customer or one hosting provider's client); anything that is not an
/// address is kept as is.
pub fn prefix_key(ip: &str) -> String {
    group_key(ip, 48).unwrap_or_else(|| ip.to_string())
}

/// The address itself in canonical form ([`normalize_address`]); anything that is not an address
/// is kept as is.
pub fn normalize_ip(ip: &str) -> String {
    normalize_address(ip).unwrap_or_else(|| ip.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_address_keys_ipv4_mapped_and_ipv6_64() {
        assert_eq!(ip_key("198.51.100.4"), "198.51.100.4");
        assert_eq!(ip_key("::ffff:198.51.100.4"), "198.51.100.4");
        assert_eq!(ip_key("2001:db8:aa:bb:1:2:3:4"), "2001:db8:aa:bb::/64");
        assert_eq!(ip_key("2001:db8:aa:bb::9"), "2001:db8:aa:bb::/64");
        assert_eq!(normalize_ip("::ffff:10.1.2.3"), "10.1.2.3");
        assert_eq!(normalize_ip("::1"), "::1");
    }

    #[test]
    fn client_source_keys_ipv4_and_ipv6_48() {
        assert_eq!(prefix_key("198.51.100.4"), "198.51.100.4");
        assert_eq!(prefix_key("::ffff:198.51.100.4"), "198.51.100.4");
        assert_eq!(prefix_key("2001:db8:aa:bb:1:2:3:4"), "2001:db8:aa::/48");
        assert_eq!(
            prefix_key("2001:db8:aa:ffff::9"),
            "2001:db8:aa::/48",
            "every /64 of the /48 has the same key"
        );
        assert_eq!(prefix_key("2001:DB8:0AA::1"), "2001:db8:aa::/48");
        assert_ne!(prefix_key("2001:db8:ab::1"), prefix_key("2001:db8:aa::1"));
        assert_eq!([ip_key(""), prefix_key(""), normalize_ip("")], ["", "", ""]);
        assert_eq!([ip_key("garbage"), prefix_key("garbage"), normalize_ip("garbage")], ["garbage"; 3]);
    }

    /// `[input, ipKey, prefixKey, normalizeIp]` computed by the former server (Node 22).
    const NODE_VECTORS: &[[&str; 4]] = &[
        ["203.0.113.7", "203.0.113.7", "203.0.113.7", "203.0.113.7"],
        ["::ffff:203.0.113.7", "203.0.113.7", "203.0.113.7", "203.0.113.7"],
        ["::FFFF:203.0.113.7", "203.0.113.7", "203.0.113.7", "203.0.113.7"],
        ["2001:DB8:0001:0002:0003::1", "2001:db8:1:2::/64", "2001:db8:1::/48", "2001:db8:0001:0002:0003::1"],
        ["[2001:db8::1%eth0]", "2001:db8:0:0::/64", "2001:db8:0::/48", "2001:db8::1"],
        ["::1", "0:0:0:0::/64", "0:0:0::/48", "::1"],
        ["::", "0:0:0:0::/64", "0:0:0::/48", "::"],
        ["1::", "1:0:0:0::/64", "1:0:0::/48", "1::"],
        ["::ffff:0:1.2.3.4", "::ffff:0:1.2.3.4", "::ffff:0:1.2.3.4", "::ffff:0:1.2.3.4"],
        ["01.2.3.4", "01.2.3.4", "01.2.3.4", "01.2.3.4"],
        ["1.2.3", "1.2.3", "1.2.3", "1.2.3"],
        [" 10.0.0.1 ", "10.0.0.1", "10.0.0.1", "10.0.0.1"],
        ["[::1]", "0:0:0:0::/64", "0:0:0::/48", "::1"],
        ["fe80::1%eth0", "fe80:0:0:0::/64", "fe80:0:0::/48", "fe80::1"],
        ["2001:db8:1:2:3:4:5.6.7.8", "2001:db8:1:2::/64", "2001:db8:1::/48", "2001:db8:1:2:3:4:5.6.7.8"],
        ["::5.6.7.8", "0:0:0:0::/64", "0:0:0::/48", "::5.6.7.8"],
        ["2001:db8::", "2001:db8:0:0::/64", "2001:db8:0::/48", "2001:db8::"],
        [
            "2001:0db8:0000:0000:0000:ff00:0042:8329",
            "2001:db8:0:0::/64",
            "2001:db8:0::/48",
            "2001:0db8:0000:0000:0000:ff00:0042:8329",
        ],
        ["2001:DB8:0AA::1", "2001:db8:aa:0::/64", "2001:db8:aa::/48", "2001:db8:0aa::1"],
        ["::ffff:1.2.3", "::ffff:1.2.3", "::ffff:1.2.3", "::ffff:1.2.3"],
        ["1:2:3:4:5:6:7:8:9", "1:2:3:4:5:6:7:8:9", "1:2:3:4:5:6:7:8:9", "1:2:3:4:5:6:7:8:9"],
        ["12345::", "12345::", "12345::", "12345::"],
        ["ffff::ffff:1.2.3.4", "ffff:0:0:0::/64", "ffff:0:0::/48", "ffff::ffff:1.2.3.4"],
        ["::ffff:c000:280", "0:0:0:0::/64", "0:0:0::/48", "::ffff:c000:280"],
        ["255.255.255.255", "255.255.255.255", "255.255.255.255", "255.255.255.255"],
        ["256.1.1.1", "256.1.1.1", "256.1.1.1", "256.1.1.1"],
        ["1.2.3.4%eth0", "1.2.3.4", "1.2.3.4", "1.2.3.4"],
        ["[1.2.3.4]", "1.2.3.4", "1.2.3.4", "1.2.3.4"],
        ["G::1", "G::1", "G::1", "G::1"],
        ["0.0.0.0", "0.0.0.0", "0.0.0.0", "0.0.0.0"],
        ["1:2:3:4:5:6:7::", "1:2:3:4::/64", "1:2:3::/48", "1:2:3:4:5:6:7::"],
        ["::2:3:4:5:6:7:8", "0:2:3:4::/64", "0:2:3::/48", "::2:3:4:5:6:7:8"],
        ["1:2:3:4:5:6::8", "1:2:3:4::/64", "1:2:3::/48", "1:2:3:4:5:6::8"],
        ["[::ffff:1.2.3.4]", "1.2.3.4", "1.2.3.4", "1.2.3.4"],
        ["::ffff:01.2.3.4", "::ffff:01.2.3.4", "::ffff:01.2.3.4", "::ffff:01.2.3.4"],
        ["0001:2:3:4:5:6:7:8", "1:2:3:4::/64", "1:2:3::/48", "0001:2:3:4:5:6:7:8"],
        ["00001::", "00001::", "00001::", "00001::"],
        ["::1:2:3:4:5:6:7", "0:1:2:3::/64", "0:1:2::/48", "::1:2:3:4:5:6:7"],
        ["1:2:3:4:5:6:7::8", "1:2:3:4:5:6:7::8", "1:2:3:4:5:6:7::8", "1:2:3:4:5:6:7::8"],
        [":1::", ":1::", ":1::", ":1::"],
        ["1::2::3", "1::2::3", "1::2::3", "1::2::3"],
        ["[::1", "[::1", "[::1", "[::1"],
        ["fe80::1%", "fe80:0:0:0::/64", "fe80:0:0::/48", "fe80::1"],
        ["%eth0", "%eth0", "%eth0", "%eth0"],
        ["::ffff:1.2.3.4%eth0", "1.2.3.4", "1.2.3.4", "1.2.3.4"],
        ["1300:6b00::0:6800:e378", "1300:6b00:0:0::/64", "1300:6b00:0::/48", "1300:6b00::0:6800:e378"],
        [
            "9a00:6500:0:b940:8000:0:cf00:0",
            "9a00:6500:0:b940::/64",
            "9a00:6500:0::/48",
            "9a00:6500:0:b940:8000:0:cf00:0",
        ],
        ["0:8338::6a00:0:4200", "0:8338:0:0::/64", "0:8338:0::/48", "0:8338::6a00:0:4200"],
        [
            "2180:f900:c100:0:c500:6d00:0:6100",
            "2180:f900:c100:0::/64",
            "2180:f900:c100::/48",
            "2180:f900:c100:0:c500:6d00:0:6100",
        ],
        ["128.184.0.0", "128.184.0.0", "128.184.0.0", "128.184.0.0"],
    ];

    #[test]
    fn keys_match_the_former_server() {
        for [input, key, prefix, normalized] in NODE_VECTORS {
            assert_eq!(ip_key(input), *key, "ipKey({input:?})");
            assert_eq!(prefix_key(input), *prefix, "prefixKey({input:?})");
            assert_eq!(normalize_ip(input), *normalized, "normalizeIp({input:?})");
        }
    }
}
