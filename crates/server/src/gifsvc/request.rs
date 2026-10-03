//! What the two GIF routes check and answer (the Node server's `src/http/routes/gif.js`): the
//! picture options of a query or a body, the texts taken from PGN tags, the quotas, the limits and
//! the error texts. The routes themselves (game lookup, PGN reading, answers) are in
//! `http::routes`.

use scacelith_gif::{DELAY_MAX_MS, DELAY_MIN_MS, MAX_PLIES, Options, Orientation, Size};
use serde_json::{Map, Value};
use unicode_normalization::UnicodeNormalization as _;

use crate::config::Config;

/// Content type of the answers.
pub const GIF_CONTENT_TYPE: &str = "image/gif";

/// The largest PGN text `POST /api/v1/gif` takes, in bytes of UTF-8.
pub const GIF_PGN_MAX_BYTES: usize = 65536;

/// Body limit of `POST /api/v1/gif` (413 `payload_too_large` above, whatever `HTTP_BODY_LIMIT`).
pub const GIF_BODY_LIMIT_BYTES: usize = 2 * GIF_PGN_MAX_BYTES + 4096;

/// The fields of the `POST /api/v1/gif` body; any other is 400 `invalid_request`
/// `unknown field "<k>"` (the first unknown key in JavaScript `Object.keys` order).
pub const GIF_BODY_FIELDS: [&str; 5] = ["pgn", "size", "orientation", "delayMs", "coords"];

/// Range of the `Retry-After` seconds of a 503 `server_busy` (drawn at random, so that refused
/// clients spread out).
pub const GIF_BUSY_RETRY_SEC: (u64, u64) = (3, 10);

/// Message of 404 `gif_disabled` (`GIF_ENABLED=false`; the route token is given back).
pub const GIF_DISABLED_MESSAGE: &str = "Animated GIFs are turned off on this server.";

/// Message of 503 `server_busy` ([`GifError::Busy`](super::GifError::Busy); every token of the
/// request is given back).
pub const SERVER_BUSY_MESSAGE: &str = "The server is busy making other GIFs; try again in a few seconds.";

/// Message of 500 `render_failed` ([`GifError::RenderFailed`](super::GifError::RenderFailed);
/// the quotas stay spent).
pub const RENDER_FAILED_MESSAGE: &str = "The GIF could not be made.";

/// Message of 422 `game_too_long`: `plies` of a stored game, `None` for a PGN cut by the reader.
pub fn game_too_long_message(plies: Option<usize>, max_plies: usize) -> String {
    match plies {
        Some(n) => format!("The game is too long for a GIF ({n} plies, at most {max_plies})."),
        None => format!("The game is too long for a GIF (more than {max_plies} plies, at most {max_plies})."),
    }
}

/// The `Retry-After` seconds of a 503 `server_busy`: uniform in [`GIF_BUSY_RETRY_SEC`].
pub fn busy_retry_after_secs() -> u64 {
    let (min, max) = GIF_BUSY_RETRY_SEC;
    // A failing system generator only loses the spread.
    let r = u64::from(getrandom::u32().unwrap_or(0));
    min + r % (max - min + 1)
}

/// The longest game the routes take: `GIF_MAX_PLIES`, at most the renderer's 1200.
pub fn max_plies(config: &Config) -> usize {
    usize::try_from(config.gif_max_plies).unwrap_or(0).min(MAX_PLIES)
}

/// Handler timeout of the GIF routes: a render waits `GIF_QUEUE_TIMEOUT_MS` at most for a thread,
/// then runs `GIF_RENDER_TIMEOUT_MS` at most, plus 5 s (503 `timeout` beyond).
pub fn handler_timeout_ms(config: &Config) -> u64 {
    u64::try_from(config.gif_queue_timeout_ms + config.gif_render_timeout_ms + 5000).unwrap_or(0)
}

/// A rate limit of the GIF routes, as the HTTP framework's route rates take it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct QuotaSpec {
    /// Bucket name (bucket keys `<key>:u<userId>` or `<key>:<address key>`, metric label).
    pub key: &'static str,
    /// Tokens per window.
    pub limit: u32,
    /// Window in milliseconds.
    pub window_ms: u64,
    /// Counted per account (else per client address: IPv4, or the IPv6 /64).
    pub by_user: bool,
    /// Counted for the whole server (the Node primary's shared stage).
    pub shared: bool,
    /// For an IPv6 client, a second bucket per /48 (`<key>/48:<prefix>`) with this limit.
    pub prefix_limit: Option<u32>,
}

/// The request limit of both routes: 30 a minute per account, taken by the router on every call.
pub const GIF_ROUTE_RATE: QuotaSpec =
    QuotaSpec { key: "gif", limit: 30, window_ms: 60_000, by_user: true, shared: false, prefix_limit: None };

