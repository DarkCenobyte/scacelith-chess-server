//! Text helpers of the emitters: name cases, comment wrapping, a line buffer.

/// Width the emitters wrap comments to.
const WIDTH: usize = 110;

/// `snake_case` of a camelCase or PascalCase name (`posHash` -> `pos_hash`).
pub fn snake(name: &str) -> String {
    let mut out = String::with_capacity(name.len() + 4);
    for (i, c) in name.chars().enumerate() {
        if c.is_ascii_uppercase() {
            if i > 0 {
                out.push('_');
            }
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

/// `SCREAMING_SNAKE_CASE` of a name (`MaxClientMessage` -> `MAX_CLIENT_MESSAGE`).
pub fn screaming(name: &str) -> String {
    snake(name).to_ascii_uppercase()
}

/// `text` wrapped into lines of at most `width` columns, each starting with `prefix`;
/// paragraphs (separated by a blank line) are kept apart by an empty comment line.
pub fn doc_lines(text: &str, prefix: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    for (p, paragraph) in text.split("\n\n").enumerate() {
        if p > 0 {
            lines.push(prefix.trim_end().to_owned());
        }
        let mut cur = String::new();
        for word in paragraph.split_whitespace() {
            if !cur.is_empty() && prefix.len() + cur.len() + 1 + word.len() > width {
                lines.push(format!("{prefix}{cur}"));
                cur.clear();
            }
            if !cur.is_empty() {
                cur.push(' ');
            }
            cur.push_str(word);
        }
        if !cur.is_empty() {
            lines.push(format!("{prefix}{cur}"));
        }
    }
    lines
}

/// A text being written line by line.
#[derive(Debug, Default)]
pub struct Lines {
    out: String,
}

impl Lines {
    /// An empty text.
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends a line.
    pub fn line(&mut self, line: &str) {
        self.out.push_str(line);
        self.out.push('\n');
    }

    /// Appends text as is.
    pub fn raw(&mut self, text: &str) {
        self.out.push_str(text);
    }

    /// Appends a Rust doc comment (`///`) indented by `indent`.
    pub fn doc(&mut self, text: &str, indent: &str) {
        for l in doc_lines(text, &format!("{indent}/// "), WIDTH) {
            self.line(&l);
        }
    }

    /// Appends a line comment (`//`) indented by `indent`.
    pub fn comment(&mut self, text: &str, indent: &str) {
        for l in doc_lines(text, &format!("{indent}// "), WIDTH) {
            self.line(&l);
        }
    }

    /// The text.
    pub fn finish(self) -> String {
        self.out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cases() {
        assert_eq!(snake("posHash"), "pos_hash");
        assert_eq!(snake("maxMsgPerSec"), "max_msg_per_sec");
        assert_eq!(snake("GestureFlag"), "gesture_flag");
        assert_eq!(screaming("MaxClientMessage"), "MAX_CLIENT_MESSAGE");
    }

    #[test]
    fn wrapping_keeps_paragraphs() {
        let lines = doc_lines("one two three\n\nfour", "/// ", 14);
        assert_eq!(lines, ["/// one two", "/// three", "///", "/// four"]);
    }
}
