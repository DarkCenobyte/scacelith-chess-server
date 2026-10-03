//! A render job (what the routes build from a game record or a PGN), its cache key, and the
//! renderer of the server: the game replayed with the chess rules, then drawn by `scacelith-gif`.

use std::marker::PhantomData;
use std::sync::atomic::AtomicBool;

use base64::Engine as _;
use scacelith_gif::{GameInfo, GameResult, Options, Player, Replay, Rules};
use sha2::{Digest, Sha256};

use super::pool::Renderer;

/// A player of a job, as the routes take it from the game record or the PGN tags.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct JobPlayer {
    /// The name (the record's, or the tag cleaned by [`tag_text`](super::tag_text)).
    pub name: String,
    /// The rating, when known.
    pub rating: Option<i64>,
}

/// Everything a GIF depends on.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct GifJob {
    /// The start position in FEN; `None` for the standard one.
    pub start_fen: Option<String>,
    /// The moves (protocol encoding `from | to << 6 | promotion << 12`).
    pub moves: Vec<u16>,
    /// White.
    pub white: JobPlayer,
    /// Black.
    pub black: JobPlayer,
    /// The result.
    pub result: GameResult,
    /// How the game ended ("Checkmate", "Time forfeit"...); `None`: the final position says it.
    pub footer: Option<String>,
    /// The picture options.
    pub options: Options,
}

/// Appends `s` as a JSON string, escaped as JavaScript's `JSON.stringify` does (serde_json
/// escapes the same characters, with lowercase `\u00xx`).
fn push_json_str(out: &mut String, s: &str) {
    out.push_str(&serde_json::to_string(s).expect("a string serializes"));
}

fn push_opt_str(out: &mut String, s: Option<&str>) {
    match s {
        Some(s) => push_json_str(out, s),
        None => out.push_str("null"),
    }
}

fn push_opt_int(out: &mut String, v: Option<i64>) {
    match v {
        Some(v) => out.push_str(&v.to_string()),
        None => out.push_str("null"),
    }
}

impl GifJob {
    /// The JSON text the cache key hashes (the Node server's `gifCacheKey`): `["v1", startFen,
    /// moves, white name, white rating, black name, black rating, result, footer, options]`.
    fn key_text(&self) -> String {
        let mut s = String::with_capacity(128 + self.moves.len() * 5);
        s.push_str("[\"v1\",");
        push_opt_str(&mut s, self.start_fen.as_deref());
        s.push_str(",[");
        for (i, m) in self.moves.iter().enumerate() {
            if i > 0 {
                s.push(',');
            }
            s.push_str(&m.to_string());
        }
        s.push_str("],");
        for p in [&self.white, &self.black] {
            push_json_str(&mut s, &p.name);
            s.push(',');
            push_opt_int(&mut s, p.rating);
            s.push(',');
        }
        push_json_str(&mut s, self.result.as_str());
        s.push(',');
        push_opt_str(&mut s, self.footer.as_deref());
        let o = &self.options;
        s.push_str(&format!(
            ",{{\"size\":\"{}\",\"orientation\":\"{}\",\"delayMs\":{},\"coords\":{}}}]",
            o.size.as_str(),
            o.orientation.as_str(),
            o.delay_ms,
            o.coords
        ));
        s
    }

    /// The cache key: SHA-256 of everything that changes the picture (never the PGN text), in
    /// base64url without padding. Two jobs with the same key make the same GIF.
    pub fn cache_key(&self) -> String {
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(Sha256::digest(self.key_text().as_bytes()))
    }

    /// The texts of the picture.
    pub fn info(&self) -> GameInfo {
        GameInfo {
            white: Player::new(self.white.name.clone(), self.white.rating),
            black: Player::new(self.black.name.clone(), self.black.rating),
            result: self.result,
            footer: self.footer.clone(),
        }
    }
}

/// The renderer of the server: replays the job's moves with the chess rules `R`, then draws the
/// GIF (stopping between frames when cancelled). Errors are the renderer's messages ("illegal
/// move at ply 3", "invalid start position (FEN)", "too many moves (...)").
pub struct GameRenderer<R> {
    rules: PhantomData<fn() -> R>,
}

impl<R> GameRenderer<R> {
    /// The renderer.
    pub fn new() -> GameRenderer<R> {
        GameRenderer { rules: PhantomData }
    }
}

impl<R> Default for GameRenderer<R> {
    fn default() -> Self {
        GameRenderer::new()
    }
}

