//! Board geometry (0x88 mailbox) and Zobrist keys, computed at compile time.
//!
//! A 0x88 square is `rank * 16 + file`; it is on the board when `s & 0x88 == 0`, so stepping off
//! the board in any direction is one test. The public API uses the protocol squares 0..63
//! (a1 = 0, h8 = 63). Piece code = colour << 3 | type (white pawn 1 .. white king 6, black pawn 9
//! .. black king 14), 0 = empty.

pub(crate) const PAWN: u8 = 1;
pub(crate) const KNIGHT: u8 = 2;
pub(crate) const BISHOP: u8 = 3;
pub(crate) const ROOK: u8 = 4;
pub(crate) const QUEEN: u8 = 5;
pub(crate) const KING: u8 = 6;

pub(crate) const W_PAWN: u8 = PAWN;
pub(crate) const B_PAWN: u8 = 8 | PAWN;
pub(crate) const W_KNIGHT: u8 = KNIGHT;
pub(crate) const B_KNIGHT: u8 = 8 | KNIGHT;
pub(crate) const W_BISHOP: u8 = BISHOP;
pub(crate) const B_BISHOP: u8 = 8 | BISHOP;
pub(crate) const W_ROOK: u8 = ROOK;
pub(crate) const B_ROOK: u8 = 8 | ROOK;
pub(crate) const W_QUEEN: u8 = QUEEN;
pub(crate) const B_QUEEN: u8 = 8 | QUEEN;
pub(crate) const W_KING: u8 = KING;
pub(crate) const B_KING: u8 = 8 | KING;

// Move flags of an internal move (`u16 | flags << 16`): the protocol's MoveFlag bits.
pub(crate) const F_CAPTURE: u32 = 1;
pub(crate) const F_EP: u32 = 2;
pub(crate) const F_CASTLE_K: u32 = 4;
pub(crate) const F_CASTLE_Q: u32 = 8;
pub(crate) const F_DOUBLE: u32 = 16;
pub(crate) const F_PROMO: u32 = 32;
pub(crate) const F_CHECK: u32 = 64;
pub(crate) const F_MATE: u32 = 128;

pub(crate) const CR_WK: u8 = 1;
pub(crate) const CR_WQ: u8 = 2;
pub(crate) const CR_BK: u8 = 4;
pub(crate) const CR_BQ: u8 = 8;

/// 0..63 square to 0x88.
#[inline]
pub(crate) const fn s88(sq: u8) -> usize {
    ((sq >> 3) as usize) * 16 + (sq & 7) as usize
}

/// On-board 0x88 square to 0..63.
#[inline]
pub(crate) const fn s64(s: usize) -> u8 {
    ((s >> 4) * 8 + (s & 7)) as u8
}

/// Is a 0x88 square (possibly the wrapped result of stepping off the board) on the board?
///
/// Steps are at most 33 squares, so a step below a1 wraps to a value whose low byte is at least
/// 0xDF, which has bit 7 set: the test also rejects it.
#[inline]
pub(crate) const fn on_board(s: usize) -> bool {
    s & 0x88 == 0
}

/// One step of `d` from `s` (wrapping; check the result with [`on_board`]).
#[inline]
pub(crate) const fn step(s: usize, d: isize) -> usize {
    s.wrapping_add_signed(d)
}

pub(crate) const KNIGHT_STEPS: [isize; 8] = [33, 31, 18, 14, -14, -18, -31, -33];
pub(crate) const KING_STEPS: [isize; 8] = [1, -1, 16, -16, 15, 17, -15, -17];
pub(crate) const ORTH_STEPS: [isize; 4] = [1, -1, 16, -16];
pub(crate) const DIAG_STEPS: [isize; 4] = [15, 17, -15, -17];
/// Diagonals then orthogonals: bishops use 0..4, rooks 4..8, queens 0..8.
pub(crate) const QUEEN_STEPS: [isize; 8] = [15, 17, -15, -17, 1, -1, 16, -16];

