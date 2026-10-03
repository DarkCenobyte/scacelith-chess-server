//! Colours, pieces, move encoding, move flags, castling rights, game status and end reasons.
//!
//! The numeric values are those of the realtime protocol (`GameStatus`, `EndReason`, `MoveFlag`)
//! and of the game's `chess::` enums, so they can be sent as they are.

use std::fmt;
use std::ops::{BitAnd, BitOr, BitOrAssign};

/// A side. The protocol encodes White as 0 and Black as 1.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum Color {
    /// White (0).
    White = 0,
    /// Black (1).
    Black = 1,
}

impl Color {
    /// The other side.
    #[must_use]
    pub const fn opposite(self) -> Color {
        match self {
            Color::White => Color::Black,
            Color::Black => Color::White,
        }
    }

    /// 0 for White, 1 for Black (array index).
    #[must_use]
    pub const fn index(self) -> usize {
        self as usize
    }

    /// The colour of a protocol value (0 White, 1 Black).
    #[must_use]
    pub const fn from_u8(value: u8) -> Option<Color> {
        match value {
            0 => Some(Color::White),
            1 => Some(Color::Black),
            _ => None,
        }
    }
}

/// A piece type, numbered like `chess::PieceType` (also the promotion field of a move).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(u8)]
pub enum PieceType {
    /// Pawn (1).
    Pawn = 1,
    /// Knight (2).
    Knight = 2,
    /// Bishop (3).
    Bishop = 3,
    /// Rook (4).
    Rook = 4,
    /// Queen (5).
    Queen = 5,
    /// King (6).
    King = 6,
}

impl PieceType {
    /// The piece type of a value 1..=6.
    #[must_use]
    pub const fn from_u8(value: u8) -> Option<PieceType> {
        match value {
            1 => Some(PieceType::Pawn),
            2 => Some(PieceType::Knight),
            3 => Some(PieceType::Bishop),
            4 => Some(PieceType::Rook),
            5 => Some(PieceType::Queen),
            6 => Some(PieceType::King),
            _ => None,
        }
    }

    /// The upper-case letter of SAN and FEN ('P', 'N', 'B', 'R', 'Q', 'K').
    #[must_use]
    pub const fn letter(self) -> char {
        match self {
            PieceType::Pawn => 'P',
            PieceType::Knight => 'N',
            PieceType::Bishop => 'B',
            PieceType::Rook => 'R',
            PieceType::Queen => 'Q',
            PieceType::King => 'K',
        }
    }
}

/// A piece on the board.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Piece {
    /// Its colour.
    pub color: Color,
    /// Its type.
    pub kind: PieceType,
}

impl Piece {
    /// The piece code of the former server and of the game: `type | colour << 3` (white pawn 1 ..
    /// white king 6, black pawn 9 .. black king 14).
    #[must_use]
    pub const fn code(self) -> u8 {
        self.kind as u8 | (self.color as u8) << 3
    }

    /// The piece of a code (see [`Piece::code`]); `None` for 0 and invalid codes.
    #[must_use]
    pub const fn from_code(code: u8) -> Option<Piece> {
        let color = match code >> 3 {
            0 => Color::White,
            1 => Color::Black,
            _ => return None,
        };
        match PieceType::from_u8(code & 7) {
            Some(kind) => Some(Piece { color, kind }),
            None => None,
        }
    }

    /// The FEN letter: upper case for White, lower case for Black.
    #[must_use]
    pub const fn fen_char(self) -> char {
        let c = self.kind.letter();
        match self.color {
            Color::White => c,
            Color::Black => c.to_ascii_lowercase(),
        }
    }
}

/// The protocol move `from | to << 6 | promotion << 12` (squares a1 = 0 .. h8 = 63, promotion 0 or
/// 2 knight, 3 bishop, 4 rook, 5 queen). Each field is masked to its width.
#[must_use]
pub const fn encode_move(from: u8, to: u8, promotion: u8) -> u16 {
    (from as u16 & 63) | (to as u16 & 63) << 6 | (promotion as u16 & 7) << 12
}

/// Origin square of a move (0..63).
#[must_use]
pub const fn move_from(m: u16) -> u8 {
    (m & 63) as u8
}

/// Target square of a move (0..63).
#[must_use]
pub const fn move_to(m: u16) -> u8 {
    (m >> 6 & 63) as u8
}

/// Promotion field of a move (0 for none, 2..5 for a legal promotion; other values are illegal).
#[must_use]
pub const fn move_promotion(m: u16) -> u8 {
    (m >> 12 & 7) as u8
}

