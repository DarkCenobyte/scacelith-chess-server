//! RFC 5322 plain-text messages: headers (From, To, Subject, Date, Message-ID, MIME), RFC 2047
//! encoded-word subjects and display names, 7bit bodies when possible and quoted-printable
//! otherwise. Header values are checked for CR/LF (no header injection). Every message this
//! module builds is pure ASCII.

use std::fmt;
use std::fmt::Write as _;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;

use crate::security::encoding::{js_is_space, js_trim, random_bytes};

/// A message that cannot be built: `bad_address` (not a mailbox this module can send to) or
/// `bad_header` (a line break in a header).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MailError {
    /// `bad_address` or `bad_header`.
    pub code: &'static str,
    /// What is wrong.
    pub message: &'static str,
}

impl MailError {
    const BAD_ADDRESS: MailError = MailError { code: "bad_address", message: "invalid mailbox" };
    const BAD_HEADER: MailError = MailError { code: "bad_header", message: "line break in a header" };
}

impl fmt::Display for MailError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.message)
    }
}

impl std::error::Error for MailError {}

fn is_local_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b"!#$%&'*+/=?^_`{|}~.-".contains(&b)
}

fn is_domain_label(l: &[u8]) -> bool {
    !l.is_empty()
        && l.len() <= 63
        && l[0].is_ascii_alphanumeric()
        && l[l.len() - 1].is_ascii_alphanumeric()
        && l.iter().all(|&b| b.is_ascii_alphanumeric() || b == b'-')
}

/// True when `s` is a plain ASCII e-mail address this module can send to: at most 254
/// characters, a local part of 1 to 64 characters of the RFC 5322 atom set and dots, and a
/// domain of letter-digit-hyphen labels.
pub fn is_valid_address(s: &str) -> bool {
    let Some((local, domain)) = s.split_once('@') else {
        return false;
    };
    s.len() <= 254
        && (1..=64).contains(&local.len())
        && local.bytes().all(is_local_char)
        && domain.split('.').all(|l| is_domain_label(l.as_bytes()))
}

/// A parsed mailbox.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mailbox {
    /// The display name (may be empty).
    pub name: String,
    /// The bare address.
    pub address: String,
}

/// JavaScript's `.`: any character but a line terminator.
fn is_js_line_terminator(c: char) -> bool {
    matches!(c, '\n' | '\r' | '\u{2028}' | '\u{2029}')
}

/// Splits `Name <addr>` the way `/^(.*?)\s*<([^<>]+)>$/` does: the name before the last `<`
/// (without trailing whitespace, no line terminator), the address between it and the final `>`.
fn split_angle(s: &str) -> Option<(&str, &str)> {
    let inner = s.strip_suffix('>')?;
    let lt = inner.rfind('<')?;
    let address = &inner[lt + 1..];
    if address.is_empty() || address.contains('>') {
        return None;
    }
    let name = inner[..lt].trim_end_matches(js_is_space);
    if name.chars().any(is_js_line_terminator) {
        return None;
    }
    Some((name, address))
}

/// Parses `Name <addr@host>`, `"Name" <addr@host>` or `addr@host`.
pub fn parse_mailbox(s: &str) -> Result<Mailbox, MailError> {
    let s = js_trim(s);
    let (name, address) = match split_angle(s) {
        Some((name, address)) => {
            let name = name.strip_prefix('"').unwrap_or(name);
            let name = name.strip_suffix('"').unwrap_or(name);
            (js_trim(name).to_string(), js_trim(address).to_string())
        }
        None => (String::new(), s.to_string()),
    };
    if !is_valid_address(&address) {
        return Err(MailError::BAD_ADDRESS);
    }
    if name.contains(['\r', '\n']) {
        return Err(MailError::BAD_HEADER);
    }
    Ok(Mailbox { name, address })
}

fn is_printable_ascii(s: &str) -> bool {
    s.bytes().all(|b| (0x20..=0x7e).contains(&b))
}

