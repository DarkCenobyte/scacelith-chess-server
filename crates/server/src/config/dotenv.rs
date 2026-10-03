//! The `.env` file parser of the former server, rule for rule (an existing file must give the same
//! values, secrets included, so no general-purpose dotenv library):
//!
//! * lines are trimmed (JavaScript white space); blank lines and `#` lines are skipped;
//! * `[export ]NAME=value` with an upper-case `NAME` (`A-Z`, `0-9`, `_`); other lines are ignored;
//! * `"value"` understands `\n`, `\"` and `\\`; `'value'` is literal;
//! * an unquoted value ends at the first ` #` (space, hash), then is trimmed; `a#b` keeps its `#`;
//! * a quoted value followed by such a comment keeps its quotes (and a note says so);
//! * a later line of the same name wins.

use indexmap::IndexMap;

use crate::util::js;

/// Parses `.env` text. `notes` receives a sentence for every quoted value followed by a comment.
pub fn parse_env_file(text: &str, notes: &mut Vec<String>) -> IndexMap<String, String> {
    let mut out = IndexMap::new();
    for raw in text.split('\n') {
        let line = js::trim(raw);
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((name, value)) = match_line(line) else {
            continue;
        };
        let value = if let Some(inner) = quoted(value, '"') {
            unescape(inner)
        } else if let Some(inner) = quoted(value, '\'') {
            inner.to_string()
        } else if let Some(hash) = value.find(" #") {
            let cut = js::trim(&value[..hash]);
            if cut.starts_with('"') || cut.starts_with('\'') {
                notes.push(format!(
                    "{name}: the value is quoted and followed by a comment on the same line of the .env file, so \
                     its quotes are part of the value. Put the comment on a line of its own (check the value \
                     first: a secret changes when its quotes go)."
                ));
            }
            cut.to_string()
        } else {
            value.to_string()
        };
        out.insert(name.to_string(), value);
    }
    out
}

/// `^(?:export\s+)?([A-Z0-9_]+)\s*=\s*(.*)$` on a trimmed line.
fn match_line(line: &str) -> Option<(&str, &str)> {
    if let Some(rest) = line.strip_prefix("export")
        && rest.starts_with(is_regex_space)
        && let Some(m) = match_assignment(rest.trim_start_matches(is_regex_space))
    {
        return Some(m);
    }
    match_assignment(line)
}

fn match_assignment(s: &str) -> Option<(&str, &str)> {
    let name_len =
        s.bytes().take_while(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || *b == b'_').count();
    if name_len == 0 {
        return None;
    }
    let (name, rest) = s.split_at(name_len);
    let value = rest.trim_start_matches(is_regex_space).strip_prefix('=')?.trim_start_matches(is_regex_space);
    // `.` does not match a line terminator, and `$` only matches at the very end.
    if value.contains(['\r', '\u{2028}', '\u{2029}']) {
        return None;
    }
    Some((name, value))
}

/// JavaScript's `\s`: the white space and line terminators that `trim` removes.
fn is_regex_space(c: char) -> bool {
    js::is_whitespace(c)
}

/// The text between a pair of `q` quotes that open and close the value.
fn quoted(value: &str, q: char) -> Option<&str> {
    if value.len() >= 2 && value.starts_with(q) && value.ends_with(q) {
        Some(&value[1..value.len() - 1])
    } else {
        None
    }
}

/// `\n`, `\"` and `\\` escapes; any other backslash stays.
fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.peek() {
                Some('n') => {
                    out.push('\n');
                    chars.next();
                    continue;
                }
                Some(&e @ ('"' | '\\')) => {
                    out.push(e);
                    chars.next();
                    continue;
                }
                _ => {}
            }
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> (Vec<(String, String)>, Vec<String>) {
        let mut notes = Vec::new();
        let map = parse_env_file(text, &mut notes);
        (map.into_iter().collect(), notes)
    }

    fn pairs(list: &[(&str, &str)]) -> Vec<(String, String)> {
        list.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    /// Ported from config.load.test.js.
    #[test]
    fn a_quoted_value_followed_by_a_comment_keeps_its_quotes() {
        let (vals, notes) = parse("A=\"x y\" # c\nB='x' # c\nC=\"a#b\"\nD=x #c\nE=\"q\"\n");
        assert_eq!(vals, pairs(&[("A", "\"x y\""), ("B", "'x'"), ("C", "a#b"), ("D", "x"), ("E", "q")]));
        assert_eq!(notes.len(), 2);
        assert!(notes[0].starts_with("A: the value is quoted and followed by a comment"));
        assert!(notes[1].starts_with("B: "));
    }

    #[test]
    fn lines_names_and_escapes() {
        let text = "\u{feff}# comment\r\n  export  FOO = bar baz  \r\nlower=1\nexportBAR=2\nexport=3\nEXPORT=4\n\
                    Q=\"a\\nb\\\"c\\\\d\\x\"\nS='a\\nb'\nH=a#b\nE=\nFOO=again\nX=\"\nY=a\u{2028}b\nZ= \u{2028}z\n=5\n";
        let (vals, notes) = parse(text);
        assert_eq!(
            vals,
            pairs(&[
                ("FOO", "again"),
                ("EXPORT", "4"),
                ("Q", "a\nb\"c\\d\\x"),
                ("S", "a\\nb"),
                ("H", "a#b"),
                ("E", ""),
                ("X", "\""),
                ("Z", "z"),
            ])
        );
        assert!(notes.is_empty());
    }
}
