//! Whole-GIF tests: the golden outputs of the Node server (byte-identical GIFs and sprites, from
//! `tests/fixtures/games.json`), and what the frames show (gif.render.test.js of the Node server),
//! checked with the tests' own decoder.

use std::collections::HashMap;
use std::sync::LazyLock;
use std::sync::atomic::AtomicBool;

use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::encoder::tests::{compose, decode};
use crate::render::render_frames;
use crate::*;

/// The fixture file: replayed games and the expected GIFs.
static FIXTURES: LazyLock<Value> = LazyLock::new(|| {
    serde_json::from_str(include_str!("../tests/fixtures/games.json")).expect("valid fixture JSON")
});

fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// A game of the fixtures as the renderer's input.
fn replay(name: &str) -> Replay {
    let g = &FIXTURES["games"][name];
    let states = g["states"]
        .as_array()
        .expect("states")
        .iter()
        .map(|line| {
            let parts: Vec<&str> = line.as_str().expect("state line").split(' ').collect();
            assert_eq!(parts.len(), 7, "{line}");
            let num = |s: &str| s.parse::<i32>().expect("number");
            let square = |s: &str| u8::try_from(num(s)).ok();
            let mut board = [0u8; 64];
            for (b, c) in board.iter_mut().zip(parts[0].chars()) {
                *b = c.to_digit(16).expect("hex digit") as u8;
            }
            BoardState {
                board,
                last_move: square(parts[1]).zip(square(parts[2])),
                check: square(parts[3]),
                side_to_move: if num(parts[4]) == 0 { Color::White } else { Color::Black },
                number: parts[5].to_string(),
                san: parts[6].to_string(),
            }
        })
        .collect();
    let flag = |k: &str| g[k].as_bool().expect("flag");
    Replay {
        states,
        final_status: FinalStatus {
            checkmate: flag("checkmate"),
            stalemate: flag("stalemate"),
            insufficient_material: flag("insufficientMaterial"),
        },
    }
}

fn player(v: &Value) -> Player {
    let rating = match &v["rating"] {
        Value::Number(n) => n.to_string(),
        Value::String(s) => s.clone(),
        _ => String::new(),
    };
    Player { name: v["name"].as_str().unwrap_or("").to_string(), rating }
}

/// A job of the fixtures, interpreted as render.js does.
fn job(v: &Value) -> (GameInfo, Options) {
    let info = GameInfo {
        white: player(&v["white"]),
        black: player(&v["black"]),
        result: GameResult::parse(v["result"].as_str().unwrap_or("*")),
        footer: v["footer"].as_str().map(String::from),
    };
    let o = &v["options"];
    let options = Options {
        size: o["size"].as_str().and_then(Size::parse).unwrap_or(Size::Medium),
        orientation: if o["orientation"] == "black" { Orientation::Black } else { Orientation::White },
        delay_ms: o["delayMs"].as_f64().map_or(DELAY_DEFAULT_MS, |d| d as u32),
        coords: o["coords"] != false,
    };
    (info, options)
}

