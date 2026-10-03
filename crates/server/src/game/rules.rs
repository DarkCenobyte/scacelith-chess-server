//! The chess rules as a game room sees them (DESIGN 5.2): a small trait implemented by
//! [`scacelith_chess::ChessGame`] for the server and by [`crate::game::testing::FakeRules`] for the
//! tests, whose scripted games do not depend on real chess positions.

use scacelith_chess::ChessGame;
use scacelith_protocol::{Color, EndReason, GameStatus};

/// A side of the board (the protocol's [`Color`] without `None`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Side {
    /// White (0).
    White = 0,
    /// Black (1).
    Black = 1,
}

impl Side {
    /// Both sides, White first.
    pub const BOTH: [Side; 2] = [Side::White, Side::Black];

    /// The side to move after `ply` plies (games always start from the initial position).
    #[must_use]
    pub const fn to_move(ply: usize) -> Side {
        if ply & 1 == 0 { Side::White } else { Side::Black }
    }

    /// Index into per-side arrays (White 0, Black 1).
    #[must_use]
    pub const fn index(self) -> usize {
        self as usize
    }

    /// The other side.
    #[must_use]
    pub const fn opponent(self) -> Side {
        match self {
            Side::White => Side::Black,
            Side::Black => Side::White,
        }
    }

    /// The protocol colour.
    #[must_use]
    pub const fn color(self) -> Color {
        match self {
            Side::White => Color::White,
            Side::Black => Color::Black,
        }
    }

    /// The side of a protocol colour (`None` for [`Color::None`]).
    #[must_use]
    pub const fn from_color(color: Color) -> Option<Side> {
        match color {
            Color::White => Some(Side::White),
            Color::Black => Some(Side::Black),
            Color::None => None,
        }
    }

    /// The wire value of an optional side (2 = none), as journal records store it.
    #[must_use]
    pub const fn code(side: Option<Side>) -> u8 {
        match side {
            Some(s) => s as u8,
            None => 2,
        }
    }

    /// The optional side of a wire value (anything but 0 and 1 is none).
    #[must_use]
    pub const fn from_code(code: u8) -> Option<Side> {
        match code {
            0 => Some(Side::White),
            1 => Some(Side::Black),
            _ => None,
        }
    }
}

/// The protocol colour of an optional side.
#[must_use]
pub const fn color_of(side: Option<Side>) -> Color {
    match side {
        Some(s) => s.color(),
        None => Color::None,
    }
}

/// The winning status of `side`.
#[must_use]
pub const fn win_for(side: Side) -> GameStatus {
    match side {
        Side::White => GameStatus::WhiteWins,
        Side::Black => GameStatus::BlackWins,
    }
}

/// What the rules say about an accepted move.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Played {
    /// `MoveFlag` bits of the move as played.
    pub flags: u8,
    /// The status after the move.
    pub status: GameStatus,
    /// The end reason after the move (`None` while the game runs).
    pub reason: EndReason,
}

/// The rules of one game, as the room uses them. The room validates every move with
/// [`Rules::is_legal`] before [`Rules::play`]; the side to move is the ply parity.
pub trait Rules: Send {
    /// Digest of the current position (`Move.posHash`).
    fn digest(&self) -> u32;

    /// Whether `m` is legal in the current position of a running game.
    fn is_legal(&self, m: u16) -> bool;

    /// Plays `m`; `None` when the rules refuse it (nothing changed).
    fn play(&mut self, m: u16) -> Option<Played>;

    /// The status of the game according to the rules.
    fn status(&self) -> GameStatus;

    /// Why the rules ended the game (`None` while it runs).
    fn reason(&self) -> EndReason;

    /// A threefold repetition can be claimed in the current position.
    fn can_claim_threefold(&self) -> bool;

    /// The fifty-move rule can be claimed in the current position.
    fn can_claim_fifty_move(&self) -> bool;

    /// Whether `side` still has mating material (a flag or an abandonment against a side that
    /// cannot mate is a draw).
    fn can_color_mate(&self, side: Side) -> bool;

    /// Ends the game with an online result (resignation, abandonment, abort...). Only informative:
    /// the room owns the result.
    fn end(&mut self, status: GameStatus, reason: EndReason);
}

fn chess_side(side: Side) -> scacelith_chess::Color {
    match side {
        Side::White => scacelith_chess::Color::White,
        Side::Black => scacelith_chess::Color::Black,
    }
}

fn status_of(status: scacelith_chess::GameStatus) -> GameStatus {
    GameStatus::from_u8(status.as_u8()).unwrap_or(GameStatus::Aborted)
}

fn reason_of(reason: scacelith_chess::EndReason) -> EndReason {
    EndReason::from_u8(reason.as_u8())
}

impl Rules for ChessGame {
    fn digest(&self) -> u32 {
        self.position().digest()
    }

    fn is_legal(&self, m: u16) -> bool {
        ChessGame::is_legal(self, m)
    }

    fn play(&mut self, m: u16) -> Option<Played> {
        let r = ChessGame::play(self, m).ok()?;
        Some(Played { flags: r.flags.bits(), status: status_of(r.status), reason: reason_of(r.reason) })
    }

    fn status(&self) -> GameStatus {
        status_of(ChessGame::status(self))
    }

    fn reason(&self) -> EndReason {
        reason_of(ChessGame::reason(self))
    }

    fn can_claim_threefold(&self) -> bool {
        ChessGame::can_claim_threefold(self)
    }

    fn can_claim_fifty_move(&self) -> bool {
        ChessGame::can_claim_fifty_move(self)
    }

    fn can_color_mate(&self, side: Side) -> bool {
        self.position().can_color_mate(chess_side(side))
    }

    fn end(&mut self, status: GameStatus, reason: EndReason) {
        let (Some(s), Some(r)) = (
            scacelith_chess::GameStatus::from_u8(status.to_u8()),
            scacelith_chess::EndReason::from_u8(reason.to_u8()),
        ) else {
            return;
        };
        // An `Ongoing` status is refused by the rules; the room never asks for it.
        let _ = ChessGame::end(self, s, r);
    }
}
