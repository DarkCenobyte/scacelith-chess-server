//! A JSON reader that keeps what a byte-for-byte comparison needs and `serde_json` drops: the
//! key order of objects, the exact text of numbers and the escaping of strings. The comparison
//! of two answers walks two such trees ([`crate::diff`]), after the volatile values were
//! replaced by markers ([`crate::normalize`]).

use std::fmt::Write as _;

/// A JSON value with its source text where it matters.
#[derive(Clone, Debug, PartialEq)]
pub enum J {
    /// `null`.
    Null,
    /// `true` or `false`.
    Bool(bool),
    /// A number, as written (`1`, `1.5`, `1e3`...).
    Num(String),
    /// A string: its value and its raw text between the quotes (escapes as written).
    Str(String, String),
    /// An array.
    Arr(Vec<J>),
    /// An object, members in their order.
    Obj(Vec<(String, J)>),
    /// A volatile value replaced by a label (a token, an id): equal when the labels are equal.
    Mask(String),
    /// A volatile number compared with a tolerance (a time, a delay): equal when the two values
    /// are at most `tol` apart. `label` names the rule for the report.
    Approx { label: &'static str, value: f64, tol: f64 },
}

impl J {
    /// The member `key` of an object.
    pub fn get(&self, key: &str) -> Option<&J> {
        match self {
            J::Obj(members) => members.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    /// The string value, if this is a string.
    pub fn as_str(&self) -> Option<&str> {
        match self {
            J::Str(s, _) => Some(s),
            _ => None,
        }
    }

    /// The number as a `u64`, if this is a non-negative integer.
    pub fn as_u64(&self) -> Option<u64> {
        match self {
            J::Num(n) => n.parse().ok(),
            _ => None,
        }
    }

    /// The number as an `f64`.
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            J::Num(n) => n.parse().ok(),
            _ => None,
        }
    }

    /// The items, if this is an array.
    pub fn as_array(&self) -> Option<&[J]> {
        match self {
            J::Arr(items) => Some(items),
            _ => None,
        }
    }

    /// Follows a path of object keys and array indexes (`"user.id"`, `"games.0.id"`).
    pub fn at(&self, path: &str) -> Option<&J> {
        let mut cur = self;
        for part in path.split('.').filter(|p| !p.is_empty()) {
            cur = match cur {
                J::Arr(items) => items.get(part.parse::<usize>().ok()?)?,
                _ => cur.get(part)?,
            };
        }
        Some(cur)
    }

    /// The value as text for a variable: a string without quotes, a number as written.
    pub fn scalar_text(&self) -> Option<String> {
        match self {
            J::Str(s, _) => Some(s.clone()),
            J::Num(n) => Some(n.clone()),
            J::Bool(b) => Some(b.to_string()),
            _ => None,
        }
    }

    /// Compact JSON text (markers as `<label>` strings), for the report.
    pub fn to_text(&self) -> String {
        let mut out = String::new();
        self.write(&mut out);
        out
    }

    fn write(&self, out: &mut String) {
        match self {
            J::Null => out.push_str("null"),
            J::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            J::Num(n) => out.push_str(n),
            J::Str(_, raw) => {
                out.push('"');
                out.push_str(raw);
                out.push('"');
            }
            J::Arr(items) => {
                out.push('[');
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    item.write(out);
                }
                out.push(']');
            }
            J::Obj(members) => {
                out.push('{');
                for (i, (k, v)) in members.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    out.push('"');
                    out.push_str(&escape(k));
                    out.push_str("\":");
                    v.write(out);
                }
                out.push('}');
            }
            J::Mask(label) => {
                let _ = write!(out, "\"<{label}>\"");
            }
            J::Approx { label, value, .. } => {
                let _ = write!(out, "\"<{label}:{value}>\"");
            }
        }
    }
}

/// JSON escaping as `JSON.stringify` does it (quote, backslash, control characters).
pub fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out
}

