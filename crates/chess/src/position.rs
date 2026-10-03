//! The position: board, legal move generation, single-move validation, make, game-state
//! predicates, the protocol digest, the Zobrist key and perft. FEN is in `fen.rs`, SAN and UCI in
//! `notation.rs`.
//!
//! Internally a move is a `u32` "im" = protocol `u16 | flags << 16` (flags = the protocol's
//! MoveFlag bits without Check/Mate), so a validated move carries what `make` needs.
//!
//! Hot path (per call, no allocation): `is_legal` = one pseudo-legality test + one attack test;
//! `play` = validation + incremental make (Zobrist, counts, king squares) + a check test, plus a
//! search for one legal reply when the move gives check (mate detection).

use std::fmt;
use std::sync::LazyLock;

use crate::tables::{
    B_BISHOP, B_KNIGHT, B_PAWN, B_QUEEN, B_ROOK, BISHOP, CASTLE_MASK, CR_BK, CR_BQ, CR_WK, CR_WQ, DIAG_STEPS,
    DIR, F_CAPTURE, F_CASTLE_K, F_CASTLE_Q, F_CHECK, F_DOUBLE, F_EP, F_MATE, F_PROMO, KING, KING_STEPS,
    KNIGHT, KNIGHT_STEPS, ORTH_STEPS, PAWN, PIECE_ASCII, QUEEN, QUEEN_STEPS, ROOK, STEP, W_BISHOP, W_KNIGHT,
    W_PAWN, W_QUEEN, W_ROOK, ZOBRIST, delta_index, on_board, s64, s88, step,
};
use crate::types::{Color, FNV_OFFSET, MoveFlags, Piece, fnv_step};

/// The FEN of the standard starting position.
pub const START_FEN: &str = "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1";

/// The deepest [`Position::perft`] accepted.
pub const MAX_PERFT_DEPTH: u32 = 64;

/// Size of a move buffer. A position accepted by [`Position::from_fen`] has at most 15 pieces
/// besides the king, so at most 15 * 27 (queens) + 10 (king and castling) = 415 legal moves.
pub(crate) const MAX_MOVES: usize = 512;

static START: LazyLock<Position> =
    LazyLock::new(|| Position::from_fen(START_FEN).expect("the standard start position is a valid FEN"));

/// A chess position with the game's rules (FIDE), legal move generation and notation.
///
/// Construct with [`Position::start`] or [`Position::from_fen`]. The position is a small `Copy`
/// value (168 bytes): clone it to explore moves.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Position {
    /// 0x88 board of piece codes.
    pub(crate) board: [u8; 128],
    /// King squares (0x88), by colour.
    pub(crate) kings: [u8; 2],
    /// Piece counts by code.
    pub(crate) counts: [u8; 16],
    pub(crate) side: Color,
    /// Castling rights (`CR_*` bits).
    pub(crate) castling: u8,
    /// En passant square (0x88), only when an en passant capture is legal.
    pub(crate) ep: Option<u8>,
    pub(crate) halfmove: u32,
    pub(crate) fullmove: u32,
    /// Zobrist key, updated incrementally.
    pub(crate) key: u64,
}

/// [`Position::play`] was given a move that is not legal in the position.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IllegalMove {
    /// The refused move.
    pub mv: u16,
}

impl fmt::Display for IllegalMove {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "illegal move {}", self.mv)
    }
}

impl std::error::Error for IllegalMove {}

/// [`Position::perft`] was asked for a depth above [`MAX_PERFT_DEPTH`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PerftDepthError(pub u32);

impl fmt::Display for PerftDepthError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "perft depth {} too large (at most {MAX_PERFT_DEPTH})", self.0)
    }
}

impl std::error::Error for PerftDepthError {}

/// Receives the generated moves.
pub(crate) trait MoveSink {
    /// One legal move (im).
    fn push(&mut self, im: u32);

    /// True when the generator may stop after the current square.
    fn satisfied(&self) -> bool {
        false
    }
}

