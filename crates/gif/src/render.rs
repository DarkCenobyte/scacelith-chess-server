//! The picture of a game and its frames (render.js of the Node server).
//!
//! Picture, top to bottom:
//! * a header band, one row per player, White first: a colour swatch, the name (bold), the
//!   rating (dimmed; omitted when empty); the side to move has an accent bar at the start of its
//!   row and an accent dot at its end; on the last frame of a finished game each row ends with the
//!   player's score (1, 0 or the one-half sign) instead;
//! * the board: the pieces on wood squares, the last move highlighted on both its squares, a red
//!   glow under a king in check, the coordinates around it (following the orientation; without
//!   them the margins are narrower);
//! * a footer: the last move ("12..." dimmed, then the SAN in bold); on the last frame of a
//!   finished game (or when a footer is given) the result in the accent colour, how the game
//!   ended, and the last move on the right.
//!
//! Frames: the start position (held max(1 s, delay)), one frame per move, the last one held 3 s;
//! the animation loops forever. Frames after the first only hold the bounding box of the pixels
//! that changed, the unchanged ones transparent (palette index 0); only the rectangles redrawn for
//! the frame (changed squares, header rows, footer) are compared.

use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::encoder::{Disposal, Frame, GifEncoder};
use crate::font::{BitmapFont, Canvas, FontName, gif_font};
use crate::jsmath;
use crate::palette::{self, SquareKind};
use crate::replay::{Color, FinalStatus, Replay};
use crate::text::{clean_text, js_trim};

/// Most plies a GIF shows (a longer game is refused).
pub const MAX_PLIES: usize = 1200;

/// Shortest delay per move (milliseconds); shorter delays are raised to it.
pub const DELAY_MIN_MS: u32 = 100;
/// Longest delay per move (milliseconds); longer delays are lowered to it.
pub const DELAY_MAX_MS: u32 = 3000;
/// Default delay per move (milliseconds).
pub const DELAY_DEFAULT_MS: u32 = 500;
/// Shortest time the start position is shown (milliseconds).
const DELAY_FIRST_MS: u32 = 1000;
/// Time the last position is shown (milliseconds).
const DELAY_LAST_MS: u32 = 3000;

/// Picture size preset.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Size {
    /// 32 px squares: 284 x 350 (268 x 342 without coordinates).
    Small,
    /// 48 px squares: 424 x 515 (400 x 503 without coordinates).
    #[default]
    Medium,
    /// 72 px squares: 628 x 762 (600 x 748 without coordinates).
    Large,
}

impl Size {
    /// The preset of a name (`small`, `medium`, `large`).
    pub fn parse(s: &str) -> Option<Size> {
        match s {
            "small" => Some(Size::Small),
            "medium" => Some(Size::Medium),
            "large" => Some(Size::Large),
            _ => None,
        }
    }

    /// The name of the preset.
    pub fn as_str(self) -> &'static str {
        match self {
            Size::Small => "small",
            Size::Medium => "medium",
            Size::Large => "large",
        }
    }

    fn spec(self) -> &'static Spec {
        match self {
            Size::Small => &SMALL,
            Size::Medium => &MEDIUM,
            Size::Large => &LARGE,
        }
    }
}

/// The side shown at the bottom of the board.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Orientation {
    /// White at the bottom.
    #[default]
    White,
    /// Black at the bottom.
    Black,
}

impl Orientation {
    /// The orientation of a name (`white`, `black`).
    pub fn parse(s: &str) -> Option<Orientation> {
        match s {
            "white" => Some(Orientation::White),
            "black" => Some(Orientation::Black),
            _ => None,
        }
    }

    /// The name of the orientation.
    pub fn as_str(self) -> &'static str {
        match self {
            Orientation::White => "white",
            Orientation::Black => "black",
        }
    }
}

/// Picture options.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Options {
    /// Size preset.
    pub size: Size,
    /// The side at the bottom.
    pub orientation: Orientation,
    /// Time per move in milliseconds, clamped to [`DELAY_MIN_MS`]..=[`DELAY_MAX_MS`].
    pub delay_ms: u32,
    /// Whether the coordinates are drawn around the board.
    pub coords: bool,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            size: Size::Medium,
            orientation: Orientation::White,
            delay_ms: DELAY_DEFAULT_MS,
            coords: true,
        }
    }
}

