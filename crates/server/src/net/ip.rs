//! Client address helpers: normalisation, the keys addresses are counted under, trusted proxy
//! matching, `X-Forwarded-For` resolution and the form an address takes in the logs
//! (DESIGN 8, "Protection per address").
//!
//! An IPv4-mapped IPv6 address (`::ffff:1.2.3.4`, what a dual-stack listener reports for an IPv4
//! client) is always turned into plain IPv4, so that one client has one key whatever the listener.
//! An IPv4 address is counted by itself; an IPv6 address by its /64 (one subscriber's network) and,
//! for site-wide sums, by its /48 (one customer's allocation).

use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::str::FromStr;

/// The canonical form of an address: IPv4-mapped IPv6 becomes IPv4.
pub fn canonical(ip: IpAddr) -> IpAddr {
    ip.to_canonical()
}

/// Parses an address as written in a header or a configuration list: surrounding spaces,
/// brackets and a zone id (`%eth0`) are dropped, an IPv4-mapped IPv6 address written with a
/// dotted tail (`::ffff:1.2.3.4`) becomes IPv4. A mapped range written in hexadecimal
/// (`::ffff:0:0/96` in a list) stays IPv6, as in the Node server, so that such a rule keeps its
/// IPv6 prefix length. `None` for anything that is not an IP address.
pub fn normalize_ip(text: &str) -> Option<IpAddr> {
    let mut a = text.trim();
    if a.len() >= 2 && a.starts_with('[') && a.ends_with(']') {
        a = &a[1..a.len() - 1];
    }
    if let Some(pct) = a.find('%') {
        a = &a[..pct];
    }
    let ip = IpAddr::from_str(a).ok()?;
    Some(if a.contains('.') { canonical(ip) } else { ip })
}

/// The key an address is counted under: an IPv4 address, or an IPv6 /64 or /48 network.
///
/// Displayed as the Node server wrote its keys: `192.0.2.1`, `2001:db8:1:2::/64` (groups in
/// lower-case hexadecimal without leading zeros, never compressed) and `2001:db8:1::/48`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum AddrKey {
    /// An IPv4 address.
    V4(Ipv4Addr),
    /// The /64 network of an IPv6 address (its first 64 bits).
    V6Net64(u64),
    /// The /48 network of an IPv6 address (its first 48 bits, in the low bits).
    V6Net48(u64),
}

impl AddrKey {
    /// The per-address key of `ip`: IPv4 itself, or the /64 of an IPv6 address.
    pub fn of(ip: IpAddr) -> AddrKey {
        match canonical(ip) {
            IpAddr::V4(v4) => AddrKey::V4(v4),
            IpAddr::V6(v6) => AddrKey::V6Net64((u128::from(v6) >> 64) as u64),
        }
    }

    /// The site key of `ip`: IPv4 itself, or the /48 of an IPv6 address (the TLS gate's group).
    pub fn site_of(ip: IpAddr) -> AddrKey {
        match canonical(ip) {
            IpAddr::V4(v4) => AddrKey::V4(v4),
            IpAddr::V6(v6) => AddrKey::V6Net48((u128::from(v6) >> 80) as u64),
        }
    }

    /// The /48 of an IPv6 address, `None` for IPv4.
    pub fn prefix48_of(ip: IpAddr) -> Option<AddrKey> {
        match canonical(ip) {
            IpAddr::V4(_) => None,
            IpAddr::V6(v6) => Some(AddrKey::V6Net48((u128::from(v6) >> 80) as u64)),
        }
    }

    /// Whether the key is an IPv6 /48 (a site rather than one address or network).
    pub fn is_prefix(self) -> bool {
        matches!(self, AddrKey::V6Net48(_))
    }

    /// The key as it may appear in the logs (`LOG_IP`): see [`for_log_text`].
    pub fn for_log(self) -> String {
        for_log_text(&self.to_string())
    }
}

