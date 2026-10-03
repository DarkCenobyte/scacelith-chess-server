//! Cross-check against the game's own C++ rules: replays `test/fixtures/chess-crosscheck.json`
//! (`tools/gen-chess-crosscheck.sh`) and requires every FEN, digest, legal move list, SAN, move
//! flag, draw/claim predicate, final result, Zobrist key and PGN to match.

mod common;

use common::{decode_base64, fen_prefix, fnv, read_json, server_path};
use scacelith_chess::{ChessGame, Color, GameStatus, PgnTags, Position, move_uci};
use serde_json::Value;

fn fixture() -> Value {
    read_json(server_path("test/fixtures/chess-crosscheck.json"))
}

fn decode_legal(b64: &str) -> Vec<u16> {
    decode_base64(b64).chunks(2).map(|b| u16::from_le_bytes([b[0], b[1]])).collect()
}

fn sorted_legal(p: &Position) -> Vec<u16> {
    let mut v = p.legal_moves();
    v.sort_unstable();
    v
}

fn describe(list: &[u16]) -> String {
    list.iter().map(|&m| move_uci(m)).collect::<Vec<_>>().join(" ")
}

fn as_u64(v: &Value) -> u64 {
    v.as_u64().unwrap_or_else(|| panic!("number expected: {v}"))
}

fn as_str(v: &Value) -> &str {
    v.as_str().unwrap_or_else(|| panic!("string expected: {v}"))
}

/// Info bits as the generator writes them.
fn info_of(game: &ChessGame) -> u64 {
    let p = game.position();
    let mut info = 0;
    if p.in_check() {
        info |= 1;
    }
    if p.has_insufficient_material() {
        info |= 2;
    }
    if p.can_color_mate(Color::White) {
        info |= 4;
    }
    if p.can_color_mate(Color::Black) {
        info |= 8;
    }
    if game.can_claim_threefold() {
        info |= 16;
    }
    if game.can_claim_fifty_move() {
        info |= 32;
    }
    info | u64::from(game.repetition_count()) << 8
}

/// `is_legal` over candidate u16 values must accept exactly the legal list. full: every from/to
/// pair with promotion 0, plus promotions 1..7 from every own pawn; otherwise from the side's
/// pieces only (promotions 1..7 for pawns on their 7th rank).
fn check_is_legal_set(pos: &Position, legal: &[u16], full: bool, at: &str) {
    let mut accepted = 0;
    for from in 0..64u8 {
        let piece = pos.piece_at(from);
        let own = piece.is_some_and(|p| p.color == pos.side());
        if !full && !own {
            continue;
        }
        let pawn = own && piece.is_some_and(|p| p.kind == scacelith_chess::PieceType::Pawn);
        let seventh = if pos.side() == Color::White { 6 } else { 1 };
        let pawn_near_end = pawn && from >> 3 == seventh;
        let max_promo = if (full && pawn) || pawn_near_end { 7 } else { 0 };
        for to in 0..64u8 {
            for promo in 0..=max_promo {
                let m = scacelith_chess::encode_move(from, to, promo);
                let ok = pos.is_legal(m);
                assert_eq!(ok, legal.contains(&m), "{at}: is_legal({} = {m}) in {}", move_uci(m), pos.fen());
                accepted += usize::from(ok);
            }
        }
    }
    assert_eq!(accepted, legal.len(), "{at}: is_legal accepted count");
    // Values that are never moves.
    let first = legal.first().copied().unwrap_or(0);
    for bad in [0x8000 | first, 0xffff, 0x8000] {
        assert!(!pos.is_legal(bad), "{at}: is_legal({bad})");
    }
}

