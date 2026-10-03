//! JavaScript string semantics the renderer reproduces: `String.prototype.trim` (its whitespace
//! set differs from Rust's), `Number(string)`, and the printable text of a name.

/// Whether `c` is whitespace for JavaScript's `trim` (WhiteSpace and LineTerminator).
pub(crate) fn is_js_whitespace(c: char) -> bool {
    matches!(
        c,
        '\u{9}' | '\u{a}' | '\u{b}' | '\u{c}' | '\u{d}' | ' ' | '\u{a0}' | '\u{1680}' | '\u{2000}'
            ..='\u{200a}' | '\u{2028}' | '\u{2029}' | '\u{202f}' | '\u{205f}' | '\u{3000}' | '\u{feff}'
    )
}

/// `s.trim()` in JavaScript.
pub(crate) fn js_trim(s: &str) -> &str {
    s.trim_matches(is_js_whitespace)
}

/// `s.trimEnd()` in JavaScript.
pub(crate) fn js_trim_end(s: &str) -> &str {
    s.trim_end_matches(is_js_whitespace)
}

/// `s.split(/[\s,]+/)` in JavaScript: the items of a list of numbers separated by runs of
/// whitespace and commas (an empty item at either end when a separator is there; `[""]` for an
/// empty string).
pub(crate) fn split_list(s: &str) -> Vec<&str> {
    let is_sep = |c: char| c == ',' || is_js_whitespace(c);
    let mut items = Vec::new();
    let mut rest = s;
    loop {
        match rest.find(is_sep) {
            Some(i) => {
                items.push(&rest[..i]);
                rest = rest[i..].trim_start_matches(is_sep);
            }
            None => {
                items.push(rest);
                return items;
            }
        }
    }
}

/// `Number(s)` in JavaScript for a string: whitespace around is ignored, an empty string is 0,
/// decimal literals (`1`, `-1.5`, `.5`, `5.`, `1e3`), `Infinity` with a sign, and unsigned `0x`,
/// `0o`, `0b` integers; anything else is NaN.
pub(crate) fn js_number(s: &str) -> f64 {
    let t = js_trim(s);
    if t.is_empty() {
        return 0.0;
    }
    let radix = match t.get(..2) {
        Some("0x" | "0X") => Some(16),
        Some("0o" | "0O") => Some(8),
        Some("0b" | "0B") => Some(2),
        _ => None,
    };
    if let Some(radix) = radix {
        let digits = &t[2..];
        if digits.is_empty() || !digits.chars().all(|c| c.is_digit(radix)) {
            return f64::NAN;
        }
        // Digit by digit, as the specification's mathematical value rounded once at the end
        // would need big integers; the inputs here are small.
        return digits
            .chars()
            .fold(0.0, |v, c| v * f64::from(radix) + f64::from(c.to_digit(radix).unwrap_or(0)));
    }
    let unsigned = t.strip_prefix(['+', '-']).unwrap_or(t);
    if unsigned == "Infinity" {
        return if t.starts_with('-') { f64::NEG_INFINITY } else { f64::INFINITY };
    }
    if is_decimal_literal(unsigned) { t.parse::<f64>().unwrap_or(f64::NAN) } else { f64::NAN }
}

/// `digits [. digits] | . digits`, then an optional exponent (`StrUnsignedDecimalLiteral`).
fn is_decimal_literal(s: &str) -> bool {
    let b = s.as_bytes();
    let digits = |mut i: usize| {
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        i
    };
    let int_end = digits(0);
    let mut i = int_end;
    let mut mantissa = int_end > 0;
    if i < b.len() && b[i] == b'.' {
        let frac_end = digits(i + 1);
        mantissa |= frac_end > i + 1;
        i = frac_end;
    }
    if !mantissa {
        return false;
    }
    if i < b.len() && (b[i] == b'e' || b[i] == b'E') {
        i += 1;
        if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
            i += 1;
        }
        let exp_end = digits(i);
        if exp_end == i {
            return false;
        }
        i = exp_end;
    }
    i == b.len()
}

/// The printable text of a name or a sentence: the characters the GIF fonts have (printable
/// ASCII, the one-half sign and the middle dot) kept, any other character shown as '?', at most
/// `max_len` characters, spaces at both ends removed (render.js `cleanText`).
pub(crate) fn clean_text(s: &str, max_len: usize) -> String {
    let mut out = String::new();
    let mut len = 0;
    for c in s.chars() {
        out.push(if (' '..='~').contains(&c) || c == '\u{bd}' || c == '\u{b7}' { c } else { '?' });
        len += 1;
        if len >= max_len {
            break;
        }
    }
    out.trim_matches(' ').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trims_like_javascript() {
        assert_eq!(js_trim("\u{feff} a b\u{3000}\n"), "a b");
        assert_eq!(js_trim("\u{85}x"), "\u{85}x");
        assert_eq!(js_trim_end("ab \t"), "ab");
    }

    #[test]
    fn splits_lists_like_javascript() {
        assert_eq!(split_list("0 0,45  45"), ["0", "0", "45", "45"]);
        assert_eq!(split_list(",1 ,"), ["", "1", ""]);
        assert_eq!(split_list(""), [""]);
    }

    #[test]
    fn numbers_like_javascript() {
        for (s, v) in
            [("", 0.0), ("  ", 0.0), ("1.5", 1.5), (" -2 ", -2.0), (".5", 0.5), ("5.", 5.0), ("+1e2", 100.0)]
        {
            assert_eq!(js_number(s), v, "{s:?}");
        }
        assert_eq!(js_number("0x1F"), 31.0);
        assert_eq!(js_number("-Infinity"), f64::NEG_INFINITY);
        for s in ["abc", "1e", "e5", ".", "-0x1", "inf", "NaN", "1 2", "0x"] {
            assert!(js_number(s).is_nan(), "{s:?}");
        }
    }

    #[test]
    fn clean_text_keeps_the_font_characters() {
        assert_eq!(clean_text("  Carlsen ½·é中 ", 64), "Carlsen ½·??");
        assert_eq!(clean_text("abcdef", 3), "abc");
        assert_eq!(clean_text("\u{1F600}x", 64), "?x");
        assert_eq!(clean_text("  ", 64), "");
    }
}