impl fmt::Display for AddrKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            AddrKey::V4(v4) => write!(f, "{v4}"),
            AddrKey::V6Net64(p) => write!(
                f,
                "{:x}:{:x}:{:x}:{:x}::/64",
                (p >> 48) & 0xffff,
                (p >> 32) & 0xffff,
                (p >> 16) & 0xffff,
                p & 0xffff
            ),
            AddrKey::V6Net48(p) => {
                write!(f, "{:x}:{:x}:{:x}::/48", (p >> 32) & 0xffff, (p >> 16) & 0xffff, p & 0xffff)
            }
        }
    }
}

/// The text is not a key ([`AddrKey`]'s `FromStr`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BadAddrKey(pub String);

impl fmt::Display for BadAddrKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "not an address key: \"{}\"", self.0)
    }
}

impl std::error::Error for BadAddrKey {}

impl FromStr for AddrKey {
    type Err = BadAddrKey;

    /// Parses `192.0.2.1`, `2001:db8:1:2::/64` or `2001:db8:1::/48` (any address of the network
    /// is accepted before the prefix length). A plain IPv6 address gives its /64.
    fn from_str(s: &str) -> Result<AddrKey, BadAddrKey> {
        let bad = || BadAddrKey(s.to_string());
        let (addr, prefix) = match s.split_once('/') {
            Some((a, p)) => (a, Some(p)),
            None => (s, None),
        };
        let ip = normalize_ip(addr).ok_or_else(bad)?;
        match (ip, prefix) {
            (IpAddr::V4(v4), None) => Ok(AddrKey::V4(v4)),
            (IpAddr::V6(_), None | Some("64")) => Ok(AddrKey::of(ip)),
            (IpAddr::V6(_), Some("48")) => Ok(AddrKey::site_of(ip)),
            _ => Err(bad()),
        }
    }
}

/// The key under which connections and requests of `ip` are counted, as a string:
/// [`AddrKey::of`] (`v6_prefix` 64) or [`AddrKey::site_of`] (`v6_prefix` 48).
pub fn group_key(ip: IpAddr, v6_prefix: u8) -> String {
    if v6_prefix == 48 { AddrKey::site_of(ip).to_string() } else { AddrKey::of(ip).to_string() }
}

/// An entry of an address list is invalid ([`IpMatcher::new`]). The texts are those of the Node
/// server, so a typo in `TRUSTED_PROXIES` or `ABUSE_EXEMPT` reads the same.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IpListError {
    /// The entry is neither an address nor a subnet.
    NotAnAddress(String),
    /// The prefix length is not an integer between 0 and 32 (IPv4) or 128 (IPv6).
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

#[derive(Debug, Clone, Copy)]
enum Rule {
    V4 { net: u32, mask: u32 },
    V6 { net: u128, mask: u128 },
}

/// A list of addresses and CIDR subnets (`10.0.0.0/8`, `::1`, `fd00::/8`): `TRUSTED_PROXIES`,
/// `ABUSE_EXEMPT`. An IPv6 rule that covers the IPv4-mapped range also matches IPv4 clients.
#[derive(Debug, Clone, Default)]
pub struct IpMatcher {
    rules: Vec<Rule>,
}

impl IpMatcher {
    /// Builds a matcher. Empty entries are skipped; an invalid one is an error, so a typo is found
    /// at start-up.
    pub fn new<S: AsRef<str>>(list: &[S]) -> Result<IpMatcher, IpListError> {
        let mut rules = Vec::new();
        for raw in list {
            let entry = raw.as_ref().trim();
            if entry.is_empty() {
                continue;
            }
            let (addr, prefix) = match entry.split_once('/') {
                Some((a, p)) => (a, Some(p)),
                None => (entry, None),
            };
            let ip = normalize_ip(addr).ok_or_else(|| IpListError::NotAnAddress(entry.to_string()))?;
            let max = if ip.is_ipv4() { 32 } else { 128 };
            let bits = match prefix {
                None => max,
                Some(p) => parse_prefix(p, max).ok_or_else(|| IpListError::BadPrefix(entry.to_string()))?,
            };
            rules.push(match ip {
                IpAddr::V4(v4) => {
                    let mask = if bits == 0 { 0 } else { u32::MAX << (32 - bits) };
                    Rule::V4 { net: u32::from(v4) & mask, mask }
                }
                IpAddr::V6(v6) => {
                    let mask = if bits == 0 { 0 } else { u128::MAX << (128 - bits) };
                    Rule::V6 { net: u128::from(v6) & mask, mask }
                }
            });
        }
        Ok(IpMatcher { rules })
    }

