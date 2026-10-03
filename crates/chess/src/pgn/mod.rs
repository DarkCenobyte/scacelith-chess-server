//! PGN: the writer of the server's game records (`GET /api/v1/games/:id/pgn`, byte-identical to
//! the former Node.js server and to the game's `chess::Game::pgn`) and the reader of the first
//! game of an untrusted PGN text (`POST /api/v1/gif`).

mod reader;
mod writer;

pub use reader::{PGN_LIMITS, PgnError, PgnGame, PgnLimits, normalize_result, read_pgn, read_pgn_bytes};
pub(crate) use writer::write_pgn;
pub use writer::{PgnComment, PgnTags};

/// JavaScript's `String.prototype.trim()` set: the Unicode White_Space characters without U+0085
/// (NEXT LINE), plus U+FEFF (ZERO WIDTH NO-BREAK SPACE).
fn is_js_space(c: char) -> bool {
    c == '\u{feff}' || (c != '\u{85}' && c.is_whitespace())
}

/// `s.trim()` as JavaScript trims (the former server's semantics for tag values and comments).
pub(crate) fn js_trim(s: &str) -> &str {
    s.trim_matches(is_js_space)
}

#[cfg(test)]
mod tests {
    use super::js_trim;

    #[test]
    fn js_trim_matches_javascript() {
        assert_eq!(js_trim(" \t\n\u{b}\u{c}\r x \u{a0}\u{feff}\u{2028}\u{3000}"), "x");
        assert_eq!(js_trim("\u{85}x\u{85}"), "\u{85}x\u{85}");
        assert_eq!(js_trim("\u{1c}x"), "\u{1c}x");
        assert_eq!(js_trim(""), "");
    }
}
