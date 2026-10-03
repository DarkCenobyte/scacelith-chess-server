//! Animated GIF of a chess game: a 2D board seen from above, the game played move by move, with
//! the players, the result and the last move around it. GPL-3.0-or-later.
//!
//! The output is byte-identical to the former Node.js server's renderer (`src/gif/*.js` at commit
//! 7531830): the same palette, anti-aliased pieces (the cburnett SVG set, rasterized here with the
//! floating-point semantics of V8), bitmap fonts (Terminus subsets), layout, frame differences and
//! LZW stream. The assets are embedded in the binary and parsed once, on first use.
//!
//! ```ignore
//! // `ChessPosition` implements `scacelith_gif::Rules` over the server's chess crate.
//! let replay = Replay::of_game::<ChessPosition>(None, &moves)?;
//! let info = GameInfo {
//!     white: Player::new("alice", Some(1520)),
//!     black: Player::new("bob", None),
//!     result: GameResult::WhiteWins,
//!     footer: Some("Resignation".into()),
//! };
//! let gif: Vec<u8> = render_gif(&replay, &info, &Options::default())?;
//! ```
//!
//! Rendering is pure and synchronous (the server runs it on low-priority threads). The first
//! render of a size also rasterizes the pieces and composes the square pictures of that size;
//! they stay cached for the process.

pub mod encoder;
mod font;
mod jsmath;
mod palette;
pub mod pieces;
mod raster;
mod render;
mod replay;
mod text;

pub use render::{
    DELAY_DEFAULT_MS, DELAY_MAX_MS, DELAY_MIN_MS, GameInfo, GameResult, MAX_PLIES, Options, Orientation,
    Player, RenderError, Size, image_size, render_gif, render_gif_cancellable,
};
pub use replay::{BoardState, Color, FinalStatus, Replay, Rules};

/// The palette of every GIF as r, g, b bytes (180 colours; index 0 is the transparent colour of
/// the frames after the first).
pub fn palette_rgb() -> &'static [u8] {
    &palette::palette().flat
}

#[cfg(test)]
mod tests;