    /// Whether the list has no entry (it then matches nothing).
    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }

    /// Whether `ip` is in the list.
    pub fn matches(&self, ip: IpAddr) -> bool {
        let ip = canonical(ip);
        let (v4, v6) = match ip {
            IpAddr::V4(a) => (Some(u32::from(a)), u128::from(a.to_ipv6_mapped())),
            IpAddr::V6(a) => (None, u128::from(a)),
        };
        self.rules.iter().any(|r| match *r {
            Rule::V4 { net, mask } => v4.is_some_and(|a| a & mask == net),
            Rule::V6 { net, mask } => v6 & mask == net,
        })
    }
}

/// A prefix length: an integer within `0..=max`. An empty one (`10.0.0.0/`) is refused, where
/// JavaScript's `Number('')` read it as 0 and trusted every address.
fn parse_prefix(text: &str, max: u32) -> Option<u32> {
    let t = text.trim();
    if t.is_empty() {
        return None;
    }
    let v: f64 = t.parse().ok()?;
    if v.fract() != 0.0 || !(0.0..=max as f64).contains(&v) {
        return None;
    }
    Some(v as u32)
}

/// The client address of a request that arrived from `peer`. `X-Forwarded-For` is read only when
/// the peer is a trusted proxy; the list is then walked from the right (the closest hop),
/// skipping trusted proxies, and the first untrusted address is the client. A malformed entry
/// stops the walk: everything to its left may be forged by the client.
pub fn resolve_client_ip(peer: IpAddr, xff: Option<&str>, trusted: &IpMatcher) -> IpAddr {
    let peer = canonical(peer);
    let Some(xff) = xff else { return peer };
    if xff.is_empty() || !trusted.matches(peer) {
        return peer;
    }
    let mut last = peer;
    for hop in xff.rsplit(',') {
        match normalize_ip(hop).map(canonical) {
            None => return last,
            Some(a) if !trusted.matches(a) => return a,
            Some(a) => last = a,
        }
    }
    last
}

/// How a listener finds the client address of a connection or request: the peer itself, or,
/// behind trusted proxies (`TLS_MODE=proxy`), the `X-Forwarded-For` walk.
#[derive(Debug, Clone, Default)]
pub struct ClientAddress {
    trusted: Option<IpMatcher>,
}

impl ClientAddress {
    /// The peer is the client.
    pub fn direct() -> ClientAddress {
        ClientAddress { trusted: None }
    }

    /// Requests come through the proxies of `trusted`.
    pub fn behind(trusted: IpMatcher) -> ClientAddress {
        ClientAddress { trusted: Some(trusted) }
    }

    /// The mode of `config`: behind `TRUSTED_PROXIES` in proxy mode, else direct.
    pub fn from_config(config: &crate::config::Config) -> Result<ClientAddress, IpListError> {
        if config.tls_mode == crate::config::TlsMode::Proxy {
            Ok(ClientAddress::behind(IpMatcher::new(&config.trusted_proxies)?))
        } else {
            Ok(ClientAddress::direct())
        }
    }

    /// The client address of a request from `peer` carrying `xff`.
    pub fn resolve(&self, peer: IpAddr, xff: Option<&str>) -> IpAddr {
        match &self.trusted {
            Some(t) => resolve_client_ip(peer, xff, t),
            None => canonical(peer),
        }
    }

    /// Whether `peer` is a trusted proxy (its malformed requests are not held against it).
    pub fn is_trusted_peer(&self, peer: IpAddr) -> bool {
        self.trusted.as_ref().is_some_and(|t| t.matches(canonical(peer)))
    }

    /// Whether requests come through proxies.
    pub fn is_proxy(&self) -> bool {
        self.trusted.is_some()
    }
}