/// Parses a whole JSON text (surrounding whitespace allowed).
pub fn parse(text: &[u8]) -> Result<J, String> {
    let text = std::str::from_utf8(text).map_err(|_| "not UTF-8".to_string())?;
    let mut p = Parser { s: text.as_bytes(), src: text, pos: 0 };
    p.ws();
    let v = p.value(0)?;
    p.ws();
    if p.pos != p.s.len() {
        return Err(format!("trailing data at byte {}", p.pos));
    }
    Ok(v)
}

/// Whether the text has no whitespace outside strings (the compact form both servers write).
pub fn is_compact(text: &[u8]) -> bool {
    let mut in_str = false;
    let mut escaped = false;
    for &b in text {
        if in_str {
            if escaped {
                escaped = false;
            } else if b == b'\\' {
                escaped = true;
            } else if b == b'"' {
                in_str = false;
            }
        } else if b == b'"' {
            in_str = true;
        } else if b.is_ascii_whitespace() {
            return false;
        }
    }
    true
}

struct Parser<'a> {
    s: &'a [u8],
    src: &'a str,
    pos: usize,
}

impl Parser<'_> {
    fn ws(&mut self) {
        while self.pos < self.s.len() && matches!(self.s[self.pos], b' ' | b'\t' | b'\n' | b'\r') {
            self.pos += 1;
        }
    }

    fn err(&self, what: &str) -> String {
        format!("{what} at byte {}", self.pos)
    }

    fn value(&mut self, depth: usize) -> Result<J, String> {
        if depth > 64 {
            return Err(self.err("nesting too deep"));
        }
        match self.s.get(self.pos) {
            Some(b'{') => self.object(depth),
            Some(b'[') => self.array(depth),
            Some(b'"') => {
                let (value, raw) = self.string()?;
                Ok(J::Str(value, raw))
            }
            Some(b't') => self.literal("true", J::Bool(true)),
            Some(b'f') => self.literal("false", J::Bool(false)),
            Some(b'n') => self.literal("null", J::Null),
            Some(b'-' | b'0'..=b'9') => self.number(),
            _ => Err(self.err("unexpected character")),
        }
    }

    fn literal(&mut self, word: &str, v: J) -> Result<J, String> {
        if self.s[self.pos..].starts_with(word.as_bytes()) {
            self.pos += word.len();
            Ok(v)
        } else {
            Err(self.err("invalid literal"))
        }
    }

    fn number(&mut self) -> Result<J, String> {
        let start = self.pos;
        if self.s[self.pos] == b'-' {
            self.pos += 1;
        }
        let digits = |p: &mut Self| {
            let s = p.pos;
            while p.pos < p.s.len() && p.s[p.pos].is_ascii_digit() {
                p.pos += 1;
            }
            p.pos > s
        };
        if !digits(self) {
            return Err(self.err("invalid number"));
        }
        if self.s.get(self.pos) == Some(&b'.') {
            self.pos += 1;
            if !digits(self) {
                return Err(self.err("invalid fraction"));
            }
        }
        if matches!(self.s.get(self.pos), Some(b'e' | b'E')) {
            self.pos += 1;
            if matches!(self.s.get(self.pos), Some(b'+' | b'-')) {
                self.pos += 1;
            }
            if !digits(self) {
                return Err(self.err("invalid exponent"));
            }
        }
        Ok(J::Num(self.src[start..self.pos].to_string()))
    }

    fn hex4(&mut self) -> Result<u32, String> {
        let h = self.src.get(self.pos..self.pos + 4).ok_or_else(|| self.err("short \\u escape"))?;
        let v = u32::from_str_radix(h, 16).map_err(|_| self.err("invalid \\u escape"))?;
        self.pos += 4;
        Ok(v)
    }

    fn string(&mut self) -> Result<(String, String), String> {
        self.pos += 1;
        let start = self.pos;
        let mut out = String::new();
        loop {
            let b = *self.s.get(self.pos).ok_or_else(|| self.err("unterminated string"))?;
            match b {
                b'"' => {
                    let raw = self.src[start..self.pos].to_string();
                    self.pos += 1;
                    return Ok((out, raw));
                }
                b'\\' => {
                    self.pos += 1;
                    let e = *self.s.get(self.pos).ok_or_else(|| self.err("unterminated escape"))?;
                    self.pos += 1;
                    match e {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{8}'),
                        b'f' => out.push('\u{c}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => {
                            let hi = self.hex4()?;
                            let c =
                                if (0xD800..0xDC00).contains(&hi) && self.s[self.pos..].starts_with(b"\\u") {
                                    self.pos += 2;
                                    let lo = self.hex4()?;
                                    char::from_u32(
                                        0x10000 + ((hi - 0xD800) << 10) + (lo.wrapping_sub(0xDC00) & 0x3FF),
                                    )
                                } else {
                                    char::from_u32(hi)
                                };
                            out.push(c.unwrap_or('\u{FFFD}'));
                        }
                        _ => return Err(self.err("invalid escape")),
                    }
                }
                0..=0x1f => return Err(self.err("control character in a string")),
                _ => {
                    let c = self.src[self.pos..].chars().next().expect("a character at a char boundary");
                    out.push(c);
                    self.pos += c.len_utf8();
                }
            }
        }
    }

    fn array(&mut self, depth: usize) -> Result<J, String> {
        self.pos += 1;
        let mut items = Vec::new();
        self.ws();
        if self.s.get(self.pos) == Some(&b']') {
            self.pos += 1;
            return Ok(J::Arr(items));
        }
        loop {
            self.ws();
            items.push(self.value(depth + 1)?);
            self.ws();
            match self.s.get(self.pos) {
                Some(b',') => self.pos += 1,
                Some(b']') => {
                    self.pos += 1;
                    return Ok(J::Arr(items));
                }
                _ => return Err(self.err("expected , or ]")),
            }
        }
    }

    fn object(&mut self, depth: usize) -> Result<J, String> {
        self.pos += 1;
        let mut members = Vec::new();
        self.ws();
        if self.s.get(self.pos) == Some(&b'}') {
            self.pos += 1;
            return Ok(J::Obj(members));
        }
        loop {
            self.ws();
            if self.s.get(self.pos) != Some(&b'"') {
                return Err(self.err("expected a key"));
            }
            let (key, _) = self.string()?;
            self.ws();
            if self.s.get(self.pos) != Some(&b':') {
                return Err(self.err("expected :"));
            }
            self.pos += 1;
            self.ws();
            let v = self.value(depth + 1)?;
            members.push((key, v));
            self.ws();
            match self.s.get(self.pos) {
                Some(b',') => self.pos += 1,
                Some(b'}') => {
                    self.pos += 1;
                    return Ok(J::Obj(members));
                }
                _ => return Err(self.err("expected , or }")),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_order_numbers_and_escapes() {
        let src = r#" {"b":1.50,"a":[true,null,"xBSu00e9BSn"],"c":-2e3} "#.replace("BS", "\\");
        let v = parse(src.as_bytes()).unwrap();
        assert_eq!(v.to_text(), src.trim());
        assert_eq!(v.at("a.2").and_then(J::as_str), Some("x\u{e9}\n"));
        assert_eq!(v.get("b"), Some(&J::Num("1.50".into())));
        assert!(parse(b"{\"a\":1,}").is_err());
        assert!(parse(b"[1] 2").is_err());
        assert!(is_compact(br#"{"a":"x y"}"#));
        assert!(!is_compact(br#"{"a": 1}"#));
        let pair = parse(r#""BSud83dBSude00""#.replace("BS", "\\").as_bytes()).unwrap();
        assert_eq!(pair.as_str(), Some("\u{1F600}"));
    }
}
