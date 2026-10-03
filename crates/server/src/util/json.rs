//! JSON text exactly as JavaScript's `JSON.stringify` writes it.
//!
//! `serde_json` prints an `f64` holding an integer as `1.0` and switches to exponent notation at
//! other thresholds (`1e16`), where JavaScript prints `1` and `10000000000000000`. Answers and log
//! lines that the former Node.js server produced keep its number format by going through these
//! writers. Key order is the insertion order of the `serde_json::Map` (`preserve_order`).
//! Strings are escaped like `JSON.stringify`: `"`, `\`, `\b \f \n \r \t`, other control
//! characters as `\u00xx`, everything else (non-ASCII included) as is.

use std::fmt::Write as _;

use serde_json::{Number, Value};

use super::js;

/// `JSON.stringify(value)`.
pub fn to_string(value: &Value) -> String {
    let mut out = String::with_capacity(128);
    write_value(&mut out, value, None, 0);
    out
}

/// `JSON.stringify(value, null, 2)`: two-space indentation, `[]` and `{}` for empty containers.
pub fn to_string_pretty(value: &Value) -> String {
    let mut out = String::with_capacity(256);
    write_value(&mut out, value, Some(2), 0);
    out
}

/// Appends `JSON.stringify(value)` to `out`.
pub fn write(out: &mut String, value: &Value) {
    write_value(out, value, None, 0);
}

/// Appends a JSON string literal (with its quotes) to `out`.
pub fn write_str(out: &mut String, s: &str) {
    out.reserve(s.len() + 2);
    out.push('"');
    let mut start = 0;
    for (i, c) in s.char_indices() {
        let escape = match c {
            '"' => "\\\"",
            '\\' => "\\\\",
            '\u{8}' => "\\b",
            '\u{c}' => "\\f",
            '\n' => "\\n",
            '\r' => "\\r",
            '\t' => "\\t",
            c if (c as u32) < 0x20 => "",
            _ => continue,
        };
        out.push_str(&s[start..i]);
        if escape.is_empty() {
            let _ = write!(out, "\\u{:04x}", c as u32);
        } else {
            out.push_str(escape);
        }
        start = i + c.len_utf8();
    }
    out.push_str(&s[start..]);
    out.push('"');
}

/// Appends a number the way JavaScript prints it (integers without a fraction).
pub fn write_number(out: &mut String, n: &Number) {
    if let Some(i) = n.as_i64() {
        let _ = write!(out, "{i}");
    } else if let Some(u) = n.as_u64() {
        let _ = write!(out, "{u}");
    } else {
        // serde_json numbers are always finite.
        out.push_str(&js::number_to_string(n.as_f64().unwrap_or(0.0)));
    }
}

fn newline(out: &mut String, indent: Option<usize>, level: usize) {
    if let Some(width) = indent {
        out.push('\n');
        out.extend(std::iter::repeat_n(' ', width * level));
    }
}

fn write_value(out: &mut String, value: &Value, indent: Option<usize>, level: usize) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => write_number(out, n),
        Value::String(s) => write_str(out, s),
        Value::Array(items) => {
            if items.is_empty() {
                out.push_str("[]");
                return;
            }
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                newline(out, indent, level + 1);
                write_value(out, item, indent, level + 1);
            }
            newline(out, indent, level);
            out.push(']');
        }
        Value::Object(map) => {
            if map.is_empty() {
                out.push_str("{}");
                return;
            }
            out.push('{');
            for (i, (k, v)) in map.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                newline(out, indent, level + 1);
                write_str(out, k);
                out.push(':');
                if indent.is_some() {
                    out.push(' ');
                }
                write_value(out, v, indent, level + 1);
            }
            newline(out, indent, level);
            out.push('}');
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn numbers_print_like_javascript() {
        let v = json!({"a": 1.0, "b": 1.5, "c": 1e21, "d": -3, "e": u64::MAX, "f": 1e16, "g": 0.1});
        assert_eq!(
            to_string(&v),
            r#"{"a":1,"b":1.5,"c":1e+21,"d":-3,"e":18446744073709551615,"f":10000000000000000,"g":0.1}"#
        );
    }

    #[test]
    fn strings_are_escaped_like_json_stringify() {
        let v = json!("q\"b\\s\u{8}\u{c}\n\r\t\u{1}\u{1f}\u{7f}\u{2028}é😀/");
        assert_eq!(to_string(&v), "\"q\\\"b\\\\s\\b\\f\\n\\r\\t\\u0001\\u001f\u{7f}\u{2028}é😀/\"");
    }

    #[test]
    fn pretty_matches_json_stringify_with_two_spaces() {
        let v = json!({"a": [], "b": {}, "c": [1, {"d": "x"}], "e": 1.5, "f": null, "g": true});
        let want = "{\n  \"a\": [],\n  \"b\": {},\n  \"c\": [\n    1,\n    {\n      \"d\": \"x\"\n    }\n  ],\n  \"e\": 1.5,\n  \"f\": null,\n  \"g\": true\n}";
        assert_eq!(to_string_pretty(&v), want);
        assert_eq!(to_string(&v), r#"{"a":[],"b":{},"c":[1,{"d":"x"}],"e":1.5,"f":null,"g":true}"#);
    }

    #[test]
    fn key_order_is_insertion_order() {
        let mut m = serde_json::Map::new();
        m.insert("z".into(), json!(1));
        m.insert("a".into(), json!(2));
        assert_eq!(to_string(&Value::Object(m)), r#"{"z":1,"a":2}"#);
    }
}
