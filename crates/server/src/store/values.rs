//! Value conventions of the store: e-mail and username normalization, JSON columns, packed move
//! lists and text truncation. They must give the same results as the auth module and the clients
//! expect (DESIGN 5.5).

use serde_json::Value;

use super::error::{Result, StoreError};

/// Defines an enum stored as TEXT (a CHECK-constrained column): `as_str`, `parse`, `Display`,
/// `ToSql` and `FromSql`.
macro_rules! text_enum {
    (
        $(#[$m:meta])*
        $vis:vis enum $name:ident { $($(#[$vm:meta])* $var:ident = $text:literal),+ $(,)? }
    ) => {
        $(#[$m])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        $vis enum $name { $($(#[$vm])* $var),+ }

        impl $name {
            /// The stored text.
            pub fn as_str(self) -> &'static str {
                match self { $($name::$var => $text),+ }
            }

            /// The value of a stored text.
            pub fn parse(s: &str) -> Option<$name> {
                match s { $($text => Some($name::$var),)+ _ => None }
            }
        }

        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(self.as_str())
            }
        }

        impl rusqlite::types::ToSql for $name {
            fn to_sql(&self) -> rusqlite::Result<rusqlite::types::ToSqlOutput<'_>> {
                Ok(rusqlite::types::ToSqlOutput::Borrowed(rusqlite::types::ValueRef::Text(self.as_str().as_bytes())))
            }
        }

        impl rusqlite::types::FromSql for $name {
            fn column_result(v: rusqlite::types::ValueRef<'_>) -> rusqlite::types::FromSqlResult<Self> {
                let s = v.as_str()?;
                $name::parse(s).ok_or_else(|| {
                    rusqlite::types::FromSqlError::Other(format!("unknown {} {s:?}", stringify!($name)).into())
                })
            }
        }
    };
}
pub(crate) use text_enum;

/// The "no cursor" value of paginated queries (JavaScript's `Number.MAX_SAFE_INTEGER`).
pub const NO_CURSOR: i64 = 9_007_199_254_740_991;

/// Longest username the store accepts, in UTF-16 code units (the auth module enforces a much
/// shorter, ASCII-only limit; this one only protects the database).
pub const USERNAME_MAX_UNITS: usize = 64;

/// Whether `c` is white space for ECMAScript's `String.prototype.trim` (WhiteSpace and
/// LineTerminator). Unlike Rust's `char::is_whitespace`, U+0085 is not, and U+FEFF is.
fn is_js_space(c: char) -> bool {
    matches!(
        c,
        '\u{0009}'..='\u{000D}'
            | '\u{0020}'
            | '\u{00A0}'
            | '\u{1680}'
            | '\u{2000}'..='\u{200A}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{202F}'
            | '\u{205F}'
            | '\u{3000}'
            | '\u{FEFF}'
    )
}

/// `s` without the leading and trailing white space JavaScript's `trim()` removes.
pub fn js_trim(s: &str) -> &str {
    s.trim_matches(is_js_space)
}

/// The address as stored in `users.email`: trimmed, `None` when empty.
pub fn clean_email(email: &str) -> Option<String> {
    let t = js_trim(email);
    (!t.is_empty()).then(|| t.to_string())
}

/// The address as compared for uniqueness and lookups: trimmed and lower-cased (Gmail dots and
/// `+tags` kept), `None` when empty.
pub fn normalize_email(email: &str) -> Option<String> {
    let t = js_trim(email);
    (!t.is_empty()).then(|| t.to_lowercase())
}

/// The lower-cased form of a username, as stored in `username_lower`.
pub fn username_lower(name: &str) -> String {
    name.to_lowercase()
}

/// Checks the length of a username (1 to [`USERNAME_MAX_UNITS`] UTF-16 code units).
pub fn check_username(name: &str) -> Result<()> {
    let units = name.encode_utf16().count();
    if units == 0 || units > USERNAME_MAX_UNITS {
        return Err(StoreError::invalid("invalid username"));
    }
    Ok(())
}

/// The longest prefix of `s` holding at most `max` UTF-16 code units, without splitting a
/// surrogate pair.
pub fn truncate_utf16(s: &str, max: usize) -> &str {
    let mut units = 0;
    for (i, c) in s.char_indices() {
        units += c.len_utf16();
        if units > max {
            return &s[..i];
        }
    }
    s
}

/// The TEXT of a JSON column: `None` (SQL NULL) for no value or JSON `null`.
pub fn json_text(v: Option<&Value>) -> Option<String> {
    match v {
        None | Some(Value::Null) => None,
        Some(v) => Some(v.to_string()),
    }
}

/// The value of a JSON column: `None` for NULL, the raw text as a string when it does not parse.
pub fn json_value(text: Option<String>) -> Option<Value> {
    text.map(|t| serde_json::from_str(&t).unwrap_or(Value::String(t)))
}

/// Packs moves as u16 little-endian.
pub fn pack_u16(values: &[u16]) -> Vec<u8> {
    values.iter().flat_map(|v| v.to_le_bytes()).collect()
}

/// Packs times as u32 little-endian.
pub fn pack_u32(values: &[u32]) -> Vec<u8> {
    values.iter().flat_map(|v| v.to_le_bytes()).collect()
}

/// Unpacks u16 little-endian values (NULL: empty; a trailing odd byte is ignored).
pub fn unpack_u16(bytes: Option<&[u8]>) -> Vec<u16> {
    bytes.unwrap_or_default().as_chunks::<2>().0.iter().map(|c| u16::from_le_bytes(*c)).collect()
}

/// Unpacks u32 little-endian values (NULL: empty; trailing bytes are ignored).
pub fn unpack_u32(bytes: Option<&[u8]>) -> Vec<u32> {
    bytes.unwrap_or_default().as_chunks::<4>().0.iter().map(|c| u32::from_le_bytes(*c)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn trim_follows_javascript() {
        assert_eq!(js_trim("\u{FEFF}\u{3000} a b \t\n\u{2029}"), "a b");
        assert_eq!(js_trim("\u{0085}x\u{0085}"), "\u{0085}x\u{0085}");
        assert_eq!(
            normalize_email("  Alice.Smith+Tag@Example.ORG "),
            Some("alice.smith+tag@example.org".into())
        );
        assert_eq!(clean_email("  Alice@Example.org "), Some("Alice@Example.org".into()));
        assert_eq!(normalize_email(" \u{00A0} "), None);
        assert_eq!(clean_email(""), None);
    }

    #[test]
    fn usernames_are_bounded_in_utf16_units() {
        assert!(check_username("a").is_ok());
        assert!(check_username(&"x".repeat(64)).is_ok());
        assert!(check_username(&"x".repeat(65)).is_err());
        assert!(check_username(&"\u{1F600}".repeat(32)).is_ok());
        assert!(check_username(&"\u{1F600}".repeat(33)).is_err());
        assert!(check_username("").is_err());
        assert_eq!(username_lower("AlIcE"), "alice");
    }

    #[test]
    fn truncation_counts_utf16_units() {
        assert_eq!(truncate_utf16("abcdef", 3), "abc");
        assert_eq!(truncate_utf16("ab\u{1F600}c", 3), "ab");
        assert_eq!(truncate_utf16("ab\u{1F600}c", 4), "ab\u{1F600}");
        assert_eq!(truncate_utf16("ab", 5), "ab");
    }

    #[test]
    fn json_columns() {
        assert_eq!(json_text(None), None);
        assert_eq!(json_text(Some(&Value::Null)), None);
        assert_eq!(json_text(Some(&json!({"b": 1, "a": [true]}))).as_deref(), Some(r#"{"b":1,"a":[true]}"#));
        assert_eq!(json_text(Some(&json!("x"))).as_deref(), Some(r#""x""#));
        assert_eq!(json_value(Some("{\"a\":2}".into())), Some(json!({"a": 2})));
        assert_eq!(json_value(Some("not json".into())), Some(json!("not json")));
        assert_eq!(json_value(None), None);
    }

    #[test]
    fn packed_arrays() {
        assert_eq!(pack_u16(&[1, 0x0302]), vec![1, 0, 2, 3]);
        assert_eq!(unpack_u16(Some(&[1, 0, 2, 3, 9])), vec![1, 0x0302]);
        assert_eq!(unpack_u16(None), Vec::<u16>::new());
        assert_eq!(pack_u32(&[0x04030201]), vec![1, 2, 3, 4]);
        assert_eq!(unpack_u32(Some(&[1, 2, 3, 4, 5, 6])), vec![0x04030201]);
    }
}