/// The render quotas, taken in this order (all or none) only when the GIF is neither cached nor
/// being rendered: per account `GIF_USER_RENDERS_PER_MIN` and `_PER_HOUR`, per address
/// `GIF_IP_RENDERS_PER_MIN` and `_PER_HOUR` (three times that per IPv6 /48).
pub fn render_quotas(config: &Config) -> [QuotaSpec; 4] {
    let n = |v: i64| u32::try_from(v.max(0)).unwrap_or(u32::MAX);
    let per_ip = |key, limit: i64, window_ms| QuotaSpec {
        key,
        limit: n(limit),
        window_ms,
        by_user: false,
        shared: true,
        prefix_limit: Some(n(limit.saturating_mul(3))),
    };
    [
        QuotaSpec {
            key: "gif_user_min",
            limit: n(config.gif_user_renders_per_min),
            window_ms: 60_000,
            by_user: true,
            shared: true,
            prefix_limit: None,
        },
        QuotaSpec {
            key: "gif_user_hour",
            limit: n(config.gif_user_renders_per_hour),
            window_ms: 3_600_000,
            by_user: true,
            shared: true,
            prefix_limit: None,
        },
        per_ip("gif_ip_min", config.gif_ip_renders_per_min, 60_000),
        per_ip("gif_ip_hour", config.gif_ip_renders_per_hour, 3_600_000),
    ]
}

/// A refused picture option: 400 `invalid_option` with `field`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InvalidOption {
    /// `size`, `orientation`, `delay` (query) or `delayMs` (body), `coords`.
    pub field: &'static str,
    /// The message of the answer.
    pub message: String,
}

impl std::fmt::Display for InvalidOption {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for InvalidOption {}

fn invalid(field: &'static str, message: impl Into<String>) -> InvalidOption {
    InvalidOption { field, message: message.into() }
}

fn invalid_size() -> InvalidOption {
    invalid("size", "size must be one of small, medium, large.")
}

fn invalid_orientation() -> InvalidOption {
    invalid("orientation", "orientation must be white or black.")
}

fn invalid_delay(field: &'static str) -> InvalidOption {
    invalid(
        field,
        format!("{field} must be an integer from {DELAY_MIN_MS} to {DELAY_MAX_MS} (milliseconds per move)."),
    )
}

/// The options of `GET /api/v1/games/:id/gif` as the query gives them (the first occurrence of
/// each key, decoded); `None` for an absent key.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct QueryOptions<'a> {
    /// `size`: small, medium or large.
    pub size: Option<&'a str>,
    /// `orientation`: white or black.
    pub orientation: Option<&'a str>,
    /// `delay`: 1 to 6 decimal digits, 100..=3000.
    pub delay: Option<&'a str>,
    /// `coords`: 0 or 1.
    pub coords: Option<&'a str>,
}

/// The picture options of a query, with the defaults (medium, white, 500 ms, coordinates)
/// filled in. Checked in the order size, orientation, delay, coords.
pub fn options_from_query(q: &QueryOptions<'_>) -> Result<Options, InvalidOption> {
    let mut out = Options::default();
    if let Some(s) = q.size {
        out.size = Size::parse(s).ok_or_else(invalid_size)?;
    }
    if let Some(s) = q.orientation {
        out.orientation = Orientation::parse(s).ok_or_else(invalid_orientation)?;
    }
    if let Some(s) = q.delay {
        let digits = (1..=6).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_digit());
        out.delay_ms = digits
            .then(|| s.parse::<u32>().ok())
            .flatten()
            .filter(|n| (DELAY_MIN_MS..=DELAY_MAX_MS).contains(n))
            .ok_or_else(|| invalid_delay("delay"))?;
    }
    match q.coords {
        None => {}
        Some("1") => out.coords = true,
        Some("0") => out.coords = false,
        Some(_) => return Err(invalid("coords", "coords must be 0 or 1.")),
    }
    Ok(out)
}

/// The picture options of a `POST /api/v1/gif` body (fields `size`, `orientation`, `delayMs`,
/// `coords`), with the defaults filled in. A JSON body has types: the delay must be a number with
/// an integral value (500.0 passes, "500" does not), coords a boolean; `null` is a value, refused.
pub fn options_from_body(body: &Map<String, Value>) -> Result<Options, InvalidOption> {
    let mut out = Options::default();
    if let Some(v) = body.get("size") {
        out.size = v.as_str().and_then(Size::parse).ok_or_else(invalid_size)?;
    }
    if let Some(v) = body.get("orientation") {
        out.orientation = v.as_str().and_then(Orientation::parse).ok_or_else(invalid_orientation)?;
    }
    if let Some(v) = body.get("delayMs") {
        let n = v
            .as_f64()
            .filter(|n| n.fract() == 0.0 && (f64::from(DELAY_MIN_MS)..=f64::from(DELAY_MAX_MS)).contains(n));
        out.delay_ms = n.map(|n| n as u32).ok_or_else(|| invalid_delay("delayMs"))?;
    }
    if let Some(v) = body.get("coords") {
        out.coords = v.as_bool().ok_or_else(|| invalid("coords", "coords must be true or false."))?;
    }
    Ok(out)
}