fn check_position(game: &ChessGame, fen: &str, digest: u64, legal_b64: &str, at: &str) -> Vec<u16> {
    let pos = game.position();
    assert_eq!(pos.fen(), fen, "{at}: fen");
    assert_eq!(u64::from(pos.digest()), digest, "{at}: digest");
    assert_eq!(pos.digest(), fnv(&fen_prefix(fen)), "{at}: digest vs fen");
    let expected = decode_legal(legal_b64);
    let got = sorted_legal(pos);
    assert!(
        got == expected,
        "{at}: legal moves in {fen}\n  game:   {}\n  server: {}",
        describe(&expected),
        describe(&got)
    );
    assert_eq!(pos.legal_move_count(), expected.len());
    assert_eq!(pos.has_legal_move(), !expected.is_empty());
    // A FEN round trip reproduces the same position and repetition key.
    let again = Position::from_fen(fen).unwrap_or_else(|| panic!("{at}: reparse {fen}"));
    assert_eq!(again.fen(), fen);
    assert_eq!(again.repetition_key(), pos.repetition_key(), "{at}: incremental vs fresh key");
    assert_eq!(again.digest(), pos.digest());
    assert_eq!(&again, pos, "{at}: the whole state");
    got
}

#[test]
fn fen_parsing_and_normalisation_match_set_fen() {
    let fixture = fixture();
    let fens = fixture["fens"].as_array().unwrap();
    assert!(fens.len() > 50);
    for entry in fens {
        let input = as_str(&entry[0]);
        let parsed = Position::from_fen(input);
        match entry[1].as_str() {
            None => assert!(parsed.is_none(), "refused by the game: {input:?}"),
            Some(fen) => {
                let p = parsed.unwrap_or_else(|| panic!("accepted by the game: {input:?}"));
                assert_eq!(p.fen(), fen, "{input:?}");
            }
        }
    }
}

#[test]
fn san_of_every_legal_move_of_hand_picked_positions() {
    let fixture = fixture();
    let cases = fixture["san"].as_array().unwrap();
    assert!(cases.len() > 20);
    for case in cases {
        let fen = as_str(&case["fen"]);
        let p = Position::from_fen(fen).unwrap();
        let moves: Vec<(u16, &str)> =
            case["moves"].as_array().unwrap().iter().map(|m| (as_u64(&m[0]) as u16, as_str(&m[1]))).collect();
        assert_eq!(sorted_legal(&p), moves.iter().map(|m| m.0).collect::<Vec<_>>(), "legal moves of {fen}");
        for (m, san) in moves {
            assert_eq!(p.san(m), san, "{fen} {}", move_uci(m));
            assert_eq!(p.parse_uci(&move_uci(m)), Some(m));
        }
        assert_eq!(p.fen(), fen, "san() leaves the position unchanged");
    }
}