/// A buffer of generated moves (im), in generation order.
pub(crate) struct MoveList {
    moves: [u32; MAX_MOVES],
    len: usize,
}

impl MoveList {
    pub(crate) fn new() -> MoveList {
        MoveList { moves: [0; MAX_MOVES], len: 0 }
    }

    pub(crate) fn as_slice(&self) -> &[u32] {
        &self.moves[..self.len]
    }
}

impl MoveSink for MoveList {
    #[inline]
    fn push(&mut self, im: u32) {
        // Never full (see MAX_MOVES); a move beyond the buffer would be dropped, not a panic.
        if let Some(slot) = self.moves.get_mut(self.len) {
            *slot = im;
            self.len += 1;
        }
    }
}

/// Stops at the first square with a legal move.
struct AnyMove(bool);

impl MoveSink for AnyMove {
    #[inline]
    fn push(&mut self, _im: u32) {
        self.0 = true;
    }

    #[inline]
    fn satisfied(&self) -> bool {
        self.0
    }
}

/// Counts the moves (perft leaves).
struct MoveCount(u64);

impl MoveSink for MoveCount {
    #[inline]
    fn push(&mut self, _im: u32) {
        self.0 += 1;
    }
}

/// Internal move from a 0..63 origin and a 0x88 target.
#[inline]
fn im(from64: u8, to: usize, flags: u32) -> u32 {
    u32::from(from64) | u32::from(s64(to)) << 6 | flags << 16
}

/// Pushes the four promotions of a pawn move, queen first (the game's order).
#[inline]
fn push_promotions(sink: &mut impl MoveSink, base: u32) {
    for t in [QUEEN, ROOK, BISHOP, KNIGHT] {
        sink.push(base | u32::from(t) << 12);
    }
}

impl Default for Position {
    /// The standard starting position.
    fn default() -> Position {
        Position::start()
    }
}

impl fmt::Debug for Position {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("Position").field(&self.fen()).finish()
    }
}

impl Position {
    /// The standard starting position.
    #[must_use]
    pub fn start() -> Position {
        *START
    }

    /// An empty board (no king: only a base for the FEN parser).
    pub(crate) const fn empty() -> Position {
        Position {
            board: [0; 128],
            kings: [0; 2],
            counts: [0; 16],
            side: Color::White,
            castling: 0,
            ep: None,
            halfmove: 0,
            fullmove: 1,
            key: 0,
        }
    }

    /// Side to move.
    #[must_use]
    pub fn side(&self) -> Color {
        self.side
    }

    /// Castling rights bits (see [`crate::castling`]).
    #[must_use]
    pub fn castling(&self) -> u8 {
        self.castling
    }

    /// En passant square 0..63, only when an en passant capture is legal.
    #[must_use]
    pub fn ep_square(&self) -> Option<u8> {
        self.ep.map(|e| s64(usize::from(e)))
    }

    /// Halfmove clock (plies since the last capture or pawn move).
    #[must_use]
    pub fn halfmove(&self) -> u32 {
        self.halfmove
    }

    /// Fullmove number (starts at 1, incremented after Black's move).
    #[must_use]
    pub fn fullmove(&self) -> u32 {
        self.fullmove
    }

    /// The piece on a square (`sq & 63`).
    #[must_use]
    pub fn piece_at(&self, sq: u8) -> Option<Piece> {
        Piece::from_code(self.board[s88(sq & 63)])
    }

    /// The square 0..63 of a king.
    #[must_use]
    pub fn king_square(&self, color: Color) -> u8 {
        s64(usize::from(self.kings[color.index()]))
    }

    /// The side to move is in check.
    #[must_use]
    pub fn in_check(&self) -> bool {
        self.attacked(self.king_sq(self.side), self.side.opposite())
    }

    /// Is the square (`sq & 63`) attacked by a piece of colour `by`?
    #[must_use]
    pub fn is_attacked(&self, sq: u8, by: Color) -> bool {
        self.attacked(s88(sq & 63), by)
    }