/// RFC 2047 encoding of a header text when it is not plain printable ASCII of at most 900
/// characters: UTF-8 base64 encoded-words of at most 45 bytes each, folded.
pub fn encode_header_text(s: &str) -> Result<String, MailError> {
    if s.contains(['\r', '\n']) {
        return Err(MailError::BAD_HEADER);
    }
    if is_printable_ascii(s) && s.len() <= 900 {
        return Ok(s.to_string());
    }
    let mut words: Vec<&str> = Vec::new();
    let mut start = 0;
    for (i, ch) in s.char_indices() {
        if i + ch.len_utf8() - start > 45 {
            words.push(&s[start..i]);
            start = i;
        }
    }
    if start < s.len() {
        words.push(&s[start..]);
    }
    let encoded: Vec<String> = words.iter().map(|w| format!("=?UTF-8?B?{}?=", STANDARD.encode(w))).collect();
    Ok(encoded.join("\r\n "))
}

fn format_mailbox(m: &Mailbox) -> Result<String, MailError> {
    if m.name.is_empty() {
        return Ok(m.address.clone());
    }
    let name = if is_printable_ascii(&m.name) && !m.name.contains(['"', '\\']) {
        format!("\"{}\"", m.name)
    } else {
        encode_header_text(&m.name)?
    };
    Ok(format!("{name} <{}>", m.address))
}

const WEEKDAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
const MONTHS: [&str; 12] =
    ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];

/// JavaScript's `Date.prototype.toUTCString()` of Unix milliseconds, e.g.
/// `"Mon, 28 Sep 2026 14:03:05 GMT"` (the date the templates print).
pub fn utc_string(ms: i64) -> String {
    let days = ms.div_euclid(86_400_000);
    let secs = ms.rem_euclid(86_400_000) / 1000;
    let (year, month, day) = crate::log::civil_from_days(days);
    let weekday = WEEKDAYS[(days + 4).rem_euclid(7) as usize];
    let sign = if year < 0 { "-" } else { "" };
    format!(
        "{weekday}, {day:02} {} {sign}{:04} {:02}:{:02}:{:02} GMT",
        MONTHS[month as usize - 1],
        year.abs(),
        secs / 3600,
        secs / 60 % 60,
        secs % 60
    )
}

/// RFC 5322 date of Unix milliseconds, e.g. `"Mon, 28 Sep 2026 14:03:05 +0000"`.
pub fn rfc5322_date(ms: i64) -> String {
    let s = utc_string(ms);
    format!("{}+0000", &s[..s.len() - 3])
}

/// Quoted-printable encoding (RFC 2045) of UTF-8 text with CRLF line breaks: at most 76
/// characters per encoded line, soft breaks never inside an `=XX` escape.
pub fn quoted_printable(text: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    for line in text.split("\r\n") {
        let bytes = line.as_bytes();
        let mut enc = String::with_capacity(bytes.len() * 3);
        for (i, &b) in bytes.iter().enumerate() {
            let last = i == bytes.len() - 1;
            if ((b == b' ' || b == b'\t') && !last) || ((0x21..=0x7e).contains(&b) && b != b'=') {
                enc.push(char::from(b));
            } else {
                let _ = write!(enc, "={b:02X}");
            }
        }
        let mut rest = enc.as_str();
        while rest.len() > 76 {
            let r = rest.as_bytes();
            let cut = if r[74] == b'=' {
                74
            } else if r[73] == b'=' {
                73
            } else {
                75
            };
            out.push(format!("{}=", &rest[..cut]));
            rest = &rest[cut..];
        }
        out.push(rest.to_string());
    }
    out.join("\r\n")
}

/// A built message: the envelope addresses and the complete message (headers and body, CRLF
/// line breaks, without the SMTP terminator).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BuiltMessage {
    /// The complete message, pure ASCII.
    pub raw: String,
    /// The bare sender address (envelope `MAIL FROM`).
    pub from: String,
    /// The bare recipient address (envelope `RCPT TO`).
    pub to: String,
    /// The `Message-ID` header value, angle brackets included.
    pub message_id: String,
}