impl<R: Rules + 'static> Renderer for GameRenderer<R> {
    fn render(&self, job: &GifJob, cancel: &AtomicBool) -> Result<Vec<u8>, String> {
        let replay = Replay::of_game::<R>(job.start_fen.as_deref(), &job.moves).map_err(|e| e.to_string())?;
        scacelith_gif::render_gif_cancellable(&replay, &job.info(), &job.options, cancel)
            .map_err(|e| e.to_string())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use scacelith_gif::{Color, Orientation, Size};

    /// The chess rules of a game without moves: the standard start position only (enough for the
    /// renderer's whole path; the server implements [`Rules`] over `scacelith-chess`).
    pub struct StartOnly;

    impl Rules for StartOnly {
        fn start() -> Self {
            StartOnly
        }
        fn from_fen(_fen: &str) -> Option<Self> {
            None
        }
        fn piece_at(&self, square: u8) -> u8 {
            const BACK: [u8; 8] = [4, 2, 3, 5, 6, 3, 2, 4];
            match square >> 3 {
                0 => BACK[usize::from(square & 7)],
                1 => 1,
                6 => 1 | 8,
                7 => BACK[usize::from(square & 7)] | 8,
                _ => 0,
            }
        }
        fn side_to_move(&self) -> Color {
            Color::White
        }
        fn fullmove(&self) -> u32 {
            1
        }
        fn in_check(&self) -> bool {
            false
        }
        fn king_square(&self, color: Color) -> u8 {
            if color == Color::White { 4 } else { 60 }
        }
        fn is_legal(&self, _mv: u16) -> bool {
            false
        }
        fn san(&self, _mv: u16) -> String {
            String::new()
        }
        fn play(&mut self, _mv: u16) {}
        fn is_checkmate(&self) -> bool {
            false
        }
        fn is_stalemate(&self) -> bool {
            false
        }
        fn has_insufficient_material(&self) -> bool {
            false
        }
    }

    pub fn opera_job() -> GifJob {
        GifJob {
            start_fen: None,
            moves: vec![796, 3364],
            white: JobPlayer { name: "Morphy".into(), rating: Some(2690) },
            black: JobPlayer { name: "Duke_Karl".into(), rating: None },
            result: GameResult::WhiteWins,
            footer: Some("Checkmate".into()),
            options: Options::default(),
        }
    }

    #[test]
    fn cache_keys_are_those_of_the_node_server() {
        // Values of gifCacheKey in the Node server for the same jobs.
        assert_eq!(opera_job().cache_key(), "XIWOEBs7Kkw-nGR_Zerue7z1aLZaTudUy4C-A3Fu5UI");
        let odd = GifJob {
            start_fen: Some("8/P7/8/8/8/8/k7/4K3 w - - 0 1".into()),
            moves: Vec::new(),
            white: JobPlayer {
                name: "q\"\\\n\u{1}\u{1f}\u{7f} \u{e9} \u{2028} \u{1f600}".into(),
                rating: Some(0),
            },
            black: JobPlayer { name: String::new(), rating: Some(3000) },
            result: GameResult::Unfinished,
            footer: None,
            options: Options {
                size: Size::Large,
                orientation: Orientation::Black,
                delay_ms: 3000,
                coords: false,
            },
        };
        assert_eq!(odd.cache_key(), "qm5K5f91SJN3moEWYk_4Hf7UyBGPwNhc0W68bgQVIpg");
        // Everything the picture shows changes the key.
        let base = opera_job().cache_key();
        let variants = [
            GifJob { footer: None, ..opera_job() },
            GifJob { moves: vec![796], ..opera_job() },
            GifJob { start_fen: Some(String::new()), ..opera_job() },
            GifJob { black: JobPlayer { name: "deleted#99".into(), rating: None }, ..opera_job() },
            GifJob { white: JobPlayer { name: "Morphy".into(), rating: None }, ..opera_job() },
            GifJob { result: GameResult::Draw, ..opera_job() },
            GifJob { options: Options { delay_ms: 700, ..Options::default() }, ..opera_job() },
        ];
        for v in variants {
            assert_ne!(v.cache_key(), base, "{v:?}");
        }
    }

    #[test]
    fn the_game_renderer_replays_and_draws_and_reports_the_errors() {
        let r = GameRenderer::<StartOnly>::new();
        let cancel = AtomicBool::new(false);
        let job = GifJob {
            moves: Vec::new(),
            options: Options { size: Size::Small, ..Options::default() },
            ..opera_job()
        };
        let gif = r.render(&job, &cancel).unwrap();
        assert_eq!(&gif[..6], b"GIF89a");
        assert_eq!(u16::from_le_bytes([gif[6], gif[7]]), 284, "small width");
        assert_eq!(r.render(&opera_job(), &cancel), Err("illegal move at ply 1".to_string()));
        let fen = GifJob { start_fen: Some("nonsense".into()), ..job.clone() };
        assert_eq!(r.render(&fen, &cancel), Err("invalid start position (FEN)".to_string()));
        assert_eq!(
            r.render(&job, &AtomicBool::new(true)),
            Err(scacelith_gif::RenderError::Cancelled.to_string())
        );
    }
}
