//! PGN writer: `chess::Game::pgn` of the game plus the additions of the server's game records
//! (tags after `Result`, extra tags, a comment per ply, the online endings' `Termination`).

use std::time::{SystemTime, UNIX_EPOCH};

use super::js_trim;
use crate::game::ChessGame;
use crate::position::Position;
use crate::types::{Color, GameStatus};

/// Longest movetext line (characters, unless a single token is longer).
const LINE_MAX: usize = 79;

/// The tags and comments of a PGN export. `Default` gives the game's defaults.
///
/// Tag order: `Event` (default "Casual game"; an empty value is kept), `Site` ("Scacelith"),
/// `Date` (default or empty: today in UTC, "YYYY.MM.DD"), `Round` ("-"), `White` ("?"), `Black`
/// ("?"), `Result`, the `after_result` tags, `SetUp` "1" and `FEN` for a custom start,
/// `TimeControl` (default or empty: "?"), `Termination`, the `extra` tags.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PgnTags {
    /// `Event`.
    pub event: Option<String>,
    /// `Site`.
    pub site: Option<String>,
    /// `Date`, "YYYY.MM.DD".
    pub date: Option<String>,
    /// `Round`.
    pub round: Option<String>,
    /// `White` player name.
    pub white: Option<String>,
    /// `Black` player name.
    pub black: Option<String>,
    /// `TimeControl`, e.g. "180+2".
    pub time_control: Option<String>,
    /// Tags written right after `Result` (the server writes `UTCDate`, `UTCTime`, the Elos...).
    pub after_result: Vec<(String, String)>,
    /// Tags written after `Termination` (the server writes `PlyCount`, `ScacelithGameId`).
    pub extra: Vec<(String, String)>,
    /// The comment after each ply (index = ply; missing, `None` or empty: no comment). A Black
    /// move right after a comment repeats its number ("12... Nf6").
    pub comments: Vec<Option<PgnComment>>,
}

/// A movetext comment. Braces and control characters are removed from its words, which are
/// trimmed; empty words are dropped, and a comment without words is not written.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PgnComment {
    /// A text, split into words at spaces (a full line may break between its words).
    Text(String),
    /// Words each kept whole on one line, e.g. `["[%clk 0:02:58.3]", "[%emt 0:00:01.7]"]` gives
    /// `{[%clk 0:02:58.3] [%emt 0:00:01.7]}`.
    Words(Vec<String>),
}

/// Escapes a tag value: `"` and `\` get a backslash, line breaks become spaces.
fn push_escaped(out: &mut String, value: &str) {
    for c in value.chars() {
        match c {
            '"' | '\\' => {
                out.push('\\');
                out.push(c);
            }
            '\n' | '\r' => out.push(' '),
            _ => out.push(c),
        }
    }
}

/// A word of a comment: braces (a '}' would end the comment) and control characters dropped,
/// then trimmed.
fn comment_word(w: &str) -> String {
    let kept: String =
        w.chars().filter(|c| !matches!(c, '{' | '}' | '\u{0}'..='\u{1f}' | '\u{7f}')).collect();
    js_trim(&kept).to_owned()
}

/// Today's date in UTC ("????.??.??" outside the years 1970..=9999).
fn today_utc() -> String {
    let Ok(since_epoch) = SystemTime::now().duration_since(UNIX_EPOCH) else {
        return "????.??.??".to_owned();
    };
    let days = i64::try_from(since_epoch.as_secs() / 86_400).unwrap_or(i64::MAX);
    let (y, m, d) = civil_from_days(days);
    if (1970..=9999).contains(&y) { format!("{y}.{m:02}.{d:02}") } else { "????.??.??".to_owned() }
}

/// The proleptic Gregorian date of a day count since 1970-01-01 (H. Hinnant's algorithm).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days.saturating_add(719_468);
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

/// Movetext lines of at most [`LINE_MAX`] characters (UTF-16 units, as the former server
/// measured them), tokens separated by one space.
struct Movetext<'a> {
    out: &'a mut String,
    line: String,
    line_len: usize,
}

