//! Web origins as browsers write them in an `Origin` header (`CORS_ORIGINS`).
//!
//! The API compares the `Origin` header with the configured origins byte for byte, so an origin
//! is only accepted in the one form a browser sends: `https://host` or `https://host:port`, in
//! lower case, without a path, a trailing slash, a query, a user name or the scheme's default
//! port, an IPv6 host in the URL Standard's short form. Plain `http://` is accepted for the
//! loopback hosts only (local development). A wildcard and `null` (the origin of sandboxed and
//! local pages) are refused.

use std::fmt::Write as _;
use std::net::{Ipv4Addr, Ipv6Addr};

/// The hosts whose `http://` origins are accepted (local development).
pub const LOOPBACK_HOSTS: [&str; 3] = ["localhost", "127.0.0.1", "[::1]"];

/// Checks that `text` is a web origin exactly as a browser sends it. The error is the reason,
/// written to follow the quoted value (`"https://x/" must have no path...`), without a final
/// period. When the browser's form of the value can be written mechanically (lower case, no
/// path, no default port, short IPv6 form) and is itself valid, the reason ends with it
/// (`; write "https://x"`). A fault that only a rewrite by hand can fix (`http://` for a host
/// that is not a loopback host, a user name, a bad host or port) is reported first, without a
/// suggestion.
pub fn check_web_origin(text: &str) -> Result<(), String> {
    let fixed = browser_form(text);
    first_fault(&fixed)?;
    first_fault(text).map_err(|reason| format!("{reason}; write \"{fixed}\""))
}

/// The first reason why `text` is not an origin as browsers send it.
fn first_fault(text: &str) -> Result<(), String> {
    const EXAMPLE: &str = "https://www.example.org";
    match text {
        "*" => return Err(format!("is a wildcard: list each origin, such as {EXAMPLE}")),
        "null" => {
            return Err("is the origin of sandboxed and local pages, which cannot be allowed".to_string());
        }
        _ => {}
    }
    if text.bytes().any(|b| b.is_ascii_uppercase()) {
        return Err("must be in lower case, as browsers send it".to_string());
    }
    let (scheme, rest) = if let Some(rest) = text.strip_prefix("https://") {
        ("https", rest)
    } else if let Some(rest) = text.strip_prefix("http://") {
        ("http", rest)
    } else {
        return Err("must start with https:// (http:// only for localhost, 127.0.0.1 and [::1])".to_string());
    };
    if rest.contains(['/', '?', '#']) {
        return Err("must have no path, query or fragment, not even a trailing slash".to_string());
    }
    if rest.contains('@') {
        return Err("must not hold a user name or password".to_string());
    }
    let (host, port) = split_host_port(rest)?;
    check_host(host)?;
    if scheme == "http" && !LOOPBACK_HOSTS.contains(&host) {
        return Err(
            "uses http://, which is accepted for localhost, 127.0.0.1 and [::1] only (local development): \
             use https://"
                .to_string(),
        );
    }
    if let Some(port) = port {
        let default = default_port(scheme);
        if port == default {
            return Err(format!("names the default port {default}, which browsers leave out"));
        }
        let valid = !port.is_empty()
            && !port.starts_with('0')
            && port.bytes().all(|b| b.is_ascii_digit())
            && port.parse::<u16>().is_ok();
        if !valid {
            return Err("has an invalid port (1 to 65535, no leading zero)".to_string());
        }
    }
    Ok(())
}

fn default_port(scheme: &str) -> &'static str {
    if scheme == "https" { "443" } else { "80" }
}

