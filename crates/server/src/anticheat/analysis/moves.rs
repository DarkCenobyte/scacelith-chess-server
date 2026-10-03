//! Protocol moves (u16: `from | to << 6 | promo << 12`) to UCI text and back, and the decoding of
//! the game record's move and time columns (little-endian BLOBs). The analysis needs no chess
//! rules: the engine checks legality, and castling is already the king's move (`e1g1`), as UCI
//! wants it.

const FILES: &[u8; 8] = b"abcdefgh";

/// Square index (a1 = 0, h8 = 63) to algebraic text.
pub fn square_name(sq: u16) -> String {
    let sq = sq & 63;
    let mut s = String::with_capacity(2);
    s.push(char::from(FILES[usize::from(sq & 7)]));
    s.push(char::from(b'1' + (sq >> 3) as u8));
    s
}

/// A u16 protocol move as UCI text.
pub fn move_to_uci(m: u16) -> String {
    let (from, to, promo) = (m & 63, (m >> 6) & 63, (m >> 12) & 7);
    let mut s = square_name(from);
    s.push_str(&square_name(to));
    match promo {
        2 => s.push('n'),
        3 => s.push('b'),
        4 => s.push('r'),
        5 => s.push('q'),
        _ => {}
    }
    s
}

/// UCI text to the u16 protocol move; `None` when malformed.
pub fn uci_to_move(s: &str) -> Option<u16> {
    let b = s.as_bytes();
    if b.len() != 4 && b.len() != 5 {
        return None;
    }
    let square = |f: u8, r: u8| -> Option<u16> {
        ((b'a'..=b'h').contains(&f) && (b'1'..=b'8').contains(&r))
            .then(|| u16::from(f - b'a') + u16::from(r - b'1') * 8)
    };
    let from = square(b[0], b[1])?;
    let to = square(b[2], b[3])?;
    let promo = match b.get(4) {
        None => 0,
        Some(b'n') => 2,
        Some(b'b') => 3,
        Some(b'r') => 4,
        Some(b'q') => 5,
        Some(_) => return None,
    };
    Some(from | (to << 6) | (promo << 12))
}

/// A move column (u16 little-endian values); a trailing odd byte is ignored.
pub fn moves_from_le(bytes: &[u8]) -> Vec<u16> {
    bytes.as_chunks::<2>().0.iter().map(|c| u16::from_le_bytes(*c)).collect()
}

/// A time column (u32 little-endian values); trailing bytes are ignored.
pub fn times_from_le(bytes: &[u8]) -> Vec<u32> {
    bytes.as_chunks::<4>().0.iter().map(|c| u32::from_le_bytes(*c)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn u16_moves_to_uci_and_back() {
        let (e2, e4, e1, g1, a7, a8) = (12u16, 28u16, 4u16, 6u16, 48u16, 56u16);
        assert_eq!(move_to_uci(e2 | (e4 << 6)), "e2e4");
        assert_eq!(move_to_uci(e1 | (g1 << 6)), "e1g1");
        assert_eq!(move_to_uci(a7 | (a8 << 6) | (5 << 12)), "a7a8q");
        assert_eq!(move_to_uci(a7 | (a8 << 6) | (2 << 12)), "a7a8n");
        assert_eq!(uci_to_move("a7a8q"), Some(a7 | (a8 << 6) | (5 << 12)));
        assert_eq!(uci_to_move("h8h9"), None);
        for bad in ["", "e2e", "e2e4qq", "e2e4k", "i2e4", "E2E4"] {
            assert_eq!(uci_to_move(bad), None, "{bad}");
        }
        for m in ["e2e4", "g8f6", "b7b8r", "h2h1b"] {
            assert_eq!(move_to_uci(uci_to_move(m).unwrap()), m);
        }
        assert_eq!(square_name(63), "h8");
    }

    #[test]
    fn game_record_columns() {
        let moves: Vec<u8> = [1u16, 2, 65535].iter().flat_map(|v| v.to_le_bytes()).collect();
        assert_eq!(moves_from_le(&moves), [1, 2, 65535]);
        assert_eq!(moves_from_le(&[7, 0, 44, 1, 9]), [7, 300]);
        let times: Vec<u8> = [70000u32, 5].iter().flat_map(|v| v.to_le_bytes()).collect();
        assert_eq!(times_from_le(&times), [70000, 5]);
        assert!(times_from_le(&[]).is_empty());
    }
}
