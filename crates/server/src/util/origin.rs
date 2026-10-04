//! Web origins as browsers write them in an `Origin` header (`CORS_ORIGINS`).
//!
//! The API compares the `Origin` header with the configured origins byte for byte, so an origin
//! is only accepted in the one form a browser sends: `https://host` or `https://host:port`, in
//! lower case, without a path, a trailing slash, a query, a user name or the scheme's default
//! port. Plain `http://` is accepted for the loopback hosts only (local development). A wildcard
//! and `null` (the origin of sandboxed and local pages) are refused.

use std::net::{Ipv4Addr, Ipv6Addr};

/// The hosts whose `http://` origins are accepted (local development).
pub const LOOPBACK_HOSTS: [&str; 3] = ["localhost", "127.0.0.1", "[::1]"];

/// Checks that `text` is a web origin exactly as a browser sends it. The error is the reason,
/// written to follow the quoted value (`"https://x/" must have no path...`), without a final
/// period.
pub fn check_web_origin(text: &str) -> Result<(), String> {
    const EXAMPLE: &str = "https://www.example.org";
    match text {
        "*" => return Err(format!("is a wildcard: list each origin, such as {EXAMPLE}")),
        "null" => {
            return Err("is the origin of sandboxed and local pages, which cannot be allowed".to_string());
        }
        _ => {}
    }
    if text.bytes().any(|b| b.is_ascii_uppercase()) {
        return Err(format!("must be in lower case, as browsers send it: \"{}\"", text.to_ascii_lowercase()));
    }
    let (scheme, rest) = if let Some(rest) = text.strip_prefix("https://") {
        ("https", rest)
    } else if let Some(rest) = text.strip_prefix("http://") {
        ("http", rest)
    } else {
        return Err("must start with https:// (http:// only for localhost, 127.0.0.1 and [::1])".to_string());
    };
    if let Some(cut) = rest.find(['/', '?', '#']) {
        return Err(format!(
            "must have no path, query or fragment, not even a trailing slash: \"{scheme}://{}\"",
            &rest[..cut]
        ));
    }
    if rest.contains('@') {
        return Err("must not hold a user name or password".to_string());
    }
    let (host, port) = split_host_port(rest)?;
    check_host(host)?;
    if let Some(port) = port {
        let default = if scheme == "https" { "443" } else { "80" };
        if port == default {
            return Err(format!(
                "names the default port {default}, which browsers leave out: \"{scheme}://{host}\""
            ));
        }
        let valid = !port.is_empty()
            && !port.starts_with('0')
            && port.bytes().all(|b| b.is_ascii_digit())
            && port.parse::<u16>().is_ok();
        if !valid {
            return Err("has an invalid port (1 to 65535, no leading zero)".to_string());
        }
    }
    if scheme == "http" && !LOOPBACK_HOSTS.contains(&host) {
        return Err(
            "uses http://, which is accepted for localhost, 127.0.0.1 and [::1] only (local development): \
             use https://"
                .to_string(),
        );
    }
    Ok(())
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
    "has an invalid IPv6 address (in brackets, in its short lower-case form, such as [2001:db8::1])"
        .to_string()
}

fn host_error() -> String {
    "has an invalid host name (letters, digits, hyphens and dots; the xn-- form for an international name)"
        .to_string()
}

/// A host as browsers write it: a DNS name in lower-case ASCII, an IPv4 address in dotted
/// decimal, or an IPv6 address in brackets in its compressed form.
fn check_host(host: &str) -> Result<(), String> {
    if let Some(inner) = host.strip_prefix('[') {
        let inner = inner.strip_suffix(']').ok_or_else(ipv6_error)?;
        // Browsers write an embedded IPv4 address in hexadecimal (`::ffff:102:304`).
        return match inner.parse::<Ipv6Addr>() {
            Ok(addr) if !inner.contains('.') && addr.to_string() == inner => Ok(()),
            _ => Err(ipv6_error()),
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

    #[test]
    fn origins_as_browsers_send_them() {
        for good in [
            "https://scacelith.com",
            "https://www.scacelith.com",
            "https://xn--bcher-kva.example",
            "https://play.example.org:8443",
            "https://203.0.113.7",
            "https://[2001:db8::1]:8443",
            "https://localhost",
            "http://localhost:8080",
            "http://127.0.0.1:8080",
            "http://[::1]:5173",
            "http://localhost",
            "https://my_host.example",
        ] {
            assert_eq!(check_web_origin(good), Ok(()), "{good}");
        }
    }

    #[test]
    fn everything_else_is_refused_with_its_reason() {
        let reason = |o: &str| check_web_origin(o).expect_err(o);
        assert!(reason("*").contains("wildcard"));
        assert!(reason("https://*.scacelith.com").contains("invalid host name"));
        assert!(reason("null").contains("sandboxed"));
        assert_eq!(
            reason("https://Scacelith.com"),
            "must be in lower case, as browsers send it: \"https://scacelith.com\""
        );
        assert!(reason("HTTPS://scacelith.com").contains("lower case"));
        assert_eq!(
            reason("https://scacelith.com/"),
            "must have no path, query or fragment, not even a trailing slash: \"https://scacelith.com\""
        );
        for o in ["https://scacelith.com/play", "https://scacelith.com?x=1", "https://scacelith.com#top"] {
            assert!(reason(o).contains("no path"), "{o}");
        }
        assert_eq!(
            reason("https://scacelith.com:443"),
            "names the default port 443, which browsers leave out: \"https://scacelith.com\""
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
    }
}