/// The result of a game.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum GameResult {
    /// 1-0.
    WhiteWins,
    /// 0-1.
    BlackWins,
    /// 1/2-1/2.
    Draw,
    /// `*`: unfinished or aborted.
    #[default]
    Unfinished,
}

impl GameResult {
    /// The result of a PGN result string: `1-0`, `0-1`, `1/2-1/2` (also written `½-½`); anything
    /// else is unfinished.
    pub fn parse(s: &str) -> GameResult {
        match s {
            "1-0" => GameResult::WhiteWins,
            "0-1" => GameResult::BlackWins,
            "1/2-1/2" | "\u{bd}-\u{bd}" => GameResult::Draw,
            _ => GameResult::Unfinished,
        }
    }

    /// The PGN result string.
    pub fn as_str(self) -> &'static str {
        match self {
            GameResult::WhiteWins => "1-0",
            GameResult::BlackWins => "0-1",
            GameResult::Draw => "1/2-1/2",
            GameResult::Unfinished => "*",
        }
    }

    /// The result as the footer shows it.
    fn text(self) -> &'static str {
        match self {
            GameResult::Draw => "\u{bd}-\u{bd}",
            r => r.as_str(),
        }
    }

    /// The scores of White and Black at the end of the rows.
    fn scores(self) -> Option<(&'static str, &'static str)> {
        match self {
            GameResult::WhiteWins => Some(("1", "0")),
            GameResult::BlackWins => Some(("0", "1")),
            GameResult::Draw => Some(("\u{bd}", "\u{bd}")),
            GameResult::Unfinished => None,
        }
    }
}

/// A player as the header shows it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct Player {
    /// The name; empty shows "White" or "Black".
    pub name: String,
    /// The rating as text (digits), empty when unknown.
    pub rating: String,
}

impl Player {
    /// A player with an optional numeric rating.
    pub fn new(name: impl Into<String>, rating: Option<i64>) -> Player {
        Player { name: name.into(), rating: rating.map(|r| r.to_string()).unwrap_or_default() }
    }
}

/// The texts of a GIF: the players, the result and how the game ended.
///
/// Texts keep the characters the fonts have (printable ASCII, the one-half sign and the middle
/// dot; others print as '?'): names at most 64 characters, ratings 12, the footer 120, and are
/// cut with an ellipsis when they do not fit.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct GameInfo {
    /// White's row.
    pub white: Player,
    /// Black's row.
    pub black: Player,
    /// The result shown on the last frame.
    pub result: GameResult,
    /// How the game ended ("Resignation"...); when absent or blank, what the final position shows
    /// (checkmate, stalemate, insufficient material for a draw).
    pub footer: Option<String>,
}

/// Why a GIF could not be rendered.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RenderError {
    /// The start position is not valid FEN.
    InvalidFen,
    /// The move of this ply (1-based) is illegal.
    IllegalMove {
        /// The ply, counted from 1.
        ply: usize,
    },
    /// More than [`MAX_PLIES`] moves.
    TooManyMoves {
        /// The number of moves.
        plies: usize,
    },
    /// The replay holds no position.
    NoPosition,
    /// The render was cancelled.
    Cancelled,
}

impl fmt::Display for RenderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RenderError::InvalidFen => f.write_str("invalid start position (FEN)"),
            RenderError::IllegalMove { ply } => write!(f, "illegal move at ply {ply}"),
            RenderError::TooManyMoves { plies } => {
                write!(f, "too many moves ({plies} plies, at most {MAX_PLIES})")
            }
            RenderError::NoPosition => f.write_str("no position to show"),
            RenderError::Cancelled => f.write_str("cancelled"),
        }
    }
}

impl std::error::Error for RenderError {}

/// Layout of a size preset.
struct Spec {
    square: u32,
    bold: FontName,
    regular: FontName,
    coord: FontName,
    pad: i32,
    row_h: i32,
    row_gap: i32,
    margin: i32,
    bar: i32,
}