/// `text` rewritten the way a browser writes the origin, as far as that is mechanical: in lower
/// case, without a path, query or fragment, without the scheme's default port and with an IPv6
/// host in its short form. Any other fault is left in place for [`first_fault`] to report. A
/// valid origin is returned unchanged.
fn browser_form(text: &str) -> String {
    let lower = text.to_ascii_lowercase();
    let Some((scheme, rest)) = lower.split_once("://") else { return lower };
    if scheme != "https" && scheme != "http" {
        return lower;
    }
    let authority = rest.find(['/', '?', '#']).map_or(rest, |cut| &rest[..cut]);
    let Ok((host, port)) = split_host_port(authority) else { return format!("{scheme}://{authority}") };
    let ipv6 = host.strip_prefix('[').and_then(|h| h.strip_suffix(']')).and_then(|h| h.parse().ok());
    let host = match ipv6 {
        Some(addr) => format!("[{}]", browser_ipv6(addr)),
        None => host.to_string(),
    };
    match port {
        Some(port) if port != default_port(scheme) => format!("{scheme}://{host}:{port}"),
        _ => format!("{scheme}://{host}"),
    }
}

/// An IPv6 address as browsers write it in a URL (the URL Standard's IPv6 serializer): the eight
/// pieces in lower-case hexadecimal without leading zeros, the first longest run of at least two
/// zero pieces written `::`. This is RFC 5952's form, except that an embedded IPv4 address stays
/// in hexadecimal (`::ffff:102:304`), where `Ipv6Addr`'s `Display` writes `::ffff:1.2.3.4`.
fn browser_ipv6(addr: Ipv6Addr) -> String {
    let pieces = addr.segments();
    let (mut zeros_at, mut zeros_len) = (0, 0);
    let mut i = 0;
    while i < pieces.len() {
        let start = i;
        while i < pieces.len() && pieces[i] == 0 {
            i += 1;
        }
        if i - start > zeros_len {
            (zeros_at, zeros_len) = (start, i - start);
        }
        i = i.max(start + 1);
    }
    let mut out = String::new();
    let mut i = 0;
    while i < pieces.len() {
        if zeros_len >= 2 && i == zeros_at {
            out.push_str(if i == 0 { "::" } else { ":" });
            i += zeros_len;
            continue;
        }
        let _ = write!(out, "{:x}", pieces[i]);
        if i + 1 < pieces.len() {
            out.push(':');
        }
        i += 1;
    }
    out
}

/// `host[:port]`, the host of an IPv6 address in brackets.
fn split_host_port(authority: &str) -> Result<(&str, Option<&str>), String> {
    if authority.starts_with('[') {
        let Some(end) = authority.find(']') else {
            return Err(ipv6_error());
        };
        let (host, after) = authority.split_at(end + 1);
        return match after.strip_prefix(':') {
            Some(port) => Ok((host, Some(port))),
            None if after.is_empty() => Ok((host, None)),
            None => Err(ipv6_error()),
        };
    }
    Ok(match authority.split_once(':') {
        Some((host, port)) => (host, Some(port)),
        None => (authority, None),
    })
}

fn ipv6_error() -> String {
    "has an invalid IPv6 address (in brackets, such as [2001:db8::1])".to_string()
}

fn host_error() -> String {
    "has an invalid host name (letters, digits, hyphens and dots; the xn-- form for an international name)"
        .to_string()
}

