//! URL decoding as JavaScript does it: the strict `decodeURIComponent` of path parameters and the
//! lenient `URLSearchParams` of query strings and form bodies.

/// `decodeURIComponent`: `%XX` sequences must be complete and decode to valid UTF-8, else `None`;
/// `+` stays `+`.
pub fn decode_uri_component(s: &str) -> Option<String> {
    if !s.contains('%') {
        return Some(s.to_string());
    }
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' {
            let hi = hex_value(*b.get(i + 1)?)?;
            let lo = hex_value(*b.get(i + 2)?)?;
            out.push(hi << 4 | lo);
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

fn hex_value(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

/// Percent-decodes leniently (a malformed `%` is kept as it is), `+` becomes a space, and the
/// bytes are read as UTF-8 with U+FFFD for invalid sequences.
fn form_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < b.len() => match (hex_value(b[i + 1]), hex_value(b[i + 2])) {
                (Some(hi), Some(lo)) => {
                    out.push(hi << 4 | lo);
                    i += 3;
                }
                _ => {
                    out.push(b'%');
                    i += 1;
                }
            },
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The pairs of an `application/x-www-form-urlencoded` string, as `URLSearchParams` reads them:
/// one leading `?` dropped, empty pairs skipped, split at the first `=` (none: empty value).
pub fn parse_urlencoded(s: &str) -> Vec<(String, String)> {
    let s = s.strip_prefix('?').unwrap_or(s);
    s.split('&')
        .filter(|p| !p.is_empty())
        .map(|p| match p.split_once('=') {
            Some((k, v)) => (form_decode(k), form_decode(v)),
            None => (form_decode(p), String::new()),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_uri_component_is_strict() {
        assert_eq!(decode_uri_component("b%C3%A9b%C3%A9").as_deref(), Some("bébé"));
        assert_eq!(decode_uri_component("a+b").as_deref(), Some("a+b"));
        assert_eq!(decode_uri_component("%E0%A4%A"), None, "a cut sequence");
        assert_eq!(decode_uri_component("%zz"), None);
        assert_eq!(decode_uri_component("%C0%AF"), None, "an overlong form");
        assert_eq!(decode_uri_component("%ED%A0%80"), None, "a surrogate");
        assert_eq!(decode_uri_component("100%"), None);
        assert_eq!(decode_uri_component("plain").as_deref(), Some("plain"));
    }

    #[test]
    fn urlencoded_reads_like_url_search_params() {
        let p = parse_urlencoded("?x=1&x=2&y=z&&flag&a+b=c%2Bd&bad=%zz%4&e=%C3%A9&f=%FF");
        let want = [
            ("x", "1"),
            ("x", "2"),
            ("y", "z"),
            ("flag", ""),
            ("a b", "c+d"),
            ("bad", "%zz%4"),
            ("e", "é"),
            ("f", "\u{fffd}"),
        ];
        let got: Vec<(&str, &str)> = p.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        assert_eq!(got, want);
        assert_eq!(parse_urlencoded("??a=1")[0].0, "?a");
        assert!(parse_urlencoded("").is_empty());
    }
}
