//! The chess rules the GIF renderer replays games with: `scacelith_chess` behind the renderer's
//! [`scacelith_gif::Rules`] trait.

use scacelith_chess::{Color as ChessColor, Position};
use scacelith_gif::{Color, Rules};

/// A position of `scacelith_chess`, as the GIF renderer sees it.
#[derive(Clone, Copy, Debug)]
pub struct ChessRules(Position);

fn chess_color(c: Color) -> ChessColor {
    match c {
        Color::White => ChessColor::White,
        Color::Black => ChessColor::Black,
    }
}

impl Rules for ChessRules {
    fn start() -> Self {
        ChessRules(Position::start())
    }

    fn from_fen(fen: &str) -> Option<Self> {
        Position::from_fen(fen).map(ChessRules)
    }

    fn piece_at(&self, square: u8) -> u8 {
        self.0.piece_at(square).map_or(0, |p| p.code())
    }

    fn side_to_move(&self) -> Color {
        match self.0.side() {
            ChessColor::White => Color::White,
            ChessColor::Black => Color::Black,
        }
    }

    fn fullmove(&self) -> u32 {
        self.0.fullmove()
    }

    fn in_check(&self) -> bool {
        self.0.in_check()
    }

    fn king_square(&self, color: Color) -> u8 {
        self.0.king_square(chess_color(color))
    }

    fn is_legal(&self, mv: u16) -> bool {
        self.0.is_legal(mv)
    }

    fn san(&self, mv: u16) -> String {
        self.0.san(mv)
    }

    fn play(&mut self, mv: u16) {
        // The renderer only plays moves it checked with `is_legal`.
        let _ = self.0.play(mv);
    }

    fn is_checkmate(&self) -> bool {
        self.0.is_checkmate()
    }

    fn is_stalemate(&self) -> bool {
        self.0.is_stalemate()
    }

    fn has_insufficient_material(&self) -> bool {
        self.0.has_insufficient_material()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use scacelith_gif::Replay;

    #[test]
    fn replays_a_short_mate() {
        // 1. f3 e5 2. g4 Qh4#
        let moves: Vec<u16> = ["f2f3", "e7e5", "g2g4", "d8h4"]
            .iter()
            .scan(Position::start(), |pos, uci| {
                let mv = pos.parse_uci(uci).expect("legal move");
                pos.play(mv).expect("legal move");
                Some(mv)
            })
            .collect();
        let replay = Replay::of_game::<ChessRules>(None, &moves).expect("legal game");
        assert_eq!(replay.states.len(), moves.len() + 1);
        assert_eq!(replay.states.last().unwrap().san, "Qh4#");
    }
}