/// A player's name (or an ending) from a PGN tag: compatibility decomposition, accents dropped,
/// every UTF-16 unit outside printable ASCII as '?' (so a character beyond the BMP is "??"),
/// runs of spaces as one, trimmed, then cut to `max` characters. "Ljubojević ♞" is
/// "Ljubojevic ?".
pub fn tag_text(v: &str, max: usize) -> String {
    let mut ascii = String::with_capacity(v.len());
    for c in v.nfkd().filter(|c| !('\u{300}'..='\u{36f}').contains(c)) {
        match c {
            ' '..='~' => ascii.push(c),
            _ => (0..c.len_utf16()).for_each(|_| ascii.push('?')),
        }
    }
    // Only ASCII remains, and every whitespace character but the space became '?'.
    let collapsed = ascii.split(' ').filter(|w| !w.is_empty()).collect::<Vec<_>>().join(" ");
    let mut out = collapsed;
    out.truncate(max);
    out
}

/// JavaScript's `\s` (WhiteSpace and LineTerminator).
fn is_js_space(c: char) -> bool {
    c == '\u{feff}' || (c.is_whitespace() && c != '\u{85}')
}

/// A rating from a `WhiteElo` / `BlackElo` tag: 1 to 4 digits between optional spaces, else
/// `None` (absent, "?", "-").
pub fn tag_rating(v: Option<&str>) -> Option<i64> {
    let s = v?.trim_matches(is_js_space);
    if (1..=4).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_digit()) { s.parse().ok() } else { None }
}