/// UCI text of a move ("e2e4", "e7e8q"), like `chess::Position::toUCI`: legality is not checked;
/// a promotion field 1..6 adds its letter (`pnbrqk`), 0 and 7 add nothing. "0000" when bit 15 is
/// set (not a move).
#[must_use]
pub fn move_uci(m: u16) -> String {
    if m > 0x7fff {
        return "0000".to_owned();
    }
    let mut s = String::with_capacity(5);
    s.push_str(square_name(move_from(m)));
    s.push_str(square_name(move_to(m)));
    let promo = move_promotion(m);
    if (1..=6).contains(&promo) {
        s.push(char::from(b" pnbrqk"[usize::from(promo)]));
    }
    s
}

const SQUARE_NAMES: [&str; 64] = [
    "a1", "b1", "c1", "d1", "e1", "f1", "g1", "h1", //
    "a2", "b2", "c2", "d2", "e2", "f2", "g2", "h2", //
    "a3", "b3", "c3", "d3", "e3", "f3", "g3", "h3", //
    "a4", "b4", "c4", "d4", "e4", "f4", "g4", "h4", //
    "a5", "b5", "c5", "d5", "e5", "f5", "g5", "h5", //
    "a6", "b6", "c6", "d6", "e6", "f6", "g6", "h6", //
    "a7", "b7", "c7", "d7", "e7", "f7", "g7", "h7", //
    "a8", "b8", "c8", "d8", "e8", "f8", "g8", "h8", //
];

/// Name of a square 0..63 ("e4"); "-" for any other value.
#[must_use]
pub fn square_name(sq: u8) -> &'static str {
    SQUARE_NAMES.get(usize::from(sq)).copied().unwrap_or("-")
}

/// Square of a name like "e4" (the file letter may be upper case), like `chess::parseSquare`.
#[must_use]
pub fn parse_square(s: &str) -> Option<u8> {
    match s.as_bytes() {
        &[f, r] => {
            let f = f.to_ascii_lowercase();
            if (b'a'..=b'h').contains(&f) && (b'1'..=b'8').contains(&r) {
                Some((r - b'1') * 8 + (f - b'a'))
            } else {
                None
            }
        }
        _ => None,
    }
}

/// FNV-1a 32 of a byte string (the protocol's `posHash` is this over the first four FEN fields).
#[must_use]
pub fn fnv1a32(bytes: &[u8]) -> u32 {
    bytes.iter().fold(FNV_OFFSET, |h, &b| fnv_step(h, b))
}

pub(crate) const FNV_OFFSET: u32 = 0x811c_9dc5;

#[inline]
pub(crate) const fn fnv_step(h: u32, byte: u8) -> u32 {
    (h ^ byte as u32).wrapping_mul(0x0100_0193)
}

/// Move flag bits (the protocol's `MoveFlag`, `MoveMade.flags`).
///
/// An en passant capture carries `CAPTURE | EN_PASSANT`, a capturing promotion
/// `CAPTURE | PROMOTION`. `CHECK` and `MATE` are set by [`crate::Position::play`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct MoveFlags(u8);

impl MoveFlags {
    /// No flag.
    pub const NONE: MoveFlags = MoveFlags(0);
    /// The move captures a piece.
    pub const CAPTURE: MoveFlags = MoveFlags(1);
    /// En passant capture.
    pub const EN_PASSANT: MoveFlags = MoveFlags(2);
    /// King-side castling.
    pub const CASTLE_KING: MoveFlags = MoveFlags(4);
    /// Queen-side castling.
    pub const CASTLE_QUEEN: MoveFlags = MoveFlags(8);
    /// Pawn double push.
    pub const DOUBLE_PUSH: MoveFlags = MoveFlags(16);
    /// Promotion.
    pub const PROMOTION: MoveFlags = MoveFlags(32);
    /// The move gives check.
    pub const CHECK: MoveFlags = MoveFlags(64);
    /// The move mates.
    pub const MATE: MoveFlags = MoveFlags(128);

    /// The protocol byte.
    #[must_use]
    pub const fn bits(self) -> u8 {
        self.0
    }

    /// Flags from a protocol byte.
    #[must_use]
    pub const fn from_bits(bits: u8) -> MoveFlags {
        MoveFlags(bits)
    }

    /// Every bit of `other` is set.
    #[must_use]
    pub const fn contains(self, other: MoveFlags) -> bool {
        self.0 & other.0 == other.0
    }

    /// At least one bit of `other` is set.
    #[must_use]
    pub const fn intersects(self, other: MoveFlags) -> bool {
        self.0 & other.0 != 0
    }
}

