//! PGN reader: the server's own PGN (tests/data/server-pgn and ChessGame::pgn round trips),
//! lichess and chess.com exports, lenient SAN, movetext structure, first game only, and hostile
//! inputs (only `PgnError`, with its line and column).

mod common;

use std::time::Instant;

use common::{XorShift, read_json, repo_path};
use scacelith_chess::{
    ChessGame, Color, EndReason, GameStatus, PGN_LIMITS, PgnComment, PgnError, PgnGame, PgnLimits, PgnTags,
    Position, START_FEN, move_uci, normalize_result, parse_san, read_pgn, read_pgn_bytes,
};

const PROMO_FEN: &str = "8/4P3/8/8/8/8/k7/4K3 w - - 0 1";

/// Debug builds of this crate run unoptimised: time limits are scaled for them.
const SLOWDOWN: f64 = if cfg!(debug_assertions) { 10.0 } else { 1.0 };

fn read(text: &str) -> PgnGame {
    read_pgn(text, &PGN_LIMITS).unwrap_or_else(|e| panic!("{e} at {}:{} in {text:?}", e.line, e.column))
}

fn read_with(text: &str, limits: PgnLimits) -> Result<PgnGame, PgnError> {
    read_pgn(text, &limits)
}

/// UCI of moves played from a FEN (or the start).
fn uci(moves: &[u16], fen: Option<&str>) -> Vec<String> {
    let mut p = fen.map_or_else(Position::start, |f| Position::from_fen(f).unwrap());
    moves
        .iter()
        .map(|&m| {
            let u = move_uci(m);
            p.play(m).unwrap_or_else(|_| panic!("{u} legal in {}", p.fen()));
            u
        })
        .collect()
}

/// Checks an error: line and column from 1, then the expected ones and a part of the message.
fn pgn_error(
    result: Result<PgnGame, PgnError>,
    line: Option<u32>,
    column: Option<u32>,
    message: &str,
) -> PgnError {
    let err = match result {
        Ok(g) => panic!("PgnError expected ({message}), got {g:?}"),
        Err(e) => e,
    };
    assert!(err.line >= 1 && err.column >= 1, "{err:?}");
    if let Some(line) = line {
        assert_eq!(err.line, line, "line of {:?}", err.message);
    }
    if let Some(column) = column {
        assert_eq!(err.column, column, "column of {:?}", err.message);
    }
    assert!(err.message.contains(message), "{:?} should contain {message:?}", err.message);
    err
}

fn err(text: &str, line: Option<u32>, column: Option<u32>, message: &str) -> PgnError {
    pgn_error(read_pgn(text, &PGN_LIMITS), line, column, message)
}

fn tag_list(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
    pairs.iter().map(|&(k, v)| (k.to_owned(), v.to_owned())).collect()
}

#[test]
fn the_pgn_files_the_server_writes() {
    let dir = repo_path("tests/data/server-pgn");
    let index = read_json(dir.join("index.json"));
    let files: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|x| x == "pgn"))
        .collect();
    assert!(files.len() >= 8);
    // Every file reads, the hand-made one too.
    for path in &files {
        let game = read(&std::fs::read_to_string(path).unwrap());
        assert!(!game.moves.is_empty(), "{}", path.display());
    }
    let games = index["games"].as_array().unwrap();
    assert!(games.len() >= 8);
    for g in games {
        let file = g["file"].as_str().unwrap();
        let text = std::fs::read_to_string(dir.join(file)).unwrap();
        let r = read(&text);
        let want: Vec<&str> = g["uci"].as_array().unwrap().iter().map(|u| u.as_str().unwrap()).collect();
        assert_eq!(uci(&r.moves, None), want, "{file}");
        assert_eq!(r.result, g["result"].as_str().unwrap(), "{file}");
        assert_eq!(r.start_fen, None);
        assert_eq!(r.tag("White"), g["white"].as_str());
        assert_eq!(r.tag("Black"), g["black"].as_str());
        assert_eq!(r.tag("ScacelithGameId"), g["gameId"].as_str());
        assert_eq!(r.tag("Termination"), g["termination"].as_str());
        assert_eq!(r.tags[0].0, "Event");
        // The bytes reader gives the same game.
        assert_eq!(read_pgn_bytes(text.as_bytes(), &PGN_LIMITS).as_ref(), Ok(&r));
    }
}

