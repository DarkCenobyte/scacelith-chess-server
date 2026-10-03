//! A game record with the game's automatic endings and claims (the game's `chess::Game`,
//! `src/chess/game.cpp`), plus the online endings of the server.

use std::fmt;

use crate::pgn::PgnTags;
use crate::position::Position;
use crate::tables::{F_CHECK, F_MATE};
use crate::types::{Color, EndReason, GameStatus, MoveFlags, move_uci};

/// A game: positions, moves, automatic endings (checkmate, stalemate, dead position, fivefold
/// repetition, 75-move rule), claims (threefold, fifty moves), resignation, agreement, flag fall
/// and any online ending ([`ChessGame::end`]).
///
/// The finishing calls return `true` only when they ended the game (`false` when it was already
/// over, or when there was nothing to claim).
#[derive(Clone, Debug)]
pub struct ChessGame {
    position: Position,
    start: Position,
    moves: Vec<u16>,
    /// Zobrist key of every position of the game (index = ply, the start included).
    keys: Vec<u64>,
    status: GameStatus,
    reason: EndReason,
}

/// The outcome of an accepted move.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PlayResult {
    /// The move's flags (with `CHECK` / `MATE`).
    pub flags: MoveFlags,
    /// The status after the move.
    pub status: GameStatus,
    /// The end reason after the move ([`EndReason::None`] while the game runs).
    pub reason: EndReason,
}

/// Why [`ChessGame::play`] refused a move (nothing changed).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlayError {
    /// The game is over.
    GameOver,
    /// The move is not legal in the current position.
    IllegalMove,
}

impl fmt::Display for PlayError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            PlayError::GameOver => "the game is over",
            PlayError::IllegalMove => "illegal move",
        })
    }
}

impl std::error::Error for PlayError {}

/// [`ChessGame::new`] was given an invalid FEN.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InvalidFen {
    /// The refused text.
    pub fen: String,
}

impl fmt::Display for InvalidFen {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid FEN: {}", self.fen)
    }
}

impl std::error::Error for InvalidFen {}

/// [`ChessGame::end`] was asked to end a game as `Ongoing`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InvalidEnd(pub GameStatus);

impl fmt::Display for InvalidEnd {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "bad status {}", self.0.as_u8())
    }
}

impl std::error::Error for InvalidEnd {}

impl Default for ChessGame {
    /// A game from the standard starting position.
    fn default() -> ChessGame {
        ChessGame::from_position(Position::start())
    }
}

impl ChessGame {
    /// A game from the standard start (`None`) or from a FEN (normalised like
    /// [`Position::from_fen`]; "" is invalid). A game can be over from its start (checkmate,
    /// stalemate, dead position, halfmove clock at 150).
    pub fn new(start_fen: Option<&str>) -> Result<ChessGame, InvalidFen> {
        match start_fen {
            None => Ok(ChessGame::default()),
            Some(fen) => Position::from_fen(fen)
                .map(ChessGame::from_position)
                .ok_or_else(|| InvalidFen { fen: fen.to_owned() }),
        }
    }

    /// A game from a start position.
    #[must_use]
    pub fn from_position(start: Position) -> ChessGame {
        let mut game = ChessGame {
            position: start,
            start,
            moves: Vec::new(),
            keys: vec![start.key()],
            status: GameStatus::Ongoing,
            reason: EndReason::None,
        };
        game.update_status(None);
        game
    }

    /// Replays a move list (journal recovery, PGN export): `None` when the FEN is invalid or a
    /// move cannot be played (illegal, or after the end of the game).
    #[must_use]
    pub fn from_moves(start_fen: Option<&str>, moves: &[u16]) -> Option<ChessGame> {
        let mut game = ChessGame::new(start_fen).ok()?;
        game.moves.reserve(moves.len());
        game.keys.reserve(moves.len());
        for &m in moves {
            game.play(m).ok()?;
        }
        Some(game)
    }

    /// The current position.
    #[must_use]
    pub fn position(&self) -> &Position {
        &self.position
    }

    /// The start position.
    #[must_use]
    pub fn start_position(&self) -> &Position {
        &self.start
    }

    /// Normalised FEN of the start position.
    #[must_use]
    pub fn start_fen(&self) -> String {
        self.start.fen()
    }

    /// The moves played (protocol u16).
    #[must_use]
    pub fn moves(&self) -> &[u16] {
        &self.moves
    }

    /// Number of moves (plies) played.
    #[must_use]
    pub fn ply(&self) -> usize {
        self.moves.len()
    }

    /// The game's status.
    #[must_use]
    pub fn status(&self) -> GameStatus {
        self.status
    }

    /// Why the game ended ([`EndReason::None`] while it runs).
    #[must_use]
    pub fn reason(&self) -> EndReason {
        self.reason
    }

    /// The game is over (any status but `Ongoing`).
    #[must_use]
    pub fn is_over(&self) -> bool {
        self.status != GameStatus::Ongoing
    }

    /// True when [`ChessGame::play`] would accept the move: the game runs and the move is legal.
    #[must_use]
    pub fn is_legal(&self, m: u16) -> bool {
        !self.is_over() && self.position.is_legal(m)
    }

    /// Plays a move; the outcome carries the move's flags and the status after it. Nothing
    /// changes when the game is over or the move is illegal.
    pub fn play(&mut self, m: u16) -> Result<PlayResult, PlayError> {
        if self.is_over() {
            return Err(PlayError::GameOver);
        }
        let im = self.position.validate(m).ok_or(PlayError::IllegalMove)?;
        let flags = self.position.play_validated(im);
        self.moves.push(m);
        self.keys.push(self.position.key());
        self.update_status(Some(flags));
        Ok(PlayResult { flags: MoveFlags::from_bits(flags as u8), status: self.status, reason: self.reason })
    }