impl BitOr for MoveFlags {
    type Output = MoveFlags;
    fn bitor(self, rhs: MoveFlags) -> MoveFlags {
        MoveFlags(self.0 | rhs.0)
    }
}

impl BitOrAssign for MoveFlags {
    fn bitor_assign(&mut self, rhs: MoveFlags) {
        self.0 |= rhs.0;
    }
}

impl BitAnd for MoveFlags {
    type Output = MoveFlags;
    fn bitand(self, rhs: MoveFlags) -> MoveFlags {
        MoveFlags(self.0 & rhs.0)
    }
}

/// Castling rights bits of [`crate::Position::castling`] (`chess::CastlingRights`).
pub mod castling {
    /// White may castle king-side (O-O).
    pub const WHITE_KING_SIDE: u8 = 1;
    /// White may castle queen-side (O-O-O).
    pub const WHITE_QUEEN_SIDE: u8 = 2;
    /// Black may castle king-side.
    pub const BLACK_KING_SIDE: u8 = 4;
    /// Black may castle queen-side.
    pub const BLACK_QUEEN_SIDE: u8 = 8;
    /// Every right.
    pub const ALL: u8 = 15;
}

/// The state of a game (the protocol's `GameStatus`; the game's C++ has no `Aborted`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum GameStatus {
    /// The game is running (0).
    Ongoing = 0,
    /// White won (1).
    WhiteWins = 1,
    /// Black won (2).
    BlackWins = 2,
    /// Drawn (3).
    Draw = 3,
    /// Aborted online: no result (4).
    Aborted = 4,
}

impl GameStatus {
    /// The status of a protocol value.
    #[must_use]
    pub const fn from_u8(value: u8) -> Option<GameStatus> {
        match value {
            0 => Some(GameStatus::Ongoing),
            1 => Some(GameStatus::WhiteWins),
            2 => Some(GameStatus::BlackWins),
            3 => Some(GameStatus::Draw),
            4 => Some(GameStatus::Aborted),
            _ => None,
        }
    }

    /// The protocol value.
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    /// The PGN result: "1-0", "0-1", "1/2-1/2", or "*" (ongoing or aborted).
    #[must_use]
    pub const fn result_str(self) -> &'static str {
        match self {
            GameStatus::WhiteWins => "1-0",
            GameStatus::BlackWins => "0-1",
            GameStatus::Draw => "1/2-1/2",
            GameStatus::Ongoing | GameStatus::Aborted => "*",
        }
    }
}

impl TryFrom<u8> for GameStatus {
    type Error = u8;

    /// The status of a protocol value; the value itself as the error when it is unknown.
    fn try_from(value: u8) -> Result<GameStatus, u8> {
        GameStatus::from_u8(value).ok_or(value)
    }
}

/// Why a game ended (the protocol's `EndReason`): 0..13 are `chess::GameEndReason`, 20 and above
/// are online-only endings.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum EndReason {
    /// Not ended (0).
    None = 0,
    /// Checkmate (1).
    Checkmate = 1,
    /// Resignation (2).
    Resignation = 2,
    /// Loss on time (3).
    Timeout = 3,
    /// Second illegal move (4).
    IllegalMoves = 4,
    /// Stalemate (5).
    Stalemate = 5,
    /// Dead position (6).
    InsufficientMaterial = 6,
    /// Flag fall, but the opponent cannot mate: draw (7).
    TimeoutVsInsufficient = 7,
    /// Fivefold repetition (8).
    FivefoldRepetition = 8,
    /// 75-move rule (9).
    SeventyFiveMoves = 9,
    /// Threefold repetition, claimed (10).
    ThreefoldClaim = 10,
    /// Fifty-move rule, claimed (11).
    FiftyMoveClaim = 11,
    /// Draw by agreement (12).
    Agreement = 12,
    /// Second illegal move, but the opponent cannot mate: draw (13).
    IllegalMovesVsInsufficient = 13,
    /// Disconnected for too long (20).
    Abandonment = 20,
    /// Abandoned, but the opponent cannot mate: draw (21).
    AbandonmentVsInsufficient = 21,
    /// Aborted (22).
    Aborted = 22,
    /// Aborted: the first move was not played in time (23).
    NoShow = 23,
    /// Forfeit for a fair-play violation (24).
    Forfeit = 24,
    /// Aborted by the server (25).
    ServerAborted = 25,
    /// Aborted: both players disconnected (26).
    BothDisconnected = 26,
}