    /// The legal moves (protocol u16, promotions expanded), in the game's generation order:
    /// squares a1, b1 .. h8, then castling.
    #[must_use]
    pub fn legal_moves(&self) -> Vec<u16> {
        let mut list = MoveList::new();
        self.generate(&mut list);
        list.as_slice().iter().map(|&m| m as u16).collect()
    }

    /// Number of legal moves.
    #[must_use]
    pub fn legal_move_count(&self) -> usize {
        let mut count = MoveCount(0);
        self.generate(&mut count);
        count.0 as usize
    }

    /// True when the move (exact u16: promotion 2..5 for a promotion, 0 otherwise) is legal.
    #[must_use]
    pub fn is_legal(&self, m: u16) -> bool {
        self.validate(m).is_some()
    }

    /// Applies a legal move and returns its flags (with `CHECK` / `MATE`). The position is
    /// unchanged when the move is illegal.
    pub fn play(&mut self, m: u16) -> Result<MoveFlags, IllegalMove> {
        let im = self.validate(m).ok_or(IllegalMove { mv: m })?;
        Ok(MoveFlags::from_bits(self.play_validated(im) as u8))
    }

    /// The side to move has at least one legal move.
    #[must_use]
    pub fn has_legal_move(&self) -> bool {
        let mut any = AnyMove(false);
        self.generate(&mut any);
        any.0
    }

    /// The side to move is checkmated.
    #[must_use]
    pub fn is_checkmate(&self) -> bool {
        self.in_check() && !self.has_legal_move()
    }

    /// The side to move is stalemated.
    #[must_use]
    pub fn is_stalemate(&self) -> bool {
        !self.in_check() && !self.has_legal_move()
    }

    /// Dead position: K v K, K+B v K, K+N v K, or only bishops, all on squares of one colour.
    #[must_use]
    pub fn has_insufficient_material(&self) -> bool {
        let c = &self.counts;
        let count = |code: u8| c[usize::from(code)];
        if count(W_PAWN) | count(B_PAWN) | count(W_ROOK) | count(B_ROOK) | count(W_QUEEN) | count(B_QUEEN)
            != 0
        {
            return false;
        }
        let knights = count(W_KNIGHT) + count(B_KNIGHT);
        let bishops = count(W_BISHOP) + count(B_BISHOP);
        if bishops == 0 {
            return knights <= 1;
        }
        if knights != 0 {
            return false;
        }
        let (mut light, mut dark) = (false, false);
        for sq in 0..64u8 {
            if self.board[s88(sq)] & 7 == BISHOP {
                if ((sq >> 3) + (sq & 7)) & 1 == 1 {
                    light = true;
                } else {
                    dark = true;
                }
            }
        }
        !(light && dark)
    }

    /// `chess::Position::canColorMate` (FIDE 6.9 approximation): false when `color` has a bare
    /// king, when it has a single minor piece and the opponent a bare king, or when the position
    /// is dead; true otherwise (a helpmate is assumed possible).
    #[must_use]
    pub fn can_color_mate(&self, color: Color) -> bool {
        if self.has_insufficient_material() {
            return false;
        }
        let mine = (color as usize) << 3;
        let theirs = mine ^ 8;
        let c = &self.counts;
        let n: u32 = (1..=5).map(|t| u32::from(c[mine | t])).sum();
        let t: u32 = (1..=5).map(|k| u32::from(c[theirs | k])).sum();
        if n == 0 {
            return false;
        }
        !(n == 1
            && u32::from(c[mine | KNIGHT as usize]) + u32::from(c[mine | BISHOP as usize]) == 1
            && t == 0)
    }

    /// The protocol `posHash`: FNV-1a 32 of the first four FEN fields
    /// ("placement side castling ep").
    #[must_use]
    pub fn digest(&self) -> u32 {
        let mut h = FNV_OFFSET;
        self.write_fen_prefix(|b| h = fnv_step(h, b));
        h
    }