    /// Occurrences of the current position (FIDE 9.2.3 identity: placement, side to move,
    /// castling rights, possible en passant capture) since the last irreversible move.
    #[must_use]
    pub fn repetition_count(&self) -> u32 {
        let Some(&current) = self.keys.last() else {
            return 0;
        };
        // Only the positions since the last capture or pawn move, with the same side to move.
        let window = self.position.halfmove() as usize;
        let same = self.keys.iter().rev().step_by(2).take(window / 2 + 1).filter(|&&k| k == current).count();
        u32::try_from(same).unwrap_or(u32::MAX)
    }

    /// The game runs and the current position occurred at least 3 times.
    #[must_use]
    pub fn can_claim_threefold(&self) -> bool {
        !self.is_over() && self.repetition_count() >= 3
    }

    /// The game runs and the halfmove clock is at least 100.
    #[must_use]
    pub fn can_claim_fifty_move(&self) -> bool {
        !self.is_over() && self.position.halfmove() >= 100
    }

    /// Applies the first valid claim: threefold repetition, then fifty moves.
    pub fn claim_draw(&mut self) -> bool {
        if self.can_claim_threefold() {
            return self.finish(GameStatus::Draw, EndReason::ThreefoldClaim);
        }
        if self.can_claim_fifty_move() {
            return self.finish(GameStatus::Draw, EndReason::FiftyMoveClaim);
        }
        false
    }

    /// `loser` resigns.
    pub fn resign(&mut self, loser: Color) -> bool {
        self.finish(Self::win_for(loser.opposite()), EndReason::Resignation)
    }

    /// Draw by agreement.
    pub fn agree_draw(&mut self) -> bool {
        self.finish(GameStatus::Draw, EndReason::Agreement)
    }

    /// The flag of `flagged` falls: loss on time, or a draw when the opponent cannot checkmate
    /// ([`Position::can_color_mate`]).
    pub fn flag_fall(&mut self, flagged: Color) -> bool {
        let winner = flagged.opposite();
        if !self.position.can_color_mate(winner) {
            return self.finish(GameStatus::Draw, EndReason::TimeoutVsInsufficient);
        }
        self.finish(Self::win_for(winner), EndReason::Timeout)
    }

    /// Ends the game with any result (online endings: abandonment, abort, forfeit...). An error
    /// for `GameStatus::Ongoing` (checked even when the game is over); otherwise `Ok(true)` when
    /// the game ended now, `Ok(false)` when it was already over.
    pub fn end(&mut self, status: GameStatus, reason: EndReason) -> Result<bool, InvalidEnd> {
        if status == GameStatus::Ongoing {
            return Err(InvalidEnd(status));
        }
        Ok(self.finish(status, reason))
    }

    /// "1-0", "0-1", "1/2-1/2", or "*" (ongoing or aborted).
    #[must_use]
    pub fn result_string(&self) -> &'static str {
        self.status.result_str()
    }

    /// SAN of every move.
    #[must_use]
    pub fn san_moves(&self) -> Vec<String> {
        let mut pos = self.start;
        self.moves
            .iter()
            .map(|&m| {
                let san = pos.san(m);
                // Every recorded move was legal when played.
                let _ = pos.play(m);
                san
            })
            .collect()
    }

    /// UCI of every move.
    #[must_use]
    pub fn uci_moves(&self) -> Vec<String> {
        self.moves.iter().map(|&m| move_uci(m)).collect()
    }

    /// PGN export (see [`PgnTags`] for the tags and the per-ply comments).
    #[must_use]
    pub fn pgn(&self, tags: &PgnTags) -> String {
        crate::pgn::write_pgn(self, tags)
    }

    // ---- internals ------------------------------------------------------------------------

    const fn win_for(winner: Color) -> GameStatus {
        match winner {
            Color::White => GameStatus::WhiteWins,
            Color::Black => GameStatus::BlackWins,
        }
    }

    fn finish(&mut self, status: GameStatus, reason: EndReason) -> bool {
        if self.is_over() {
            return false;
        }
        self.status = status;
        self.reason = reason;
        true
    }

    /// `chess::Game::updateStatus`, in its order. `flags`: those of the move just played (they
    /// already say check and mate), `None` at the start.
    fn update_status(&mut self, flags: Option<u32>) {
        let p = &self.position;
        let check = flags.map_or_else(|| p.in_check(), |f| f & F_CHECK != 0);
        let no_move = match flags {
            Some(f) if check => f & F_MATE != 0,
            _ => !p.has_legal_move(),
        };
        if no_move {
            if check {
                // Checkmate takes precedence over the 75-move rule (FIDE 9.6.2).
                self.finish(Self::win_for(p.side().opposite()), EndReason::Checkmate);
            } else {
                self.finish(GameStatus::Draw, EndReason::Stalemate);
            }
            return;
        }
        if p.has_insufficient_material() {
            self.finish(GameStatus::Draw, EndReason::InsufficientMaterial);
        } else if self.repetition_count() >= 5 {
            self.finish(GameStatus::Draw, EndReason::FivefoldRepetition);
        } else if p.halfmove() >= 150 {
            self.finish(GameStatus::Draw, EndReason::SeventyFiveMoves);
        }
    }
}