const SMALL: Spec = Spec {
    square: 32,
    bold: FontName::B12,
    regular: FontName::N12,
    coord: FontName::N12,
    pad: 6,
    row_h: 20,
    row_gap: 2,
    margin: 14,
    bar: 2,
};
const MEDIUM: Spec = Spec {
    square: 48,
    bold: FontName::B16,
    regular: FontName::N16,
    coord: FontName::N16,
    pad: 8,
    row_h: 28,
    row_gap: 3,
    margin: 20,
    bar: 3,
};
const LARGE: Spec = Spec {
    square: 72,
    bold: FontName::B24,
    regular: FontName::N24,
    coord: FontName::B16,
    pad: 12,
    row_h: 40,
    row_gap: 4,
    margin: 26,
    bar: 4,
};

/// Picture size of a preset in pixels (width, height), without rendering.
pub fn image_size(size: Size, coords: bool) -> (u32, u32) {
    let l = Layout::new(size.spec(), coords);
    (l.width as u32, l.height as u32)
}

/// Positions of the parts of the picture.
struct Layout {
    s: i32,
    board_px: i32,
    margin: i32,
    width: i32,
    height: i32,
    row_x: i32,
    row_w: i32,
    row1_y: i32,
    row2_y: i32,
    board_x: i32,
    board_y: i32,
    footer_y: i32,
}

impl Layout {
    fn new(spec: &Spec, coords: bool) -> Layout {
        let s = spec.square as i32;
        let board_px = 8 * s;
        let margin = if coords { spec.margin } else { spec.pad };
        let row1_y = spec.pad;
        let row2_y = row1_y + spec.row_h + spec.row_gap;
        let board_y = row2_y + spec.row_h + spec.pad;
        let footer_y = board_y + board_px + margin;
        Layout {
            s,
            board_px,
            margin,
            width: board_px + 2 * margin,
            height: footer_y + spec.row_h + spec.pad,
            row_x: margin,
            row_w: board_px,
            row1_y,
            row2_y,
            board_x: margin,
            board_y,
            footer_y,
        }
    }
}

/// `Math.round(v)` of a small value, as an integer.
fn round_i(v: f64) -> i32 {
    jsmath::round(v) as i32
}

/// Text vertically centred on the capital letters in a row of height `h` at `y`.
fn text_top(font: &BitmapFont, y: i32, h: i32) -> i32 {
    y + round_i(f64::from(h - font.cap_height) / 2.0) - font.cap_top
}

/// A rectangle redrawn for the current frame.
#[derive(Clone, Copy)]
struct Rect {
    x: i32,
    y: i32,
    w: i32,
    h: i32,
}

/// One text of the footer: the text, its font, its colour.
type Part<'a> = (&'a str, &'a BitmapFont, u8);

/// The drawing state of one render.
struct Painter<'a> {
    spec: &'static Spec,
    l: Layout,
    canvas: Canvas,
    dirty: Vec<Rect>,
    ui: palette::Ui,
    bold: &'a BitmapFont,
    regular: &'a BitmapFont,
}

