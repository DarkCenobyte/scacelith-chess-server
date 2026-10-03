//! Helpers shared by the integration tests.
#![allow(dead_code)]

use std::path::PathBuf;

use scacelith_chess::{ChessGame, Position, encode_move, parse_square};

/// A path relative to the repository root.
pub fn repo_path(rel: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../..").join(rel)
}

/// A path relative to `dedicated-server/`.
pub fn server_path(rel: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..").join(rel)
}

/// A path relative to this crate's `tests/fixtures`.
pub fn fixture_path(rel: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures").join(rel)
}

pub fn read_json(path: PathBuf) -> serde_json::Value {
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// Standard base64 (with padding).
pub fn decode_base64(s: &str) -> Vec<u8> {
    let value = |c: u8| match c {
        b'A'..=b'Z' => u32::from(c - b'A'),
        b'a'..=b'z' => u32::from(c - b'a') + 26,
        b'0'..=b'9' => u32::from(c - b'0') + 52,
        b'+' => 62,
        b'/' => 63,
        _ => panic!("bad base64 {c}"),
    };
    let mut out = Vec::new();
    for chunk in s.as_bytes().chunks(4) {
        let pad = chunk.iter().filter(|&&c| c == b'=').count();
        let mut v = 0;
        for &c in chunk {
            v = v << 6 | if c == b'=' { 0 } else { value(c) };
        }
        let bytes = [(v >> 16) as u8, (v >> 8) as u8, v as u8];
        out.extend_from_slice(&bytes[..3 - pad]);
    }
    out
}

/// FNV-1a 32 of a string's bytes (an independent reference for the digest).
pub fn fnv(s: &str) -> u32 {
    let mut h: u32 = 0x811c_9dc5;
    for b in s.bytes() {
        h ^= u32::from(b);
        h = h.wrapping_mul(0x0100_0193);
    }
    h
}

/// The first four fields of a FEN.
pub fn fen_prefix(fen: &str) -> String {
    fen.split(' ').take(4).collect::<Vec<_>>().join(" ")
}

pub fn pos(fen: &str) -> Position {
    Position::from_fen(fen).unwrap_or_else(|| panic!("valid FEN {fen}"))
}

pub fn sq(name: &str) -> u8 {
    parse_square(name).unwrap_or_else(|| panic!("square {name}"))
}

pub fn mv(from: &str, to: &str, promo: u8) -> u16 {
    encode_move(sq(from), sq(to), promo)
}

/// Plays UCI moves; panics when one is refused.
pub fn line(game: &mut ChessGame, ucis: &str) {
    for u in ucis.split(' ') {
        let m =
            game.position().parse_uci(u).unwrap_or_else(|| panic!("{u} legal in {}", game.position().fen()));
        game.play(m).unwrap_or_else(|e| panic!("play {u}: {e}"));
    }
}

pub fn game_after(start: Option<&str>, ucis: &str) -> ChessGame {
    let mut g = ChessGame::new(start).unwrap();
    line(&mut g, ucis);
    g
}

/// The deterministic generator of the former tests: `seed = seed * 1103515245 + 12345` (u32),
/// `(seed >> 8) % n`.
pub struct Lcg(pub u32);

impl Lcg {
    pub fn below(&mut self, n: usize) -> usize {
        self.0 = self.0.wrapping_mul(1_103_515_245).wrapping_add(12345);
        (self.0 >> 8) as usize % n
    }
}

/// The xorshift32 of the former PGN tests, as a float in [0, 1).
pub struct XorShift(pub u32);

impl XorShift {
    pub fn new(seed: u32) -> XorShift {
        XorShift(if seed == 0 { 1 } else { seed })
    }

    pub fn next_f64(&mut self) -> f64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.0 = x;
        f64::from(x) / 4_294_967_296.0
    }

    pub fn below(&mut self, n: usize) -> usize {
        (self.next_f64() * n as f64) as usize
    }
}
