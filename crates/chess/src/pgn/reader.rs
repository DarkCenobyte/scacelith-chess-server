//! PGN reader: the first game of an untrusted PGN text, as protocol moves.
//!
//! It reads what the game's own reader reads (`src/chess/pgn.cpp`, same lexing and the same
//! lenient SAN), so every PGN the server writes and the usual lichess / chess.com exports are
//! accepted:
//!  * tag pairs with `\"` and `\\` escapes (and an unescaped quote inside a value when a later
//!    quote on the line closes the tag); `SetUp` / `FEN` start positions (`SetUp "0"` ignores the
//!    FEN); `Variant` standard / chess / normal / from position, or Chess960 with a FEN whose
//!    castling rights all survive on the standard squares; any other variant is refused;
//!  * movetext: move numbers ("12.", "12...", "12…"), comments `{ ... }` and `;` to the end of
//!    the line, `%` escape lines, NAGs (`$1`), suffix glyphs (`! ? !! ?? !? ?!`), text evaluations
//!    (`+- = -/+ ±`), variations `( ... )` skipped (nesting capped), "e.p." dropped; the moves in
//!    lenient SAN ([`crate::parse_san`]), promotions as e8=Q, e8Q or e8/Q in movetext
//!    (`parse_san` also reads e8(Q), but in movetext its '(' opens a variation, as in the game's
//!    reader);
//!  * the end of the first game: its termination marker, a tag pair after its movetext (the next
//!    game), or the end of the text.
//!
//! Null moves ("--", "Z0") are refused. Moves after an automatic ending (fivefold repetition, 75
//! moves) are kept, as on paper: only legality is checked.
//!
//! Hostile input: hard caps on bytes, plies, tags, tag lengths, token length and variation
//! nesting, linear time, and every failure is a [`PgnError`] with a line and a column (1-based,
//! columns in characters). The error texts are those of the former Node.js server (they reach the
//! HTTP API).

use std::fmt;

use super::js_trim;
use crate::notation::parse_san;
use crate::position::{Position, START_FEN};

/// Caps of [`read_pgn`]. `Default` is [`PGN_LIMITS`]; callers usually lower `max_bytes` and
/// `max_plies`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PgnLimits {
    /// Size of the text in UTF-8 bytes.
    pub max_bytes: usize,
    /// Moves of the main line.
    pub max_plies: usize,
    /// Tag pairs.
    pub max_tags: usize,
    /// Characters of a tag name.
    pub max_tag_name: usize,
    /// UTF-16 units of a tag value.
    pub max_tag_value: usize,
    /// Nesting of variations.
    pub max_depth: usize,
    /// UTF-16 units of a movetext token.
    pub max_token: usize,
}

/// The default caps.
pub const PGN_LIMITS: PgnLimits = PgnLimits {
    max_bytes: 1 << 20,
    max_plies: 1500,
    max_tags: 128,
    max_tag_name: 64,
    max_tag_value: 2048,
    max_depth: 64,
    max_token: 40,
};

impl Default for PgnLimits {
    fn default() -> PgnLimits {
        PGN_LIMITS
    }
}

/// The only error of [`read_pgn`]: where (1-based line, and column in characters) and what.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PgnError {
    /// Line, from 1. A line ends at LF, or at a CR not followed by LF.
    pub line: u32,
    /// Column in characters (Unicode scalar values), from 1.
    pub column: u32,
    /// What is wrong (stable English texts).
    pub message: String,
}

impl PgnError {
    fn new(line: u32, column: u32, message: impl Into<String>) -> PgnError {
        PgnError { line, column, message: message.into() }
    }
}

impl fmt::Display for PgnError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for PgnError {}

/// The first game of a PGN text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PgnGame {
    /// The tag pairs in file order (duplicates kept).
    pub tags: Vec<(String, String)>,
    /// `None` for the standard start position, else the normalised FEN of the start.
    pub start_fen: Option<String>,
    /// The moves of the main line (protocol u16).
    pub moves: Vec<u16>,
    /// "1-0", "0-1", "1/2-1/2" or "*": the movetext's termination marker, else the first
    /// `Result` tag (trimmed, normalised), else "*".
    pub result: &'static str,
}

impl PgnGame {
    /// The value of the first tag of that name.
    #[must_use]
    pub fn tag(&self, name: &str) -> Option<&str> {
        self.tags.iter().find(|(n, _)| n == name).map(|(_, v)| v.as_str())
    }

    /// The value of the last tag of that name.
    #[must_use]
    pub fn last_tag(&self, name: &str) -> Option<&str> {
        self.tags.iter().rev().find(|(n, _)| n == name).map(|(_, v)| v.as_str())
    }
}