#[test]
fn chess_game_pgn_round_trips() {
    let mut r = XorShift::new(2026);
    let fens = [
        None,
        Some("r3k2r/pppq1ppp/2npbn2/4p3/4P3/2NPBN2/PPPQ1PPP/R3K2R b KQkq - 4 9"),
        Some("8/P6k/8/8/8/8/6Kp/8 w - - 0 60"),
        Some("4k3/8/8/2pP4/8/8/8/4K3 w - c6 0 1"),
    ];
    let mut endings = std::collections::BTreeSet::new();
    for n in 0..60 {
        let fen = fens[n % fens.len()];
        let mut g = ChessGame::new(fen).unwrap();
        let plies = r.below(140);
        while g.ply() < plies && !g.is_over() {
            let legal = g.position().legal_moves();
            g.play(legal[r.below(legal.len())]).unwrap();
        }
        if !g.is_over() {
            match n % 4 {
                0 => assert!(g.resign(g.position().side())),
                1 => assert!(g.agree_draw()),
                2 => assert_eq!(g.end(GameStatus::Aborted, EndReason::Aborted), Ok(true)),
                _ => {}
            }
        }
        endings.insert(g.reason().as_u8());
        let comments = (0..g.ply())
            .map(|i| match i % 3 {
                0 => Some(PgnComment::Words(vec!["[%clk 0:02:58.3]".into(), "[%emt 0:00:01.7]".into()])),
                1 => Some(PgnComment::Text("a } brace".into())),
                _ => None,
            })
            .collect();
        let tags = PgnTags {
            white: Some("alice".into()),
            black: Some("deleted#42".into()),
            event: Some(r#"Test "quoted" \ event"#.into()),
            date: Some("2026.10.01".into()),
            comments,
            after_result: tag_list(&[("WhiteElo", "1500")]),
            extra: vec![("ScacelithGameId".into(), n.to_string())],
            ..PgnTags::default()
        };
        let text = g.pgn(&tags);
        let back = read(&text);
        assert_eq!(back.moves, g.moves(), "{text}");
        assert_eq!(back.result, g.result_string());
        assert_eq!(back.start_fen, fen.map(|f| ChessGame::new(Some(f)).unwrap().start_fen()));
        assert_eq!(back.tag("Event"), Some(r#"Test "quoted" \ event"#));
        assert_eq!(back.tag("ScacelithGameId"), Some(n.to_string().as_str()));
    }
    assert!(endings.len() >= 4, "{endings:?}");
}

const LICHESS: &str = r#"[Event "Rated Blitz game"]
[Site "https://lichess.org/abcdefgh"]
[Date "2024.03.02"]
[White "alice"]
[Black "bob"]
[Result "0-1"]
[UTCDate "2024.03.02"]
[UTCTime "10:11:12"]
[WhiteElo "1850"]
[BlackElo "1902"]
[WhiteRatingDiff "-6"]
[BlackRatingDiff "+5"]
[Variant "Standard"]
[TimeControl "180+2"]
[ECO "B01"]
[Opening "Scandinavian Defense: Mieses-Kotroc Variation"]
[Termination "Normal"]
[Annotator "lichess.org"]

1. e4 { [%eval 0.36] [%clk 0:03:00] } 1... d5 { [%eval 0.59] [%clk 0:03:00] } 2. exd5 { [%eval 0.5] [%clk 0:03:01] } 2... Qxd5 { [%eval 0.62] [%clk 0:03:01] } 3. Nc3 { [%eval 0.45] [%clk 0:03:02] } 3... Qa5 { [%eval 0.57] [%clk 0:03:02] } 4. Bc4?! { (0.57 → -0.10) Inaccuracy. d4 was best. } { [%eval -0.1] [%clk 0:02:58] } (4. d4 Nf6 5. Nf3 c6) 4... Nf6 { [%eval 0.0] [%clk 0:02:59] } 5. Qf3?? { [%eval -3.2] [%clk 0:02:50] } 5... Qe5+ $6 6. Kd1 Bg4 7. Qxg4 Nxg4 { White resigns. } 0-1


"#;

const CHESS_COM: &str = r#"[Event "Live Chess"]
[Site "Chess.com"]
[Date "2024.05.06"]
[Round "-"]
[White "Carol"]
[Black "Dave"]
[Result "1/2-1/2"]
[CurrentPosition "4k3/8/8/8/8/8/8/4K3 w - - 0 40"]
[Timezone "UTC"]
[ECO "C20"]
[ECOUrl "https://www.chess.com/openings/Kings-Pawn-Opening"]
[UTCDate "2024.05.06"]
[UTCTime "18:00:00"]
[WhiteElo "1200"]
[BlackElo "1210"]
[TimeControl "600"]
[Termination "Game drawn by agreement"]
[StartTime "18:00:00"]
[EndDate "2024.05.06"]
[EndTime "18:20:00"]
[Link "https://www.chess.com/game/live/123456789"]

1. e4 {[%clk 0:09:58.1]} 1... e5 {[%clk 0:09:57.3]} 2. Nf3 {[%clk 0:09:55]} 2... Nc6 {[%clk 0:09:50.2]} 3. Bb5 {[%clk 0:09:49]} 3... a6 {[%clk 0:09:45.9]} 4. Bxc6 {[%clk 0:09:44]} 4... dxc6 {[%clk 0:09:40]} 5. O-O {[%clk 0:09:39]} 5... f6 {[%clk 0:09:30]} 1/2-1/2
"#;

#[test]
fn lichess_and_chess_com_exports() {
    let a = read(LICHESS);
    assert_eq!(
        uci(&a.moves, None),
        [
            "e2e4", "d7d5", "e4d5", "d8d5", "b1c3", "d5a5", "f1c4", "g8f6", "d1f3", "a5e5", "e1d1", "c8g4",
            "f3g4", "f6g4"
        ]
    );
    assert_eq!(a.result, "0-1");
    assert_eq!(a.tags.len(), 18);
    let b = read(CHESS_COM);
    assert_eq!(b.moves.len(), 10);
    assert_eq!(uci(&b.moves, None)[8], "e1g1");
    assert_eq!(b.result, "1/2-1/2");
    // Windows line ends, a byte order mark, old Mac line ends.
    let crlf = format!("\u{feff}{}", LICHESS.replace('\n', "\r\n"));
    assert_eq!(read(&crlf).moves, a.moves);
    assert_eq!(read_pgn_bytes(crlf.as_bytes(), &PGN_LIMITS).unwrap().moves, a.moves);
    assert_eq!(read(&LICHESS.replace('\n', "\r")).moves, a.moves);
}

#[test]
fn lenient_san() {
    let cases: &[(&str, &[&str], &str)] = &[
        (START_FEN, &["e4", "e2e4", "e2-e4", "e4!", "e4!?", "e4?!"], "e2e4"),
        (
            START_FEN,
            &["Nf3", "nf3", "Ng1f3", "Ng1-f3", "Ngf3", "N1f3", "Nf3+", "\u{2658}f3", "\u{265e}f3", "g1f3"],
            "g1f3",
        ),
        ("r3k2r/8/8/8/8/8/8/R3K2R w KQkq - 0 1", &["O-O", "0-0", "o-o", "OO", "O-O+", "e1g1", "Kg1"], "e1g1"),
        ("r3k2r/8/8/8/8/8/8/R3K2R w KQkq - 0 1", &["O-O-O", "0-0-0", "OOO", "e1c1"], "e1c1"),
        ("r3k2r/8/8/8/8/8/8/R3K2R b KQkq - 0 1", &["O-O", "0-0"], "e8g8"),
        (PROMO_FEN, &["e8=Q", "e8Q", "e8(Q)", "e8/Q", "e8=q", "e8=Q+", "e7e8q", "e7-e8=Q"], "e7e8q"),
        (PROMO_FEN, &["e8=N", "e8N", "e7e8n"], "e7e8n"),
        ("3r4/4P3/8/8/8/8/k7/4K3 w - - 0 1", &["exd8=R", "exd8R", "ed8R", "e7xd8=R", "e7:d8=R"], "e7d8r"),
        // b-pawn capture first, then a bishop: "bxc3" is the pawn, "Bxc3" the bishop.
        ("4k3/8/8/8/8/2n5/1P1B4/4K3 w - - 0 1", &["bxc3", "b2c3", "bc3", "b2xc3"], "b2c3"),
        ("4k3/8/8/8/8/2n5/1P1B4/4K3 w - - 0 1", &["Bxc3", "Bdc3", "Bd2xc3", "B2c3"], "d2c3"),
        // A lower-case bishop only when no pawn move matches.
        ("4k3/8/8/8/8/8/8/4KB2 w - - 0 1", &["bc4", "Bc4", "bxc4"], "f1c4"),
        // Legal UCI comes before the lower-case piece letter: "b1d2" is the knight, not "B1d2".
        ("rnbqkbnr/ppp1pppp/8/3p4/3P4/8/PPP1PPPP/RNBQKBNR w KQkq - 0 2", &["b1d2", "Nd2", "Nbd2"], "b1d2"),
        ("rnbqkbnr/ppp1pppp/8/3p4/3P4/8/PPP1PPPP/RNBQKBNR w KQkq - 0 2", &["bd2", "Bd2", "B1d2"], "c1d2"),
        ("rnbqkbnr/ppp1pppp/8/3p4/2PP4/8/PP2PPPP/RNBQKBNR b KQkq - 0 2", &["b8d7", "Nd7"], "b8d7"),
        ("rnbqkbnr/ppp1pppp/8/3p4/2PP4/8/PP2PPPP/RNBQKBNR b KQkq - 0 2", &["bd7", "B8d7"], "c8d7"),
        ("4k3/8/8/8/8/8/8/2B1K3 w - - 0 1", &["b1d2"], "c1d2"), // not UCI here: no piece on b1
        // En passant with or without "e.p.".
        ("4k3/8/8/2pP4/8/8/8/4K3 w - c6 0 1", &["dxc6", "dxc6e.p.", "dxc6 e.p.", "dc6", "d5c6"], "d5c6"),
        // Disambiguation; over-disambiguated forms.
        ("4k3/8/8/8/8/8/8/1N2KN2 w - - 0 1", &["Nbd2", "Nb1d2", "Nb1-d2"], "b1d2"),
        ("4k3/8/8/8/8/8/8/1N2KN2 w - - 0 1", &["Nfd2", "Nf1d2"], "f1d2"),
    ];
    for &(fen, texts, want) in cases {
        let p = Position::from_fen(fen).unwrap();
        for &text in texts {
            let m = parse_san(&p, text).unwrap_or_else(|| panic!("{text} in {fen}"));
            assert_eq!(move_uci(m), want, "{text} in {fen}");
        }
    }
    let refused: &[(&str, &[&str])] = &[
        (START_FEN, &["e5", "Ke2", "O-O", "Nf4", "Pe4", "xyz", "", "e", "Nf3f3", "e2e5"]),
        (PROMO_FEN, &["e8", "e8=K", "e8=P", "e7e8"]),
        ("4k3/8/8/8/8/8/8/1N2KN2 w - - 0 1", &["Nd2", "N1d2"]), // ambiguous
    ];
    for &(fen, texts) in refused {
        let p = Position::from_fen(fen).unwrap();
        for &text in texts {
            assert_eq!(parse_san(&p, text), None, "{text} refused in {fen}");
        }
    }
}

/// The promotion forms the reader's documentation lists for movetext.
fn documented_promotion_forms() -> Vec<String> {
    let source = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/pgn/reader.rs")).unwrap();
    let doc: Vec<&str> = source.lines().map_while(|l| l.strip_prefix("//!")).map(str::trim).collect();
    let doc = doc.join(" ");
    let start = doc.find("promotions as ").expect("the documentation lists the promotion forms")
        + "promotions as ".len();
    let rest = &doc[start..];
    let end = [rest.find(" in movetext"), rest.find(';')].into_iter().flatten().min().unwrap();
    rest[..end]
        .trim_end_matches(',')
        .split(", ")
        .flat_map(|s| s.split(" or "))
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .collect()
}

#[test]
fn promotions_every_documented_form_is_read_in_movetext() {
    let fen = "4k3/P7/8/8/8/8/8/4K3 w - - 0 1";
    let forms = documented_promotion_forms();
    assert!(forms.len() >= 3, "{forms:?}");
    for form in &forms {
        let mv = form.replacen("e8", "a8", 1);
        let g = read(&format!("[FEN \"{fen}\"]\n\n1. {mv} *"));
        assert_eq!(uci(&g.moves, Some(fen)), ["a7a8q"], "{form} in movetext");
    }
    // The parenthesis opens a variation in movetext, as in the game's reader (src/chess/pgn.cpp).
    err(&format!("[FEN \"{fen}\"]\n\n1. a8(Q) *"), Some(3), Some(4), "illegal move 'a8'");
    let p = Position::from_fen(fen).unwrap();
    assert_eq!(parse_san(&p, "a8(Q)").map(move_uci).as_deref(), Some("a7a8q"));
}

#[test]
fn movetext_numbers_comments_escapes_nags_glyphs_evaluations_variations_results() {
    let text = r#"[White "a \"quoted\" \\ name"]
[Black "b"]
% an escape line of another program: 1. d4
1. e4 ; a comment to the end of the line 1. d4
{ a comment
  over lines (1. d4) } 1... e5 $1 2.Nf3 !? Nc6 +- 3.Bb5 (3. Bc4 Bc5 (3... Nf6 4. Ng5 (4. d3)) 4. c3) 3...a6 ± 4.Ba4 <reserved> Nf6 5.O-O Be7 = 6.Re1 b5
7.Bb3 d6 8.c3 O-O ½-½"#;
    let r = read(text);
    assert_eq!(r.tags[0].1, r#"a "quoted" \ name"#);
    assert_eq!(
        uci(&r.moves, None),
        [
            "e2e4", "e7e5", "g1f3", "b8c6", "f1b5", "a7a6", "b5a4", "g8f6", "e1g1", "f8e7", "f1e1", "b7b5",
            "a4b3", "d7d6", "c2c3", "e8g8"
        ]
    );
    assert_eq!(r.result, "1/2-1/2");
    // Black to move first, "1..." numbering, FEN with SetUp.
    let fen = "rnbqkbnr/pppppppp/8/8/4P3/8/PPPP1PPP/RNBQKBNR b KQkq - 0 1";
    let b = read(&format!("[SetUp \"1\"]\n[FEN \"{fen}\"]\n\n1... c5 2. Nf3 *"));
    assert_eq!(uci(&b.moves, Some(fen)), ["c7c5", "g1f3"]);
    assert_eq!(b.start_fen.as_deref(), Some(fen));
    assert_eq!(b.result, "*");
    // SetUp "0" ignores the FEN; a FEN equal to the start position gives None.
    assert_eq!(read("[SetUp \"0\"]\n[FEN \"8/8/8/8/8/8/k7/K7 w - - 0 1\"]\n1. e4 *").moves.len(), 1);
    assert_eq!(read(&format!("[FEN \"{START_FEN}\"]\n1. e4 *")).start_fen, None);
    // Chess960 tags with a standard setup.
    assert_eq!(read(&format!("[Variant \"Chess960\"]\n[FEN \"{START_FEN}\"]\n1. e4 *")).moves.len(), 1);
    // Result: the movetext's, else the Result tag, else '*'.
    assert_eq!(read("[Result \"0-1\"]\n1. e4 e5").result, "0-1");
    assert_eq!(read("[Result \"garbage\"]\n1. e4 e5").result, "*");
    assert_eq!(read("1. e4 e5 1-0").result, "1-0");
    assert_eq!(read("[Result \"0.5-0.5\"]\n1. e4 e5").result, "1/2-1/2");
    // A game of tags only.
    let tags_only = read("[Event \"x\"]\n[Result \"1-0\"]\n");
    assert!(tags_only.moves.is_empty());
    assert_eq!(tags_only.result, "1-0");
    assert_eq!(normalize_result("1/2"), Some("1/2-1/2"));
    assert_eq!(normalize_result("2-0"), None);
}

#[test]
fn only_the_first_game_is_read() {
    let r = read("[Event \"one\"]\n1. e4 e5 1-0\n\n[Event \"two\"]\n1. d4 d5 0-1\n");
    assert_eq!(r.tags[0].1, "one");
    assert_eq!(r.moves.len(), 2);
    assert_eq!(r.result, "1-0");
    // Without a termination marker: the next tag pair ends the game.
    let r2 = read("[Event \"one\"]\n1. e4 e5\n[Event \"two\"]\n1. d4 Ke7 garbage {");
    assert_eq!(r2.moves.len(), 2);
    assert_eq!(r2.result, "*");
    // A broken tag after the movetext opens the next game too.
    assert_eq!(read("1. e4 e5\n[Event broken\n1. d4").moves.len(), 2);
    // Moves after an automatic ending are kept (only legality matters).
    let shuffle = "Nf3 Nf6 Ng1 Ng8 ".repeat(5);
    assert_eq!(read(&format!("{shuffle} e4 *")).moves.len(), 21);
}

#[test]
fn bytes_utf8_and_latin1() {
    let text = "[White \"J\u{f6}rg\"]\n1. e4 *";
    assert_eq!(read_pgn_bytes(text.as_bytes(), &PGN_LIMITS).unwrap().tags[0].1, "J\u{f6}rg");
    let latin1: Vec<u8> = text.chars().map(|c| u8::try_from(u32::from(c)).unwrap()).collect();
    assert_eq!(read_pgn_bytes(&latin1, &PGN_LIMITS).unwrap().tags[0].1, "J\u{f6}rg");
    // A byte order mark is not part of the first tag.
    let bom = [&[0xef, 0xbb, 0xbf][..], text.as_bytes()].concat();
    assert_eq!(read_pgn_bytes(&bom, &PGN_LIMITS).unwrap().tags[0].0, "White");
}

#[test]
fn hostile_and_broken_inputs_give_a_pgn_error_with_line_and_column() {
    err("", Some(1), Some(1), "no game");
    err("  \n { only a comment } \n", None, None, "no game");
    // (The former `readPgn(42)` and `readPgn(null)` cases cannot be written: the input is text.)
    err("1. e4 e5\n2. Ke3 *", Some(2), Some(4), "illegal move 'Ke3'");
    err(
        "[White \"\u{e9}\u{e9}\"] 1. e4 e5 2. Qh5 Nc6 3. Bc4 Nf6 4. Qxf7# Ke7",
        Some(1),
        Some(54),
        "illegal move",
    );
    err("1. e4 -- 2. d4", None, None, "null moves");
    err("1. e4 Z0", None, None, "null moves");
    err("[FEN \"4k3/8/8/8/8/8/8/1N2KN2 w - - 0 1\"]\n1. Nd2", Some(2), Some(4), "illegal move 'Nd2'");
    err("[White \"a\"]\n[FEN \"8/8/8 w - - 0 1\"]\n1. e4", Some(2), Some(1), "invalid FEN");
    err("[Variant \"Crazyhouse\"]\n1. e4", Some(1), Some(1), "variant 'Crazyhouse' is not supported");
    err("[Variant \"Chess960\"]\n1. e4", None, None, "Chess960 game without a FEN");
    err(
        "[Variant \"Chess960\"]\n[FEN \"bqnbrkrn/pppppppp/8/8/8/8/PPPPPPPP/BQNBRKRN w GEge - 0 1\"]\n1. e4",
        None,
        None,
        "Chess960",
    );
    err("1. e4 { never closed", Some(1), Some(7), "unterminated comment");
    err("1. e4 { runs into\n[Event \"next\"]\n1. d4", None, None, "unterminated comment");
    err("[White \"never closed]\n1. e4", Some(1), Some(1), "unterminated value");
    err("[White]\n1. e4", None, None, "quoted value expected");
    err("[ \"x\"]", None, None, "tag name expected");
    err("[White \"x\" junk]", None, None, "']' expected");
    err("1. e4 (1. d4 d5", Some(1), Some(7), "unterminated variation");
    err("1. e4 (1. d4 1-0", None, None, "unterminated variation before the result");
    err("1. e4 ) e5", Some(1), Some(7), "without a variation");
    err("1. e4 \u{1} e5", Some(1), Some(7), "control character");
    err("1. e4 # e5", None, None, "unexpected character '#'");
    err("1. e4 $999", None, None, "malformed NAG");
    err("1. e4 < unclosed", None, None, "'<' without '>'");
    err(&format!("1. e4 {}", "x".repeat(41)), None, None, "token too long");
    // Caps.
    let limits = |f: fn(&mut PgnLimits)| {
        let mut l = PGN_LIMITS;
        f(&mut l);
        l
    };
    pgn_error(read_with("1. e4 *", limits(|l| l.max_bytes = 4)), None, None, "too large");
    pgn_error(read_pgn_bytes(&[0; 100], &limits(|l| l.max_bytes = 99)), None, None, "too large");
    pgn_error(read_with(&"\u{e9}".repeat(60), limits(|l| l.max_bytes = 100)), None, None, "too large");
    let shuffle = "Nf3 Nf6 Ng1 Ng8 ".repeat(10);
    pgn_error(
        read_with(&shuffle, limits(|l| l.max_plies = 39)),
        Some(1),
        Some(157),
        "too many moves (more than 39)",
    );
    assert_eq!(read_with(&shuffle, limits(|l| l.max_plies = 40)).unwrap().moves.len(), 40);
    let max_tags = u32::try_from(PGN_LIMITS.max_tags).unwrap();
    err(&"[A \"1\"]\n".repeat(PGN_LIMITS.max_tags + 1), Some(max_tags + 1), None, "too many tags");
    err(&format!("[{} \"x\"]", "A".repeat(65)), None, None, "tag name too long");
    err(&format!("[A \"{}\"]", "x".repeat(2049)), None, None, "value of tag A too long");
    err(&format!("1. e4 {}", "(".repeat(65)), None, None, "nested too deeply");
    assert_eq!(read(&format!("1. e4 {}{} e5 *", "( d4 ".repeat(64), ")".repeat(64))).moves.len(), 2);
}

#[test]
fn fuzz_mutated_pgn_gives_a_game_or_a_pgn_error() {
    let base =
        std::fs::read_to_string(repo_path("tests/data/server-pgn/02-resignation-castling.pgn")).unwrap();
    let base: Vec<char> = base.chars().collect();
    let alphabet: Vec<char> =
        "[]{}()\"\\;%$!?+-=*/.:0123456789 \n\rabcdefghKQRBNOxo#\u{bd}\u{2026}\u{0}\u{e9}".chars().collect();
    let mut r = XorShift::new(7);
    let (mut games, mut errors) = (0, 0);
    for _ in 0..3000 {
        let mut chars = base.clone();
        let edits = 1 + r.below(6);
        for _ in 0..edits {
            let at = r.below(chars.len() + 1);
            let c = alphabet[r.below(alphabet.len())];
            let op = r.next_f64();
            if op < 0.4 {
                if at < chars.len() {
                    chars.remove(at);
                }
            } else if op < 0.7 {
                chars.insert(at, c);
            } else if at < chars.len() {
                chars[at] = c;
            } else {
                chars.push(c);
            }
        }
        let text: String = chars.into_iter().collect();
        match read_pgn(&text, &PGN_LIMITS) {
            Ok(g) => {
                let mut p =
                    g.start_fen.as_deref().map_or_else(Position::start, |f| Position::from_fen(f).unwrap());
                for &m in &g.moves {
                    assert!(p.play(m).is_ok(), "every move legal\n--- input ---\n{text}");
                }
                assert!(["1-0", "0-1", "1/2-1/2", "*"].contains(&g.result));
                games += 1;
            }
            Err(e) => {
                assert!(e.line >= 1 && e.column >= 1 && !e.message.is_empty(), "{e:?}");
                errors += 1;
            }
        }
    }
    assert!(games > 100 && errors > 100, "{games} games, {errors} errors");
}

#[test]
fn large_inputs_are_read_in_linear_time() {
    // 1 MiB of comments and variations around a short game.
    let filler = format!("{{ {}}} (1. d4 d5 2. c4 e6) ", "a comment that goes on and on ".repeat(20));
    let big = format!("[Event \"big\"]\n1. e4 {} e5 *", filler.repeat((1 << 20) / filler.len() - 1));
    assert!(big.len() < 1 << 20 && big.len() > 900_000);
    let t0 = Instant::now();
    assert_eq!(read(&big).moves.len(), 2);
    let ms = t0.elapsed().as_secs_f64() * 1e3;
    assert!(ms < 2000.0 * SLOWDOWN, "{ms} ms");
    // 1500 plies of shuffling knights (no automatic ending stops the reader).
    let long = "Nf3 Nf6 Ng1 Ng8 ".repeat(375);
    let t1 = Instant::now();
    assert_eq!(read(&long).moves.len(), 1500);
    let ms = t1.elapsed().as_secs_f64() * 1e3;
    assert!(ms < 2000.0 * SLOWDOWN, "{ms} ms");
    // A line full of quotes inside a tag value does not cost one pass per quote.
    err(&format!("[A \"{}\"]\n1. e4 *", "\"".repeat(3000)), None, None, "too long");
    assert_eq!(read(&format!("[A \"{}\"]\n1. e4 *", "\"".repeat(2000))).tags[0].1, "\"".repeat(2000));
    let t2 = Instant::now();
    read(&format!("[A \"{}\"]\n1. e4 *", "\" ".repeat(1000)));
    let ms = t2.elapsed().as_secs_f64() * 1e3;
    assert!(ms < 500.0 * SLOWDOWN, "{ms} ms");
    assert_eq!(Color::Black as u8, 1);
}

#[test]
fn many_tag_pairs_on_one_long_line_are_read_in_linear_time() {
    // Every tag pair looks for the last closing quote of its line: once per line, not once per
    // tag.
    let max_tags = PGN_LIMITS.max_tags;
    let tags: String = (0..=max_tags).map(|i| format!("[T{i} \"v\"] ")).collect();
    let too_many = tags.clone() + &"x".repeat((1 << 20) - tags.len() - 1);
    assert!(too_many.len() < 1 << 20 && too_many.len() > 1_000_000);
    let t0 = Instant::now();
    err(&too_many, Some(1), None, "too many tags");
    let ms0 = t0.elapsed().as_secs_f64() * 1e3;
    assert!(ms0 < 300.0 * SLOWDOWN, "{ms0} ms");
    // A valid game: 128 tags on its first line, then a comment of almost 1 MiB on the same line.
    let head: String = (0..max_tags).map(|i| format!("[T{i} \"v\"] ")).collect::<String>() + "{ ";
    let valid = head.clone() + &"c".repeat((1 << 20) - head.len() - 16) + " } 1. e4 e5 *";
    assert!(valid.len() < 1 << 20 && valid.len() > 1_000_000);
    let t1 = Instant::now();
    let g = read(&valid);
    let ms1 = t1.elapsed().as_secs_f64() * 1e3;
    assert_eq!(g.tags.len(), max_tags);
    assert_eq!(g.moves.len(), 2);
    assert!(ms1 < 300.0 * SLOWDOWN, "{ms1} ms");
    // The same answers as one tag per line (the cache of the line's last quote changes nothing).
    let lines = tags.replace("] ", "]\n");
    err(&lines, Some(u32::try_from(max_tags).unwrap() + 1), None, "too many tags");
    assert_eq!(read("[A \"x\"y\"] [B \"a\"b\"]\n1. e4 *").tags, tag_list(&[("A", "x\"y"), ("B", "a\"b")]));
    assert_eq!(
        read("[A \"x\" ] [B \"y\"]   [C \"z\\\"w\"]\n1. e4 *").tags,
        tag_list(&[("A", "x"), ("B", "y"), ("C", "z\"w")])
    );
    println!("tags then 1 MiB on one line: {ms0:.1} ms (too many tags), {ms1:.1} ms (valid)");
}
