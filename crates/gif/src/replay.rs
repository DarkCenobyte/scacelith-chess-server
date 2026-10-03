//! What the frames show: the positions of a game replayed move by move. The renderer takes them
//! as plain data ([`Replay`]); [`Replay::of_game`] builds them with any chess rules implementing
//! [`Rules`] (the server implements it over `scacelith-chess`), so this crate has no rules engine
//! of its own.

use crate::render::{MAX_PLIES, RenderError};

/// A side of the board.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Color {
    /// White.
    White,
    /// Black.
    Black,
}

/// The board as one frame shows it: the start position or the position after a move.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BoardState {
    /// Piece code per square, a1 = 0, b1 = 1, ..., h8 = 63: type 1..6 (pawn, knight, bishop,
    /// rook, queen, king) | colour << 3 (0 White, 1 Black); 0 for an empty square.
    pub board: [u8; 64],
    /// From and to squares of the move that led here (highlighted); `None` at the start.
    pub last_move: Option<(u8, u8)>,
    /// Square of the king of the side to move when it is in check (the red glow).
    pub check: Option<u8>,
    /// The side to move.
    pub side_to_move: Color,
    /// Move number of the move that led here, as the footer shows it: "12." (White) or "12..."
    /// (Black); empty at the start.
    pub number: String,
    /// Standard algebraic notation of the move that led here; empty at the start.
    pub san: String,
}

/// What the final position says about the end of the game (shown when the job gives no footer).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FinalStatus {
    /// The side to move is checkmated.
    pub checkmate: bool,
    /// The side to move is stalemated.
    pub stalemate: bool,
    /// Neither side can checkmate (shown only for a drawn game).
    pub insufficient_material: bool,
}

/// A replayed game: one state per frame (the start position, then one per move) and the status
/// of the final position.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Replay {
    /// The start position followed by the position after each move; never empty.
    pub states: Vec<BoardState>,
    /// The status of the last position.
    pub final_status: FinalStatus,
}

/// The chess rules a replay needs. Moves are the protocol's u16 encoding
/// (`from | to << 6 | promotion << 12`), squares 0..63 with a1 = 0.
pub trait Rules: Sized {
    /// The standard start position.
    fn start() -> Self;
    /// A position from FEN; `None` when the FEN is invalid.
    fn from_fen(fen: &str) -> Option<Self>;
    /// The piece code on a square (see [`BoardState::board`]).
    fn piece_at(&self, square: u8) -> u8;
    /// The side to move.
    fn side_to_move(&self) -> Color;
    /// The fullmove number (1 at the start of a game, incremented after Black's move).
    fn fullmove(&self) -> u32;
    /// Whether the side to move is in check.
    fn in_check(&self) -> bool;
    /// The square of a side's king.
    fn king_square(&self, color: Color) -> u8;
    /// Whether a move is legal in this position.
    fn is_legal(&self, mv: u16) -> bool;
    /// The SAN of a legal move, with its check or mate suffix.
    fn san(&self, mv: u16) -> String;
    /// Plays a legal move.
    fn play(&mut self, mv: u16);
    /// Whether the side to move is checkmated.
    fn is_checkmate(&self) -> bool;
    /// Whether the side to move is stalemated.
    fn is_stalemate(&self) -> bool;
    /// Whether neither side has mating material.
    fn has_insufficient_material(&self) -> bool;
}

fn state_of<R: Rules>(pos: &R, last_move: Option<(u8, u8)>, number: String, san: String) -> BoardState {
    let side = pos.side_to_move();
    BoardState {
        board: std::array::from_fn(|sq| pos.piece_at(sq as u8)),
        last_move,
        check: pos.in_check().then(|| pos.king_square(side)),
        side_to_move: side,
        number,
        san,
    }
}

