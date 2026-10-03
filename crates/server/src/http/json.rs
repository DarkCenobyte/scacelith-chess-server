//! JSON as JavaScript writes and reads it: `JSON.stringify` output (compact, keys in insertion
//! order, numbers formatted like `Number.prototype.toString`), `JSON.parse` input through
//! serde_json, and the key order of `Object.keys` (integer-like keys first, ascending).
//!
//! Strings are escaped as `JSON.stringify` does (`\"`, `\\`, `\b`, `\f`, `\n`, `\r`, `\t`, other
//! control characters as lower-case `\u00xx`, nothing else); a double is written `1` rather than
//! `1.0`, with the JavaScript exponent rules (`1e+21`, `1e-7`, `0.000001`).

use std::fmt::Write as _;

use serde_json::{Map, Value};

/// Serializes a value as `JSON.stringify` does.
pub fn stringify(value: &Value) -> String {
    let mut out = String::with_capacity(128);
    write_value(&mut out, value);
    out
}

/// Serializes a value as `JSON.stringify` does, into bytes.
pub fn to_vec(value: &Value) -> Vec<u8> {
    stringify(value).into_bytes()
}

fn write_value(out: &mut String, value: &Value) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                let _ = write!(out, "{i}");
            } else if let Some(u) = n.as_u64() {
                let _ = write!(out, "{u}");
            } else {
                out.push_str(&js_number(n.as_f64().unwrap_or(f64::NAN)));
            }
        }
        Value::String(s) => write_string(out, s),
        Value::Array(items) => {
            out.push('[');
            for (i, v) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_value(out, v);
            }
            out.push(']');
        }
        Value::Object(map) => {
            out.push('{');
            for (i, (k, v)) in map.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_string(out, k);
                out.push(':');
                write_value(out, v);
            }
            out.push('}');
        }
    }
}

/// Appends a JSON string literal as `JSON.stringify` writes it.
pub fn write_string(out: &mut String, s: &str) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str(r#"\""#),
            '\\' => out.push_str(r"\\"),
            '\u{8}' => out.push_str(r"\b"),
            '\u{c}' => out.push_str(r"\f"),
            '\n' => out.push_str(r"\n"),
            '\r' => out.push_str(r"\r"),
            '\t' => out.push_str(r"\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// A number as JavaScript's `String(n)` writes it: the shortest digits that read back the same
/// double, integers without a fraction, exponent form below 1e-6 and from 1e21 (`1e+21`,
/// `1.5e-7`), `NaN`, `Infinity`, and `0` for negative zero.
pub fn js_number(v: f64) -> String {
    if v.is_nan() {
        return "NaN".into();
    }
    if v.is_infinite() {
        return if v > 0.0 { "Infinity".into() } else { "-Infinity".into() };
    }
    if v == 0.0 {
        return "0".into();
    }
    // Rust's `{:e}` gives the shortest round-trip digits: "d.ddde-7".
    let sci = format!("{:e}", v.abs());
    let (mantissa, exp) = sci.split_once('e').expect("LowerExp writes an exponent");
    let exp: i32 = exp.parse().expect("LowerExp writes an integer exponent");
    let digits: String = mantissa.chars().filter(|c| *c != '.').collect();
    let k = digits.len() as i32;
    let n = exp + 1;
    let mut out = String::with_capacity(24);
    if v < 0.0 {
        out.push('-');
    }
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
        out.push_str(&digits[..1]);
        if k > 1 {
            out.push('.');
            out.push_str(&digits[1..]);
        }
        out.push('e');
        out.push(if n > 0 { '+' } else { '-' });
        out.push_str(&(n - 1).abs().to_string());
    }
    out
}

/// The body is not JSON (`JSON.parse` would throw).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidJson;

impl std::fmt::Display for InvalidJson {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("invalid JSON")
    }
}

impl std::error::Error for InvalidJson {}

/// Parses a JSON text as `JSON.parse` does, where serde_json agrees: duplicate keys keep the
/// last value at the first key's place. Documented differences: a number beyond the double range
/// (`1e400`), nesting deeper than 128 levels and lone surrogate escapes are refused here, where
/// JavaScript reads `Infinity`, nests on, or keeps the lone surrogate.
pub fn parse(text: &str) -> Result<Value, InvalidJson> {
    serde_json::from_str(text).map_err(|_| InvalidJson)
}

/// Whether `key` is an array index as JavaScript orders object keys: the canonical decimal form
/// of an integer from 0 to 2^32 - 2.
fn is_array_index(key: &str) -> bool {
    if key.is_empty() || key.len() > 10 || !key.bytes().all(|b| b.is_ascii_digit()) {
        return false;
    }
    if key.len() > 1 && key.starts_with('0') {
        return false;
    }
    key.parse::<u64>().is_ok_and(|n| n < u64::from(u32::MAX))
}