/// Indexed by `a + 119 - b` for two on-board 0x88 squares: the step `d` with `b + n * d == a`
/// (n >= 1) along a queen line, 0 when not aligned.
pub(crate) static DIR: [i8; 240] = build_dir();
/// Indexed like [`DIR`]: bit `1 << KNIGHT` for a knight jump, `1 << KING` for a king step.
pub(crate) static STEP: [u8; 240] = build_step();

const fn build_dir() -> [i8; 240] {
    let mut t = [0i8; 240];
    let mut i = 0;
    while i < 8 {
        let d = QUEEN_STEPS[i];
        let mut n = 1;
        while n < 8 {
            t[(n * d + 119) as usize] = d as i8;
            n += 1;
        }
        i += 1;
    }
    t
}

const fn build_step() -> [u8; 240] {
    let mut t = [0u8; 240];
    let mut i = 0;
    while i < 8 {
        t[(KNIGHT_STEPS[i] + 119) as usize] |= 1 << KNIGHT;
        t[(KING_STEPS[i] + 119) as usize] |= 1 << KING;
        i += 1;
    }
    t
}

/// The index of a pair of on-board 0x88 squares in [`DIR`] / [`STEP`].
#[inline]
pub(crate) const fn delta_index(a: usize, b: usize) -> usize {
    (a + 119).wrapping_sub(b) % 240
}

/// Castling rights kept when a move starts or ends on a square (0..63).
pub(crate) static CASTLE_MASK: [u8; 64] = build_castle_mask();

const fn build_castle_mask() -> [u8; 64] {
    let mut t = [15u8; 64];
    t[0] = 15 & !CR_WQ;
    t[4] = 15 & !(CR_WK | CR_WQ);
    t[7] = 15 & !CR_WK;
    t[56] = 15 & !CR_BQ;
    t[60] = 15 & !(CR_BK | CR_BQ);
    t[63] = 15 & !CR_BK;
    t
}

/// FEN letter of a piece code (0 for empty and invalid codes).
pub(crate) const PIECE_ASCII: [u8; 16] =
    [0, b'P', b'N', b'B', b'R', b'Q', b'K', 0, 0, b'p', b'n', b'b', b'r', b'q', b'k', 0];

/// Piece code of a FEN letter (0 when not a piece letter).
pub(crate) const fn piece_of_ascii(c: u8) -> u8 {
    match c {
        b'P' => W_PAWN,
        b'N' => W_KNIGHT,
        b'B' => W_BISHOP,
        b'R' => W_ROOK,
        b'Q' => W_QUEEN,
        b'K' => W_KING,
        b'p' => B_PAWN,
        b'n' => B_KNIGHT,
        b'b' => B_BISHOP,
        b'r' => B_ROOK,
        b'q' => B_QUEEN,
        b'k' => B_KING,
        _ => 0,
    }
}

/// The Zobrist keys of the game's `chess::bb::kZobrist`: splitmix64 from seed 0x5CACE117C4E55
/// in the same order (pieces by colour, type 1..=6, square; then the four castling rights; then
/// the en passant files; then the side), so [`crate::Position::key`] equals
/// `chess::Position::hash()`.
pub(crate) struct Zobrist {
    /// Indexed by `piece code * 64 + square`.
    pub(crate) pieces: [u64; 16 * 64],
    /// XOR of the keys of the rights set in the index.
    pub(crate) castling: [u64; 16],
    /// Indexed by file.
    pub(crate) ep: [u64; 8],
    /// Black to move.
    pub(crate) side: u64,
}

pub(crate) static ZOBRIST: Zobrist = build_zobrist();