impl Replay {
    /// Replays `moves` from `start_fen` (`None` or empty: the standard start position).
    ///
    /// Errors: more than [`MAX_PLIES`] moves (checked first), an invalid FEN, an illegal move.
    pub fn of_game<R: Rules>(start_fen: Option<&str>, moves: &[u16]) -> Result<Replay, RenderError> {
        if moves.len() > MAX_PLIES {
            return Err(RenderError::TooManyMoves { plies: moves.len() });
        }
        let mut pos = match start_fen {
            None | Some("") => R::start(),
            Some(fen) => R::from_fen(fen).ok_or(RenderError::InvalidFen)?,
        };
        let mut states = Vec::with_capacity(moves.len() + 1);
        states.push(state_of(&pos, None, String::new(), String::new()));
        for (i, &mv) in moves.iter().enumerate() {
            if !pos.is_legal(mv) {
                return Err(RenderError::IllegalMove { ply: i + 1 });
            }
            let san = pos.san(mv);
            let number = match pos.side_to_move() {
                Color::White => format!("{}.", pos.fullmove()),
                Color::Black => format!("{}...", pos.fullmove()),
            };
            pos.play(mv);
            states.push(state_of(&pos, Some(((mv & 63) as u8, ((mv >> 6) & 63) as u8)), number, san));
        }
        let final_status = FinalStatus {
            checkmate: pos.is_checkmate(),
            stalemate: pos.is_stalemate(),
            insufficient_material: pos.has_insufficient_material(),
        };
        Ok(Replay { states, final_status })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scripted game: the legal move of each ply and what the position looks like after it.
    #[derive(Clone)]
    struct Script {
        ply: usize,
        moves: &'static [(u16, &'static str)],
        side: Color,
        fullmove: u32,
    }

    impl Rules for Script {
        fn start() -> Self {
            Script {
                ply: 0,
                moves: &[(12 | 28 << 6, "e4"), (52 | 36 << 6, "e5"), (5 | 33 << 6, "Bb5")],
                side: Color::White,
                fullmove: 1,
            }
        }
        fn from_fen(fen: &str) -> Option<Self> {
            (fen == "black to move").then(|| Script { side: Color::Black, fullmove: 7, ..Script::start() })
        }
        fn piece_at(&self, square: u8) -> u8 {
            u8::from(usize::from(square) < self.ply)
        }
        fn side_to_move(&self) -> Color {
            self.side
        }
        fn fullmove(&self) -> u32 {
            self.fullmove
        }
        fn in_check(&self) -> bool {
            self.ply == 3
        }
        fn king_square(&self, color: Color) -> u8 {
            if color == Color::White { 4 } else { 60 }
        }
        fn is_legal(&self, mv: u16) -> bool {
            self.moves.get(self.ply).is_some_and(|m| m.0 == mv)
        }
        fn san(&self, _mv: u16) -> String {
            self.moves[self.ply].1.to_string()
        }
        fn play(&mut self, _mv: u16) {
            self.ply += 1;
            if self.side == Color::Black {
                self.fullmove += 1;
            }
            self.side = if self.side == Color::White { Color::Black } else { Color::White };
        }
        fn is_checkmate(&self) -> bool {
            false
        }
        fn is_stalemate(&self) -> bool {
            false
        }
        fn has_insufficient_material(&self) -> bool {
            self.ply == 3
        }
    }

    #[test]
    fn replays_states_numbers_and_errors() {
        let r = Replay::of_game::<Script>(None, &[12 | 28 << 6, 52 | 36 << 6, 5 | 33 << 6]).unwrap();
        assert_eq!(r.states.len(), 4);
        assert_eq!(r.states[0].last_move, None);
        assert_eq!((r.states[0].number.as_str(), r.states[0].san.as_str()), ("", ""));
        let texts: Vec<_> = r.states[1..].iter().map(|s| format!("{} {}", s.number, s.san)).collect();
        assert_eq!(texts, ["1. e4", "1... e5", "2. Bb5"]);
        assert_eq!(r.states[2].last_move, Some((52, 36)));
        assert_eq!(r.states[3].check, Some(60));
        assert_eq!(r.states[3].side_to_move, Color::Black);
        assert_eq!(r.states[2].board[1], 1);
        assert!(r.final_status.insufficient_material);

        let b = Replay::of_game::<Script>(Some("black to move"), &[12 | 28 << 6, 52 | 36 << 6]).unwrap();
        assert_eq!(b.states[1].number, "7...");
        assert_eq!(b.states[2].number, "8.");
        assert_eq!(Replay::of_game::<Script>(Some(""), &[]).unwrap().states.len(), 1);
        assert_eq!(Replay::of_game::<Script>(Some("nonsense"), &[]), Err(RenderError::InvalidFen));
        assert_eq!(
            Replay::of_game::<Script>(None, &[12 | 28 << 6, 0]),
            Err(RenderError::IllegalMove { ply: 2 })
        );
        let too_many = vec![0u16; MAX_PLIES + 1];
        assert_eq!(
            Replay::of_game::<Script>(Some("nonsense"), &too_many),
            Err(RenderError::TooManyMoves { plies: 1201 })
        );
        assert_eq!(RenderError::IllegalMove { ply: 6 }.to_string(), "illegal move at ply 6");
        assert_eq!(
            RenderError::TooManyMoves { plies: 1201 }.to_string(),
            "too many moves (1201 plies, at most 1200)"
        );
        assert_eq!(RenderError::InvalidFen.to_string(), "invalid start position (FEN)");
    }
}