    /// The 64-bit Zobrist key (placement, side to move, castling rights, en passant square when a
    /// capture is possible), equal to the game's `chess::Position::hash()`.
    #[must_use]
    pub fn key(&self) -> u64 {
        self.key
    }

    /// The repetition identity as 16 lowercase hex digits (the Zobrist key).
    #[must_use]
    pub fn repetition_key(&self) -> String {
        format!("{:016x}", self.key)
    }

    /// Number of leaf nodes of the legal move tree of `depth` plies (bulk counting at the last
    /// ply). Depth 0 is 1.
    pub fn perft(&self, depth: u32) -> Result<u64, PerftDepthError> {
        match depth {
            0 => Ok(1),
            1..=MAX_PERFT_DEPTH => Ok(self.perft_rec(depth)),
            _ => Err(PerftDepthError(depth)),
        }
    }

    // ---- internals ------------------------------------------------------------------------

    fn perft_rec(&self, depth: u32) -> u64 {
        if depth <= 1 {
            let mut count = MoveCount(0);
            self.generate(&mut count);
            return count.0;
        }
        let mut list = MoveList::new();
        self.generate(&mut list);
        let mut total = 0;
        for &m in list.as_slice() {
            let mut child = *self;
            child.make(m);
            total += child.perft_rec(depth - 1);
        }
        total
    }

    /// The piece code on a 0x88 square (masked: never out of bounds).
    #[inline(always)]
    fn at(&self, s: usize) -> u8 {
        self.board[s & 0x7f]
    }

    #[inline(always)]
    fn king_sq(&self, color: Color) -> usize {
        usize::from(self.kings[color.index()])
    }

    /// Emits the bytes of the first four FEN fields (shared by `fen()` and `digest()`).
    pub(crate) fn write_fen_prefix(&self, mut emit: impl FnMut(u8)) {
        for r in (0..8).rev() {
            let mut empty = 0u8;
            for f in 0..8 {
                let p = self.board[r * 16 + f];
                if p == 0 {
                    empty += 1;
                    continue;
                }
                if empty > 0 {
                    emit(b'0' + empty);
                    empty = 0;
                }
                emit(PIECE_ASCII[usize::from(p & 15)]);
            }
            if empty > 0 {
                emit(b'0' + empty);
            }
            if r > 0 {
                emit(b'/');
            }
        }
        emit(b' ');
        emit(match self.side {
            Color::White => b'w',
            Color::Black => b'b',
        });
        emit(b' ');
        if self.castling == 0 {
            emit(b'-');
        }
        for (bit, letter) in [(CR_WK, b'K'), (CR_WQ, b'Q'), (CR_BK, b'k'), (CR_BQ, b'q')] {
            if self.castling & bit != 0 {
                emit(letter);
            }
        }
        emit(b' ');
        match self.ep {
            None => emit(b'-'),
            Some(e) => {
                emit(b'a' + (e & 7));
                emit(b'1' + (e >> 4));
            }
        }
    }

    /// Recomputes the Zobrist key from scratch.
    pub(crate) fn rehash(&mut self) {
        let z = &ZOBRIST;
        let mut key = 0;
        for sq in 0..64u8 {
            let p = self.board[s88(sq)];
            if p != 0 {
                key ^= z.pieces[usize::from(p) * 64 + usize::from(sq)];
            }
        }
        key ^= z.castling[usize::from(self.castling)];
        if let Some(e) = self.ep {
            key ^= z.ep[usize::from(e & 7)];
        }
        if self.side == Color::Black {
            key ^= z.side;
        }
        self.key = key;
    }

    /// Is the 0x88 square `s` attacked by colour `by`?
    #[inline]
    pub(crate) fn attacked(&self, s: usize, by: Color) -> bool {
        self.attacked_with(s, by, |a| self.at(a))
    }

