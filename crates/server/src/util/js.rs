//! JavaScript-compatible number formatting, rounding and string helpers.
//!
//! The HTTP API, the logs, the metrics and `check-config` print what the former Node.js server
//! printed. These functions reproduce the ECMAScript algorithms the Node code relied on:
//! `Number.prototype.toString()` (shortest round trip, exponent from 1e21 and below 1e-6),
//! `Math.round` (ties toward +infinity), `Number.prototype.toFixed` (ties away from zero on the
//! exact binary value), `String.prototype.trim` (the ECMAScript white space set) and string
//! lengths in UTF-16 code units (`String.prototype.length`).

/// Formats `v` like JavaScript's `String(v)`: `1`, `0.25`, `1e+21`, `1e-7`, `NaN`, `Infinity`.
///
/// The digits are the shortest that read back as `v`, as ECMAScript requires.
pub fn number_to_string(v: f64) -> String {
    if v.is_nan() {
        return "NaN".to_string();
    }
    if v == 0.0 {
        return "0".to_string();
    }
    if v.is_infinite() {
        return if v > 0.0 { "Infinity" } else { "-Infinity" }.to_string();
    }
    let mut out = String::with_capacity(24);
    if v < 0.0 {
        out.push('-');
    }
    let abs = v.abs();
    // Integers below 2^53 print exactly: their shortest round-trip digits are the integer itself.
    if abs < 9_007_199_254_740_992.0 && abs.fract() == 0.0 {
        out.push_str(&(abs as u64).to_string());
        return out;
    }
    // `{:e}` gives the shortest round-trip digits as `d[.ddd]e<exp>`.
    let sci = format!("{abs:e}");
    let (mantissa, exp) = sci.split_once('e').expect("LowerExp output always has an exponent");
    let exp: i32 = exp.parse().expect("LowerExp exponent is an integer");
    let digits: String = mantissa.chars().filter(|c| *c != '.').collect();
    let k = digits.len() as i32;
    let n = exp + 1;
    if k <= n && n <= 21 {
        out.push_str(&digits);
        out.extend(std::iter::repeat_n('0', (n - k) as usize));
    } else if 0 < n && n <= 21 {
        out.push_str(&digits[..n as usize]);
        out.push('.');
        out.push_str(&digits[n as usize..]);
    } else if -6 < n && n <= 0 {
        out.push_str("0.");
        out.extend(std::iter::repeat_n('0', (-n) as usize));
        out.push_str(&digits);
    } else {
        let e = n - 1;
        out.push_str(&digits[..1]);
        if k > 1 {
            out.push('.');
            out.push_str(&digits[1..]);
        }
        out.push('e');
        out.push(if e >= 0 { '+' } else { '-' });
        out.push_str(&e.unsigned_abs().to_string());
    }
    out
}

/// JavaScript's `Math.round`: the nearest integer, ties toward +infinity (`-2.5` gives `-2`).
pub fn round(x: f64) -> f64 {
    if !x.is_finite() || x.fract() == 0.0 {
        return x;
    }
    let floor = x.floor();
    let r = if x - floor >= 0.5 { floor + 1.0 } else { floor };
    if r == 0.0 && x < 0.0 { -0.0 } else { r }
}

/// [`round`] as an integer (saturating outside the `i64` range, `NaN` gives 0).
pub fn round_i64(x: f64) -> i64 {
    round(x) as i64
}

/// JavaScript's `Number.prototype.toFixed(digits)`: `(2.5).toFixed(0)` is `"3"`,
/// `(0.125).toFixed(2)` is `"0.13"`, `(-0.0001).toFixed(2)` is `"-0.00"`, and from 1e21 the
/// result is `String(x)`.
///
/// # Panics
/// When `digits` is above 100 (a `RangeError` in JavaScript).
pub fn to_fixed(x: f64, digits: u32) -> String {
    assert!(digits <= 100, "toFixed() digits must be between 0 and 100");
    if !x.is_finite() {
        return number_to_string(x);
    }
    let sign = if x < 0.0 { "-" } else { "" };
    let abs = x.abs();
    if abs >= 1e21 {
        return format!("{sign}{}", number_to_string(abs));
    }
    // Rust rounds the exact binary value half to even; ECMAScript picks the larger candidate on
    // an exact tie. Off a tie both agree; on one, the next double up rounds the same way as JS.
    let value = if is_decimal_tie(abs, digits) { abs.next_up() } else { abs };
    format!("{sign}{value:.prec$}", prec = digits as usize)
}

/// Whether `x * 10^digits` is exactly halfway between two integers (`x` positive and finite).
fn is_decimal_tie(x: f64, digits: u32) -> bool {
    let bits = x.to_bits();
    let exp_bits = ((bits >> 52) & 0x7ff) as i64;
    let fraction = bits & ((1u64 << 52) - 1);
    let (mantissa, exp) =
        if exp_bits == 0 { (fraction, -1074) } else { (fraction | (1u64 << 52), exp_bits - 1075) };
    if mantissa == 0 {
        return false;
    }
    // x * 10^d * 2 = mantissa * 5^d * 2^(exp + d + 1) is an odd integer exactly when the power
    // of two is negative and cancels every trailing zero bit of the mantissa (5^d is odd).
    let shift = -(exp + i64::from(digits) + 1);
    shift >= 0 && i64::from(mantissa.trailing_zeros()) == shift
}