/// A client address as it may appear in the logs: see [`for_log_text`].
pub fn for_log(ip: IpAddr) -> String {
    for_log_text(&canonical(ip).to_string())
}

/// An address (or an address key) as it may appear in the logs, by `LOG_IP` as the logger applies
/// it ([`crate::log::ip`]): `truncated` (the default) keeps the IPv4 /24 or the IPv6 /48, `full`
/// keeps it whole, `hashed` replaces it with `ip:` and 12 characters of an HMAC keyed by
/// `SERVER_SECRET` and the day (so one address can be followed within a day, never recovered).
pub fn for_log_text(ip: &str) -> String {
    crate::log::ip(ip).unwrap_or_default()
}

/// IPv4 /24 (`192.0.2.0/24`) or IPv6 /48 (`2001:db8:1::/48`). An IPv4-mapped IPv6 address is
/// treated as IPv4; any other text with a colon is read as IPv6 (an address key such as
/// `2001:db8:1:2::/64` gives its /48); anything else is returned unchanged.
pub fn truncate_ip(ip: &str) -> String {
    let mut a = ip;
    if a.len() > 7 && a[..7].eq_ignore_ascii_case("::ffff:") && a.contains('.') {
        a = &a[7..];
    }
    if !a.contains(':') && a.contains('.') {
        let p: Vec<&str> = a.split('.').collect();
        return if p.len() == 4 { format!("{}.{}.{}.0/24", p[0], p[1], p[2]) } else { a.to_string() };
    }
    match expand_ipv6(a) {
        Some(g) => format!("{:x}:{:x}:{:x}::/48", g[0], g[1], g[2]),
        None => a.to_string(),
    }
}

/// The eight groups of an IPv6 address written loosely (a zone id, a dotted IPv4 tail, a
/// trailing `/prefix` read as a zero group), as the Node server's `expandIPv6`.
fn expand_ipv6(a: &str) -> Option<[u16; 8]> {
    let s = a.split('%').next().unwrap_or("");
    let halves: Vec<&str> = s.split("::").collect();
    if halves.len() > 2 {
        return None;
    }
    let split = |h: &str| -> Vec<String> {
        if h.is_empty() { Vec::new() } else { h.split(':').map(str::to_string).collect() }
    };
    let mut head = split(halves[0]);
    let mut tail = if halves.len() == 2 { split(halves[1]) } else { Vec::new() };
    let last = if halves.len() == 2 { &mut tail } else { &mut head };
    if last.last().is_some_and(|x| x.contains('.')) {
        let v4: Ipv4Addr = last.pop().unwrap_or_default().parse().ok()?;
        let b = v4.octets();
        last.push(format!("{:x}", u16::from(b[0]) << 8 | u16::from(b[1])));
        last.push(format!("{:x}", u16::from(b[2]) << 8 | u16::from(b[3])));
    }
    let fill = if halves.len() == 2 { 8usize.saturating_sub(head.len() + tail.len()) } else { 0 };
    let all: Vec<u16> = head
        .iter()
        .map(|x| js_parse_hex(x))
        .chain(std::iter::repeat_n(0, fill))
        .chain(tail.iter().map(|x| js_parse_hex(x)))
        .collect();
    <[u16; 8]>::try_from(all).ok()
}

/// `parseInt(x, 16) || 0` for one group: the leading hexadecimal digits, 0 when there are none.
fn js_parse_hex(x: &str) -> u16 {
    let digits: String = x.chars().take_while(char::is_ascii_hexdigit).collect();
    u32::from_str_radix(&digits, 16).map(|v| v as u16).unwrap_or(0)
}