const fn splitmix64(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

const fn build_zobrist() -> Zobrist {
    let mut state = 0x5_CACE_117C_4E55u64;
    let mut pieces = [0u64; 16 * 64];
    let mut c = 0;
    while c < 2 {
        let mut t = 1;
        while t < 7 {
            let mut s = 0;
            while s < 64 {
                pieces[((c << 3) | t) * 64 + s] = splitmix64(&mut state);
                s += 1;
            }
            t += 1;
        }
        c += 1;
    }
    let mut rights = [0u64; 4];
    let mut i = 0;
    while i < 4 {
        rights[i] = splitmix64(&mut state);
        i += 1;
    }
    let mut castling = [0u64; 16];
    let mut m = 0;
    while m < 16 {
        let mut i = 0;
        while i < 4 {
            if m & (1 << i) != 0 {
                castling[m] ^= rights[i];
            }
            i += 1;
        }
        m += 1;
    }
    let mut ep = [0u64; 8];
    let mut f = 0;
    while f < 8 {
        ep[f] = splitmix64(&mut state);
        f += 1;
    }
    let side = splitmix64(&mut state);
    Zobrist { pieces, castling, ep, side }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zobrist_keys_are_the_games() {
        let z = &ZOBRIST;
        let piece = |code: usize, sq: usize| z.pieces[code * 64 + sq];
        assert_eq!(piece(1, 0), 0x0643_f2c4_c9bd_bdc9); // white pawn a1
        assert_eq!(piece(1, 1), 0xec83_6183_2afc_24a9); // white pawn b1
        assert_eq!(piece(6, 63), 0x5ae5_60d8_7854_d77c); // white king h8
        assert_eq!(piece(9, 0), 0x7615_40a7_b345_b1a8); // black pawn a1
        assert_eq!(piece(14, 63), 0xcef8_8146_7204_5dec); // black king h8
        assert_eq!(z.castling[0], 0);
        assert_eq!(z.castling[1], 0xef18_590c_d2f8_f319);
        assert_eq!(z.castling[2], 0x114a_2f0c_204d_06d1);
        assert_eq!(z.castling[4], 0x3924_da10_da0e_19c7);
        assert_eq!(z.castling[8], 0x0d3a_7930_f9c5_0f6a);
        assert_eq!(z.castling[15], 0xca4c_d520_d17e_e365);
        assert_eq!(
            z.ep,
            [
                0x9f2d_7837_754a_b070,
                0xfe21_46ba_fccf_664d,
                0x570e_d34e_0a1d_c4d5,
                0x4a7b_b504_6047_477e,
                0xa666_dc30_05f8_85b6,
                0x2b31_d322_b51a_649a,
                0x5e6a_222b_8c5f_5db5,
                0xad8f_e292_55af_615a,
            ]
        );
        assert_eq!(z.side, 0xa4d5_d38a_1cb2_100e);
    }

    #[test]
    fn geometry() {
        for sq in 0..64u8 {
            assert!(on_board(s88(sq)));
            assert_eq!(s64(s88(sq)), sq);
        }
        // Every step off the board from every square is detected.
        for sq in 0..64u8 {
            let s = s88(sq);
            for d in KNIGHT_STEPS.iter().chain(KING_STEPS.iter()) {
                let t = step(s, *d);
                let (r, f) = ((sq >> 3) as isize, (sq & 7) as isize);
                let (dr, df) = ((*d + 8).div_euclid(16), (*d + 8).rem_euclid(16) - 8);
                let inside = (0..8).contains(&(r + dr)) && (0..8).contains(&(f + df));
                assert_eq!(on_board(t), inside, "square {sq} step {d}");
            }
        }
        assert_eq!(DIR[delta_index(s88(63), s88(0))], 17);
        assert_eq!(DIR[delta_index(s88(0), s88(63))], -17);
        assert_eq!(DIR[delta_index(s88(7), s88(0))], 1);
        assert_eq!(DIR[delta_index(s88(10), s88(0))], 0);
        assert_eq!(STEP[delta_index(s88(10), s88(0))], 1 << KNIGHT);
        assert_eq!(STEP[delta_index(s88(9), s88(0))], 1 << KING);
    }
}