    /// Attack test on the board seen through `at` (the piece code of an on-board square), which
    /// lets `safe_after` test a move without playing it.
    #[inline(always)]
    fn attacked_with(&self, s: usize, by: Color, at: impl Fn(usize) -> u8) -> bool {
        let (pawn, a1, a2) = match by {
            Color::White => (W_PAWN, step(s, -15), step(s, -17)),
            Color::Black => (B_PAWN, step(s, 15), step(s, 17)),
        };
        if (on_board(a1) && at(a1) == pawn) || (on_board(a2) && at(a2) == pawn) {
            return true;
        }
        let own = (by as u8) << 3;
        let (knight, king) = (own | KNIGHT, own | KING);
        for i in 0..8 {
            let a = step(s, KNIGHT_STEPS[i]);
            if on_board(a) && at(a) == knight {
                return true;
            }
            let a = step(s, KING_STEPS[i]);
            if on_board(a) && at(a) == king {
                return true;
            }
        }
        let (rook, bishop, queen) = (own | ROOK, own | BISHOP, own | QUEEN);
        let ray = |d: isize, slider: u8| {
            let mut a = step(s, d);
            while on_board(a) {
                let p = at(a);
                if p != 0 {
                    return p == slider || p == queen;
                }
                a = step(a, d);
            }
            false
        };
        ORTH_STEPS.iter().any(|&d| ray(d, rook)) || DIAG_STEPS.iter().any(|&d| ray(d, bishop))
    }

    /// Does the pseudo-legal, non-castling move `from -> to` (0x88) leave the mover's king safe?
    /// Tests the board as it would be after the move, without changing it.
    fn safe_after(&self, from: usize, to: usize, flags: u32) -> bool {
        let us = self.side;
        let piece = self.at(from);
        let ep_victim = if flags & F_EP != 0 {
            match us {
                Color::White => step(to, -16),
                Color::Black => step(to, 16),
            }
        } else {
            usize::MAX
        };
        let ksq = if piece & 7 == KING { to } else { self.king_sq(us) };
        !self.attacked_with(ksq, us.opposite(), |a| {
            if a == to {
                piece
            } else if a == from || a == ep_victim {
                0
            } else {
                self.at(a)
            }
        })
    }

    /// Legality filter of the generator (castling is generated fully checked).
    #[inline]
    fn legal_pseudo(&self, from: usize, to: usize, flags: u32, check: bool, ksq: usize) -> bool {
        if from == ksq || check || flags & F_EP != 0 {
            return self.safe_after(from, to, flags);
        }
        // Not in check: a piece that is not on a line with its king, or that stays on the same
        // ray from the king, cannot expose the king.
        let d = DIR[delta_index(ksq, from)];
        if d == 0 || DIR[delta_index(ksq, to)] == d {
            return true;
        }
        self.safe_after(from, to, flags)
    }

