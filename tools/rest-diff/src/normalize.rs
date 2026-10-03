//! Rules that replace the volatile values of an answer (ids, times, tokens, server ids) by
//! markers before the comparison. A rule names one kind of value and checks its format: a value
//! that does not have the format of its kind stays as it is, so that the comparison reports it.
//! Whole bodies are never ignored.

use sha2::{Digest, Sha256};

use crate::http::Resp;
use crate::json::{self, J};

/// What the rules of one side need to know.
#[derive(Clone, Debug, Default)]
pub struct Context {
    /// Game ids of this side and their labels (`G1`...), the same label on both sides.
    pub games: Vec<(u64, String)>,
    /// Literal strings of this side replaced by a label (a username made unique per side...).
    pub literals: Vec<(String, String)>,
}

/// An answer reduced to what is compared.
#[derive(Clone, Debug)]
pub struct Normalized {
    /// Status code (0: no answer).
    pub status: u16,
    /// Reason phrase.
    pub reason: String,
    /// Protocol of the status line.
    pub version: String,
    /// Transport failure, when no answer came.
    pub error: Option<String>,
    /// Headers: name as sent, normalised value.
    pub headers: Vec<(String, String)>,
    /// Whether the server closed the connection after the answer.
    pub closed: Option<bool>,
    /// Whether the body was chunked.
    pub chunked: bool,
    /// The body.
    pub body: Body,
    /// The raw body length.
    pub raw_len: usize,
    /// SHA-256 of the raw body (hex).
    pub raw_sha: String,
}

/// A normalised body.
#[derive(Clone, Debug)]
pub enum Body {
    /// No body.
    Empty,
    /// JSON (`compact`: written without whitespace).
    Json { value: J, compact: bool },
    /// Text (HTML, PGN, plain), normalised.
    Text(String),
    /// Bytes compared by length and hash.
    Binary,
}

/// Keys whose numeric value is a time (epoch ms): compared within 30 s.
const TIME_KEYS: &[&str] = &[
    "expiresAt",
    "createdAt",
    "lastLoginAt",
    "lastSeenAt",
    "startedAt",
    "endedAt",
    "exportedAt",
    "updatedAt",
    "at",
    "startsAt",
    "endsAt",
    "until",
    "revokedAt",
    "liftedAt",
    "requestedAt",
];

/// Normalises one answer with the rules of its side.
pub fn normalize(resp: &Resp, ctx: &Context) -> Normalized {
    let content_type = resp.header("content-type").unwrap_or_default().to_ascii_lowercase();
    let body = if resp.body.is_empty() {
        Body::Empty
    } else if content_type.starts_with("application/json") {
        match json::parse(&resp.body) {
            Ok(v) => {
                Body::Json { value: normalize_json(None, v, ctx), compact: json::is_compact(&resp.body) }
            }
            Err(_) => Body::Text(normalize_text(&String::from_utf8_lossy(&resp.body), ctx)),
        }
    } else if content_type.starts_with("text/") || content_type.starts_with("application/x-chess-pgn") {
        Body::Text(normalize_text(&String::from_utf8_lossy(&resp.body), ctx))
    } else if content_type.is_empty() && std::str::from_utf8(&resp.body).is_ok() {
        Body::Text(normalize_text(&String::from_utf8_lossy(&resp.body), ctx))
    } else {
        Body::Binary
    };
    let headers = resp.headers.iter().map(|(k, v)| (k.clone(), normalize_header(k, v, ctx))).collect();
    Normalized {
        status: resp.status,
        reason: resp.reason.clone(),
        version: resp.version.clone(),
        error: resp.error.as_ref().map(|e| error_class(e)),
        headers,
        closed: resp.closed,
        chunked: resp.chunked,
        body,
        raw_len: resp.body.len(),
        raw_sha: hex::encode(Sha256::digest(&resp.body)),
    }
}

/// A transport failure without its variable details (OS error numbers, byte counts).
fn error_class(e: &str) -> String {
    if e.starts_with("connection closed without an answer") {
        "connection closed without an answer".into()
    } else if e.starts_with("read:") && (e.contains("reset") || e.contains("Connection reset")) {
        "connection reset".into()
    } else if (e.starts_with("connect:") || e.starts_with("TLS:")) && e.contains("reset") {
        // A reset right after the TCP handshake: whether the client sees it on `connect` or on
        // the first TLS read is a matter of timing on the client's side.
        "connection reset before TLS".into()
    } else if e.starts_with("write:") {
        "connection closed while sending".into()
    } else if e.starts_with("body cut") {
        "body cut short".into()
    } else {
        e.to_string()
    }
}