impl EndReason {
    /// Every reason, in numeric order.
    pub const ALL: [EndReason; 21] = [
        EndReason::None,
        EndReason::Checkmate,
        EndReason::Resignation,
        EndReason::Timeout,
        EndReason::IllegalMoves,
        EndReason::Stalemate,
        EndReason::InsufficientMaterial,
        EndReason::TimeoutVsInsufficient,
        EndReason::FivefoldRepetition,
        EndReason::SeventyFiveMoves,
        EndReason::ThreefoldClaim,
        EndReason::FiftyMoveClaim,
        EndReason::Agreement,
        EndReason::IllegalMovesVsInsufficient,
        EndReason::Abandonment,
        EndReason::AbandonmentVsInsufficient,
        EndReason::Aborted,
        EndReason::NoShow,
        EndReason::Forfeit,
        EndReason::ServerAborted,
        EndReason::BothDisconnected,
    ];

    /// The reason of a protocol value.
    #[must_use]
    pub const fn from_u8(value: u8) -> Option<EndReason> {
        Some(match value {
            0 => EndReason::None,
            1 => EndReason::Checkmate,
            2 => EndReason::Resignation,
            3 => EndReason::Timeout,
            4 => EndReason::IllegalMoves,
            5 => EndReason::Stalemate,
            6 => EndReason::InsufficientMaterial,
            7 => EndReason::TimeoutVsInsufficient,
            8 => EndReason::FivefoldRepetition,
            9 => EndReason::SeventyFiveMoves,
            10 => EndReason::ThreefoldClaim,
            11 => EndReason::FiftyMoveClaim,
            12 => EndReason::Agreement,
            13 => EndReason::IllegalMovesVsInsufficient,
            20 => EndReason::Abandonment,
            21 => EndReason::AbandonmentVsInsufficient,
            22 => EndReason::Aborted,
            23 => EndReason::NoShow,
            24 => EndReason::Forfeit,
            25 => EndReason::ServerAborted,
            26 => EndReason::BothDisconnected,
            _ => return None,
        })
    }

    /// The protocol value.
    #[must_use]
    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    /// English description, the PGN comment of the ending (the game's `assets/i18n/en.lang`
    /// `reason.*` texts for 1..13); "" for [`EndReason::None`].
    #[must_use]
    pub const fn text(self) -> &'static str {
        match self {
            EndReason::None => "",
            EndReason::Checkmate => "Checkmate",
            EndReason::Resignation => "Resignation",
            EndReason::Timeout => "Loss on time",
            EndReason::IllegalMoves => "Second illegal move (forfeit)",
            EndReason::Stalemate => "Stalemate",
            EndReason::InsufficientMaterial => "Dead position (insufficient material)",
            EndReason::TimeoutVsInsufficient => "Flag fall, but the opponent cannot checkmate",
            EndReason::FivefoldRepetition => "Fivefold repetition",
            EndReason::SeventyFiveMoves => "75-move rule",
            EndReason::ThreefoldClaim => "Threefold repetition (claimed)",
            EndReason::FiftyMoveClaim => "50-move rule (claimed)",
            EndReason::Agreement => "Draw by agreement",
            EndReason::IllegalMovesVsInsufficient => "Second illegal move, but the opponent cannot checkmate",
            EndReason::Abandonment => "Abandoned (disconnected for too long)",
            EndReason::AbandonmentVsInsufficient => "Abandoned, but the opponent cannot checkmate",
            EndReason::Aborted => "Game aborted",
            EndReason::NoShow => "Aborted: first move not played in time",
            EndReason::Forfeit => "Forfeit (fair play violation)",
            EndReason::ServerAborted => "Aborted by the server",
            EndReason::BothDisconnected => "Aborted: both players disconnected",
        }
    }

    /// The PGN `Termination` tag value of a game ended with this reason and `status`:
    /// "unterminated" without a result (ongoing or aborted), "time forfeit" for a flag fall (also
    /// drawn), "rules infraction" for illegal moves and fair-play forfeits, "abandoned" for
    /// abandonments, "normal" otherwise.
    #[must_use]
    pub const fn termination(self, status: GameStatus) -> &'static str {
        if matches!(status, GameStatus::Ongoing | GameStatus::Aborted) {
            return "unterminated";
        }
        match self {
            EndReason::Timeout | EndReason::TimeoutVsInsufficient => "time forfeit",
            EndReason::IllegalMoves | EndReason::IllegalMovesVsInsufficient | EndReason::Forfeit => {
                "rules infraction"
            }
            EndReason::Abandonment | EndReason::AbandonmentVsInsufficient => "abandoned",
            _ => "normal",
        }
    }
}