impl Painter<'_> {
    fn fill_rect(&mut self, x: i32, y: i32, w: i32, h: i32, color: u8) {
        let width = self.l.width;
        let len = self.canvas.data.len() as i64;
        for yy in y..y + h {
            let start = (i64::from(yy) * i64::from(width) + i64::from(x)).clamp(0, len) as usize;
            let end = (i64::from(yy) * i64::from(width) + i64::from(x) + i64::from(w)).clamp(0, len) as usize;
            if start < end {
                self.canvas.data[start..end].fill(color);
            }
        }
    }

    fn mark(&mut self, x: i32, y: i32, w: i32, h: i32) {
        self.dirty.push(Rect { x, y, w, h });
    }

    fn draw_row(&mut self, y: i32, name: &str, rating: &str, color: Color, active: bool, score: &str) {
        let (spec, l, ui) = (self.spec, &self.l, self.ui);
        let (row_x, row_w, width) = (l.row_x, l.row_w, l.width);
        let (bold, regular) = (self.bold, self.regular);
        self.fill_rect(row_x, y, row_w, spec.row_h, ui.page);
        self.mark(row_x, y, row_w, spec.row_h);
        if active {
            self.fill_rect(row_x, y, spec.bar, spec.row_h, ui.accent);
        }
        let swatch = bold.cap_height + 2;
        let mut x = row_x + spec.bar + spec.pad;
        let sy = y + round_i(f64::from(spec.row_h - swatch) / 2.0);
        self.fill_rect(x, sy, swatch, swatch, ui.swatch_edge);
        let fill = if color == Color::White { ui.swatch_white } else { ui.swatch_black };
        self.fill_rect(x + 1, sy + 1, swatch - 2, swatch - 2, fill);
        x += swatch + spec.pad;
        let right = row_x + row_w - spec.pad;
        let score_w = if score.is_empty() { 0 } else { bold.measure(score) + spec.pad };
        let rating_w = if rating.is_empty() { 0 } else { regular.measure(rating) + regular.space * 2 };
        let name = bold.fit(name, (right - score_w - rating_w - x).max(0));
        let ty = text_top(bold, y, spec.row_h);
        x = bold.draw(&mut self.canvas, x, ty, &name, ui.text);
        if !rating.is_empty() {
            let rx = x + regular.space * 2 - bold.gap;
            regular.draw(&mut self.canvas, rx, text_top(regular, y, spec.row_h), rating, ui.dim);
        }
        if !score.is_empty() {
            bold.draw(&mut self.canvas, right - bold.measure(score), ty, score, ui.text);
        } else if active {
            // The side to move: a dot at the end of the row.
            let d = f64::from(swatch - 2);
            let cx = f64::from(right) - d / 2.0;
            let cy = f64::from(sy) + f64::from(swatch) / 2.0;
            let top = i64::from(round_i(cy - d / 2.0));
            let left = i64::from(round_i(cx - d / 2.0));
            let n = (swatch - 2).max(0) as i64;
            for yy in 0..n {
                for xx in 0..n {
                    let ddx = xx as f64 + 0.5 - d / 2.0;
                    let ddy = yy as f64 + 0.5 - d / 2.0;
                    if ddx * ddx + ddy * ddy <= (d / 2.0) * (d / 2.0) + 0.25 {
                        // Not clipped, like a typed array write: only the buffer bounds hold.
                        let i = (top + yy) * i64::from(width) + left + xx;
                        if let Some(p) = usize::try_from(i).ok().and_then(|i| self.canvas.data.get_mut(i)) {
                            *p = ui.accent;
                        }
                    }
                }
            }
        }
    }

    fn draw_footer(&mut self, left: &[Part<'_>], right: &str) {
        let (spec, ui) = (self.spec, self.ui);
        let (row_x, row_w, footer_y) = (self.l.row_x, self.l.row_w, self.l.footer_y);
        let (bold, regular) = (self.bold, self.regular);
        self.fill_rect(row_x, footer_y, row_w, spec.row_h, ui.page);
        self.mark(row_x, footer_y, row_w, spec.row_h);
        let ty = text_top(bold, footer_y, spec.row_h);
        let mut x = row_x;
        for &(text, font, color) in left {
            if text.is_empty() {
                continue;
            }
            let fitted = font.fit(text, row_x + row_w - x);
            let top = if std::ptr::eq(font, bold) { ty } else { text_top(font, footer_y, spec.row_h) };
            x = font.draw(&mut self.canvas, x, top, &fitted, color) + font.space;
        }
        if !right.is_empty() {
            let w = regular.measure(right);
            if x + w <= row_x + row_w {
                regular.draw(
                    &mut self.canvas,
                    row_x + row_w - w,
                    text_top(regular, footer_y, spec.row_h),
                    right,
                    ui.dim,
                );
            }
        }
    }
}