    /// Generates the legal moves into `sink`, in the game's order: squares a1, b1 .. h8 (pawn
    /// pushes, double push, captures towards the a-file then the h-file; knight and king steps;
    /// slider rays diagonals first), then castling. Stops after a square when the sink is
    /// satisfied.
    pub(crate) fn generate(&self, sink: &mut impl MoveSink) {
        let us = self.side;
        let them = us.opposite();
        let ksq = self.king_sq(us);
        let check = self.attacked(ksq, them);
        let own = (us as u8) << 3;
        let (up, start_rank, last_rank): (isize, usize, usize) = match us {
            Color::White => (16, 1, 7),
            Color::Black => (-16, 6, 0),
        };
        let ep = self.ep.map(usize::from);
        for sq in 0..64u8 {
            let s = s88(sq);
            let p = self.board[s];
            if p == 0 || p & 8 != own {
                continue;
            }
            match p & 7 {
                PAWN => {
                    // A pawn never stands on its last rank: the push target is on the board.
                    let to = step(s, up);
                    if self.at(to) == 0 {
                        if to >> 4 == last_rank {
                            if self.legal_pseudo(s, to, F_PROMO, check, ksq) {
                                push_promotions(sink, im(sq, to, F_PROMO));
                            }
                        } else {
                            if self.legal_pseudo(s, to, 0, check, ksq) {
                                sink.push(im(sq, to, 0));
                            }
                            let to2 = step(to, up);
                            if s >> 4 == start_rank
                                && self.at(to2) == 0
                                && self.legal_pseudo(s, to2, F_DOUBLE, check, ksq)
                            {
                                sink.push(im(sq, to2, F_DOUBLE));
                            }
                        }
                    }
                    for side in [-1, 1] {
                        let to = step(s, up + side);
                        if !on_board(to) {
                            continue;
                        }
                        let c = self.at(to);
                        if c != 0 && c & 8 != own {
                            if to >> 4 == last_rank {
                                if self.legal_pseudo(s, to, F_PROMO | F_CAPTURE, check, ksq) {
                                    push_promotions(sink, im(sq, to, F_PROMO | F_CAPTURE));
                                }
                            } else if self.legal_pseudo(s, to, F_CAPTURE, check, ksq) {
                                sink.push(im(sq, to, F_CAPTURE));
                            }
                        } else if Some(to) == ep && self.safe_after(s, to, F_EP) {
                            sink.push(im(sq, to, F_CAPTURE | F_EP));
                        }
                    }
                }
                KNIGHT | KING => {
                    let steps = if p & 7 == KNIGHT { &KNIGHT_STEPS } else { &KING_STEPS };
                    for &d in steps {
                        let to = step(s, d);
                        if !on_board(to) {
                            continue;
                        }
                        let c = self.at(to);
                        if c != 0 && c & 8 == own {
                            continue;
                        }
                        let fl = if c != 0 { F_CAPTURE } else { 0 };
                        if self.legal_pseudo(s, to, fl, check, ksq) {
                            sink.push(im(sq, to, fl));
                        }
                    }
                }
                t @ (BISHOP | ROOK | QUEEN) => {
                    let dirs = match t {
                        BISHOP => &QUEEN_STEPS[..4],
                        ROOK => &QUEEN_STEPS[4..],
                        _ => &QUEEN_STEPS[..],
                    };
                    for &d in dirs {
                        let mut to = step(s, d);
                        while on_board(to) {
                            let c = self.at(to);
                            if c == 0 {
                                if self.legal_pseudo(s, to, 0, check, ksq) {
                                    sink.push(im(sq, to, 0));
                                }
                                to = step(to, d);
                                continue;
                            }
                            if c & 8 != own && self.legal_pseudo(s, to, F_CAPTURE, check, ksq) {
                                sink.push(im(sq, to, F_CAPTURE));
                            }
                            break;
                        }
                    }
                }
                _ => {}
            }
            if sink.satisfied() {
                return;
            }
        }
        // Castling (fully checked: rights, empty squares, not out of / through / into check).
        // With a right, the king and the rook stand on their initial squares.
        let (ks, qs, k) = match us {
            Color::White => (CR_WK, CR_WQ, 0x04usize),
            Color::Black => (CR_BK, CR_BQ, 0x74usize),
        };
        if self.castling & (ks | qs) != 0 && !check {
            let k64 = s64(k);
            let empty = |s: usize| self.at(s) == 0;
            if self.castling & ks != 0
                && empty(k + 1)
                && empty(k + 2)
                && !self.attacked(k + 1, them)
                && !self.attacked(k + 2, them)
            {
                sink.push(im(k64, k + 2, F_CASTLE_K));
            }
            if self.castling & qs != 0
                && empty(k - 1)
                && empty(k - 2)
                && empty(k - 3)
                && !self.attacked(k - 1, them)
                && !self.attacked(k - 2, them)
            {
                sink.push(im(k64, k - 2, F_CASTLE_Q));
            }
        }
    }