#[test]
fn games_replayed_move_by_move() {
    let fixture = fixture();
    let tags = &fixture["pgnTags"];
    let tag = |k: &str| Some(as_str(&tags[k]).to_owned());
    let pgn_tags = PgnTags {
        event: tag("event"),
        site: tag("site"),
        date: tag("date"),
        round: tag("round"),
        white: tag("white"),
        black: tag("black"),
        time_control: tag("timeControl"),
        ..PgnTags::default()
    };
    let games = fixture["games"].as_array().unwrap();
    assert!(games.len() >= 250, "a few hundred games");
    let (mut plies, mut full_scans, mut pgns) = (0usize, 0usize, 0usize);
    for (gi, g) in games.iter().enumerate() {
        let start = g["start"].as_str();
        let mut game = ChessGame::new(start).unwrap();
        let ply_list = g["plies"].as_array().unwrap();
        for (i, ply) in ply_list.iter().enumerate() {
            let at = format!("game {gi} ply {i}");
            let (fen, digest, legal_b64) = (as_str(&ply[0]), as_u64(&ply[1]), as_str(&ply[2]));
            let (m, san, flags, info) =
                (as_u64(&ply[3]) as u16, as_str(&ply[4]), as_u64(&ply[5]), as_u64(&ply[6]));
            assert_eq!(game.status(), GameStatus::Ongoing, "{at}: status");
            let legal = check_position(&game, fen, digest, legal_b64, &at);
            assert_eq!(info_of(&game), info, "{at}: info bits in {fen}");
            let full = (plies + i) % 16 == 0;
            full_scans += usize::from(full);
            check_is_legal_set(game.position(), &legal, full, &at);
            assert_eq!(game.position().san(m), san, "{at}: SAN in {fen}");
            assert_eq!(game.position().parse_uci(&move_uci(m)), Some(m));
            assert!(game.is_legal(m));
            let r = game.play(m).unwrap_or_else(|e| panic!("{at}: play: {e}"));
            assert_eq!(u64::from(r.flags.bits()), flags, "{at}: flags of {san} in {fen}");
            assert_eq!(r.status, game.status());
            assert_eq!(r.reason, game.reason());
        }
        plies += ply_list.len();
        let final_ = &g["final"];
        let at = format!("game {gi} final");
        let fen = as_str(&final_[0]);
        check_position(&game, fen, as_u64(&final_[1]), as_str(&final_[2]), &at);
        assert_eq!(info_of(&game), as_u64(&final_[3]), "{at}: info bits in {fen}");
        assert_eq!(u64::from(game.status().as_u8()), as_u64(&g["status"]), "{at}: status in {fen}");
        assert_eq!(u64::from(game.reason().as_u8()), as_u64(&g["reason"]), "{at}: reason in {fen}");
        assert_eq!(
            game.position().repetition_key(),
            as_str(&g["hash"]),
            "{at}: Zobrist key = chess::Position::hash()"
        );
        let sans: Vec<&str> = ply_list.iter().map(|p| as_str(&p[4])).collect();
        assert_eq!(game.san_moves(), sans);
        if let Some(pgn) = g["pgn"].as_str() {
            assert_eq!(game.pgn(&pgn_tags), pgn, "{at}: pgn");
            pgns += 1;
        }
        // The same game rebuilt from its move list (journal replay).
        let replay = ChessGame::from_moves(start, game.moves()).unwrap();
        assert_eq!(replay.position().fen(), fen);
        assert_eq!(replay.status(), game.status());
        assert_eq!(replay.reason(), game.reason());
    }
    assert!(plies > 10000, "plies checked: {plies}");
    assert!(full_scans > 500);
    assert!(pgns > 20);
}

#[test]
fn the_fixture_exercises_the_special_rules() {
    let fixture = fixture();
    let mut seen = std::collections::BTreeMap::<&str, u32>::new();
    let mut count = |k| *seen.entry(k).or_default() += 1;
    for g in fixture["games"].as_array().unwrap() {
        for p in g["plies"].as_array().unwrap() {
            let (m, f, info) = (as_u64(&p[3]), as_u64(&p[5]), as_u64(&p[6]));
            for (bit, name) in [(2, "ep"), (4, "castleK"), (8, "castleQ")] {
                if f & bit != 0 {
                    count(name);
                }
            }
            if f & 32 != 0 {
                count("promo");
                if m >> 12 != 5 {
                    count("underPromo");
                }
            }
            if info >> 8 >= 3 {
                count("rep3");
            }
            if info & 32 != 0 {
                count("fifty");
            }
        }
        match as_u64(&g["reason"]) {
            1 => count("mate"),
            5 => count("stalemate"),
            6 => count("dead"),
            8 => count("fivefold"),
            9 => count("seventyFive"),
            _ => {}
        }
    }
    for k in [
        "ep",
        "castleK",
        "castleQ",
        "underPromo",
        "promo",
        "mate",
        "stalemate",
        "fivefold",
        "seventyFive",
        "dead",
        "rep3",
        "fifty",
    ] {
        let v = seen.get(k).copied().unwrap_or(0);
        assert!(v >= 3, "{k}: {v}");
    }
}
