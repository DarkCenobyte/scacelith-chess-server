//! Text helpers of the administration commands: times, table cells, plain tables and the dates
//! of the options, exactly as the former `bin/admin.js` printed and read them.

use serde_json::Value;

use crate::log::iso_time;
use crate::util::js;

/// Milliseconds in a day.
const DAY_MS: i64 = 86_400_000;

/// A time as `2026-09-01T12:00:00Z` (milliseconds shown only when not 0); `-` for none or 0.
pub fn iso(t: Option<i64>) -> String {
    match t {
        None | Some(0) => "-".into(),
        Some(t) => {
            let s = iso_time(t);
            match s.strip_suffix(".000Z") {
                Some(base) => format!("{base}Z"),
                None => s,
            }
        }
    }
}

/// Whether a character is printed as `\uXXXX` in a cell: control characters (C0, DEL, C1), line
/// and paragraph separators and bidirectional controls, so that a player's text (a report
/// comment) cannot break a table or forge lines of the output.
fn unprintable(c: char) -> bool {
    matches!(c, '\u{0}'..='\u{1f}' | '\u{7f}'..='\u{9f}' | '\u{200e}' | '\u{200f}' | '\u{2028}' | '\u{2029}'
        | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
}

/// A cell's text with its unprintable characters escaped.
pub fn cell(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if unprintable(c) {
            out.push_str(&format!("\\u{:04x}", c as u32));
        } else {
            out.push(c);
        }
    }
    out
}

/// A plain table: a header, a rule of dashes and the rows, columns padded to their widest cell
/// and separated by two spaces, trailing spaces trimmed; `(none)` without rows.
pub fn table(cols: &[&str], rows: &[Vec<String>]) -> String {
    if rows.is_empty() {
        return "(none)\n".into();
    }
    let rows: Vec<Vec<String>> = rows.iter().map(|r| r.iter().map(|c| cell(c)).collect()).collect();
    let widths: Vec<usize> = cols
        .iter()
        .enumerate()
        .map(|(i, c)| rows.iter().map(|r| js::utf16_len(&r[i])).fold(js::utf16_len(c), usize::max))
        .collect();
    let line = |vals: Vec<String>| {
        let padded: Vec<String> = vals
            .iter()
            .zip(&widths)
            .map(|(v, w)| format!("{v}{}", " ".repeat(w.saturating_sub(js::utf16_len(v)))))
            .collect();
        js::trim_end(&padded.join("  ")).to_string()
    };
    let mut lines = vec![
        line(cols.iter().map(|c| cell(c)).collect()),
        line(widths.iter().map(|w| "-".repeat(*w)).collect()),
    ];
    lines.extend(rows.into_iter().map(line));
    lines.join("\n") + "\n"
}

/// `String(v)` of a JSON value, as a template literal interpolates it: `undefined` when absent.
pub fn js_string(v: Option<&Value>) -> String {
    match v {
        None => "undefined".into(),
        Some(Value::Null) => "null".into(),
        Some(Value::Bool(b)) => b.to_string(),
        Some(Value::Number(n)) => js::number_to_string(n.as_f64().unwrap_or(f64::NAN)),
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(a)) => a
            .iter()
            .map(|x| if x.is_null() { String::new() } else { js_string(Some(x)) })
            .collect::<Vec<_>>()
            .join(","),
        Some(Value::Object(_)) => "[object Object]".into(),
    }
}

/// `String(v ?? '-')`: the text of a table cell.
pub fn or_dash(v: Option<&Value>) -> String {
    match v {
        None | Some(Value::Null) => "-".into(),
        v => js_string(v),
    }
}

/// JavaScript truthiness of a JSON value.
pub fn truthy(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::Number(n)) => n.as_f64().is_some_and(|x| x != 0.0 && !x.is_nan()),
        Some(Value::String(s)) => !s.is_empty(),
        Some(_) => true,
    }
}

/// `Number(x).toFixed(digits)`, `-` for a missing value.
pub fn fixed(v: Option<&Value>, digits: u32) -> String {
    match v {
        None | Some(Value::Null) => "-".into(),
        Some(v) => js::to_fixed(number(v), digits),
    }
}

/// A percentage without decimals (`0.9` is `90`), `-` for a missing value.
pub fn pct(v: Option<&Value>) -> String {
    match v {
        None | Some(Value::Null) => "-".into(),
        Some(v) => js::to_fixed(number(v) * 100.0, 0),
    }
}

/// `Number(v)` of a JSON value.
fn number(v: &Value) -> f64 {
    match v {
        Value::Number(n) => n.as_f64().unwrap_or(f64::NAN),
        Value::Bool(b) => f64::from(u8::from(*b)),
        Value::String(s) => {
            let t = js::trim(s);
            if t.is_empty() { 0.0 } else { t.parse().unwrap_or(f64::NAN) }
        }
        _ => f64::NAN,
    }
}

/// The first `units` UTF-16 units of a text (`String.prototype.slice(0, units)`).
pub fn slice(s: &str, units: usize) -> &str {
    js::truncate_utf16(s, units)
}