    /// Validates one protocol move without generating the move list: the internal move when
    /// legal. Strict: a promotion needs 2..5 in the promotion field, any other move 0.
    pub(crate) fn validate(&self, m: u16) -> Option<u32> {
        if m > 0x7fff {
            return None;
        }
        let from64 = (m & 63) as u8;
        let to64 = (m >> 6 & 63) as u8;
        let promo = (m >> 12) as u8;
        if from64 == to64 {
            return None;
        }
        let (from, to) = (s88(from64), s88(to64));
        let us = self.side as u8;
        let p = self.board[from];
        if p == 0 || p >> 3 != us {
            return None;
        }
        let cap = self.board[to];
        if cap != 0 && cap >> 3 == us {
            return None;
        }
        let t = p & 7;
        let mut flags = if cap != 0 { F_CAPTURE } else { 0 };
        let delta = to as isize - from as isize;
        if t == PAWN {
            let (up, start_rank, last_rank): (isize, usize, usize) = match self.side {
                Color::White => (16, 1, 7),
                Color::Black => (-16, 6, 0),
            };
            if delta == up {
                if cap != 0 {
                    return None;
                }
            } else if delta == 2 * up {
                if cap != 0 || self.at(step(from, up)) != 0 || from >> 4 != start_rank {
                    return None;
                }
                flags |= F_DOUBLE;
            } else if delta == up - 1 || delta == up + 1 {
                if cap == 0 {
                    if self.ep != Some(to as u8) {
                        return None;
                    }
                    flags |= F_CAPTURE | F_EP;
                }
            } else {
                return None;
            }
            if to >> 4 == last_rank {
                if !(KNIGHT..=QUEEN).contains(&promo) {
                    return None;
                }
                flags |= F_PROMO;
            } else if promo != 0 {
                return None;
            }
        } else {
            if promo != 0 {
                return None;
            }
            let idx = delta_index(to, from);
            match t {
                KNIGHT => {
                    if STEP[idx] & (1 << KNIGHT) == 0 {
                        return None;
                    }
                }
                KING => {
                    if STEP[idx] & (1 << KING) == 0 {
                        return self.validate_castling(m, from, delta);
                    }
                }
                _ => {
                    let d = DIR[idx];
                    if d == 0 {
                        return None;
                    }
                    let diagonal = matches!(d, 15 | 17 | -15 | -17);
                    if (t == ROOK && diagonal) || (t == BISHOP && !diagonal) {
                        return None;
                    }
                    let mut s = step(from, isize::from(d));
                    while s != to {
                        if self.at(s) != 0 {
                            return None;
                        }
                        s = step(s, isize::from(d));
                    }
                }
            }
        }
        if !self.safe_after(from, to, flags) {
            return None;
        }
        Some(u32::from(m) | flags << 16)
    }

    /// Castling as the king's two-square move from its home square: right, empty path, and the
    /// king not in check, not passing through or landing on an attacked square.
    fn validate_castling(&self, m: u16, from: usize, delta: isize) -> Option<u32> {
        let them = self.side.opposite();
        let (home, ks, qs) = match self.side {
            Color::White => (0x04, CR_WK, CR_WQ),
            Color::Black => (0x74, CR_BK, CR_BQ),
        };
        if from != home {
            return None;
        }
        let empty = |s: usize| self.at(s) == 0;
        let safe = |s: usize| !self.attacked(s, them);
        match delta {
            2 if self.castling & ks != 0
                && empty(from + 1)
                && empty(from + 2)
                && safe(from)
                && safe(from + 1)
                && safe(from + 2) =>
            {
                Some(u32::from(m) | F_CASTLE_K << 16)
            }
            -2 if self.castling & qs != 0
                && empty(from - 1)
                && empty(from - 2)
                && empty(from - 3)
                && safe(from)
                && safe(from - 1)
                && safe(from - 2) =>
            {
                Some(u32::from(m) | F_CASTLE_Q << 16)
            }
            _ => None,
        }
    }