fn normalize_header(name: &str, value: &str, ctx: &Context) -> String {
    match name.to_ascii_lowercase().as_str() {
        "date" => "<http-date>".into(),
        _ => normalize_text(value, ctx),
    }
}

/// Is `s` shaped like a UUID?
fn is_uuid(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 36
        && b.iter().enumerate().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => *c == b'-',
            _ => c.is_ascii_hexdigit(),
        })
}

fn is_b64url(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
}

/// The marker of a token-shaped string (`sct_` + 43 base64url characters...), if it is one.
fn token_label(s: &str) -> Option<&'static str> {
    for (prefix, label) in [("sct_", "session-token"), ("mfa_", "mfa-token"), ("sso_", "sso-ticket")] {
        if let Some(rest) = s.strip_prefix(prefix)
            && is_b64url(rest)
            && rest.len() >= 20
        {
            return Some(label);
        }
    }
    None
}

fn is_recovery_code(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 12
        && b.iter().enumerate().all(|(i, c)| match i {
            4 | 9 => *c == b'-',
            _ => c.is_ascii_lowercase() || c.is_ascii_digit(),
        })
}

fn is_base32(s: &str) -> bool {
    s.len() >= 16 && s.bytes().all(|c| c.is_ascii_uppercase() || (b'2'..=b'7').contains(&c))
}

fn game_label(ctx: &Context, id: u64) -> Option<&str> {
    ctx.games.iter().find(|(g, _)| *g == id).map(|(_, l)| l.as_str())
}

/// Normalises a JSON value found under `key`.
pub fn normalize_json(key: Option<&str>, v: J, ctx: &Context) -> J {
    match v {
        J::Obj(members) => {
            J::Obj(members.into_iter().map(|(k, x)| (k.clone(), normalize_json(Some(&k), x, ctx))).collect())
        }
        J::Arr(items) => J::Arr(
            items
                .into_iter()
                .map(|x| match (key, &x) {
                    (Some("recoveryCodes"), J::Str(s, _)) if is_recovery_code(s) => {
                        J::Mask("recovery-code".into())
                    }
                    _ => normalize_json(key, x, ctx),
                })
                .collect(),
        ),
        J::Num(n) => {
            let f: f64 = n.parse().unwrap_or(f64::NAN);
            if let Ok(id) = n.parse::<u64>()
                && let Some(label) = game_label(ctx, id)
            {
                return J::Mask(format!("game:{label}"));
            }
            match key {
                Some(k) if TIME_KEYS.contains(&k) && f > 1e12 => {
                    J::Approx { label: "time", value: f, tol: 30_000.0 }
                }
                Some("retryAfter") => J::Approx { label: "seconds", value: f, tol: 1.0 },
                Some("spentMs" | "clockMs") => J::Approx { label: "clock-ms", value: f, tol: 5_000.0 },
                _ => J::Num(n),
            }
        }
        J::Str(s, raw) => {
            if let Some(label) = token_label(&s) {
                return J::Mask(label.into());
            }
            if is_uuid(&s) {
                return J::Mask("uuid".into());
            }
            if let Ok(id) = s.parse::<u64>()
                && let Some(label) = game_label(ctx, id)
            {
                return J::Mask(format!("game:{label}"));
            }
            match key {
                Some("challenge") if s.len() > 20 => J::Mask("pow-challenge".into()),
                Some("secret") if is_base32(&s) => J::Mask("totp-secret".into()),
                Some("state" | "attemptId") if is_b64url(&s) && s.len() >= 20 => J::Mask("sso-state".into()),
                _ => {
                    let t = normalize_text(&s, ctx);
                    if t == s { J::Str(s, raw) } else { J::Str(t.clone(), json::escape(&t)) }
                }
            }
        }
        other => other,
    }
}

