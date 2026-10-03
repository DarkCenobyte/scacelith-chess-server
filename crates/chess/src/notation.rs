//! SAN output, UCI input, and the lenient SAN reader of the PGN reader (the game's
//! `chess::Position::toSAN`, `parseUCI` and `parseSAN`).

use crate::position::Position;
use crate::tables::{
    F_CAPTURE, F_CASTLE_K, F_CASTLE_Q, F_PROMO, KING, KNIGHT, PAWN, QUEEN, piece_of_ascii, s88,
};
use crate::types::{encode_move, move_from, move_promotion, move_to, square_name};

/// The SAN letters of piece types 1..=6 (index 0 unused).
const UPPER: &[u8; 7] = b" PNBRQK";

/// Square of two ASCII bytes like `b'e', b'4'` (the file letter may be upper case).
fn square_of(file: u8, rank: u8) -> Option<u8> {
    let file = file.to_ascii_lowercase();
    if (b'a'..=b'h').contains(&file) && (b'1'..=b'8').contains(&rank) {
        Some((rank - b'1') * 8 + (file - b'a'))
    } else {
        None
    }
}

impl Position {
    /// Standard Algebraic Notation of a legal move ("Nbd7", "exd8=Q+", "O-O#"), exactly as the
    /// game writes it; "" when the move is not legal. Disambiguation only counts the other pieces
    /// that can legally make the move (a pinned piece does not).
    #[must_use]
    pub fn san(&self, m: u16) -> String {
        let Some(im) = self.validate(m) else {
            return String::new();
        };
        let flags = im >> 16;
        let (from, to) = (move_from(m), move_to(m));
        let piece = self.board[s88(from)];
        let from_name = square_name(from).as_bytes();
        let mut s = String::with_capacity(8);
        if flags & F_CASTLE_K != 0 {
            s.push_str("O-O");
        } else if flags & F_CASTLE_Q != 0 {
            s.push_str("O-O-O");
        } else if piece & 7 == PAWN {
            if flags & F_CAPTURE != 0 {
                s.push(char::from(from_name[0]));
                s.push('x');
            }
            s.push_str(square_name(to));
            if flags & F_PROMO != 0 {
                s.push('=');
                s.push(char::from(UPPER[usize::from(move_promotion(m) & 7) % 7]));
            }
        } else {
            s.push(char::from(UPPER[usize::from(piece & 7) % 7]));
            // Only the other pieces of the same code can make the move ambiguous: validate theirs
            // instead of generating every legal move.
            let (mut ambiguous, mut same_file, mut same_rank) = (false, false, false);
            for other in 0..64u8 {
                if other == from
                    || self.board[s88(other)] != piece
                    || self.validate(encode_move(other, to, 0)).is_none()
                {
                    continue;
                }
                ambiguous = true;
                same_file |= other & 7 == from & 7;
                same_rank |= other >> 3 == from >> 3;
            }
            if ambiguous {
                if !same_file {
                    s.push(char::from(from_name[0]));
                } else if !same_rank {
                    s.push(char::from(from_name[1]));
                } else {
                    s.push_str(square_name(from));
                }
            }
            if flags & F_CAPTURE != 0 {
                s.push('x');
            }
            s.push_str(square_name(to));
        }
        let mut after = *self;
        after.make(im);
        if after.in_check() {
            s.push(if after.has_legal_move() { '+' } else { '#' });
        }
        s
    }

    /// Parses a UCI move ("e2e4", "e7e8q") like `chess::Position::parseUCI`: exactly 4 or 5
    /// characters, squares with an optional upper-case file letter, a promotion letter n/b/r/q in
    /// either case; `None` unless that exact move is legal ("e7e8" for a promotion, "e2e4q" and
    /// the king-takes-rook form "e1h1" are refused).
    #[must_use]
    pub fn parse_uci(&self, s: &str) -> Option<u16> {
        let b = s.as_bytes();
        if b.len() != 4 && b.len() != 5 {
            return None;
        }
        let from = square_of(b[0], b[1])?;
        let to = square_of(b[2], b[3])?;
        let promo = match b.get(4) {
            Some(&c) => {
                let t = piece_of_ascii(c) & 7;
                if !(KNIGHT..=QUEEN).contains(&t) {
                    return None;
                }
                t
            }
            None => 0,
        };
        let m = encode_move(from, to, promo);
        self.is_legal(m).then_some(m)
    }
}