/// A result text of the movetext or of a `Result` tag, normalised: "1-0", "0-1", "1/2-1/2" and
/// "*" as they are; "½-½", "0.5-0.5" and "1/2" as "1/2-1/2"; `None` for anything else. ("0.5-0.5"
/// only works in a tag: in movetext its '.' is read as the period of a move number.)
#[must_use]
pub fn normalize_result(s: &str) -> Option<&'static str> {
    match s {
        "1-0" => Some("1-0"),
        "0-1" => Some("0-1"),
        "*" => Some("*"),
        "1/2-1/2" | "½-½" | "0.5-0.5" | "1/2" => Some("1/2-1/2"),
        _ => None,
    }
}

/// Reads the first game of a PGN text (see the module documentation).
pub fn read_pgn(text: &str, limits: &PgnLimits) -> Result<PgnGame, PgnError> {
    if text.len() > limits.max_bytes {
        return Err(too_large(limits));
    }
    read_chars(text.chars().collect(), limits)
}

/// Reads the first game of a PGN file: UTF-8 (a byte order mark is skipped), or Latin-1 when
/// the bytes are not valid UTF-8.
pub fn read_pgn_bytes(bytes: &[u8], limits: &PgnLimits) -> Result<PgnGame, PgnError> {
    if bytes.len() > limits.max_bytes {
        return Err(too_large(limits));
    }
    let chars = match std::str::from_utf8(bytes) {
        Ok(text) => text.strip_prefix('\u{feff}').unwrap_or(text).chars().collect(),
        Err(_) => bytes.iter().map(|&b| char::from(b)).collect(),
    };
    read_chars(chars, limits)
}

fn too_large(limits: &PgnLimits) -> PgnError {
    PgnError::new(1, 1, format!("PGN too large (more than {} bytes)", limits.max_bytes))
}

// ---- Lexer -------------------------------------------------------------------------------------

enum Tok {
    End,
    Tag { name: String, value: String },
    Comment,
    Open,
    Close,
    Nag,
    Symbol(String),
    Star,
    Bad { message: String, tag_like: bool },
}

/// A token and the position of its first character.
struct Token {
    tok: Tok,
    line: u32,
    column: u32,
}

fn is_space(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\r' | '\n' | '\u{c}' | '\u{b}')
}

fn is_name_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

struct Lexer<'a> {
    s: Vec<char>,
    lim: &'a PgnLimits,
    p: usize,
    line: u32,
    col: u32,
    // The last quote closing a tag on the stretch [lc_start, lc_end) of a line, found once: many
    // tag pairs on one long line cost one pass over it, not one pass per tag.
    lc_start: usize,
    lc_end: usize,
    lc_last: Option<usize>,
}