/// The keys of an object in the order of `Object.keys`: array indexes first, ascending, then the
/// other keys in insertion order.
pub fn js_keys(map: &Map<String, Value>) -> Vec<&str> {
    let mut index: Vec<(u64, &str)> = Vec::new();
    let mut other: Vec<&str> = Vec::new();
    for k in map.keys() {
        if is_array_index(k) {
            index.push((k.parse().expect("checked digits"), k));
        } else {
            other.push(k);
        }
    }
    index.sort_unstable_by_key(|(n, _)| *n);
    index.into_iter().map(|(_, k)| k).chain(other).collect()
}

/// Whether a JSON number is an integer as `Number.isSafeInteger` sees it (`8080.0` is one).
pub fn safe_integer(v: &Value) -> Option<i64> {
    let n = v.as_number()?;
    if let Some(i) = n.as_i64() {
        return (i.unsigned_abs() <= MAX_SAFE_INTEGER as u64).then_some(i);
    }
    if n.is_u64() {
        return None;
    }
    let f = n.as_f64()?;
    (f.is_finite() && f.fract() == 0.0 && f.abs() <= MAX_SAFE_INTEGER as f64).then_some(f as i64)
}

/// `Number.MAX_SAFE_INTEGER`.
pub const MAX_SAFE_INTEGER: i64 = (1 << 53) - 1;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn numbers_print_like_javascript() {
        let cases: [(f64, &str); 18] = [
            (1.0, "1"),
            (-1.0, "-1"),
            (1.5, "1.5"),
            (0.1, "0.1"),
            (100.0, "100"),
            (1e21, "1e+21"),
            (1.5e21, "1.5e+21"),
            (123456789012345680000.0, "123456789012345680000"),
            (1e-6, "0.000001"),
            (1e-7, "1e-7"),
            (1.5e-7, "1.5e-7"),
            (0.000123, "0.000123"),
            (-0.0, "0"),
            (2.5e-324, "5e-324"),
            (f64::MAX, "1.7976931348623157e+308"),
            (1234.5678, "1234.5678"),
            (9007199254740993.0, "9007199254740992"),
            (0.1 + 0.2, "0.30000000000000004"),
        ];
        for (v, s) in cases {
            assert_eq!(js_number(v), s, "{v:e}");
        }
        assert_eq!(js_number(f64::NAN), "NaN");
        assert_eq!(js_number(f64::NEG_INFINITY), "-Infinity");
    }

    #[test]
    fn stringify_matches_json_stringify() {
        let v = json!({"b": 1, "a": [1.0, 2.5, null, true], "s": "\"\\\u{8}\u{c}\n\r\t\u{1}\u{7f}/é\u{2028}", "n": -0.0});
        assert_eq!(
            stringify(&v),
            "{\"b\":1,\"a\":[1,2.5,null,true],\"s\":\"\\\"\\\\\\b\\f\\n\\r\\t\\u0001\u{7f}/é\u{2028}\",\"n\":0}"
        );
        assert_eq!(stringify(&json!(1e21)), "1e+21");
        assert_eq!(stringify(&json!(u64::MAX)), "18446744073709551615");
    }

    #[test]
    fn parse_keeps_the_last_duplicate_at_the_first_place() {
        let v = parse(r#"{"a":1,"b":2,"a":3}"#).expect("valid");
        assert_eq!(stringify(&v), r#"{"a":3,"b":2}"#);
        assert!(parse("{bad").is_err());
        assert!(parse(" [1] ").is_ok());
        assert!(parse("[1] x").is_err());
    }

    #[test]
    fn object_keys_put_indexes_first() {
        let v = parse(r#"{"b":1,"10":2,"a":3,"2":4,"01":5,"4294967295":6,"4294967294":7}"#).expect("valid");
        let Value::Object(m) = v else { panic!("an object") };
        assert_eq!(js_keys(&m), ["2", "10", "4294967294", "b", "a", "01", "4294967295"]);
    }

    #[test]
    fn safe_integers_include_integral_doubles() {
        assert_eq!(safe_integer(&json!(8080.0)), Some(8080));
        assert_eq!(safe_integer(&json!(-0.0)), Some(0));
        assert_eq!(safe_integer(&json!(1.5)), None);
        assert_eq!(safe_integer(&json!(9007199254740991_i64)), Some(9007199254740991));
        assert_eq!(safe_integer(&json!(9007199254740992_i64)), None);
        assert_eq!(safe_integer(&json!(u64::MAX)), None);
        assert_eq!(safe_integer(&json!("3")), None);
    }
}