    /// Sets the en passant square (0x88) when an en passant capture onto it is legal for the
    /// side to move (key updated).
    pub(crate) fn set_ep(&mut self, e: usize) {
        let z = &ZOBRIST;
        if let Some(old) = self.ep.take() {
            self.key ^= z.ep[usize::from(old & 7)];
        }
        let pawn = (self.side as u8) << 3 | PAWN;
        let (c1, c2) = match self.side {
            Color::White => (step(e, -15), step(e, -17)),
            Color::Black => (step(e, 15), step(e, 17)),
        };
        let can_capture = |c: usize| on_board(c) && self.at(c) == pawn && self.safe_after(c, e, F_EP);
        if can_capture(c1) || can_capture(c2) {
            self.ep = Some(e as u8);
            self.key ^= z.ep[e & 7];
        }
    }

    /// Makes a validated internal move.
    pub(crate) fn make(&mut self, m: u32) {
        let from64 = (m & 63) as u8;
        let to64 = (m >> 6 & 63) as u8;
        let flags = m >> 16;
        let (from, to) = (s88(from64), s88(to64));
        let us = self.side;
        let piece = self.board[from];
        let cap = self.board[to];
        let z = &ZOBRIST;
        let zp = |code: u8, sq: u8| z.pieces[usize::from(code & 15) * 64 + usize::from(sq & 63)];
        let mut key = self.key;
        if let Some(e) = self.ep.take() {
            key ^= z.ep[usize::from(e & 7)];
        }
        key ^= z.castling[usize::from(self.castling)];
        if cap != 0 {
            key ^= zp(cap, to64);
            self.counts[usize::from(cap & 15)] -= 1;
        }
        if flags & F_EP != 0 {
            let cs = match us {
                Color::White => step(to, -16),
                Color::Black => step(to, 16),
            };
            let cp = self.at(cs);
            self.board[cs & 0x7f] = 0;
            key ^= zp(cp, s64(cs));
            self.counts[usize::from(cp & 15)] -= 1;
        }
        key ^= zp(piece, from64);
        self.board[from] = 0;
        let mut placed = piece;
        if flags & F_PROMO != 0 {
            placed = (us as u8) << 3 | (m >> 12 & 7) as u8;
            self.counts[usize::from(piece & 15)] -= 1;
            self.counts[usize::from(placed & 15)] += 1;
        }
        self.board[to] = placed;
        key ^= zp(placed, to64);
        if piece & 7 == KING {
            self.kings[us.index()] = to as u8;
            if flags & (F_CASTLE_K | F_CASTLE_Q) != 0 {
                let (rf, rt) = if flags & F_CASTLE_K != 0 { (to + 1, to - 1) } else { (to - 2, to + 1) };
                let rook = self.at(rf);
                self.board[rf & 0x7f] = 0;
                self.board[rt & 0x7f] = rook;
                key ^= zp(rook, s64(rf)) ^ zp(rook, s64(rt));
            }
        }
        self.castling &= CASTLE_MASK[usize::from(from64)] & CASTLE_MASK[usize::from(to64)];
        key ^= z.castling[usize::from(self.castling)];
        self.halfmove = if piece & 7 == PAWN || cap != 0 { 0 } else { self.halfmove.saturating_add(1) };
        if us == Color::Black {
            self.fullmove = self.fullmove.saturating_add(1);
        }
        self.side = us.opposite();
        self.key = key ^ z.side;
        if flags & F_DOUBLE != 0 {
            self.set_ep((from + to) >> 1);
        }
    }

    /// Plays a validated internal move; returns its MoveFlag bits with Check / Mate.
    pub(crate) fn play_validated(&mut self, m: u32) -> u32 {
        self.make(m);
        let mut flags = m >> 16;
        if self.in_check() {
            flags |= F_CHECK;
            if !self.has_legal_move() {
                flags |= F_MATE;
            }
        }
        flags
    }

    /// The generated legal moves (im), for the SAN reader.
    pub(crate) fn move_list(&self) -> MoveList {
        let mut list = MoveList::new();
        self.generate(&mut list);
        list
    }
}
