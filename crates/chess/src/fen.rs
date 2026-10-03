//! FEN parsing and printing, exactly like the game's `chess::Position::setFEN` / `fen()`.

use crate::position::Position;
use crate::tables::{
    B_KING, B_ROOK, CR_BK, CR_BQ, CR_WK, CR_WQ, KING, PAWN, W_KING, W_ROOK, piece_of_ascii, s88, step,
};
use crate::types::{Color, parse_square};

/// A move counter: 1..=9 ASCII digits, no sign.
fn parse_counter(s: &str) -> Option<u32> {
    if s.is_empty() || s.len() > 9 || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

impl Position {
    /// Parses a FEN like the game's `chess::Position::setFEN`.
    ///
    /// Fields are separated by runs of spaces, tabs, CR or LF; the halfmove clock and fullmove
    /// number are optional (default 0 and 1; a fullmove 0 becomes 1). The position is normalised:
    /// castling rights whose king or rook is not on its initial square are dropped, and the en
    /// passant square is kept only when an en passant capture is legal (an implausible one is
    /// silently dropped).
    ///
    /// `None` on malformed or impossible input: not 4 to 6 fields, a bad placement, missing or
    /// extra kings, more than 16 pieces or 8 pawns for a side, pawns on the first or last rank,
    /// a bad side, castling field, en passant square or counter, or the side not to move in
    /// check.
    #[must_use]
    pub fn from_fen(fen: &str) -> Option<Position> {
        let mut fields = [""; 6];
        let mut n = 0;
        for field in fen.split([' ', '\t', '\n', '\r']).filter(|f| !f.is_empty()) {
            *fields.get_mut(n)? = field;
            n += 1;
        }
        if n < 4 {
            return None;
        }
        let mut p = Position::empty();

        let (mut rank, mut file) = (7usize, 0usize);
        for c in fields[0].bytes() {
            match c {
                b'/' => {
                    if file != 8 || rank == 0 {
                        return None;
                    }
                    rank -= 1;
                    file = 0;
                }
                b'1'..=b'8' => {
                    file += usize::from(c - b'0');
                    if file > 8 {
                        return None;
                    }
                }
                _ => {
                    let code = piece_of_ascii(c);
                    if code == 0 || file > 7 {
                        return None;
                    }
                    let s = rank * 16 + file;
                    p.board[s] = code;
                    p.counts[usize::from(code)] += 1;
                    if code & 7 == KING {
                        p.kings[usize::from(code >> 3)] = s as u8;
                    }
                    file += 1;
                }
            }
        }
        if rank != 0 || file != 8 {
            return None;
        }
        for color in 0..2 {
            let base = color << 3;
            if p.counts[base | usize::from(KING)] != 1 {
                return None;
            }
            let total: u32 = (1..=6).map(|t| u32::from(p.counts[base | t])).sum();
            if total > 16 || p.counts[base | usize::from(PAWN)] > 8 {
                return None;
            }
        }
        if (0..8).any(|f| p.board[f] & 7 == PAWN || p.board[0x70 + f] & 7 == PAWN) {
            return None;
        }

        p.side = match fields[1] {
            "w" => Color::White,
            "b" => Color::Black,
            _ => return None,
        };

        let mut rights = 0u8;
        if fields[2] != "-" {
            for c in fields[2].bytes() {
                rights |= match c {
                    b'K' => CR_WK,
                    b'Q' => CR_WQ,
                    b'k' => CR_BK,
                    b'q' => CR_BQ,
                    _ => return None,
                };
            }
        }
        // Keep only the rights whose king and rook stand on their initial squares.
        if p.board[0x04] != W_KING {
            rights &= !(CR_WK | CR_WQ);
        }
        if p.board[0x74] != B_KING {
            rights &= !(CR_BK | CR_BQ);
        }
        if p.board[0x07] != W_ROOK {
            rights &= !CR_WK;
        }
        if p.board[0x00] != W_ROOK {
            rights &= !CR_WQ;
        }
        if p.board[0x77] != B_ROOK {
            rights &= !CR_BK;
        }
        if p.board[0x70] != B_ROOK {
            rights &= !CR_BQ;
        }
        p.castling = rights;

        if n >= 5 {
            p.halfmove = parse_counter(fields[4])?;
        }
        if n >= 6 {
            p.fullmove = parse_counter(fields[5])?;
        }
        p.fullmove = p.fullmove.max(1);

        // The side that just moved cannot be in check.
        let them = p.side.opposite();
        if p.attacked(usize::from(p.kings[them.index()]), p.side) {
            return None;
        }

        p.rehash();
        if fields[3] != "-" {
            let e = s88(parse_square(fields[3])?);
            // Plausible only on the 6th (3rd) rank, behind an enemy pawn that just made a double
            // push, with its two squares empty.
            let (up, ep_rank): (isize, usize) = match p.side {
                Color::White => (16, 5),
                Color::Black => (-16, 2),
            };
            let victim = (them as u8) << 3 | PAWN;
            if e >> 4 == ep_rank
                && p.board[step(e, -up) & 0x7f] == victim
                && p.board[e] == 0
                && p.board[step(e, up) & 0x7f] == 0
            {
                p.set_ep(e);
            }
        }
        Some(p)
    }

    /// The FEN, exactly as the game's `chess::Position::fen()` prints it.
    #[must_use]
    pub fn fen(&self) -> String {
        let mut s = String::with_capacity(90);
        self.write_fen_prefix(|b| s.push(char::from(b)));
        s.push(' ');
        s.push_str(&self.halfmove.to_string());
        s.push(' ');
        s.push_str(&self.fullmove.to_string());
        s
    }
}