/// An IPv6 address from its key's network bits (tests and diagnostics).
pub fn network_address(key: AddrKey) -> IpAddr {
    match key {
        AddrKey::V4(v4) => IpAddr::V4(v4),
        AddrKey::V6Net64(p) => IpAddr::V6(Ipv6Addr::from(u128::from(p) << 64)),
        AddrKey::V6Net48(p) => IpAddr::V6(Ipv6Addr::from(u128::from(p) << 80)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        normalize_ip(s).expect("a valid address")
    }

    #[test]
    fn normalizes_mapped_bracketed_and_zoned_addresses() {
        assert_eq!(ip("::ffff:192.0.2.7"), ip("192.0.2.7"));
        assert_eq!(ip("::FFFF:192.0.2.7"), ip("192.0.2.7"));
        assert_eq!(ip("[2001:db8::1]"), ip("2001:db8::1"));
        assert_eq!(ip(" fe80::1%eth0 "), ip("fe80::1"));
        assert_eq!(normalize_ip("garbage"), None);
        assert_eq!(normalize_ip(""), None);
        assert_eq!(ip("::1").to_string(), "::1");
    }

    #[test]
    fn keys_match_the_node_strings() {
        assert_eq!(group_key(ip("198.51.100.4"), 64), "198.51.100.4");
        assert_eq!(group_key(ip("::ffff:198.51.100.4"), 64), "198.51.100.4");
        assert_eq!(group_key(ip("2001:db8:aa:bb:1:2:3:4"), 64), "2001:db8:aa:bb::/64");
        assert_eq!(group_key(ip("2001:db8:aa:bb::9"), 64), "2001:db8:aa:bb::/64");
        assert_eq!(group_key(ip("198.51.100.4"), 48), "198.51.100.4");
        assert_eq!(group_key(ip("2001:db8:aa:bb:1:2:3:4"), 48), "2001:db8:aa::/48");
        assert_eq!(group_key(ip("2001:db8:aa:ffff::9"), 48), "2001:db8:aa::/48");
        assert_eq!(group_key(ip("2001:DB8:0AA::1"), 48), "2001:db8:aa::/48");
        assert_ne!(group_key(ip("2001:db8:ab::1"), 48), group_key(ip("2001:db8:aa::1"), 48));
        assert_eq!(group_key(ip("2001:0db8::1"), 64), "2001:db8:0:0::/64");
        // An IPv6 address with a dotted IPv4 tail gets its real /64 and /48.
        assert_eq!(group_key(ip("2001:db8:1:2:3:4:5.6.7.8"), 64), "2001:db8:1:2::/64");
        assert_eq!(group_key(ip("2001:db8:1:2:3:4:5.6.7.8"), 48), "2001:db8:1::/48");
        assert_eq!(group_key(ip("2001:db8::1:2:3:4.5.6.7"), 64), "2001:db8:0:1::/64");
    }

    #[test]
    fn keys_round_trip_through_text() {
        for k in ["198.51.100.9", "2001:db8:1::/48", "2001:db8:1:77::/64", "0:0:0:0::/64"] {
            assert_eq!(k.parse::<AddrKey>().expect("a key").to_string(), k);
        }
        assert!("bad".parse::<AddrKey>().is_err());
        assert!("10.0.0.0/8".parse::<AddrKey>().is_err());
        assert_eq!(network_address("2001:db8:1::/48".parse().expect("a key")), ip("2001:db8:1::"));
    }

    #[test]
    fn random_addresses_give_node_keys() {
        // The keys of random addresses, compressed or not, agree with a direct rendering.
        let mut s: u64 = 11;
        let mut rnd = |n: u64| {
            s = (s.wrapping_mul(1103515245).wrapping_add(12345)) & 0x7fff_ffff;
            s % n
        };
        for _ in 0..2000 {
            let g: Vec<u16> = (0..8).map(|_| if rnd(3) != 0 { rnd(65536) as u16 } else { 0 }).collect();
            let a = Ipv6Addr::new(g[0], g[1], g[2], g[3], g[4], g[5], g[6], g[7]);
            let key = group_key(IpAddr::V6(a), 64);
            if a.to_ipv4_mapped().is_none() {
                assert_eq!(key, format!("{:x}:{:x}:{:x}:{:x}::/64", g[0], g[1], g[2], g[3]));
                assert_eq!(group_key(IpAddr::V6(a), 48), format!("{:x}:{:x}:{:x}::/48", g[0], g[1], g[2]));
            }
            let v4 = format!("{}.{}.{}.{}", rnd(256), rnd(256), rnd(256), rnd(256));
            assert_eq!(group_key(ip(&v4), 64), v4);
        }
    }

    #[test]
    fn matcher_reads_addresses_and_subnets() {
        let m = IpMatcher::new(&["10.0.0.0/8", "::1", "fd00::/8", " ", "192.0.2.1"]).expect("valid");
        assert!(m.matches(ip("10.200.3.4")));
        assert!(m.matches(ip("::ffff:10.1.1.1")));
        assert!(m.matches(ip("::1")));
        assert!(m.matches(ip("fd12::5")));
        assert!(m.matches(ip("192.0.2.1")));
        assert!(!m.matches(ip("192.0.2.2")));
        assert!(!m.matches(ip("2001:db8::1")));
        assert!(IpMatcher::new::<&str>(&[]).expect("empty").is_empty());
        let mapped = IpMatcher::new(&["::ffff:0:0/96"]).expect("valid");
        assert!(mapped.matches(ip("203.0.113.9")), "an IPv6 rule over the mapped range matches IPv4");
        let all = IpMatcher::new(&["0.0.0.0/0"]).expect("valid");
        assert!(all.matches(ip("1.2.3.4")) && !all.matches(ip("::2")));
    }

    #[test]
    fn matcher_errors_name_the_entry() {
        assert_eq!(
            IpMatcher::new(&["10.0.0.0/33"]).unwrap_err().to_string(),
            "bad prefix length: \"10.0.0.0/33\""
        );
        assert_eq!(IpMatcher::new(&["fd00::/1.5"]).unwrap_err(), IpListError::BadPrefix("fd00::/1.5".into()));
        assert_eq!(
            IpMatcher::new(&["proxy.local"]).unwrap_err().to_string(),
            "not an IP address or subnet: \"proxy.local\""
        );
        assert!(IpMatcher::new(&["::ffff:10.0.0.0/104"]).is_err(), "a mapped address is IPv4: at most /32");
        assert!(IpMatcher::new(&["fd00::/128"]).is_ok());
    }

    #[test]
    fn forwarded_for_is_read_from_trusted_proxies_only() {
        let trusted = IpMatcher::new(&["127.0.0.1", "10.0.0.0/8"]).expect("valid");
        let peer = ip("127.0.0.1");
        assert_eq!(resolve_client_ip(peer, Some("203.0.113.5"), &trusted), ip("203.0.113.5"));
        assert_eq!(
            resolve_client_ip(peer, Some("198.51.100.1, 203.0.113.5, 10.0.0.2"), &trusted),
            ip("203.0.113.5")
        );
        assert_eq!(resolve_client_ip(peer, Some("10.0.0.3, 10.0.0.2"), &trusted), ip("10.0.0.3"));
        assert_eq!(resolve_client_ip(peer, Some("203.0.113.5, junk, 10.0.0.2"), &trusted), ip("10.0.0.2"));
        assert_eq!(resolve_client_ip(peer, Some("junk"), &trusted), peer);
        assert_eq!(resolve_client_ip(peer, None, &trusted), peer);
        assert_eq!(resolve_client_ip(ip("198.51.100.7"), Some("203.0.113.5"), &trusted), ip("198.51.100.7"));
        assert_eq!(
            resolve_client_ip(ip("::ffff:127.0.0.1"), Some("[2001:db8::1]"), &trusted),
            ip("2001:db8::1")
        );
    }

    #[test]
    fn truncation_keeps_the_24_or_the_48() {
        assert_eq!(truncate_ip("2001:db8:1:2:3:4:5.6.7.8"), "2001:db8:1::/48");
        assert_eq!(truncate_ip("2001:db8::1:2:3:4.5.6.7"), "2001:db8:0::/48");
        for a in ["::ffff:192.0.2.7", "::FFFF:192.0.2.7", "192.0.2.7"] {
            assert_eq!(truncate_ip(a), "192.0.2.0/24");
        }
        assert_eq!(truncate_ip("2001:db8:1:2::/64"), "2001:db8:1::/48");
        assert_eq!(truncate_ip("garbage"), "garbage");
        assert_eq!(truncate_ip("1.2.3"), "1.2.3");
    }
}