/// What the final position shows, when the caller does not say how the game ended.
fn derived_ending(fin: FinalStatus, result: GameResult) -> &'static str {
    if fin.checkmate {
        "Checkmate"
    } else if fin.stalemate {
        "Stalemate"
    } else if result == GameResult::Draw && fin.insufficient_material {
        "Insufficient material"
    } else {
        ""
    }
}

/// The delay of a frame in centiseconds.
fn frame_delay_cs(ply: usize, last: usize, delay_ms: u32) -> u16 {
    let ms = if ply == 0 {
        if last == 0 { DELAY_LAST_MS } else { delay_ms.max(DELAY_FIRST_MS) }
    } else if ply == last {
        DELAY_LAST_MS
    } else {
        delay_ms
    };
    jsmath::round(f64::from(ms) / 10.0) as u16
}

/// Renders a game as an animated GIF.
pub fn render_gif(replay: &Replay, info: &GameInfo, options: &Options) -> Result<Vec<u8>, RenderError> {
    render_gif_cancellable(replay, info, options, &AtomicBool::new(false))
}

/// [`render_gif`], stopping with [`RenderError::Cancelled`] soon after `cancel` becomes true
/// (checked before each frame).
pub fn render_gif_cancellable(
    replay: &Replay,
    info: &GameInfo,
    options: &Options,
    cancel: &AtomicBool,
) -> Result<Vec<u8>, RenderError> {
    render_frames(replay, info, options, cancel, &mut |_, _| {})
}