/// What a message is made of. `message_id` (angle brackets included) is generated as
/// `<32 hex digits@sender domain>` when absent or empty.
#[derive(Clone, Copy, Debug)]
pub struct MessageParts<'a> {
    /// `Name <addr>` or `addr`.
    pub from: &'a str,
    /// `Name <addr>` or `addr`.
    pub to: &'a str,
    /// The subject (any text without line breaks).
    pub subject: &'a str,
    /// The body (`\n` or `\r\n` line breaks).
    pub text: &'a str,
    /// The `Date` header, Unix milliseconds.
    pub date_ms: i64,
    /// A fixed `Message-ID` (tests).
    pub message_id: Option<&'a str>,
}

/// Builds the complete message.
pub fn build_message(m: &MessageParts<'_>) -> Result<BuiltMessage, MailError> {
    let from = parse_mailbox(m.from)?;
    let to = parse_mailbox(m.to)?;
    let message_id = match m.message_id.filter(|id| !id.is_empty()) {
        Some(id) => id.to_string(),
        None => {
            let domain = from.address.split_once('@').map_or("", |(_, d)| d);
            format!("<{}@{domain}>", hex::encode(random_bytes::<16>()))
        }
    };
    let body = normalize_line_breaks(m.text);
    let seven_bit = body.is_ascii() && body.split("\r\n").all(|l| l.len() <= 998);
    let headers = [
        format!("From: {}", format_mailbox(&from)?),
        format!("To: {}", format_mailbox(&to)?),
        format!("Subject: {}", encode_header_text(m.subject)?),
        format!("Date: {}", rfc5322_date(m.date_ms)),
        format!("Message-ID: {message_id}"),
        "MIME-Version: 1.0".to_string(),
        "Content-Type: text/plain; charset=utf-8".to_string(),
        format!("Content-Transfer-Encoding: {}", if seven_bit { "7bit" } else { "quoted-printable" }),
        "Auto-Submitted: auto-generated".to_string(),
    ];
    let body = if seven_bit { body } else { quoted_printable(&body) };
    let raw = format!("{}\r\n\r\n{body}", headers.join("\r\n"));
    Ok(BuiltMessage { raw, from: from.address, to: to.address, message_id })
}