/// A host as browsers write it: a DNS name in lower-case ASCII, an IPv4 address in dotted
/// decimal, or an IPv6 address in brackets in its short form ([`browser_ipv6`]).
fn check_host(host: &str) -> Result<(), String> {
    if let Some(inner) = host.strip_prefix('[') {
        let inner = inner.strip_suffix(']').ok_or_else(ipv6_error)?;
        return match inner.parse::<Ipv6Addr>() {
            Ok(addr) if browser_ipv6(addr) == inner => Ok(()),
            Ok(_) => {
                Err("has its IPv6 address in a form browsers do not send (they write the short hexadecimal \
                 form, an embedded IPv4 address included)"
                    .to_string())
            }
            Err(_) => Err(ipv6_error()),
        };
    }
    if host.is_empty() || host.len() > 253 {
        return Err(host_error());
    }
    // A host whose last label is a number is an IPv4 address to a browser (127.1 is 127.0.0.1).
    let last = host.rsplit('.').next().unwrap_or(host);
    let numeric = (!last.is_empty() && last.bytes().all(|b| b.is_ascii_digit()))
        || last.strip_prefix("0x").is_some_and(|h| h.bytes().all(|b| b.is_ascii_hexdigit()));
    if numeric {
        return match host.parse::<Ipv4Addr>() {
            Ok(addr) if addr.to_string() == host => Ok(()),
            _ => Err("has an invalid IPv4 address (four decimal numbers, no leading zero)".to_string()),
        };
    }
    let label_ok = |l: &str| {
        (1..=63).contains(&l.len())
            && l.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
    };
    if host.split('.').all(label_ok) { Ok(()) } else { Err(host_error()) }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The suggestion at the end of a reason (`; write "..."`), if any.
    fn suggestion(reason: &str) -> Option<&str> {
        reason.split_once("; write \"").map(|(_, s)| s.strip_suffix('"').unwrap())
    }

    #[test]
    fn origins_as_browsers_send_them() {
        for good in [
            "https://scacelith.com",
            "https://www.scacelith.com",
            "https://xn--bcher-kva.example",
            "https://play.example.org:8443",
            "https://203.0.113.7",
            "https://[2001:db8::1]:8443",
            "https://[::ffff:102:304]",
            "https://[::ffff:0:0]:8443",
            "https://[64:ff9b::c000:221]",
            "https://[1::2:0:0:3:4]",
            "https://[1:0:0:2::3]",
            "https://[2001:db8:0:1:1:1:1:1]",
            "https://localhost",
            "http://localhost:8080",
            "http://127.0.0.1:8080",
            "http://[::1]:5173",
            "http://localhost",
            "https://my_host.example",
        ] {
            assert_eq!(check_web_origin(good), Ok(()), "{good}");
            assert_eq!(browser_form(good), good, "a valid origin is its own browser form");
        }
    }

    #[test]
    fn ipv6_hosts_are_written_as_the_url_standard_serializes_them() {
        // What browsers send (here as Node's WHATWG `URL` gives `origin`), not `Ipv6Addr`'s
        // `Display`, which writes an IPv4-mapped address in dotted decimal.
        for (written, browser) in [
            ("::ffff:1.2.3.4", "::ffff:102:304"),
            ("::ffff:102:304", "::ffff:102:304"),
            ("::1.2.3.4", "::102:304"),
            ("::ffff:0:0", "::ffff:0:0"),
            ("64:ff9b::192.0.2.33", "64:ff9b::c000:221"),
            ("0:0:0:0:0:0:0:1", "::1"),
            ("::", "::"),
            ("1::", "1::"),
            ("1:0:0:2:0:0:3:4", "1::2:0:0:3:4"),
            ("1:0:0:2:0:0:0:3", "1:0:0:2::3"),
            ("2001:0DB8::0001", "2001:db8::1"),
            ("2001:db8:0:1:1:1:1:1", "2001:db8:0:1:1:1:1:1"),
            ("fe80::1", "fe80::1"),
        ] {
            assert_eq!(browser_ipv6(written.parse().unwrap()), browser, "{written}");
        }
        assert_eq!("::ffff:102:304".parse::<Ipv6Addr>().unwrap().to_string(), "::ffff:1.2.3.4");
    }

    #[test]
    fn everything_else_is_refused_with_its_reason() {
        let reason = |o: &str| check_web_origin(o).expect_err(o);
        assert!(reason("*").contains("wildcard"));
        assert!(reason("https://*.scacelith.com").contains("invalid host name"));
        assert!(reason("null").contains("sandboxed"));
        assert_eq!(
            reason("https://Scacelith.com"),
            "must be in lower case, as browsers send it; write \"https://scacelith.com\""
        );
        assert!(reason("HTTPS://scacelith.com").contains("lower case"));
        assert_eq!(
            reason("https://scacelith.com/"),
            "must have no path, query or fragment, not even a trailing slash; write \"https://scacelith.com\""
        );
        for o in ["https://scacelith.com/play", "https://scacelith.com?x=1", "https://scacelith.com#top"] {
            assert!(reason(o).contains("no path"), "{o}");
        }
        assert_eq!(
            reason("https://scacelith.com:443"),
            "names the default port 443, which browsers leave out; write \"https://scacelith.com\""
        );
        assert!(reason("http://localhost:80").contains("default port 80"));
        for o in ["https://scacelith.com:", "https://scacelith.com:0", "https://scacelith.com:08443"] {
            assert!(reason(o).contains("invalid port"), "{o}");
        }
        assert!(reason("https://scacelith.com:65536").contains("invalid port"));
        assert!(reason("https://user:pw@scacelith.com").contains("user name"));
        for o in ["http://scacelith.com", "http://192.168.1.2:8080", "http://app.localhost"] {
            assert!(reason(o).contains("uses http://"), "{o}");
        }
        for o in ["scacelith.com", "ftp://scacelith.com", "//scacelith.com", "wss://scacelith.com"] {
            assert!(reason(o).contains("must start with https://"), "{o}");
        }
        for o in
            ["https://", "https://exa mple.org", "https://bücher.example", "https://a..b", "https://a.b."]
        {
            assert!(reason(o).contains("invalid host name"), "{o}");
        }
        for o in ["https://127.1", "https://010.0.0.1", "https://1.2.3.256", "https://example.0x1f"] {
            assert!(reason(o).contains("IPv4"), "{o}");
        }
        for o in ["https://[::0:1]", "https://[0:0:0:0:0:0:0:1]", "https://[::ffff:1.2.3.4]", "https://[::1"]
        {
            assert!(reason(o).contains("IPv6"), "{o}");
        }
        assert!(reason("https://[::1]x").contains("IPv6"));
        assert!(reason("https://[2001:db8::1:1:1:1:1]").contains("IPv6"), "one zero piece is not compressed");
        assert!(reason("https://[1:0:0:2::3:4]").contains("IPv6"), "the first longest run is compressed");
    }

    #[test]
    fn the_suggested_origin_fixes_every_fault_at_once_or_is_left_out() {
        for (bad, fixed) in [
            ("https://scacelith.com:443/", "https://scacelith.com"),
            ("HTTPS://Scacelith.com/", "https://scacelith.com"),
            ("https://Scacelith.com:443/play?x=1#top", "https://scacelith.com"),
            ("https://play.example.org:8443/", "https://play.example.org:8443"),
            ("http://LOCALHOST:80/", "http://localhost"),
            ("https://[::ffff:1.2.3.4]", "https://[::ffff:102:304]"),
            ("https://[0:0:0:0:0:0:0:1]:443/", "https://[::1]"),
            ("http://[0::1]:8080", "http://[::1]:8080"),
            ("https://[2001:DB8::1]", "https://[2001:db8::1]"),
        ] {
            let reason = check_web_origin(bad).expect_err(bad);
            assert_eq!(suggestion(&reason), Some(fixed), "{bad}: {reason}");
        }
        // A fault the browser's form cannot fix comes first, without a suggestion.
        for (bad, why) in [
            ("http://scacelith.com:80", "uses http://"),
            ("HTTP://Scacelith.com/", "uses http://"),
            ("https://user@Scacelith.com/", "user name"),
            ("https://Scacelith.com:0/", "invalid port"),
            ("https://Exa mple.org/", "invalid host name"),
            ("HTTPS://*.Scacelith.com", "invalid host name"),
            ("ftp://Scacelith.com/", "must start with https://"),
        ] {
            let reason = check_web_origin(bad).expect_err(bad);
            assert!(reason.contains(why), "{bad}: {reason}");
            assert_eq!(suggestion(&reason), None, "{bad}: {reason}");
        }
        // Whatever is suggested is accepted.
        for bad in [
            "https://Scacelith.com",
            "https://scacelith.com/",
            "https://scacelith.com:443",
            "http://localhost:80",
            "https://[::0:1]",
            "https://[1:0:0:2::3:4]",
            "https://[2001:db8::1:1:1:1:1]",
            "https://[::ffff:1.2.3.4]:443",
        ] {
            let reason = check_web_origin(bad).expect_err(bad);
            let fixed = suggestion(&reason).unwrap_or_else(|| panic!("{bad}: {reason}"));
            assert_eq!(check_web_origin(fixed), Ok(()), "{bad} -> {fixed}");
        }
    }
}