impl Movetext<'_> {
    fn emit(&mut self, token: &str) {
        let len = token.encode_utf16().count();
        if !self.line.is_empty() && self.line_len + 1 + len > LINE_MAX {
            self.out.push_str(&self.line);
            self.out.push('\n');
            self.line.clear();
            self.line_len = 0;
        }
        if !self.line.is_empty() {
            self.line.push(' ');
            self.line_len += 1;
        }
        self.line.push_str(token);
        self.line_len += len;
    }

    /// Writes a comment; false when it has no word.
    fn comment(&mut self, comment: &PgnComment) -> bool {
        let words: Vec<String> = match comment {
            PgnComment::Text(text) => text.split(' ').map(comment_word).collect(),
            PgnComment::Words(words) => words.iter().map(|w| comment_word(w)).collect(),
        };
        let words: Vec<&String> = words.iter().filter(|w| !w.is_empty()).collect();
        let last = words.len().wrapping_sub(1);
        for (i, word) in words.iter().enumerate() {
            let mut token = String::with_capacity(word.len() + 2);
            if i == 0 {
                token.push('{');
            }
            token.push_str(word);
            if i == last {
                token.push('}');
            }
            self.emit(&token);
        }
        !words.is_empty()
    }

    fn finish(self) {
        self.out.push_str(&self.line);
        self.out.push('\n');
    }
}

/// The PGN of a game (see [`PgnTags`]): tag pairs, an empty line, the movetext wrapped at 80
/// columns, the end reason as a comment, the result.
pub(crate) fn write_pgn(game: &ChessGame, tags: &PgnTags) -> String {
    let result = game.result_string();
    let start = game.start_position();
    let mut out = String::with_capacity(512 + game.ply() * 12);
    let mut tag = |name: &str, value: &str| {
        out.push('[');
        out.push_str(name);
        out.push_str(" \"");
        push_escaped(&mut out, value);
        out.push_str("\"]\n");
    };
    let non_empty = |v: &Option<String>| v.as_deref().filter(|s| !s.is_empty()).map(str::to_owned);
    tag("Event", tags.event.as_deref().unwrap_or("Casual game"));
    tag("Site", tags.site.as_deref().unwrap_or("Scacelith"));
    tag("Date", &non_empty(&tags.date).unwrap_or_else(today_utc));
    tag("Round", tags.round.as_deref().unwrap_or("-"));
    tag("White", tags.white.as_deref().unwrap_or("?"));
    tag("Black", tags.black.as_deref().unwrap_or("?"));
    tag("Result", result);
    for (name, value) in &tags.after_result {
        tag(name, value);
    }
    if *start != Position::start() {
        tag("SetUp", "1");
        tag("FEN", &start.fen());
    }
    tag("TimeControl", &non_empty(&tags.time_control).unwrap_or_else(|| "?".to_owned()));
    tag("Termination", game.reason().termination(game.status()));
    for (name, value) in &tags.extra {
        tag(name, value);
    }
    out.push('\n');

    let mut text = Movetext { out: &mut out, line: String::with_capacity(96), line_len: 0 };
    let mut move_no = u64::from(start.fullmove());
    let mut side = start.side();
    let mut after_comment = false;
    for (i, san) in game.san_moves().iter().enumerate() {
        if side == Color::White {
            text.emit(&format!("{move_no}."));
        } else if i == 0 || after_comment {
            text.emit(&format!("{move_no}..."));
        }
        text.emit(san);
        after_comment = match tags.comments.get(i) {
            Some(Some(comment)) => text.comment(comment),
            _ => false,
        };
        if side == Color::Black {
            move_no += 1;
        }
        side = side.opposite();
    }
    if game.status() != GameStatus::Ongoing {
        let reason = game.reason().text();
        if !reason.is_empty() {
            for word in format!("{{{reason}}}").split(' ') {
                text.emit(word);
            }
        }
    }
    text.emit(result);
    text.finish();
    out
}

#[cfg(test)]
mod tests {
    use super::civil_from_days;

    #[test]
    fn civil_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(20_724), (2026, 9, 28));
        assert_eq!(civil_from_days(-1), (1969, 12, 31));
        assert_eq!(civil_from_days(11_016), (2000, 2, 29));
        assert_eq!(civil_from_days(2_932_896), (9999, 12, 31));
    }
}
