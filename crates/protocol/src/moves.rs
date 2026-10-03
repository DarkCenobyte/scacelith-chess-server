//! Move encoding and the position hash of the protocol (`docs/PROTOCOL.md`, "Moves and
//! positions").
//!
//! A move is a u16 `from | to << 6 | promo << 12`: squares are `file + 8 * rank` (a1 = 0, h8 = 63)
//! and `promo` is 0 (none), 2 (knight), 3 (bishop), 4 (rook) or 5 (queen). Castling is the king's
//! two-square move. Bit 15 is always 0.
//!
//! `posHash` is the FNV-1a 32 hash of the first four FEN fields (placement, side to move,
//! castling, en passant square) joined by single spaces.

/// Promotion piece letters by `promo` value (`None`: not a promotion value).
const PROMO_LETTERS: [Option<char>; 8] = [None, None, Some('n'), Some('b'), Some('r'), Some('q'), None, None];

/// The move `from -> to` with promotion `promo` (each masked to its bits).
pub const fn pack_move(from: u8, to: u8, promo: u8) -> u16 {
    (from as u16 & 63) | (to as u16 & 63) << 6 | (promo as u16 & 7) << 12
}

/// `(from, to, promo)` of a move.
pub const fn unpack_move(m: u16) -> (u8, u8, u8) {
    ((m & 63) as u8, (m >> 6 & 63) as u8, (m >> 12 & 7) as u8)
}

/// UCI text of a move (`"e2e4"`, `"e7e8q"`), or `None` when bit 15 is set or `promo` is not
/// one of 0, 2, 3, 4, 5.
pub fn move_to_uci(m: u16) -> Option<String> {
    if m & 0x8000 != 0 {
        return None;
    }
    let (from, to, promo) = unpack_move(m);
    let mut text = String::with_capacity(5);
    for square in [from, to] {
        text.push(char::from(b'a' + square % 8));
        text.push(char::from(b'1' + square / 8));
    }
    match (promo, PROMO_LETTERS[usize::from(promo)]) {
        (0, _) => {}
        (_, Some(letter)) => text.push(letter),
        (_, None) => return None,
    }
    Some(text)
}

/// The move of a UCI text (`"e2e4"`, `"e7e8q"`; surrounding spaces and case are ignored), or
/// `None` when the text is not a move.
pub fn uci_to_move(text: &str) -> Option<u16> {
    let text = text.trim().as_bytes();
    if !(4..=5).contains(&text.len()) {
        return None;
    }
    let square = |file: u8, rank: u8| -> Option<u8> {
        let file = file.to_ascii_lowercase();
        ((b'a'..=b'h').contains(&file) && (b'1'..=b'8').contains(&rank))
            .then(|| file - b'a' + 8 * (rank - b'1'))
    };
    let from = square(text[0], text[1])?;
    let to = square(text[2], text[3])?;
    let promo = match text.get(4).map(u8::to_ascii_lowercase) {
        None => 0,
        Some(b'n') => 2,
        Some(b'b') => 3,
        Some(b'r') => 4,
        Some(b'q') => 5,
        Some(_) => return None,
    };
    Some(pack_move(from, to, promo))
}

/// FNV-1a 32 of `bytes` (offset basis 0x811C9DC5, prime 0x01000193).
pub const fn fnv1a32(bytes: &[u8]) -> u32 {
    let mut hash = 0x811c_9dc5u32;
    let mut i = 0;
    while i < bytes.len() {
        hash = (hash ^ bytes[i] as u32).wrapping_mul(0x0100_0193);
        i += 1;
    }
    hash
}

/// `posHash` of a position given as FEN: FNV-1a 32 of its first four fields joined by single
/// spaces. The en passant field must be `-` unless an en passant capture is legal (the FEN the
/// game and the server write).
pub fn fen_digest(fen: &str) -> u32 {
    let mut hash = fnv1a32(b"");
    for (i, field) in fen.split_whitespace().take(4).enumerate() {
        if i > 0 {
            hash = (hash ^ u32::from(b' ')).wrapping_mul(0x0100_0193);
        }
        for &byte in field.as_bytes() {
            hash = (hash ^ u32::from(byte)).wrapping_mul(0x0100_0193);
        }
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packing() {
        assert_eq!(uci_to_move("e2e4"), Some(0x070c));
        assert_eq!(uci_to_move(" E7E8Q "), Some(0x5f34));
        assert_eq!(uci_to_move("e1g1"), Some(0x0184));
        assert_eq!(uci_to_move("a1a1"), Some(0));
        for bad in ["", "e2e", "e2e9", "i2e4", "e2e4k", "e2e4qq", "e2-e4"] {
            assert_eq!(uci_to_move(bad), None, "{bad}");
        }
        assert_eq!(unpack_move(0x5f34), (52, 60, 5));
        assert_eq!(pack_move(52 + 64, 60, 5 + 8), 0x5f34);
        for m in 0..0x8000u16 {
            match move_to_uci(m) {
                Some(text) => assert_eq!(uci_to_move(&text), Some(m)),
                None => assert!(matches!(unpack_move(m).2, 1 | 6 | 7)),
            }
        }
        assert_eq!(move_to_uci(0x8000 | 0x070c), None);
    }

    #[test]
    fn position_hashes() {
        assert_eq!(fnv1a32(b""), 2_166_136_261);
        assert_eq!(fen_digest("rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1"), 923_150_620);
        assert_eq!(fen_digest("rnbqkbnr/pppppppp/8/8/4P3/8/PPPP1PPP/RNBQKBNR b KQkq -"), 1_150_555_523);
        assert_eq!(fen_digest(" r3k2r/8/8/3pP3/8/8/8/R3K2R  w KQkq d6 0 1"), 4_101_590_597);
        assert_eq!(fen_digest("8/8/8/8/8/8/8/K6k w - - 0 1"), 132_864_131);
    }
}
