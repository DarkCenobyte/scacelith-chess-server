//! Client address parsing and address lists (`ABUSE_EXEMPT`, `TRUSTED_PROXIES`).
//!
//! Addresses are normalised as the Node server did: surrounding brackets and an IPv6 zone are
//! dropped, and an IPv4-mapped IPv6 address (`::ffff:1.2.3.4`, what a dual-stack listener reports
//! for an IPv4 client) becomes the IPv4 address, so one client has one form whatever the listener.

use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

/// Parses a client address: trimmed, `[...]` and `%zone` removed, IPv4-mapped IPv6 turned into
/// IPv4. `None` for anything that is not an IP address (host names included).
pub fn normalize_ip(text: &str) -> Option<IpAddr> {
    let mut a = super::js::trim(text);
    if a.len() >= 2 && a.starts_with('[') && a.ends_with(']') {
        a = &a[1..a.len() - 1];
    }
    if let Some(pct) = a.find('%') {
        a = &a[..pct];
    }
    match a.parse::<IpAddr>().ok()? {
        IpAddr::V6(v6) => Some(canonical(v6)),
        v4 => Some(v4),
    }
}

/// An IPv4-mapped IPv6 address as IPv4; any other address unchanged.
pub fn canonical_ip(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => canonical(v6),
        v4 => v4,
    }
}

fn canonical(v6: Ipv6Addr) -> IpAddr {
    match v6.to_ipv4_mapped() {
        Some(v4) => IpAddr::V4(v4),
        None => IpAddr::V6(v6),
    }
}

/// Why an entry of an address list was refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IpListError {
    /// The entry is neither an address nor `address/prefix`.
    NotAnAddress(String),
    /// The prefix length is not an integer within 0..32 (IPv4) or 0..128 (IPv6).
    BadPrefix(String),
}

impl fmt::Display for IpListError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            IpListError::NotAnAddress(e) => write!(f, "not an IP address or subnet: \"{e}\""),
            IpListError::BadPrefix(e) => write!(f, "bad prefix length: \"{e}\""),
        }
    }
}

impl std::error::Error for IpListError {}

/// A list of addresses and CIDR subnets (`10.0.0.0/8`, `::1`, `2001:db8::/48`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct IpMatcher {
    v4: Vec<(u32, u32)>,
    v6: Vec<(u128, u128)>,
}

impl IpMatcher {
    /// Parses every entry (each trimmed; empty entries skipped). The first invalid entry is an
    /// error, so a typo in the configuration is found at start-up. Host bits of a subnet are
    /// ignored (`10.1.2.3/8` is `10.0.0.0/8`).
    pub fn parse<S: AsRef<str>>(list: &[S]) -> Result<IpMatcher, IpListError> {
        let mut m = IpMatcher::default();
        for raw in list {
            let entry = super::js::trim(raw.as_ref());
            if entry.is_empty() {
                continue;
            }
            let (addr_text, prefix_text) = match entry.split_once('/') {
                Some((a, p)) => (a, Some(p)),
                None => (entry, None),
            };
            let addr = normalize_ip(addr_text).ok_or_else(|| IpListError::NotAnAddress(entry.to_string()))?;
            let max = if addr.is_ipv4() { 32 } else { 128 };
            let prefix = match prefix_text {
                None => max,
                Some(p) => {
                    let p = super::js::trim(p);
                    match p.parse::<u32>() {
                        Ok(n) if n <= max && !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()) => n,
                        _ => return Err(IpListError::BadPrefix(entry.to_string())),
                    }
                }
            };
            match addr {
                IpAddr::V4(a) => {
                    let mask = if prefix == 0 { 0 } else { u32::MAX << (32 - prefix) };
                    m.v4.push((u32::from(a) & mask, mask));
                }
                IpAddr::V6(a) => {
                    let mask = if prefix == 0 { 0 } else { u128::MAX << (128 - prefix) };
                    m.v6.push((u128::from(a) & mask, mask));
                }
            }
        }
        Ok(m)
    }

    /// Whether the list holds no entry.
    pub fn is_empty(&self) -> bool {
        self.v4.is_empty() && self.v6.is_empty()
    }

    /// Whether `ip` (IPv4-mapped IPv6 counts as IPv4) is in the list.
    pub fn contains(&self, ip: IpAddr) -> bool {
        match canonical_ip(ip) {
            IpAddr::V4(a) => {
                let a = u32::from(a);
                self.v4.iter().any(|(net, mask)| a & mask == *net)
            }
            IpAddr::V6(a) => {
                let a = u128::from(a);
                self.v6.iter().any(|(net, mask)| a & mask == *net)
            }
        }
    }

    /// Whether the address written in `text` is in the list (`false` when it is not an address).
    pub fn contains_text(&self, text: &str) -> bool {
        normalize_ip(text).is_some_and(|ip| self.contains(ip))
    }
}

/// The unspecified IPv4 address, a placeholder for an unknown client.
pub const UNKNOWN_IP: IpAddr = IpAddr::V4(Ipv4Addr::UNSPECIFIED);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_like_node() {
        assert_eq!(normalize_ip(" ::FFFF:192.0.2.7 ").unwrap().to_string(), "192.0.2.7");
        assert_eq!(normalize_ip("[2001:DB8::1]").unwrap().to_string(), "2001:db8::1");
        assert_eq!(normalize_ip("fe80::1%eth0").unwrap().to_string(), "fe80::1");
        assert_eq!(normalize_ip("192.0.2.7").unwrap().to_string(), "192.0.2.7");
        for bad in ["", "localhost", "300.1.2.3", "01.2.3.4", "1.2.3", "::g"] {
            assert!(normalize_ip(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn matcher_entries_and_errors() {
        let m = IpMatcher::parse(&[
            " 203.0.113.7 ",
            "2001:db8:12::/48",
            "10.1.2.3/8",
            "",
            "::ffff:198.51.100.0/24",
        ])
        .unwrap();
        assert!(m.contains_text("198.51.100.200"));
        assert!(m.contains_text("203.0.113.7"));
        assert!(!m.contains_text("203.0.113.8"));
        assert!(m.contains_text("10.200.0.1"));
        assert!(m.contains_text("::ffff:10.0.0.1"));
        assert!(m.contains_text("2001:db8:12:ffff::1"));
        assert!(!m.contains_text("2001:db8:13::1"));
        assert!(!m.contains_text("not an address"));
        assert!(IpMatcher::parse::<&str>(&[]).unwrap().is_empty());
        assert!(IpMatcher::parse(&["0.0.0.0/0"]).unwrap().contains_text("8.8.8.8"));
        let err = |e: &str| IpMatcher::parse(&["127.0.0.1", e]).unwrap_err().to_string();
        assert_eq!(err("school.example.org"), "not an IP address or subnet: \"school.example.org\"");
        assert_eq!(err("10.0.0.0/33"), "bad prefix length: \"10.0.0.0/33\"");
        assert_eq!(err("2001:db8::/129"), "bad prefix length: \"2001:db8::/129\"");
        assert_eq!(err("300.1.2.3"), "not an IP address or subnet: \"300.1.2.3\"");
        assert_eq!(err("10.0.0.0/"), "bad prefix length: \"10.0.0.0/\"");
        assert_eq!(err("10.0.0.0/+8"), "bad prefix length: \"10.0.0.0/+8\"");
    }
}