/// The renderer, calling `on_frame` with each frame's whole picture (tests).
pub(crate) fn render_frames(
    replay: &Replay,
    info: &GameInfo,
    options: &Options,
    cancel: &AtomicBool,
    on_frame: &mut dyn FnMut(usize, &[u8]),
) -> Result<Vec<u8>, RenderError> {
    let states = &replay.states;
    if states.len() > MAX_PLIES + 1 {
        return Err(RenderError::TooManyMoves { plies: states.len() - 1 });
    }
    let Some(last) = states.len().checked_sub(1) else { return Err(RenderError::NoPosition) };
    let spec = options.size.spec();
    let flip = options.orientation == Orientation::Black;
    let delay_ms = options.delay_ms.clamp(DELAY_MIN_MS, DELAY_MAX_MS);
    let result = info.result;
    let ending = match info.footer.as_deref() {
        Some(f) if !js_trim(f).is_empty() => clean_text(f, 120),
        _ => derived_ending(replay.final_status, result).to_string(),
    };
    let pal = palette::palette();
    let ui = pal.ui;
    let bold = gif_font(spec.bold);
    let regular = gif_font(spec.regular);
    let coord_font = gif_font(spec.coord);
    let l = Layout::new(spec, options.coords);
    let tiles = palette::tiles(spec.square);
    let (width, height) = (l.width as usize, l.height as usize);
    let mut p = Painter {
        spec,
        canvas: Canvas { width, height, data: vec![ui.page; width * height] },
        l,
        dirty: Vec::new(),
        ui,
        bold,
        regular,
    };
    let (s, board_x, board_y, board_px, margin) = (p.l.s, p.l.board_x, p.l.board_y, p.l.board_px, p.l.margin);

    // Coordinates (drawn once, part of the first frame only).
    if options.coords {
        for i in 0..8 {
            let file = char::from(b'a' + if flip { 7 - i } else { i } as u8).to_string();
            let rank = (if flip { i + 1 } else { 8 - i }).to_string();
            let fx = board_x + i * s + round_i(f64::from(s - coord_font.measure(&file)) / 2.0);
            coord_font.draw(
                &mut p.canvas,
                fx,
                text_top(coord_font, board_y + board_px, margin) - 1,
                &file,
                ui.dim,
            );
            let rx = round_i(f64::from(margin - coord_font.measure(&rank)) / 2.0);
            coord_font.draw(&mut p.canvas, rx, text_top(coord_font, board_y + i * s, s), &rank, ui.dim);
        }
    }

    let white_name =
        Some(clean_text(&info.white.name, 64)).filter(|n| !n.is_empty()).unwrap_or_else(|| "White".into());
    let black_name =
        Some(clean_text(&info.black.name, 64)).filter(|n| !n.is_empty()).unwrap_or_else(|| "Black".into());
    let white_rating = clean_text(&info.white.rating, 12);
    let black_rating = clean_text(&info.black.rating, 12);

    let (gif_w, gif_h) = (width as u16, height as u16);
    let mut enc = GifEncoder::new(gif_w, gif_h, &pal.flat, Some(0), ui.page)
        .expect("the picture and the palette are valid");
    let mut prev = vec![0u8; width * height];
    let mut sub = vec![0u8; width * height];
    let mut shown: [Option<(SquareKind, u8)>; 64] = [None; 64];
    let finished = result != GameResult::Unfinished;
    let su = s as usize;

    for (ply, st) in states.iter().enumerate() {
        if cancel.load(Ordering::Relaxed) {
            return Err(RenderError::Cancelled);
        }
        let end = ply == last;
        // Squares.
        for sq in 0..64u8 {
            let (file, rank) = (i32::from(sq & 7), i32::from(sq >> 3));
            let light = (file + rank) & 1 == 1;
            let kind = if st.check == Some(sq) {
                if light { SquareKind::LightCheck } else { SquareKind::DarkCheck }
            } else if st.last_move.is_some_and(|(f, t)| f == sq || t == sq) {
                if light { SquareKind::LightHighlight } else { SquareKind::DarkHighlight }
            } else if light {
                SquareKind::Light
            } else {
                SquareKind::Dark
            };
            let piece = st.board[usize::from(sq)];
            if shown[usize::from(sq)] == Some((kind, piece)) {
                continue;
            }
            shown[usize::from(sq)] = Some((kind, piece));
            let col = if flip { 7 - file } else { file };
            let row = if flip { rank } else { 7 - rank };
            let (x, y) = (board_x + col * s, board_y + row * s);
            let tile = tiles.get(kind, piece);
            for (yy, line) in tile.chunks_exact(su).enumerate() {
                let o = (y as usize + yy) * width + x as usize;
                p.canvas.data[o..o + su].copy_from_slice(line);
            }
            p.mark(x, y, s, s);
        }
        // Header rows and footer.
        let show_end = end && (finished || !ending.is_empty());
        let scores = if show_end { result.scores() } else { None };
        let (white_score, black_score) = scores.unwrap_or(("", ""));
        let to_move = |c: Color| !show_end && st.side_to_move == c;
        p.draw_row(p.l.row1_y, &white_name, &white_rating, Color::White, to_move(Color::White), white_score);
        p.draw_row(p.l.row2_y, &black_name, &black_rating, Color::Black, to_move(Color::Black), black_score);
        if show_end {
            let text = if st.number.is_empty() && st.san.is_empty() {
                String::new()
            } else {
                format!("{} {}", st.number, st.san)
            };
            p.draw_footer(&[(result.text(), bold, ui.accent), (&ending, regular, ui.text)], &text);
        } else {
            p.draw_footer(&[(&st.number, regular, ui.dim), (&st.san, bold, ui.text)], "");
        }

        let delay_cs = frame_delay_cs(ply, last, delay_ms);
        let data = &p.canvas.data;
        if ply == 0 {
            let frame = Frame {
                x: 0,
                y: 0,
                width: gif_w,
                height: gif_h,
                pixels: data,
                delay_cs,
                disposal: Disposal::Keep,
                transparent: None,
            };
            enc.add_frame(&frame).expect("frames stay inside the picture");
            prev.copy_from_slice(data);
        } else {
            // Exact box of the changed pixels (they are all inside the redrawn rectangles).
            let (mut bx0, mut by0, mut bx1, mut by1) = (width, height, 0usize, 0usize);
            let mut changed = false;
            for r in &p.dirty {
                let (x0, x1) = (r.x as usize, (r.x + r.w) as usize);
                for y in r.y as usize..(r.y + r.h) as usize {
                    let row = y * width;
                    let (now, before) = (&data[row + x0..row + x1], &prev[row + x0..row + x1]);
                    let Some(first) = now.iter().zip(before).position(|(a, b)| a != b) else { continue };
                    let last = now.iter().zip(before).rposition(|(a, b)| a != b).unwrap_or(first);
                    changed = true;
                    bx0 = bx0.min(x0 + first);
                    bx1 = bx1.max(x0 + last);
                    by0 = by0.min(y);
                    by1 = by1.max(y);
                }
            }
            if !changed {
                // Nothing changed: a one-pixel transparent frame keeps the timing.
                let frame = Frame {
                    x: 0,
                    y: 0,
                    width: 1,
                    height: 1,
                    pixels: &[0],
                    delay_cs,
                    disposal: Disposal::Keep,
                    transparent: Some(0),
                };
                enc.add_frame(&frame).expect("frames stay inside the picture");
            } else {
                // The changed pixels; everything else is the transparent index 0.
                let (w, h) = (bx1 - bx0 + 1, by1 - by0 + 1);
                let box_pixels = &mut sub[..w * h];
                box_pixels.fill(0);
                for r in &p.dirty {
                    // Every dirty rectangle holding a change is inside the box, but one without
                    // any may stick out of it.
                    let (x0, x1) = ((r.x as usize).max(bx0), ((r.x + r.w) as usize).min(bx1 + 1));
                    let (y0, y1) = ((r.y as usize).max(by0), ((r.y + r.h) as usize).min(by1 + 1));
                    if x0 >= x1 {
                        continue;
                    }
                    for y in y0..y1 {
                        let row = y * width;
                        let dst = &mut box_pixels[(y - by0) * w + x0 - bx0..][..x1 - x0];
                        for ((d, &v), &b) in
                            dst.iter_mut().zip(&data[row + x0..row + x1]).zip(&prev[row + x0..row + x1])
                        {
                            if v != b {
                                *d = v;
                            }
                        }
                    }
                }
                for r in &p.dirty {
                    for y in r.y as usize..(r.y + r.h) as usize {
                        let range = y * width + r.x as usize..y * width + (r.x + r.w) as usize;
                        prev[range.clone()].copy_from_slice(&data[range]);
                    }
                }
                let frame = Frame {
                    x: bx0 as u16,
                    y: by0 as u16,
                    width: w as u16,
                    height: h as u16,
                    pixels: box_pixels,
                    delay_cs,
                    disposal: Disposal::Keep,
                    transparent: Some(0),
                };
                enc.add_frame(&frame).expect("frames stay inside the picture");
            }
        }
        on_frame(ply, data);
        p.dirty.clear();
    }
    Ok(enc.finish())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picture_sizes_of_the_presets() {
        assert_eq!(image_size(Size::Small, true), (284, 350));
        assert_eq!(image_size(Size::Small, false), (268, 342));
        assert_eq!(image_size(Size::Medium, true), (424, 515));
        assert_eq!(image_size(Size::Medium, false), (400, 503));
        assert_eq!(image_size(Size::Large, true), (628, 762));
        assert_eq!(image_size(Size::Large, false), (600, 748));
    }

    #[test]
    fn delays_of_the_frames() {
        assert_eq!(frame_delay_cs(0, 0, 500), 300);
        assert_eq!(frame_delay_cs(0, 5, 400), 100);
        assert_eq!(frame_delay_cs(0, 5, 2000), 200);
        assert_eq!(frame_delay_cs(3, 5, 505), 51);
        assert_eq!(frame_delay_cs(5, 5, 100), 300);
    }

    #[test]
    fn results_and_endings() {
        assert_eq!(GameResult::parse("\u{bd}-\u{bd}"), GameResult::Draw);
        assert_eq!(GameResult::parse("1/2"), GameResult::Unfinished);
        assert_eq!(GameResult::Draw.text(), "\u{bd}-\u{bd}");
        let mate = FinalStatus { checkmate: true, ..FinalStatus::default() };
        assert_eq!(derived_ending(mate, GameResult::Unfinished), "Checkmate");
        let bare = FinalStatus { insufficient_material: true, ..FinalStatus::default() };
        assert_eq!(derived_ending(bare, GameResult::Draw), "Insufficient material");
        assert_eq!(derived_ending(bare, GameResult::WhiteWins), "");
    }
}