impl<'a> Lexer<'a> {
    fn new(s: Vec<char>, lim: &'a PgnLimits) -> Lexer<'a> {
        let p = usize::from(s.first() == Some(&'\u{feff}'));
        Lexer { s, lim, p, line: 1, col: 1, lc_start: 0, lc_end: 0, lc_last: None }
    }

    fn at(&self, q: usize) -> Option<char> {
        self.s.get(q).copied()
    }

    /// A line ends at '\n', and at a '\r' not followed by '\n'.
    fn line_break(&self, q: usize) -> bool {
        match self.at(q) {
            Some('\n') => true,
            Some('\r') => self.at(q + 1) != Some('\n'),
            _ => false,
        }
    }

    fn at_end(&self) -> bool {
        self.p >= self.s.len()
    }

    fn advance(&mut self) {
        if self.line_break(self.p) {
            self.line = self.line.saturating_add(1);
            self.col = 1;
        } else {
            self.col = self.col.saturating_add(1);
        }
        self.p += 1;
    }

    fn skip_line(&mut self) {
        while !self.at_end() && !self.line_break(self.p) {
            self.advance();
        }
    }

    fn skip_spaces_in_line(&mut self) {
        while matches!(self.at(self.p), Some(' ' | '\t')) {
            self.advance();
        }
    }

    /// At the start of a line: does it open a tag pair? (A comment running into it was never
    /// closed.)
    fn tag_line_ahead(&self) -> bool {
        let mut q = self.p;
        while matches!(self.at(q), Some(' ' | '\t')) {
            q += 1;
        }
        if self.at(q) != Some('[') {
            return false;
        }
        q += 1;
        let name_start = q;
        while self.at(q).is_some_and(is_name_char) {
            q += 1;
        }
        let named = q > name_start;
        while matches!(self.at(q), Some(' ' | '\t')) {
            q += 1;
        }
        named && self.at(q) == Some('"')
    }

    /// After a quote at `q`: only spaces then ']' on this line.
    fn closes_tag(&self, q: usize) -> bool {
        let mut q = q + 1;
        while matches!(self.at(q), Some(' ' | '\t')) {
            q += 1;
        }
        self.at(q) == Some(']')
    }

    fn next(&mut self) -> Token {
        loop {
            while self.at(self.p).is_some_and(is_space) {
                self.advance();
            }
            let (line, column) = (self.line, self.col);
            let token = |tok: Tok| Token { tok, line, column };
            let bad = |message: String| token(Tok::Bad { message, tag_like: false });
            let Some(c) = self.at(self.p) else {
                return token(Tok::End);
            };
            if c == '%' && self.col == 1 {
                self.skip_line();
                continue;
            }
            if c == '<' {
                let mut q = self.p + 1;
                while q < self.s.len() && self.s[q] != '>' && !self.line_break(q) {
                    q += 1;
                }
                if self.at(q) == Some('>') {
                    while self.p <= q {
                        self.advance();
                    }
                    continue;
                }
                self.advance();
                return bad("'<' without '>' on its line".to_owned());
            }
            if c == '.' || c == '…' {
                self.advance();
                continue;
            }
            match c {
                '[' => return self.tag_pair(line, column),
                '{' => return self.brace_comment(line, column),
                ';' => {
                    self.skip_line();
                    return token(Tok::Comment);
                }
                '(' | ')' | '*' => {
                    self.advance();
                    return token(match c {
                        '(' => Tok::Open,
                        ')' => Tok::Close,
                        _ => Tok::Star,
                    });
                }
                '$' => return self.nag(line, column),
                '!' | '?' => {
                    while matches!(self.at(self.p), Some('!' | '?')) {
                        self.advance();
                    }
                    return token(Tok::Nag);
                }
                _ => {}
            }
            if c == '+' || c == '=' || (c == '-' && self.at(self.p + 1) != Some('-')) {
                // Text evaluations of some exports ("+-", "=", "-/+"): ignored.
                while matches!(self.at(self.p), Some('+' | '-' | '=' | '/')) {
                    self.advance();
                }
                continue;
            }
            if c.is_ascii_alphanumeric() || c == '-' || !c.is_ascii() {
                match self.symbol() {
                    Some(tok) => return token(tok),
                    None => continue, // a lone "e.p."
                }
            }
            self.advance();
            return bad(if c.is_ascii_control() {
                "unexpected control character".to_owned()
            } else {
                format!("unexpected character '{c}'")
            });
        }
    }

    fn tag_pair(&mut self, line: u32, column: u32) -> Token {
        let bad = |message: String| Token { tok: Tok::Bad { message, tag_like: true }, line, column };
        self.advance(); // '['
        self.skip_spaces_in_line();
        let mut name = String::new();
        while let Some(c) = self.at(self.p).filter(|&c| is_name_char(c)) {
            name.push(c);
            self.advance();
            if name.len() > self.lim.max_tag_name {
                self.skip_line();
                return bad("tag name too long".to_owned());
            }
        }
        if name.is_empty() {
            self.skip_line();
            return bad("tag name expected after '['".to_owned());
        }
        self.skip_spaces_in_line();
        if self.at(self.p) != Some('"') {
            self.skip_line();
            return bad(format!("quoted value expected in tag {name}"));
        }
        self.advance();
        // The last closing quote from here to the end of the line. A later tag of a line already
        // scanned reuses that scan: the line's last closing quote is also the last one after p
        // when it lies after it, and one at or before p counts as none below.
        if !(self.lc_start..self.lc_end).contains(&self.p) {
            let mut q = self.p;
            let mut last = None;
            while q < self.s.len() && !self.line_break(q) {
                if self.s[q] == '"' && self.closes_tag(q) {
                    last = Some(q);
                }
                q += 1;
            }
            self.lc_start = self.p;
            self.lc_end = q;
            self.lc_last = last;
        }
        let last_close = self.lc_last;
        let mut value = String::new();
        let mut value_len = 0;
        let mut too_long = false;
        loop {
            if self.at_end() || self.line_break(self.p) {
                return bad(format!("unterminated value of tag {name}"));
            }
            let mut c = self.s[self.p];
            if c == '\\' && matches!(self.at(self.p + 1), Some('"' | '\\')) {
                self.advance();
                c = self.s[self.p];
            } else if c == '"' && (last_close.is_none_or(|last| last <= self.p) || self.closes_tag(self.p)) {
                self.advance();
                break;
            }
            // The cap counts UTF-16 units, as the former server did.
            let units = c.len_utf16();
            if value_len + units <= self.lim.max_tag_value {
                value.push(c);
                value_len += units;
            } else {
                too_long = true;
            }
            self.advance();
        }
        self.skip_spaces_in_line();
        if self.at(self.p) != Some(']') {
            self.skip_line();
            return bad(format!("']' expected after the value of tag {name}"));
        }
        self.advance();
        if too_long {
            return bad(format!("value of tag {name} too long"));
        }
        Token { tok: Tok::Tag { name, value }, line, column }
    }

    fn brace_comment(&mut self, line: u32, column: u32) -> Token {
        let unterminated = || Token {
            tok: Tok::Bad { message: "unterminated comment".to_owned(), tag_like: false },
            line,
            column,
        };
        self.advance(); // '{'
        loop {
            match self.at(self.p) {
                None => return unterminated(),
                Some('}') => {
                    self.advance();
                    break;
                }
                Some(_) => {
                    let new_line = self.line_break(self.p);
                    self.advance();
                    if new_line && self.tag_line_ahead() {
                        return unterminated();
                    }
                }
            }
        }
        Token { tok: Tok::Comment, line, column }
    }

    fn nag(&mut self, line: u32, column: u32) -> Token {
        self.advance(); // '$'
        let (mut n, mut digits) = (0u32, 0u32);
        while let Some(d) = self.at(self.p).filter(char::is_ascii_digit) {
            if digits < 4 {
                n = n * 10 + (u32::from(d) - u32::from('0'));
            }
            digits = digits.saturating_add(1);
            self.advance();
        }
        let tok = if digits == 0 || n > 255 {
            Tok::Bad { message: "malformed NAG".to_owned(), tag_like: false }
        } else {
            Tok::Nag
        };
        Token { tok, line, column }
    }

    fn starts_with_ep(&self, q: usize) -> bool {
        self.s.get(q..q + 4) == Some(&['e', '.', 'p', '.'])
    }

    /// A move, a result or a move number; `None` when nothing but "e.p." was read.
    fn symbol(&mut self) -> Option<Tok> {
        let mut sym = String::new();
        // UTF-16 units taken, as the former server counted them (it could take the first half of
        // a character beyond the cap).
        let mut units = 0;
        let mut too_long = false;
        while let Some(c) = self.at(self.p) {
            if self.starts_with_ep(self.p) {
                for _ in 0..4 {
                    self.advance();
                }
                continue;
            }
            if !(c.is_ascii_alphanumeric()
                || !c.is_ascii()
                || matches!(c, '_' | '+' | '#' | '=' | ':' | '-' | '/'))
            {
                break;
            }
            let room = self.lim.max_token.saturating_sub(units);
            let len = c.len_utf16();
            if len <= room {
                sym.push(c);
            } else {
                too_long = true;
            }
            units += len.min(room);
            self.advance();
        }
        if units == 0 {
            return None;
        }
        if too_long {
            return Some(Tok::Bad { message: "token too long".to_owned(), tag_like: false });
        }
        Some(Tok::Symbol(sym))
    }
}

// ---- Reader ------------------------------------------------------------------------------------

fn is_chess960(variant: &str) -> bool {
    matches!(variant, "chess960" | "chess 960" | "fischerandom" | "fischer random" | "960")
}

/// The start position from the tags (first tag of each name): the position, and its FEN when it
/// is not the standard start.
fn start_position(
    tags: &[(String, String)],
    at: &[(u32, u32)],
) -> Result<(Position, Option<String>), PgnError> {
    let index = |name: &str| tags.iter().position(|(n, _)| n == name);
    let error_at = |i: usize, message: &str| {
        let (line, column) = at.get(i).copied().unwrap_or((1, 1));
        PgnError::new(line, column, message)
    };
    let (variant, fen_tag, setup) = (index("Variant"), index("FEN"), index("SetUp"));
    let mut chess960 = None;
    if let Some(vi) = variant {
        let raw = &tags[vi].1;
        let v = js_trim(raw).to_lowercase();
        if is_chess960(&v) {
            chess960 = Some(vi);
        } else if !matches!(v.as_str(), "" | "standard" | "chess" | "normal" | "from position") {
            return Err(error_at(vi, &format!("variant '{raw}' is not supported")));
        }
    }
    let use_fen = !setup.is_some_and(|si| js_trim(&tags[si].1) == "0");
    let Some(fi) = fen_tag.filter(|_| use_fen) else {
        if let Some(vi) = chess960 {
            return Err(error_at(vi, "Chess960 game without a FEN tag"));
        }
        return Ok((Position::start(), None));
    };
    let fen = js_trim(&tags[fi].1);
    let Some(pos) = Position::from_fen(fen) else {
        let message =
            if chess960.is_some() { "Chess960 castling rights are not supported" } else { "invalid FEN" };
        return Err(error_at(fi, message));
    };
    if chess960.is_some() {
        let field = fen.split([' ', '\t', '\n', '\r']).filter(|f| !f.is_empty()).nth(2).unwrap_or("");
        let asked = field.chars().fold(0u8, |bits, c| {
            bits | match c {
                'K' => 1,
                'Q' => 2,
                'k' => 4,
                'q' => 8,
                _ => 0,
            }
        });
        if asked != pos.castling() {
            return Err(error_at(fi, "Chess960 castling from this setup is not supported"));
        }
    }
    let normal = pos.fen();
    Ok((pos, (normal != START_FEN).then_some(normal)))
}

/// The movetext state once the first move-text token was read.
struct Movetext {
    pos: Position,
    start_fen: Option<String>,
}

fn read_chars(s: Vec<char>, lim: &PgnLimits) -> Result<PgnGame, PgnError> {
    let mut lex = Lexer::new(s, lim);
    let mut tags: Vec<(String, String)> = Vec::new();
    let mut tag_at: Vec<(u32, u32)> = Vec::new();
    let mut moves: Vec<u16> = Vec::new();
    let mut game: Option<Movetext> = None;
    let mut depth = 0usize;
    let mut movetext_result = None;
    let mut any = false;
    let mut last_open = (1, 1);
    loop {
        let Token { tok, line, column } = lex.next();
        let error = |message: String| PgnError::new(line, column, message);
        match tok {
            Tok::End => break,
            Tok::Tag { name, value } => {
                if game.is_some() {
                    break; // the next game (this one had no termination marker)
                }
                any = true;
                if tags.len() >= lim.max_tags {
                    return Err(error(format!("too many tags (more than {})", lim.max_tags)));
                }
                tags.push((name, value));
                tag_at.push((line, column));
                continue;
            }
            Tok::Bad { message, tag_like } => {
                if tag_like && game.is_some() {
                    break; // a broken tag pair opens the next game
                }
                return Err(error(message));
            }
            Tok::Comment => continue,
            _ => {}
        }
        any = true;
        let state = match &mut game {
            Some(state) => state,
            None => {
                let (pos, start_fen) = start_position(&tags, &tag_at)?;
                game.insert(Movetext { pos, start_fen })
            }
        };
        let sym = match tok {
            Tok::Open => {
                depth += 1;
                if depth > lim.max_depth {
                    return Err(error("variations nested too deeply".to_owned()));
                }
                last_open = (line, column);
                continue;
            }
            Tok::Close => {
                if depth == 0 {
                    return Err(error("')' without a variation".to_owned()));
                }
                depth -= 1;
                continue;
            }
            Tok::Star => None,
            Tok::Symbol(sym) => Some(sym),
            _ => continue, // a NAG
        };
        let result = match &sym {
            None => Some("*"),
            Some(text) => normalize_result(text),
        };
        if let Some(result) = result {
            if depth > 0 {
                return Err(error("unterminated variation before the result".to_owned()));
            }
            movetext_result = Some(result);
            break;
        }
        let Some(sym) = sym else { continue };
        if depth > 0 {
            continue; // variations are skipped
        }
        if sym.bytes().all(|b| b.is_ascii_digit()) {
            continue; // a move number
        }
        if sym == "--" || sym == "Z0" {
            return Err(error("null moves are not supported".to_owned()));
        }
        if !sym.bytes().any(|b| b.is_ascii_alphanumeric()) {
            continue; // an annotation glyph ("±", "∞")
        }
        if moves.len() >= lim.max_plies {
            return Err(error(format!("too many moves (more than {})", lim.max_plies)));
        }
        let m = parse_san(&state.pos, &sym)
            .filter(|&m| state.pos.play(m).is_ok())
            .ok_or_else(|| error(format!("illegal move '{sym}'")))?;
        moves.push(m);
    }
    if !any {
        return Err(PgnError::new(1, 1, "no game found"));
    }
    if depth > 0 {
        return Err(PgnError::new(last_open.0, last_open.1, "unterminated variation"));
    }
    // A game of tags only: its FEN and Variant are checked too.
    let start_fen = match game {
        Some(state) => state.start_fen,
        None => start_position(&tags, &tag_at)?.1,
    };
    let tag_result = tags.iter().find(|(n, _)| n == "Result").and_then(|(_, v)| normalize_result(js_trim(v)));
    Ok(PgnGame { tags, start_fen, moves, result: movetext_result.or(tag_result).unwrap_or("*") })
}