/// Reads one move in lenient SAN (the game's `chess::Position::parseSAN`): the legal move the
/// text names, or `None`.
///
/// Accepted: check, mate and annotation suffixes (`+ # ! ?`), a leading move number ("12.e4",
/// "12...e5"), figurines (♘f3), castling as O-O, 0-0, o-o, OO and the long forms, promotions as
/// e8=Q, e8Q, e8/Q, e8(Q) (letter in either case), captures with x, X, : or nothing, long
/// algebraic (Ng1-f3, e2e4), over-disambiguated moves, "e.p." suffixes, plain UCI, and a
/// lower-case piece letter when the text is neither a pawn move nor legal UCI ("nf3", but "bc4"
/// is a b-pawn capture when one exists and "b1d2" is the b1 knight's move). The text must match
/// exactly one legal move ("Nd2" with two knights able to go there is refused).
#[must_use]
pub fn parse_san(pos: &Position, text: &str) -> Option<u16> {
    let mut s = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '♔' | '♚' => s.push('K'),
            '♕' | '♛' => s.push('Q'),
            '♖' | '♜' => s.push('R'),
            '♗' | '♝' => s.push('B'),
            '♘' | '♞' => s.push('N'),
            '♙' | '♟' | ' ' | '\t' | '\r' | '\n' | '\u{c}' | '\u{b}' => {}
            _ => s.push(c),
        }
    }
    // A leading move number: digits then periods.
    let digits = s.bytes().take_while(u8::is_ascii_digit).count();
    let dots = s.bytes().skip(digits).take_while(|&b| b == b'.').count();
    if digits > 0 && dots > 0 {
        s.drain(..digits + dots);
    }
    // Check, mate and annotation suffixes; "e.p." after a move.
    loop {
        if s.ends_with(['+', '#', '!', '?']) {
            s.pop();
        } else if s.len() > 4 && s.ends_with("e.p.") {
            s.truncate(s.len() - 4);
        } else {
            break;
        }
    }
    if s.is_empty() {
        return None;
    }
    let list = pos.move_list();
    let legal = list.as_slice();
    if let Some(m) = parse_san_strict(pos, &s, legal) {
        return Some(m);
    }
    // Plain UCI before the lower-case piece letter, so that "b1d2" is the knight and not "B1d2".
    if let Some(m) = pos.parse_uci(&s.to_ascii_lowercase()) {
        return Some(m);
    }
    let first = s.chars().next()?;
    if matches!(first, 'n' | 'b' | 'r' | 'q' | 'k') {
        let mut upper = String::with_capacity(s.len());
        upper.push(first.to_ascii_uppercase());
        upper.push_str(&s[1..]);
        return parse_san_strict(pos, &upper, legal);
    }
    None
}

/// `chess::Position::parseSANStrict`: the one legal move (from `legal`, internal moves) the
/// cleaned-up text names.
fn parse_san_strict(pos: &Position, s: &str, legal: &[u32]) -> Option<u16> {
    let castle: String = s.chars().map(|c| if c == '0' || c == 'o' { 'O' } else { c }).collect();
    let king_side = castle == "O-O" || castle == "OO";
    if king_side || castle == "O-O-O" || castle == "OOO" {
        return legal.iter().map(|&im| im as u16).find(|&m| {
            let (from, to) = (move_from(m), move_to(m));
            pos.board[s88(from)] & 7 == KING
                && (to & 7).abs_diff(from & 7) == 2
                && ((to & 7) > (from & 7)) == king_side
        });
    }
    let mut body = s;
    let mut piece = PAWN;
    if let Some(rest) = body.strip_prefix(['N', 'B', 'R', 'Q', 'K']) {
        piece = piece_of_ascii(body.as_bytes()[0]) & 7;
        body = rest;
    }
    let mut promo = 0;
    if piece == PAWN {
        body = body.strip_suffix(')').unwrap_or(body);
        let b = body.as_bytes();
        if b.len() >= 3 {
            let t = piece_of_ascii(b[b.len() - 1]) & 7;
            let prev = b[b.len() - 2];
            if (KNIGHT..=QUEEN).contains(&t) && matches!(prev, b'1'..=b'8' | b'=' | b'(' | b'/') {
                promo = t;
                body = body[..body.len() - 1].trim_end_matches(['=', '(', '/']);
            }
        }
    }
    // The squares: up to 4 characters once capture marks are removed, the target last.
    let mut core = [0u8; 4];
    let mut n = 0;
    for c in body.chars().filter(|c| !matches!(c, 'x' | 'X' | ':' | '-')) {
        if !c.is_ascii() || n == core.len() {
            return None;
        }
        core[n] = c as u8;
        n += 1;
    }
    if n < 2 {
        return None;
    }
    let to = square_of(core[n - 2], core[n - 1])?;
    let (mut file, mut rank) = (None, None);
    for &c in &core[..n - 2] {
        if (b'a'..=b'h').contains(&c) && file.is_none() {
            file = Some(c - b'a');
        } else if (b'1'..=b'8').contains(&c) && rank.is_none() {
            rank = Some(c - b'1');
        } else {
            return None;
        }
    }
    let mut matches = legal.iter().map(|&im| im as u16).filter(|&m| {
        let from = move_from(m);
        move_to(m) == to
            && pos.board[s88(from)] & 7 == piece
            && file.is_none_or(|f| from & 7 == f)
            && rank.is_none_or(|r| from >> 3 == r)
            && move_promotion(m) == promo
    });
    let found = matches.next()?;
    matches.next().is_none().then_some(found)
}