/// Whether `c` is white space for `String.prototype.trim` (ECMAScript WhiteSpace and
/// LineTerminator: unlike `char::is_whitespace`, U+FEFF is included and U+0085 is not).
pub fn is_whitespace(c: char) -> bool {
    matches!(
        c,
        '\u{9}' | '\u{a}' | '\u{b}' | '\u{c}' | '\u{d}' | ' ' | '\u{a0}' | '\u{1680}' | '\u{2000}'
            ..='\u{200a}' | '\u{2028}' | '\u{2029}' | '\u{202f}' | '\u{205f}' | '\u{3000}' | '\u{feff}'
    )
}

/// `String.prototype.trim`.
pub fn trim(s: &str) -> &str {
    s.trim_matches(is_whitespace)
}

/// `String.prototype.trimStart`.
pub fn trim_start(s: &str) -> &str {
    s.trim_start_matches(is_whitespace)
}

/// `String.prototype.trimEnd`.
pub fn trim_end(s: &str) -> &str {
    s.trim_end_matches(is_whitespace)
}

/// Length in UTF-16 code units, JavaScript's `String.prototype.length`.
pub fn utf16_len(s: &str) -> usize {
    s.chars().map(char::len_utf16).sum()
}

/// The longest prefix of `s` that holds at most `max_units` UTF-16 code units, cut on a character
/// boundary (where JavaScript's `slice(0, n)` would split a surrogate pair, the whole character
/// is left out).
pub fn truncate_utf16(s: &str, max_units: usize) -> &str {
    let mut units = 0;
    for (i, c) in s.char_indices() {
        units += c.len_utf16();
        if units > max_units {
            return &s[..i];
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn number_to_string_matches_javascript() {
        let cases: &[(f64, &str)] = &[
            (1.0, "1"),
            (0.25, "0.25"),
            (1e21, "1e+21"),
            (1e-7, "1e-7"),
            (1.5e-7, "1.5e-7"),
            (123456789012345680000.0, "123456789012345680000"),
            (0.000001, "0.000001"),
            (f64::MAX, "1.7976931348623157e+308"),
            (5e-324, "5e-324"),
            (-0.0, "0"),
            (0.1 + 0.2, "0.30000000000000004"),
            (100.0, "100"),
            (1e20, "100000000000000000000"),
            (12.345678, "12.345678"),
            (9007199254740992.0, "9007199254740992"),
            (-1.5e300, "-1.5e+300"),
            (-42.0, "-42"),
            (f64::NAN, "NaN"),
            (f64::INFINITY, "Infinity"),
            (f64::NEG_INFINITY, "-Infinity"),
            (0.1, "0.1"),
            (123.456, "123.456"),
        ];
        for (v, want) in cases {
            assert_eq!(number_to_string(*v), *want, "{v:e}");
        }
    }

    #[test]
    fn round_matches_math_round() {
        let cases: &[(f64, f64)] = &[
            (0.5, 1.0),
            (1.5, 2.0),
            (2.5, 3.0),
            (-0.5, -0.0),
            (-1.5, -1.0),
            (-2.5, -2.0),
            (0.49999999999999994, 0.0),
            (-0.49999999999999994, -0.0),
            (4503599627370495.5, 4503599627370496.0),
            (1.4999999999999998, 1.0),
            (7.0, 7.0),
        ];
        for (x, want) in cases {
            let r = round(*x);
            assert_eq!(r, *want, "Math.round({x})");
            assert_eq!(r.is_sign_negative(), want.is_sign_negative(), "sign of Math.round({x})");
        }
        assert_eq!(round_i64(2.5), 3);
        assert!(round(f64::NAN).is_nan());
    }

    #[test]
    fn to_fixed_matches_javascript() {
        let cases: &[(f64, u32, &str)] = &[
            (0.125, 2, "0.13"),
            (0.25, 1, "0.3"),
            (1.005, 2, "1.00"),
            (2.5, 0, "3"),
            (-2.5, 0, "-3"),
            (1e21, 2, "1e+21"),
            (0.000001, 3, "0.000"),
            (123.456, 1, "123.5"),
            (-0.0001, 2, "-0.00"),
            (1.45, 1, "1.4"),
            (8.345, 2, "8.35"),
            (-0.0, 2, "0.00"),
            (12.0, 2, "12.00"),
        ];
        for (x, d, want) in cases {
            assert_eq!(to_fixed(*x, *d), *want, "({x}).toFixed({d})");
        }
    }

    #[test]
    fn trim_uses_the_ecmascript_white_space_set() {
        assert_eq!(trim(" \u{85}x\u{feff} "), "\u{85}x");
        assert_eq!(trim("\u{feff}\u{a0}\u{1680}\u{2000}\u{200a}\u{200b}x"), "\u{200b}x");
        assert_eq!(trim("\u{202f}\u{205f}\u{3000}x\u{b}\u{c}\u{2028}\u{2029}"), "x");
        assert_eq!(trim_start("  a "), "a ");
        assert_eq!(trim_end("  a "), "  a");
    }

    #[test]
    fn utf16_lengths_and_cuts() {
        assert_eq!(utf16_len("abc"), 3);
        assert_eq!(utf16_len("é€"), 2);
        assert_eq!(utf16_len("😀"), 2);
        assert_eq!(truncate_utf16("ab😀c", 3), "ab");
        assert_eq!(truncate_utf16("ab😀c", 4), "ab😀");
        assert_eq!(truncate_utf16("abc", 10), "abc");
    }
}