impl TryFrom<u8> for EndReason {
    type Error = u8;

    /// The reason of a protocol value; the value itself as the error when it is unknown.
    fn try_from(value: u8) -> Result<EndReason, u8> {
        EndReason::from_u8(value).ok_or(value)
    }
}

impl fmt::Display for Color {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Color::White => "white",
            Color::Black => "black",
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enum_values_are_the_protocol_values() {
        assert_eq!(Color::White as u8, 0);
        assert_eq!(Color::Black as u8, 1);
        let types = [
            PieceType::Pawn,
            PieceType::Knight,
            PieceType::Bishop,
            PieceType::Rook,
            PieceType::Queen,
            PieceType::King,
        ];
        for (i, t) in types.into_iter().enumerate() {
            assert_eq!(t as usize, i + 1);
            assert_eq!(PieceType::from_u8(t as u8), Some(t));
        }
        assert_eq!(PieceType::from_u8(0), None);
        assert_eq!(PieceType::from_u8(7), None);
        let statuses = [
            (GameStatus::Ongoing, 0),
            (GameStatus::WhiteWins, 1),
            (GameStatus::BlackWins, 2),
            (GameStatus::Draw, 3),
            (GameStatus::Aborted, 4),
        ];
        for (s, v) in statuses {
            assert_eq!(s.as_u8(), v);
            assert_eq!(GameStatus::try_from(v), Ok(s));
        }
        assert_eq!(GameStatus::try_from(5), Err(5));
        let reasons: Vec<u8> = EndReason::ALL.iter().map(|r| r.as_u8()).collect();
        assert_eq!(reasons, [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 20, 21, 22, 23, 24, 25, 26]);
        for v in 0..=255u8 {
            match EndReason::from_u8(v) {
                Some(r) => assert_eq!(r.as_u8(), v),
                None => assert!(!reasons.contains(&v)),
            }
        }
        let flags = [
            MoveFlags::CAPTURE,
            MoveFlags::EN_PASSANT,
            MoveFlags::CASTLE_KING,
            MoveFlags::CASTLE_QUEEN,
            MoveFlags::DOUBLE_PUSH,
            MoveFlags::PROMOTION,
            MoveFlags::CHECK,
            MoveFlags::MATE,
        ];
        for (i, f) in flags.into_iter().enumerate() {
            assert_eq!(f.bits(), 1 << i);
        }
        assert_eq!(
            [
                castling::WHITE_KING_SIDE,
                castling::WHITE_QUEEN_SIDE,
                castling::BLACK_KING_SIDE,
                castling::BLACK_QUEEN_SIDE
            ],
            [1, 2, 4, 8]
        );
    }

    #[test]
    fn piece_codes() {
        for code in 0..=255u8 {
            match Piece::from_code(code) {
                Some(p) => assert_eq!(p.code(), code),
                None => assert!(!matches!(code, 1..=6 | 9..=14)),
            }
        }
        let bn = Piece { color: Color::Black, kind: PieceType::Knight };
        assert_eq!(bn.code(), 10);
        assert_eq!(bn.fen_char(), 'n');
    }

    #[test]
    fn end_reason_texts() {
        assert_eq!(EndReason::None.text(), "");
        assert_eq!(EndReason::Checkmate.text(), "Checkmate");
        assert_eq!(EndReason::Timeout.text(), "Loss on time");
        assert_eq!(EndReason::BothDisconnected.text(), "Aborted: both players disconnected");
    }

    #[test]
    fn moves_and_squares() {
        assert_eq!(encode_move(12, 28, 0), 1804);
        assert_eq!(encode_move(52, 60, 5), 0x5F34);
        assert_eq!(move_from(0x5F34), 52);
        assert_eq!(move_to(0x5F34), 60);
        assert_eq!(move_promotion(0x5F34), 5);
        assert_eq!(move_uci(encode_move(52, 60, 5)), "e7e8q");
        assert_eq!(move_uci(encode_move(52, 60, 7)), "e7e8");
        assert_eq!(move_uci(0), "a1a1");
        assert_eq!(move_uci(0x8000), "0000");
        assert_eq!(square_name(0), "a1");
        assert_eq!(square_name(63), "h8");
        assert_eq!(square_name(64), "-");
        assert_eq!(parse_square("E4"), Some(28));
        assert_eq!(parse_square("é4"), None);
        assert_eq!(fnv1a32(b""), 0x811c_9dc5);
        assert_eq!(fnv1a32(b"rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq -"), 923_150_620);
    }
}