/// `\n` and `\r\n` become `\r\n` (a lone `\r` stays).
pub(crate) fn normalize_line_breaks(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + s.len() / 32);
    let mut rest = s;
    while let Some(i) = rest.find('\n') {
        out.push_str(rest[..i].strip_suffix('\r').unwrap_or(&rest[..i]));
        out.push_str("\r\n");
        rest = &rest[i + 1..];
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
pub(crate) mod tests {
    use serde_json::Value;

    use super::*;

    /// Vectors computed by the former server (Node 22): templates, built messages,
    /// quoted-printable, encoded words, dates and dot stuffing.
    pub(crate) fn vectors() -> Value {
        serde_json::from_str(include_str!("test-vectors.json")).expect("valid vector file")
    }

    /// 2026-09-28T14:03:05Z.
    pub(crate) const WHEN: i64 = 1_790_604_185_000;

    fn headers(raw: &str) -> Vec<(String, String)> {
        let head = raw.split("\r\n\r\n").next().unwrap().replace("\r\n ", " ");
        head.split("\r\n")
            .map(|l| {
                let i = l.find(':').unwrap();
                (l[..i].to_string(), l[i + 2..].to_string())
            })
            .collect()
    }

    fn header(raw: &str, name: &str) -> String {
        headers(raw).into_iter().find(|(k, _)| k == name).map(|(_, v)| v).unwrap()
    }

    fn decode_words(s: &str) -> String {
        let mut out = Vec::new();
        for w in s.split(' ') {
            let b64 = w.strip_prefix("=?UTF-8?B?").and_then(|w| w.strip_suffix("?=")).unwrap();
            out.extend(STANDARD.decode(b64).unwrap());
        }
        String::from_utf8(out).unwrap()
    }

    fn decode_qp(s: &str) -> String {
        let text = s.replace("=\r\n", "");
        let (b, mut bytes, mut i) = (text.as_bytes(), Vec::new(), 0);
        while i < b.len() {
            if b[i] == b'=' {
                bytes.push(u8::from_str_radix(&text[i + 1..i + 3], 16).unwrap());
                i += 3;
            } else {
                bytes.push(b[i]);
                i += 1;
            }
        }
        String::from_utf8(bytes).unwrap()
    }

    fn parts<'a>(from: &'a str, to: &'a str, subject: &'a str, text: &'a str) -> MessageParts<'a> {
        MessageParts { from, to, subject, text, date_ms: WHEN, message_id: None }
    }

    #[test]
    fn rfc_5322_headers_of_an_ascii_message() {
        let m = build_message(&parts(
            "Scacelith <no-reply@chess.example.org>",
            "alice@example.com",
            "Confirm",
            "Hello\nworld",
        ))
        .unwrap();
        assert_eq!(header(&m.raw, "From"), "\"Scacelith\" <no-reply@chess.example.org>");
        assert_eq!(header(&m.raw, "To"), "alice@example.com");
        assert_eq!(header(&m.raw, "Subject"), "Confirm");
        assert_eq!(header(&m.raw, "Date"), "Mon, 28 Sep 2026 14:03:05 +0000");
        let id = header(&m.raw, "Message-ID");
        assert!(
            id.len() == 1 + 32 + "@chess.example.org>".len() && id.ends_with("@chess.example.org>"),
            "{id}"
        );
        assert!(id[1..33].bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()));
        assert_eq!(id, m.message_id);
        assert_eq!(header(&m.raw, "MIME-Version"), "1.0");
        assert_eq!(header(&m.raw, "Content-Type"), "text/plain; charset=utf-8");
        assert_eq!(header(&m.raw, "Content-Transfer-Encoding"), "7bit");
        assert!(m.raw.ends_with("\r\n\r\nHello\r\nworld"));
        assert_eq!(m.from, "no-reply@chess.example.org");
        assert_eq!(rfc5322_date(0), "Thu, 01 Jan 1970 00:00:00 +0000");
    }

    #[test]
    fn utf8_subject_as_encoded_words_and_body_as_quoted_printable() {
        let subject = "Échec et mat ♔ ".repeat(6).trim().to_string();
        let text = format!(
            "Voilà un message très long avec des caractères accentués et un = signe, {}\nfin ",
            "é".repeat(80)
        );
        let m = build_message(&parts("Échecs <no-reply@example.org>", "bob@example.com", &subject, &text))
            .unwrap();
        assert_eq!(decode_words(&header(&m.raw, "Subject")), subject);
        assert!(header(&m.raw, "From").starts_with("=?UTF-8?B?"));
        assert_eq!(header(&m.raw, "Content-Transfer-Encoding"), "quoted-printable");
        let body = m.raw.split_once("\r\n\r\n").unwrap().1;
        for l in body.split("\r\n") {
            assert!(l.len() <= 76, "line too long: {}", l.len());
        }
        assert!(m.raw.bytes().all(|b| (0x20..=0x7e).contains(&b) || b == b'\r' || b == b'\n'));
        assert_eq!(decode_qp(body), text.replace('\n', "\r\n"));
        assert_eq!(decode_qp(&quoted_printable("a \r\nb\t")), "a \r\nb\t");
    }

    #[test]
    fn header_injection_and_bad_addresses_are_refused() {
        let e = build_message(&parts("a@example.org", "x@example.com", "hi\r\nBcc: evil@example.com", ""));
        assert_eq!(e.unwrap_err(), MailError::BAD_HEADER);
        assert!(build_message(&parts("a@example.org", "x@example.com\r\nBcc: e@x.com", "", "")).is_err());
        assert_eq!(parse_mailbox("not an address").unwrap_err().code, "bad_address");
        assert_eq!(encode_header_text("a\nb").unwrap_err().to_string(), "line break in a header");
        assert_eq!(
            parse_mailbox("\"Chess Club\" <club@example.org>").unwrap(),
            Mailbox { name: "Chess Club".into(), address: "club@example.org".into() }
        );
        assert_eq!(parse_mailbox("Name\n<a@b.example>").unwrap().name, "Name");
        assert!(parse_mailbox("Na\nme <a@b.example>").is_err());
        assert_eq!(parse_mailbox("a<b> <c@d.example>").unwrap().name, "a<b>");
        assert_eq!(parse_mailbox("  bare@example.org ").unwrap().address, "bare@example.org");
    }

    #[test]
    fn addresses() {
        for ok in ["a@b", "first.last+tag@sub.example.org", "x@a-b.c", "!#$%&'*+/=?^_`{|}~-@x.y"] {
            assert!(is_valid_address(ok), "{ok}");
        }
        let long_label = format!("a@{}.org", "b".repeat(64));
        let long = format!("{}@{}.org", "a".repeat(64), "b".repeat(250 - 64));
        for bad in [
            "",
            "a",
            "@b",
            "a@",
            "a@b@c",
            "a@-b",
            "a@b-",
            "a@b..c",
            "a@.b",
            "a b@c",
            "é@x.y",
            "a@é.y",
            &long_label,
            &long,
        ] {
            assert!(!is_valid_address(bad), "{bad}");
        }
        assert!(!is_valid_address(&format!("{}@x.y", "a".repeat(65))));
    }

    #[test]
    fn messages_match_the_former_server() {
        let v = vectors();
        for m in v["messages"].as_array().unwrap() {
            let built = build_message(&MessageParts {
                from: m["from"].as_str().unwrap(),
                to: m["to"].as_str().unwrap(),
                subject: m["subject"].as_str().unwrap(),
                text: m["text"].as_str().unwrap(),
                date_ms: WHEN,
                message_id: Some("<00112233445566778899aabbccddeeff@example.org>"),
            })
            .unwrap();
            assert_eq!(built.raw, m["raw"].as_str().unwrap(), "{}", m["subject"]);
        }
    }

    #[test]
    fn encodings_and_dates_match_the_former_server() {
        let v = vectors();
        for p in v["qp"].as_array().unwrap() {
            assert_eq!(quoted_printable(p[0].as_str().unwrap()), p[1].as_str().unwrap(), "{:?}", p[0]);
        }
        for p in v["words"].as_array().unwrap() {
            assert_eq!(encode_header_text(p[0].as_str().unwrap()).unwrap(), p[1].as_str().unwrap());
        }
        for p in v["dates"].as_array().unwrap() {
            let ms = p[0].as_i64().unwrap();
            assert_eq!(rfc5322_date(ms), p[1].as_str().unwrap());
            assert_eq!(utc_string(ms), p[2].as_str().unwrap());
        }
        for (ms, s) in [
            (-62_198_755_200_000, "Fri, 01 Jan -0001 00:00:00 GMT"),
            (-62_167_219_200_000, "Sat, 01 Jan 0000 00:00:00 GMT"),
            (253_402_300_799_000, "Fri, 31 Dec 9999 23:59:59 GMT"),
            (8_640_000_000_000_000, "Sat, 13 Sep 275760 00:00:00 GMT"),
        ] {
            assert_eq!(utc_string(ms), s);
        }
    }

    #[test]
    fn line_breaks_are_normalised() {
        assert_eq!(normalize_line_breaks("a\nb\r\nc\rd\n"), "a\r\nb\r\nc\rd\r\n");
        assert_eq!(normalize_line_breaks("\r\r\n"), "\r\r\n");
    }
}