#[test]
fn golden_gifs_are_byte_identical_to_the_node_server() {
    let mut replays: HashMap<String, Replay> = HashMap::new();
    let mut failures = Vec::new();
    for j in FIXTURES["jobs"].as_array().expect("jobs") {
        let name = j["name"].as_str().expect("job name");
        let game = j["game"].as_str().expect("game name");
        let r = replays.entry(game.to_string()).or_insert_with(|| replay(game));
        let (info, options) = job(j);
        let gif = render_gif(r, &info, &options).expect("the fixture jobs render");
        let (want_len, want_hash) =
            (j["bytes"].as_u64().expect("bytes") as usize, j["sha256"].as_str().expect("sha256"));
        let hash = sha256_hex(&gif);
        if gif.len() != want_len || hash != want_hash {
            failures.push(format!("{name}: {} bytes {hash}, want {want_len} {want_hash}", gif.len()));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn sprites_and_palette_are_those_of_the_node_server() {
    assert_eq!(sha256_hex(palette_rgb()), "71ba692323f2079a46c3f350af054d41cbd22cdeb8cda3b903977012eb67f018");
    let mut failures = Vec::new();
    for (size, codes) in FIXTURES["sprites"].as_object().expect("sprites") {
        for (code, want) in codes.as_object().expect("codes") {
            let sprite = pieces::piece_sprite(code.parse().expect("code"), size.parse().expect("size"))
                .expect("valid sprite");
            let bytes: Vec<u8> = sprite.iter().flat_map(|v| v.to_le_bytes()).collect();
            if sha256_hex(&bytes) != want.as_str().expect("hash") {
                failures.push(format!("piece {code} at {size}"));
            }
        }
    }
    assert!(failures.is_empty(), "sprites differ: {failures:?}");
}

/// The Opera game job of the Node render tests.
fn opera_job(size: Size) -> (Replay, GameInfo, Options) {
    let info = GameInfo {
        white: Player::new("Morphy", Some(2690)),
        black: Player::new("Duke_Karl", None),
        result: GameResult::WhiteWins,
        footer: Some("Checkmate".into()),
    };
    (replay("opera"), info, Options { size, ..Options::default() })
}

fn rgb_at(palette: &[u8], screen: &[u8], width: usize, x: usize, y: usize) -> [u8; 3] {
    let i = usize::from(screen[y * width + x]) * 3;
    [palette[i], palette[i + 1], palette[i + 2]]
}

#[test]
fn deterministic_and_one_frame_per_position_with_the_delays() {
    let (r, info, options) = opera_job(Size::Small);
    let a = render_gif(&r, &info, &options).unwrap();
    assert_eq!(a, render_gif(&r, &info, &options).unwrap());
    assert_eq!(&a[..6], b"GIF89a");

    let gif = decode(&render_gif(&r, &info, &Options { delay_ms: 400, ..options }).unwrap()).unwrap();
    let plies = r.states.len() - 1;
    assert_eq!(gif.frames.len(), plies + 1);
    assert_eq!(gif.repeat, Some(0));
    assert_eq!(gif.frames[0].delay_cs, 100);
    assert!(gif.frames[1..plies].iter().all(|f| f.delay_cs == 40));
    assert_eq!(gif.frames[plies].delay_cs, 300);
    // The first frame is the whole picture; later frames are transparent sub-rectangles.
    let f0 = &gif.frames[0];
    assert_eq!((f0.x, f0.y, f0.width, f0.height, f0.transparent), (0, 0, gif.width, gif.height, None));
    for f in &gif.frames[1..] {
        assert_eq!(f.transparent, Some(0));
        assert!(u32::from(f.width) * u32::from(f.height) < u32::from(gif.width) * u32::from(gif.height));
    }
    // Index 0 (transparent) is never on screen.
    assert!(compose(&gif).iter().all(|s| !s.contains(&0)));
    // Delay clamped to 100..3000 ms; a long delay also holds the start that long.
    let fast = decode(&render_gif(&r, &info, &Options { delay_ms: 1, ..options }).unwrap()).unwrap();
    assert_eq!(fast.frames[1].delay_cs, 10);
    let slow =
        decode(&render_gif(&r, &info, &Options { delay_ms: 1_000_000_000, ..options }).unwrap()).unwrap();
    assert_eq!((slow.frames[0].delay_cs, slow.frames[1].delay_cs), (300, 300));
    // No move: one frame.
    let none = Replay { states: r.states[..1].to_vec(), final_status: FinalStatus::default() };
    let info0 = GameInfo { result: GameResult::Unfinished, footer: Some(String::new()), ..info.clone() };
    assert_eq!(decode(&render_gif(&none, &info0, &options).unwrap()).unwrap().frames.len(), 1);
}

#[test]
fn picture_sizes_and_the_gif_decodes_to_exactly_the_rendered_frames() {
    let (r, info, _) = opera_job(Size::Small);
    let short = Replay { states: r.states[..13].to_vec(), final_status: FinalStatus::default() };
    for size in [Size::Small, Size::Medium, Size::Large] {
        for coords in [true, false] {
            let options = Options { size, coords, ..Options::default() };
            let mut frames = Vec::new();
            let buf = render_frames(&short, &info, &options, &AtomicBool::new(false), &mut |_, px| {
                frames.push(px.to_vec())
            })
            .unwrap();
            let gif = decode(&buf).unwrap();
            let (w, h) = image_size(size, coords);
            assert_eq!((u32::from(gif.width), u32::from(gif.height)), (w, h), "{size:?}");
            let screens = compose(&gif);
            assert_eq!(screens.len(), frames.len());
            for (k, (s, f)) in screens.iter().zip(&frames).enumerate() {
                assert!(s == f, "{size:?} coords {coords} frame {k}");
            }
            assert_eq!(&gif.palette[..palette_rgb().len()], palette_rgb());
        }
    }
}

#[test]
fn board_squares_last_move_highlight_check_glow_orientation() {
    let (r, info, _) = opera_job(Size::Medium);
    let (s, m, board_y) = (48usize, 20usize, 8 + 2 * 28 + 3 + 8);
    let square = |name: &str| {
        let b = name.as_bytes();
        usize::from(b[0] - b'a') + 8 * usize::from(b[1] - b'1')
    };
    for flip in [false, true] {
        let corner = |sq: usize| {
            let (file, rank) = (sq & 7, sq >> 3);
            let (col, row) = if flip { (7 - file, rank) } else { (file, 7 - rank) };
            (m + col * s + 1, board_y + row * s + 1)
        };
        let options = Options {
            orientation: if flip { Orientation::Black } else { Orientation::White },
            ..Options::default()
        };
        let gif = decode(&render_gif(&r, &info, &options).unwrap()).unwrap();
        let screens = compose(&gif);
        let (start, end) = (&screens[0], &screens[screens.len() - 1]);
        let w = usize::from(gif.width);
        let at = |screen: &[u8], (x, y): (usize, usize)| rgb_at(&gif.palette, screen, w, x, y);
        let sum = |c: [u8; 3]| c.iter().map(|&v| u32::from(v)).sum::<u32>();
        // a1 is dark, h1 light.
        assert!(
            sum(at(start, corner(square("h1")))) > sum(at(start, corner(square("a1")))),
            "light squares are lighter"
        );
        // Last move Rd1-d8#: both squares highlighted (yellow-green).
        for sq in ["d1", "d8"] {
            let c = at(end, corner(square(sq)));
            assert!(c[1] > c[2] + 40, "{sq} highlighted: {c:?}");
        }
        // The mated king's square glows red; at the start it holds the black king (dark bulb).
        let (ex, ey) = corner(square("e8"));
        let near = at(end, (ex - 1 + 12, ey - 1 + 11));
        assert!(near[0] > 200 && near[1] < 120, "e8 glows red: {near:?}");
        let king = at(start, (ex - 1 + 24, ey - 1 + 19));
        assert!(sum(king) < 200, "black king on e8: {king:?}");
    }
}

#[test]
fn header_and_footer_texts_change_the_picture() {
    let (r, info, options) = opera_job(Size::Small);
    let base = render_gif(&r, &info, &options).unwrap();
    let variants = [
        GameInfo { white: Player::new("Anderssen", Some(2690)), ..info.clone() },
        GameInfo { black: Player::new("Duke_Karl", Some(1800)), ..info.clone() },
        GameInfo { footer: Some("White wins".into()), ..info.clone() },
        GameInfo { result: GameResult::Unfinished, footer: Some(String::new()), ..info.clone() },
    ];
    for v in &variants {
        assert_ne!(render_gif(&r, v, &options).unwrap(), base);
    }
    // A blank footer is no footer: the final position says how the game ended.
    let blank = GameInfo { footer: Some(" \u{feff} ".into()), ..info.clone() };
    assert_eq!(render_gif(&r, &blank, &options).unwrap(), base);
}

#[test]
fn invalid_replays_are_refused_and_a_cancelled_render_stops() {
    let (r, info, options) = opera_job(Size::Small);
    let empty = Replay { states: Vec::new(), final_status: FinalStatus::default() };
    assert_eq!(render_gif(&empty, &info, &options), Err(RenderError::NoPosition));
    let long =
        Replay { states: vec![r.states[0].clone(); MAX_PLIES + 2], final_status: FinalStatus::default() };
    assert_eq!(render_gif(&long, &info, &options), Err(RenderError::TooManyMoves { plies: MAX_PLIES + 1 }));
    assert_eq!(
        render_gif_cancellable(&r, &info, &options, &AtomicBool::new(true)),
        Err(RenderError::Cancelled)
    );
}

#[test]
fn an_unchanged_position_is_a_one_pixel_transparent_frame() {
    let (r, info, options) = opera_job(Size::Small);
    let mut states = r.states[..2].to_vec();
    states.push(states[1].clone());
    states.push(states[1].clone());
    let same = Replay { states, final_status: FinalStatus::default() };
    let info = GameInfo { result: GameResult::Unfinished, footer: None, ..info };
    let gif = decode(&render_gif(&same, &info, &options).unwrap()).unwrap();
    let f = &gif.frames[2];
    assert_eq!(
        (f.x, f.y, f.width, f.height, f.transparent, f.pixels.as_slice()),
        (0, 0, 1, 1, Some(0), &[0][..])
    );
    assert_eq!(f.delay_cs, 50);
}

#[test]
fn a_long_game_stays_small() {
    let r = replay("random300");
    let info =
        GameInfo { white: Player::new("a", None), black: Player::new("b", None), ..GameInfo::default() };
    let buf = render_gif(&r, &info, &Options { size: Size::Small, ..Options::default() }).unwrap();
    assert_eq!(decode(&buf).unwrap().frames.len(), 301);
    assert!(buf.len() < 1536 * 1024, "{} bytes", buf.len());
}