/// Normalises free text: game ids, the per-side literals, token-shaped runs, TOTP secrets in
/// `otpauth:` URIs, PGN clock comments and times.
pub fn normalize_text(s: &str, ctx: &Context) -> String {
    let mut out = s.to_string();
    for (lit, label) in &ctx.literals {
        if !lit.is_empty() {
            out = out.replace(lit.as_str(), &format!("<{label}>"));
        }
    }
    // Longest ids first, so that no id is replaced inside a longer one.
    let mut games: Vec<&(u64, String)> = ctx.games.iter().collect();
    games.sort_by_key(|(id, _)| std::cmp::Reverse(id.to_string().len()));
    for (id, label) in games {
        out = replace_number(&out, &id.to_string(), &format!("<{label}>"));
    }
    out = mask_tokens(&out);
    out = mask_between(&out, "secret=", '&', "<totp-secret>");
    out = mask_between(&out, "[%clk ", ']', "<clock>");
    out = mask_between(&out, "[%emt ", ']', "<clock>");
    out = mask_between(&out, "[UTCTime \"", '"', "<time>");
    out
}

/// Replaces `num` where it is not part of a longer digit run.
fn replace_number(s: &str, num: &str, with: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find(num) {
        let before_digit = rest[..i].chars().next_back().is_some_and(|c| c.is_ascii_digit());
        let after = &rest[i + num.len()..];
        let after_digit = after.chars().next().is_some_and(|c| c.is_ascii_digit());
        out.push_str(&rest[..i]);
        if before_digit || after_digit {
            out.push_str(num);
        } else {
            out.push_str(with);
        }
        rest = after;
    }
    out.push_str(rest);
    out
}

/// Replaces what follows each `start` up to `end` (exclusive) by `with`.
fn mask_between(s: &str, start: &str, end: char, with: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find(start) {
        out.push_str(&rest[..i + start.len()]);
        let after = &rest[i + start.len()..];
        let stop = after.find(end).unwrap_or(after.len());
        out.push_str(with);
        rest = &after[stop..];
    }
    out.push_str(rest);
    out
}

/// Replaces runs of 40 or more base64url characters that mix letters and digits (link tokens,
/// session tokens) by `<token>`, keeping a `sct_`/`mfa_`/`sso_` prefix visible.
fn mask_tokens(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut run = String::new();
    let flush = |run: &mut String, out: &mut String| {
        let looks_random = run.len() >= 40
            && run.bytes().any(|c| c.is_ascii_digit())
            && run.bytes().any(|c| c.is_ascii_alphabetic());
        if looks_random {
            out.push_str("<token>");
        } else {
            out.push_str(run);
        }
        run.clear();
    };
    for c in s.chars() {
        if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
            run.push(c);
        } else {
            flush(&mut run, &mut out);
            out.push(c);
        }
    }
    flush(&mut run, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rules() {
        let ctx = Context { games: vec![(4100000000001, "G1".into())], literals: vec![] };
        let v = json::parse(
            br#"{"token":"sct_L_8GDd7uzfQ3QQWtqrsWXDTsFWzRwIvJcwIGHhjWPS8","expiresAt":1798658839708,
                "id":4100000000001,"retryAfter":3,"serverId":"07dd26af-672a-43af-a8af-34011c7e977b",
                "recoveryCodes":["j7v5-3ezx-zn"],"n":12}"#,
        )
        .unwrap();
        let n = normalize_json(None, v, &ctx);
        assert_eq!(n.get("token"), Some(&J::Mask("session-token".into())));
        assert_eq!(n.get("id"), Some(&J::Mask("game:G1".into())));
        assert!(matches!(n.get("expiresAt"), Some(J::Approx { label: "time", .. })));
        assert_eq!(n.get("serverId"), Some(&J::Mask("uuid".into())));
        assert_eq!(n.at("recoveryCodes.0"), Some(&J::Mask("recovery-code".into())));
        assert_eq!(n.get("n"), Some(&J::Num("12".into())));
        let t = normalize_text(
            "filename=\"scacelith-4100000000001.pgn\" 41000000000012 ?token=hPwidd3xoejxU_BVdcy-tg9RzNnFCOi_moIy6O58JM8 \
             {[%clk 0:03:00.0] [%emt 0:00:01.7]}",
            &ctx,
        );
        assert_eq!(
            t,
            "filename=\"scacelith-<G1>.pgn\" 41000000000012 ?token=<token> {[%clk <clock>] [%emt <clock>]}"
        );
    }
}