/// The ending shown from a `Termination` tag: `None` when absent, empty or "normal" (any case;
/// the final position then says it), else its text with the first letter in upper case
/// ("time forfeit" is "Time forfeit").
pub fn tag_ending(v: Option<&str>) -> Option<String> {
    let s = tag_text(v.unwrap_or(""), 60);
    if s.is_empty() || s.eq_ignore_ascii_case("normal") {
        return None;
    }
    let mut chars = s.chars();
    let first = chars.next().expect("not empty");
    Some(first.to_ascii_uppercase().to_string() + chars.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;
    use scacelith_gif::DELAY_DEFAULT_MS;
    use serde_json::json;

    fn body(v: Value) -> Map<String, Value> {
        v.as_object().expect("an object").clone()
    }

    #[test]
    fn query_options_defaults_and_validation() {
        assert_eq!(options_from_query(&QueryOptions::default()), Ok(Options::default()));
        assert_eq!(Options::default().delay_ms, DELAY_DEFAULT_MS);
        let q = QueryOptions {
            size: Some("large"),
            orientation: Some("black"),
            delay: Some("0100"),
            coords: Some("0"),
        };
        let o = options_from_query(&q).unwrap();
        assert_eq!(
            (o.size, o.orientation, o.delay_ms, o.coords),
            (Size::Large, Orientation::Black, 100, false)
        );
        let err = |q: QueryOptions| options_from_query(&q).unwrap_err();
        assert_eq!(err(QueryOptions { size: Some("huge"), ..Default::default() }), invalid_size());
        assert_eq!(err(QueryOptions { size: Some(""), ..Default::default() }).field, "size");
        assert_eq!(
            err(QueryOptions { orientation: Some("White"), ..Default::default() }),
            invalid_orientation()
        );
        for d in ["99", "3001", "", "1e3", "+500", "500.0", " 500", "0003000x", "1000000"] {
            let e = err(QueryOptions { delay: Some(d), ..Default::default() });
            assert_eq!(e.field, "delay", "{d:?}");
            assert_eq!(e.message, "delay must be an integer from 100 to 3000 (milliseconds per move).");
        }
        assert_eq!(
            options_from_query(&QueryOptions { delay: Some("003000"), ..Default::default() })
                .unwrap()
                .delay_ms,
            3000
        );
        assert_eq!(
            err(QueryOptions { coords: Some("true"), ..Default::default() }).message,
            "coords must be 0 or 1."
        );
        // The first refused option is the one reported.
        let both = QueryOptions { orientation: Some("x"), coords: Some("x"), ..Default::default() };
        assert_eq!(err(both).field, "orientation");
    }

    #[test]
    fn body_options_take_typed_values() {
        assert_eq!(options_from_body(&Map::new()), Ok(Options::default()));
        let o =
            options_from_body(&body(json!({"size": "small", "delayMs": 500.0, "coords": false}))).unwrap();
        assert_eq!((o.size, o.delay_ms, o.coords), (Size::Small, 500, false));
        assert_eq!(options_from_body(&body(json!({"delayMs": 3000}))).unwrap().delay_ms, 3000);
        let err = |v: Value| options_from_body(&body(v)).unwrap_err();
        for v in [json!("500"), json!(500.5), json!(99), json!(null), json!(true), json!(-0.0)] {
            let e = err(json!({ "delayMs": v }));
            assert_eq!(e.field, "delayMs");
            assert_eq!(e.message, "delayMs must be an integer from 100 to 3000 (milliseconds per move).");
        }
        assert_eq!(err(json!({"coords": 1})).message, "coords must be true or false.");
        assert_eq!(err(json!({"size": null})).field, "size");
        assert_eq!(err(json!({"orientation": ["white"]})).field, "orientation");
    }

    #[test]
    fn tag_texts_ratings_and_endings() {
        assert_eq!(tag_text("\u{dc}n\u{ef}c\u{f6}d\u{e9} \u{2713}  name", 48), "Unicode ? name");
        assert_eq!(tag_text("Ljubojevi\u{107} \u{265e}", 48), "Ljubojevic ?");
        assert_eq!(tag_text("M\u{fc}ller, Hans", 48), "Muller, Hans");
        // Values of the Node server's tagText.
        assert_eq!(tag_text(" a\tb\n\u{1f600} \u{fb01} \u{bd} \u{216b} ", 48), "a?b??? fi 1?2 XII");
        assert_eq!(tag_text(&"x".repeat(60), 48), "x".repeat(48));
        assert_eq!(tag_text("\u{a0}lead", 48), "lead");
        assert_eq!(tag_text("", 48), "");

        assert_eq!(tag_rating(Some("2690")), Some(2690));
        assert_eq!(tag_rating(Some(" 0042\u{a0}")), Some(42));
        for v in [None, Some("?"), Some("-"), Some(""), Some("12345"), Some("1 2"), Some("\u{85}12")] {
            assert_eq!(tag_rating(v), None, "{v:?}");
        }

        assert_eq!(tag_ending(Some("time forfeit")).as_deref(), Some("Time forfeit"));
        assert_eq!(tag_ending(Some("  NORMAL ")), None);
        assert_eq!(tag_ending(Some("")), None);
        assert_eq!(tag_ending(None), None);
        // Cut to 60 characters, not trimmed again.
        let long = tag_ending(Some(&"abandoned ".repeat(10))).unwrap();
        assert_eq!((long.len(), long.starts_with("Abandoned "), long.ends_with(' ')), (60, true, true));
    }

    #[test]
    fn limits_quotas_and_messages() {
        let c = Config::for_tests();
        assert_eq!(max_plies(&c), usize::try_from(c.gif_max_plies).unwrap().min(1200));
        assert_eq!(
            handler_timeout_ms(&Config {
                gif_queue_timeout_ms: 1234,
                gif_render_timeout_ms: 5678,
                ..c.clone()
            }),
            11_912
        );
        let q = render_quotas(&Config {
            gif_user_renders_per_min: 4,
            gif_user_renders_per_hour: 30,
            gif_ip_renders_per_min: 12,
            gif_ip_renders_per_hour: 120,
            ..c
        });
        let keys: Vec<_> =
            q.iter().map(|q| (q.key, q.limit, q.window_ms, q.by_user, q.prefix_limit)).collect();
        assert_eq!(
            keys,
            [
                ("gif_user_min", 4, 60_000, true, None),
                ("gif_user_hour", 30, 3_600_000, true, None),
                ("gif_ip_min", 12, 60_000, false, Some(36)),
                ("gif_ip_hour", 120, 3_600_000, false, Some(360)),
            ]
        );
        assert!(q.iter().all(|q| q.shared));
        assert_eq!(
            (GIF_ROUTE_RATE.limit, GIF_ROUTE_RATE.window_ms, GIF_ROUTE_RATE.by_user),
            (30, 60_000, true)
        );
        assert_eq!(GIF_BODY_LIMIT_BYTES, 135_168);
        assert_eq!(
            game_too_long_message(Some(41), 40),
            "The game is too long for a GIF (41 plies, at most 40)."
        );
        assert_eq!(
            game_too_long_message(None, 40),
            "The game is too long for a GIF (more than 40 plies, at most 40)."
        );
        for _ in 0..200 {
            assert!((3..=10).contains(&busy_retry_after_secs()));
        }
    }
}