/// Days from 1970-01-01 to a date of the proleptic Gregorian calendar (H. Hinnant's algorithm).
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = i64::from(if m > 2 { m - 3 } else { m + 9 });
    let doy = (153 * mp + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Days in a month.
fn month_days(y: i64, m: u32) -> u32 {
    let next = if m == 12 { days_from_civil(y + 1, 1, 1) } else { days_from_civil(y, m + 1, 1) };
    (next - days_from_civil(y, m, 1)) as u32
}

fn digits(s: &str, n: usize) -> Option<u32> {
    (s.len() == n && s.bytes().all(|b| b.is_ascii_digit())).then(|| s.parse().ok()).flatten()
}

/// A date of an option: `YYYY-MM-DD` (00:00 UTC) or an ISO 8601 time with its offset
/// (`2026-05-01T18:30Z`, seconds and fractions optional). The calendar date must exist
/// (`2026-02-31` is refused, not rolled over). Unix milliseconds.
pub fn parse_date(s: &str) -> Option<i64> {
    let (date, time) = match s.split_once('T') {
        Some((d, t)) => (d, Some(t)),
        None => (s, None),
    };
    let mut parts = date.split('-');
    let y = i64::from(digits(parts.next()?, 4)?);
    let m = digits(parts.next()?, 2)?;
    let d = digits(parts.next()?, 2)?;
    if parts.next().is_some() || !(1..=12).contains(&m) || d < 1 || d > month_days(y, m) {
        return None;
    }
    let day_ms = days_from_civil(y, m, d) * DAY_MS;
    let Some(time) = time else { return Some(day_ms) };
    // The offset: Z, or +HH:MM / -HH:MM.
    let (clock, offset_ms) = if let Some(c) = time.strip_suffix('Z') {
        (c, 0)
    } else {
        let at = time.rfind(['+', '-'])?;
        let (c, off) = time.split_at(at);
        let sign = if off.starts_with('-') { -1 } else { 1 };
        let (oh, om) = off[1..].split_once(':')?;
        let (oh, om) = (digits(oh, 2)?, digits(om, 2)?);
        if oh > 23 || om > 59 {
            return None;
        }
        (c, sign * (i64::from(oh) * 3_600_000 + i64::from(om) * 60_000))
    };
    let mut fields = clock.split(':');
    let h = digits(fields.next()?, 2)?;
    let mi = digits(fields.next()?, 2)?;
    let (sec, ms) = match fields.next() {
        None => (0, 0),
        Some(sf) => {
            let (sec, ms) = match sf.split_once('.') {
                Some((sec, frac)) => {
                    if frac.is_empty() || !frac.bytes().all(|b| b.is_ascii_digit()) {
                        return None;
                    }
                    // Milliseconds: the first three digits.
                    let ms = format!("{frac:0<3}")[..3].parse::<i64>().ok()?;
                    (sec, ms)
                }
                None => (sf, 0),
            };
            (digits(sec, 2)?, ms)
        }
    };
    if fields.next().is_some() || mi > 59 || sec > 59 || h > 24 || (h == 24 && (mi, sec, ms) != (0, 0, 0)) {
        return None;
    }
    let t = i64::from(h) * 3_600_000 + i64::from(mi) * 60_000 + i64::from(sec) * 1000 + ms;
    Some(day_ms + t - offset_ms)
}

/// The UTC date of a time as `YYYY-MM-DD`.
#[cfg(test)]
pub fn date_of(ms: i64) -> String {
    let (y, m, d) = crate::log::civil_from_days(ms.div_euclid(DAY_MS));
    format!("{y:04}-{m:02}-{d:02}")
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn times_drop_zero_milliseconds_and_nothing_is_a_dash() {
        assert_eq!(iso(Some(1_800_000_000_000)), "2027-01-15T08:00:00Z");
        assert_eq!(iso(Some(1_800_000_000_123)), "2027-01-15T08:00:00.123Z");
        assert_eq!((iso(None), iso(Some(0))), ("-".into(), "-".into()));
    }

    #[test]
    fn tables_pad_their_columns_and_escape_what_could_forge_lines() {
        let rows = vec![vec!["1".into(), "x\nSanctions\u{9b}\u{202e}".into()], vec!["22".into(), "".into()]];
        assert_eq!(
            table(&["id", "comment"], &rows),
            "id  comment\n--  ----------------------------\n1   x\\u000aSanctions\\u009b\\u202e\n22\n"
        );
        assert_eq!(table(&["id"], &[]), "(none)\n");
    }

    #[test]
    fn values_print_as_javascript_strings() {
        assert_eq!(js_string(None), "undefined");
        assert_eq!(js_string(Some(&json!(3.8))), "3.8");
        assert_eq!(js_string(Some(&json!(2.0))), "2");
        assert_eq!(js_string(Some(&json!(["a", null, 1]))), "a,,1");
        assert_eq!(or_dash(Some(&Value::Null)), "-");
        assert_eq!((fixed(Some(&json!(97.25)), 1), fixed(None, 1)), ("97.3".into(), "-".into()));
        assert_eq!(pct(Some(&json!(0.9))), "90");
    }

    #[test]
    fn dates_are_days_or_times_with_their_offset_and_must_exist() {
        let day = parse_date("2026-09-01").unwrap();
        assert_eq!(day, 1_788_220_800_000);
        assert_eq!(date_of(day), "2026-09-01");
        assert_eq!(parse_date("2026-09-01T12:00Z"), Some(day + 12 * 3_600_000));
        assert_eq!(parse_date("2026-09-01T12:00:30.5Z"), Some(day + 12 * 3_600_000 + 30_500));
        assert_eq!(parse_date("2026-09-01T14:00+02:00"), Some(day + 12 * 3_600_000));
        assert_eq!(parse_date("2026-09-01T10:00:00.000-02:00"), Some(day + 12 * 3_600_000));
        assert_eq!(parse_date("2024-02-29"), Some(1_709_164_800_000));
        for bad in [
            "2026-02-29",
            "2026-02-31",
            "2026-04-31T10:00Z",
            "2026-00-10",
            "2026-06-00",
            "yesterday",
            "2026-9-1",
            "2026-09-01T12:00",
            "2026-09-01T25:00Z",
            "2026-09-01T12:60Z",
            "2026-09-01T12:00+2:00",
            "2026-09-01 ",
        ] {
            assert_eq!(parse_date(bad), None, "{bad}");
        }
    }
}
