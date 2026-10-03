//! FEN parsing / normalisation / round trips, SAN, UCI, legal move queries, dead positions (the
//! cases of the game's tests/chess_tests.cpp and of the former server's notation tests).

mod common;

use common::{Lcg, fen_prefix, fnv, mv, pos, sq};
use scacelith_chess::{
    Color, MoveFlags, Piece, PieceType, Position, START_FEN, castling, move_from, move_to, move_uci,
    parse_square, square_name,
};

const KIWIPETE: &str = "r3k2r/p1ppqpb1/bn2pnp1/3PN3/1p2P3/2N2Q1p/PPPBBPPP/R3K2R w KQkq - 0 1";

fn san_of(fen: &str, uci: &str) -> String {
    let p = pos(fen);
    let m = p.parse_uci(uci).unwrap_or_else(|| panic!("{uci} legal in {fen}"));
    p.san(m)
}

#[test]
fn squares() {
    assert_eq!(square_name(0), "a1");
    assert_eq!(square_name(28), "e4");
    assert_eq!(square_name(63), "h8");
    assert_eq!(square_name(64), "-");
    assert_eq!(square_name(255), "-");
    assert_eq!(parse_square("e4"), Some(28));
    assert_eq!(parse_square("E4"), Some(28));
    assert_eq!(parse_square("h8"), Some(63));
    assert_eq!(parse_square("i1"), None);
    assert_eq!(parse_square("a9"), None);
    assert_eq!(parse_square("e"), None);
    assert_eq!(parse_square("e44"), None);
}

#[test]
fn fen_round_trips_digest_and_repetition_key() {
    let fens = [
        START_FEN,
        KIWIPETE,
        "8/2p5/3p4/KP5r/1R3p1k/8/4P1P1/8 w - - 0 1",
        "r3k2r/Pppp1ppp/1b3nbN/nP6/BBP1P3/q4N2/Pp1P2PP/R2Q1RK1 w kq - 0 1",
        "rnbq1k1r/pp1Pbppp/2p5/8/2B5/8/PPP1NnPP/RNBQK2R w KQ - 1 8",
        "rnbqkbnr/ppp1p1pp/8/3pPp2/8/8/PPPP1PPP/RNBQKBNR w KQkq f6 0 3",
        "rnbqkbnr/pppp1ppp/8/8/3Pp3/8/PPP1PPPP/RNBQKBNR b KQkq d3 0 3",
        "8/8/8/8/8/8/8/K6k w - - 99 150",
        "4k3/8/8/8/8/8/8/4K2R w K - 0 1",
        "r3k3/8/8/8/8/8/8/4K3 b q - 5 40",
    ];
    for f in fens {
        let p = pos(f);
        assert_eq!(p.fen(), f);
        assert_eq!(p.digest(), fnv(&fen_prefix(f)));
        let q = pos(&p.fen());
        assert_eq!(q.repetition_key(), p.repetition_key());
        assert_eq!(q.fen(), f);
    }
    let start = Position::start();
    assert_eq!(start.fen(), START_FEN);
    assert_eq!(Position::default(), start);
    assert_eq!(start.side(), Color::White);
    assert_eq!(start.castling(), 15);
    assert_eq!(start.ep_square(), None);
    assert_eq!(start.halfmove(), 0);
    assert_eq!(start.fullmove(), 1);
    assert_eq!(start.king_square(Color::White), sq("e1"));
    assert_eq!(start.king_square(Color::Black), sq("e8"));
    assert_eq!(start.piece_at(sq("d1")), Some(Piece { color: Color::White, kind: PieceType::Queen }));
    assert_eq!(start.piece_at(sq("d1")).map(Piece::code), Some(5));
    assert_eq!(start.piece_at(sq("g8")).map(Piece::code), Some(8 | 2));
    assert_eq!(start.piece_at(sq("e4")), None);
    let key = start.repetition_key();
    assert!(key.len() == 16 && key.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)));
    // Optional move counters.
    assert_eq!(pos("4k3/8/8/8/8/8/8/4K3 w - -").fen(), "4k3/8/8/8/8/8/8/4K3 w - - 0 1");
    assert_eq!(pos("4k3/8/8/8/8/8/8/4K3 w - - 0 0").fen(), "4k3/8/8/8/8/8/8/4K3 w - - 0 1");
    // Transpositions reach the same key; the side to move matters.
    let (mut a, mut b) = (Position::start(), Position::start());
    for m in ["g1f3", "g8f6", "b1c3"] {
        a.play(a.parse_uci(m).unwrap()).unwrap();
    }
    for m in ["b1c3", "g8f6", "g1f3"] {
        b.play(b.parse_uci(m).unwrap()).unwrap();
    }
    assert_eq!(a.repetition_key(), b.repetition_key());
    assert_eq!(a.digest(), b.digest());
    assert_ne!(a.repetition_key(), Position::start().repetition_key());
}

#[test]
fn digest_and_key_vectors() {
    // Protocol posHash vectors (PROTOCOL.md, moves and positions) and the Zobrist keys of the
    // game's chess::Position::hash() (src/chess/chess.h).
    let start = Position::start();
    assert_eq!(start.digest(), 923_150_620);
    assert_eq!(start.digest(), 0x3706_291C);
    assert_eq!(start.key(), 0x433b_89fa_981a_aa3c);
    let mut e4 = start;
    e4.play(mv("e2", "e4", 0)).unwrap();
    assert_eq!(e4.digest(), 1_150_555_523);
    assert_eq!(e4.repetition_key(), "3b7231ef057ac043");
    let d6 = pos("rnbqkbnr/ppp1pppp/8/3pP3/8/8/PPPP1PPP/RNBQKBNR w KQkq d6 0 2");
    assert_eq!(d6.digest(), 748_388_051);
    assert_eq!(d6.repetition_key(), "b6a00c005da37621");
    let kiwipete = pos(KIWIPETE);
    assert_eq!(kiwipete.digest(), 512_637_518);
    assert_eq!(kiwipete.repetition_key(), "8335c367cbadff13");
}

#[test]
fn fen_rejected_like_set_fen() {
    let bad = [
        "",
        "   ",
        "hello",
        "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP w KQkq - 0 1",
        "rnbqkbnr/pppppppp/9/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1",
        "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR x KQkq - 0 1",
        "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkx - 0 1",
        "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq e9 0 1",
        "8/8/8/8/8/8/8/K7 w - - 0 1",
        "k7/8/8/8/8/8/8/KK6 w - - 0 1",
        "P3k3/8/8/8/8/8/8/4K3 w - - 0 1",
        "4k3/8/8/8/8/8/8/p3K3 w - - 0 1",
        "4k3/8/8/8/8/P7/PPPPPPPP/4K3 w - - 0 1",
        "4k3/8/8/8/8/N7/PPPPPPPP/RNBQKBNR w KQ - 0 1",
        "4k2R/8/8/8/8/8/8/4K3 w - - 0 1",
        "4k3/8/8/8/8/8/8/4K3 w - - x 1",
        "4k3/8/8/8/8/8/8/4K3 w - - -1 1",
        "4k3/8/8/8/8/8/8/4K3 w - - 1000000000 1",
        "4k3/8/8/8/8/8/8/4K3 w - - 0 1 extra",
        "4k3/8/8/8/8/8/8/4K3 w -",
        // Non-ASCII anywhere.
        "4k3/8/8/8/8/8/8/4K3 w - é6 0 1",
        "4k3/8/8/8/8/8/8/4K3 w - - ٣ 1",
        "4k3/8/8/8/8/8/8/4K3\u{a0}w - - 0 1",
        "4k3/8/8/8/8/8/8/4K3 w K\u{301} - 0 1",
    ];
    for f in bad {
        assert!(Position::from_fen(f).is_none(), "{f:?}");
    }
    assert!(Position::from_fen("4k3/8/8/8/8/8/8/4K2R b - - 0 1").is_some()); // Black in check, to move: fine
    assert!(Position::from_fen("4k3/8/8/8/8/8/8/4K3\tw\t-\t-\t0\t1").is_some()); // tabs separate fields
    assert_eq!(
        pos("4k3/8/8/8/8/8/8/4K3 w - - 999999999 999999999").fen(),
        "4k3/8/8/8/8/8/8/4K3 w - - 999999999 999999999"
    );
}

#[test]
fn fen_normalisation_castling_rights_and_en_passant() {
    // Castling rights without king/rook on their squares are dropped.
    assert_eq!(pos("4k3/8/8/8/8/8/8/4K3 w KQkq - 0 1").castling(), 0);
    assert_eq!(pos("r3k3/8/8/8/8/8/8/4K2R w KQkq - 0 1").fen(), "r3k3/8/8/8/8/8/8/4K2R w Kq - 0 1");
    assert_eq!(pos("r3k2r/8/8/8/8/8/8/R4K1R w KQkq - 0 1").fen(), "r3k2r/8/8/8/8/8/8/R4K1R w kq - 0 1");
    assert_eq!(pos("r3k2r/8/8/8/8/8/8/R3K2R w qkQK - 0 1").fen(), "r3k2r/8/8/8/8/8/8/R3K2R w KQkq - 0 1");
    assert_eq!(
        pos("r3k2r/8/8/8/8/8/8/R3K2R w KQkq - 0 1").castling(),
        castling::WHITE_KING_SIDE
            | castling::WHITE_QUEEN_SIDE
            | castling::BLACK_KING_SIDE
            | castling::BLACK_QUEEN_SIDE
    );
    // En passant square kept only when a capture is legal.
    let p = pos("rnbqkbnr/ppp1pppp/8/3p4/8/8/PPPPPPPP/RNBQKBNR w KQkq d6 0 2");
    assert_eq!(p.ep_square(), None);
    assert_eq!(p.fen(), "rnbqkbnr/ppp1pppp/8/3p4/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 2");
    assert_eq!(
        pos("rnbqkbnr/ppp1pppp/8/3pP3/8/8/PPPP1PPP/RNBQKBNR w KQkq d6 0 2").ep_square(),
        Some(sq("d6"))
    );
    // Wrong rank: silently dropped, like the game.
    assert_eq!(pos("rnbqkbnr/ppp1pppp/8/3pP3/8/8/PPPP1PPP/RNBQKBNR w KQkq d3 0 2").ep_square(), None);
    // Pinned capturer (rank pin through both pawns): not stored.
    assert_eq!(pos("4k3/8/8/KPp4r/8/8/8/8 w - c6 0 2").ep_square(), None);
    assert_eq!(pos("4k3/8/8/1Pp5/8/8/8/K7 w - c6 0 2").ep_square(), Some(sq("c6")));
    // Diagonal pin of the capturer.
    assert_eq!(pos("4k3/8/8/2pP4/8/8/8/4K1b1 w - c6 0 1").fen(), "4k3/8/8/2pP4/8/8/8/4K1b1 w - c6 0 1");
    assert_eq!(pos("4k3/8/8/2pP4/8/b7/8/4K3 w - c6 0 1").ep_square(), Some(sq("c6")));
    assert_eq!(pos("7k/8/8/8/3pP3/8/8/B6K b - e3 0 1").ep_square(), None); // d4 pawn pinned on a1-h8
    // The capture removes the checking pawn: legal although in check.
    assert_eq!(pos("8/8/8/8/k1pP4/8/8/4K3 b - d3 0 1").ep_square(), Some(sq("d3")));
    // Discovered check along the rank by removing both pawns: illegal.
    assert_eq!(pos("8/8/8/8/k1pP3Q/8/8/4K3 b - d3 0 1").ep_square(), None);
    // After a double push the ep square appears only when capturable.
    let mut p = Position::start();
    p.play(p.parse_uci("e2e4").unwrap()).unwrap();
    assert_eq!(p.ep_square(), None);
    assert_eq!(p.fen(), "rnbqkbnr/pppppppp/8/8/4P3/8/PPPP1PPP/RNBQKBNR b KQkq - 0 1");
    for u in ["g8f6", "e4e5", "d7d5"] {
        p.play(p.parse_uci(u).unwrap()).unwrap();
    }
    assert_eq!(p.fen(), "rnbqkb1r/ppp1pppp/5n2/3pP3/8/8/PPPP1PPP/RNBQKBNR w KQkq d6 0 3");
    let mut p = pos("4k3/2p5/8/KP5r/8/8/8/8 b - - 0 1");
    p.play(p.parse_uci("c7c5").unwrap()).unwrap();
    assert_eq!(p.ep_square(), None);
    assert_eq!(p.parse_uci("b5c6"), None);
}

#[test]
fn san_disambiguation_promotions_captures_checks_mates_castling() {
    assert_eq!(san_of(START_FEN, "g1f3"), "Nf3");
    assert_eq!(san_of(START_FEN, "e2e4"), "e4");
    let knights = "rnbqkb1r/ppp1pppp/5n2/3p4/8/8/PPPPPPPP/RNBQKBNR b KQkq - 0 1";
    assert_eq!(san_of(knights, "b8d7"), "Nbd7");
    assert_eq!(san_of(knights, "f6d7"), "Nfd7");
    assert_eq!(san_of(knights, "b8c6"), "Nc6");
    let rooks = "4k3/8/8/R7/8/8/8/R3K3 w - - 0 1";
    assert_eq!(san_of(rooks, "a1a3"), "R1a3");
    assert_eq!(san_of(rooks, "a5a3"), "R5a3");
    assert_eq!(san_of(rooks, "a1d1"), "Rd1");
    assert_eq!(san_of(rooks, "a5a8"), "Ra8+");
    let queens = "1k6/8/8/8/4Q2Q/8/K7/7Q w - - 0 1";
    assert_eq!(san_of(queens, "h4e1"), "Qh4e1");
    assert_eq!(san_of(queens, "e4e1"), "Qee1");
    assert_eq!(san_of(queens, "h1e1"), "Q1e1");
    let knights_file = "4k3/8/8/8/8/N7/8/N3K3 w - - 0 1";
    assert_eq!(san_of(knights_file, "a1c2"), "N1c2");
    assert_eq!(san_of(knights_file, "a3c2"), "N3c2");
    let promo = "k2r4/4P3/8/8/8/8/8/4K3 w - - 0 1";
    assert_eq!(san_of(promo, "e7d8q"), "exd8=Q+");
    assert_eq!(san_of(promo, "e7d8r"), "exd8=R+");
    assert_eq!(san_of(promo, "e7d8b"), "exd8=B");
    assert_eq!(san_of(promo, "e7d8n"), "exd8=N");
    assert_eq!(san_of(promo, "e7e8q"), "e8=Q");
    assert_eq!(san_of("8/2P1k3/8/8/8/8/8/K7 w - - 0 1", "c7c8n"), "c8=N+");
    assert_eq!(san_of("4rkr1/4p1p1/8/8/8/8/8/4K2R w K - 0 1", "e1g1"), "O-O#");
    assert_eq!(san_of("5k2/8/8/8/8/8/8/4K2R w K - 0 1", "e1g1"), "O-O+");
    assert_eq!(san_of("3k4/8/8/8/8/8/8/R3K3 w Q - 0 1", "e1c1"), "O-O-O+");
    assert_eq!(san_of("r3k3/8/8/8/8/8/8/R3K3 w Qq - 0 1", "e1c1"), "O-O-O");
    assert_eq!(san_of("r3k3/8/8/8/8/8/8/4K3 b q - 0 1", "e8c8"), "O-O-O");
    assert_eq!(san_of("rnbqkbnr/ppp1p1pp/8/3pPp2/8/8/PPPP1PPP/RNBQKBNR w KQkq f6 0 3", "e5f6"), "exf6");
    assert_eq!(san_of("rnbqkbnr/ppp2ppp/3p4/4p3/4P3/8/PPPP1PPP/RNBQKBNR w KQkq - 0 3", "f1b5"), "Bb5+");
    assert_eq!(san_of("4k3/8/8/2p1p3/3P4/8/8/4K3 w - - 0 1", "d4c5"), "dxc5");
    assert_eq!(san_of("4k3/8/8/2p1p3/3P4/8/8/4K3 w - - 0 1", "d4e5"), "dxe5");
    assert_eq!(san_of("rnbqkbnr/pppp1ppp/8/4p3/6P1/5P2/PPPPP2P/RNBQKBNR b KQkq - 0 2", "d8h4"), "Qh4#");
    assert_eq!(san_of("6k1/5ppp/8/8/8/8/8/R3R1K1 w - - 0 1", "e1e8"), "Re8#");
    assert_eq!(san_of("6k1/5ppp/8/8/8/8/8/R3R1K1 w - - 0 1", "a1d1"), "Rad1");
    assert_eq!(san_of("6k1/5ppp/8/8/8/8/8/R3R1K1 w - - 0 1", "e1d1"), "Red1");
    assert_eq!(san_of("6k1/4pppp/8/8/8/8/8/R3R1K1 w - - 0 1", "a1a8"), "Ra8#");
    // A pinned piece does not count for disambiguation (only legal moves do).
    assert_eq!(san_of("4k3/8/8/8/1b6/8/3N4/4K1N1 w - - 0 1", "g1f3"), "Nf3");
    // Illegal -> empty, and the position is unchanged by san().
    let p = Position::start();
    assert_eq!(p.san(mv("e2", "e5", 0)), "");
    assert_eq!(p.san(mv("e2", "e4", 5)), "");
    assert_eq!(p.san(mv("e2", "e4", 0) | 0x8000), "");
    assert_eq!(p.fen(), START_FEN);
}

#[test]
fn san_disambiguation_matches_the_legal_move_list_over_random_games() {
    // san() validates only the other pieces of the same code; the reference filters every legal move.
    let starts = [
        START_FEN,
        KIWIPETE,
        "q3k2q/8/8/8/8/8/1QQQ4/Q3K2Q w - - 0 1",
        "rn2k1nr/8/8/8/8/8/8/RN2K1NR w KQkq - 0 1",
        "b3k2b/8/8/8/8/8/8/B3K2B w - - 0 1",
        "k7/2PPPPPP/8/8/8/8/2pppppp/K7 w - - 0 1",
    ];
    let mut rng = Lcg(0x5a17);
    let mut checked = 0;
    for fen in starts {
        for _ in 0..4 {
            let mut p = pos(fen);
            for _ in 0..150 {
                let legal = p.legal_moves();
                if legal.is_empty() {
                    break;
                }
                for &m in &legal {
                    let (from, to) = (move_from(m), move_to(m));
                    let piece = p.piece_at(from).unwrap();
                    if piece.kind == PieceType::Pawn
                        || (piece.kind == PieceType::King && (to & 7).abs_diff(from & 7) == 2)
                    {
                        continue;
                    }
                    let (mut ambiguous, mut same_file, mut same_rank) = (false, false, false);
                    for &o in &legal {
                        let of = move_from(o);
                        if move_to(o) != to || of == from || p.piece_at(of) != Some(piece) {
                            continue;
                        }
                        ambiguous = true;
                        same_file |= of & 7 == from & 7;
                        same_rank |= of >> 3 == from >> 3;
                    }
                    let name = square_name(from);
                    let dis = if !ambiguous {
                        ""
                    } else if !same_file {
                        &name[..1]
                    } else if !same_rank {
                        &name[1..]
                    } else {
                        name
                    };
                    let capture = if p.piece_at(to).is_some() { "x" } else { "" };
                    let want = format!("{}{dis}{capture}{}", piece.kind.letter(), square_name(to));
                    let san = p.san(m);
                    assert_eq!(san.trim_end_matches(['+', '#']), want, "{} {}", p.fen(), move_uci(m));
                    checked += 1;
                }
                p.play(legal[rng.below(legal.len())]).unwrap();
            }
        }
    }
    assert!(checked > 10000, "{checked} piece moves checked");
}

#[test]
fn uci_output_and_strict_parsing() {
    let p = Position::start();
    assert_eq!(move_uci(mv("e2", "e4", 0)), "e2e4");
    assert_eq!(move_uci(mv("e7", "e8", 2)), "e7e8n");
    assert_eq!(move_uci(0xfffb), "0000");
    assert_eq!(p.parse_uci("e2e4"), Some(mv("e2", "e4", 0)));
    assert_eq!(p.parse_uci("E2E4"), Some(mv("e2", "e4", 0)));
    assert_eq!(p.parse_uci("e2e5"), None);
    assert_eq!(p.parse_uci("e2e4q"), None);
    assert_eq!(p.parse_uci("e2"), None);
    assert_eq!(p.parse_uci("z9e4"), None);
    assert_eq!(p.parse_uci("e2e4 "), None);
    assert_eq!(p.parse_uci("é2e4"), None);
    assert_eq!(p.parse_uci("e2é4"), None);
    let promo = pos("k2r4/4P3/8/8/8/8/8/4K3 w - - 0 1");
    assert_eq!(promo.parse_uci("e7e8"), None); // promotion needs a piece
    assert_eq!(promo.parse_uci("e7e8k"), None);
    assert_eq!(promo.parse_uci("e7e8p"), None);
    assert_eq!(promo.parse_uci("e7e8q"), Some(mv("e7", "e8", 5)));
    assert_eq!(promo.parse_uci("e7e8Q"), Some(mv("e7", "e8", 5)));
    assert_eq!(promo.parse_uci("e7d8n"), Some(mv("e7", "d8", 2)));
    let castle = pos("r3k2r/8/8/8/8/8/8/R3K2R w KQkq - 0 1");
    assert_eq!(castle.parse_uci("e1g1"), Some(mv("e1", "g1", 0)));
    assert_eq!(castle.parse_uci("e1c1"), Some(mv("e1", "c1", 0)));
    assert_eq!(castle.parse_uci("e1h1"), None); // king-takes-rook notation is not used
}

#[test]
fn legal_move_queries() {
    let p = Position::start();
    assert_eq!(p.legal_moves().len(), 20);
    assert_eq!(p.legal_move_count(), 20);
    assert!(p.is_legal(mv("e2", "e4", 0)));
    assert!(!p.is_legal(mv("e2", "e5", 0)));
    assert!(!p.is_legal(mv("e2", "e4", 5))); // promo bits on a normal move
    assert!(!p.is_legal(mv("e2", "e4", 0) | 0x8000)); // bit 15
    assert!(!p.is_legal(mv("e7", "e5", 0))); // not the side to move
    assert!(!p.in_check());
    assert!(p.is_attacked(sq("f3"), Color::White));
    assert!(!p.is_attacked(sq("e4"), Color::White));
    let mut q = p;
    assert!(q.play(mv("e2", "e5", 0)).is_err());
    assert_eq!(q.play(0).map_err(|e| e.to_string()), Err("illegal move 0".to_owned()));
    assert_eq!(q, p);
    assert_eq!(q.fen(), START_FEN);
    // Castling: rights, path, check.
    let c = pos("r3k2r/8/8/8/8/8/8/R3K2R w KQkq - 0 1");
    assert_eq!(c.clone().play(mv("e1", "g1", 0)), Ok(MoveFlags::CASTLE_KING));
    assert_eq!(c.clone().play(mv("e1", "c1", 0)), Ok(MoveFlags::CASTLE_QUEEN));
    let through = pos("r3k2r/8/8/8/8/8/5r2/R3K2R w KQkq - 0 1"); // f1 attacked
    assert!(!through.is_legal(mv("e1", "g1", 0)));
    assert!(through.is_legal(mv("e1", "c1", 0)));
    let b1 = pos("r3k2r/8/8/8/8/8/1r6/R3K2R w KQkq - 0 1"); // b1 attacked: O-O-O still legal
    assert!(b1.is_legal(mv("e1", "c1", 0)));
    let in_check = pos("r3k2r/8/8/8/8/8/4r3/R3K2R w KQkq - 0 1");
    assert!(in_check.in_check());
    assert!(!in_check.is_legal(mv("e1", "g1", 0)));
    assert!(!in_check.is_legal(mv("e1", "c1", 0)));
    let blocked = pos("r3k2r/8/8/8/8/8/8/RN2K1NR w KQkq - 0 1");
    assert!(!blocked.is_legal(mv("e1", "g1", 0)));
    assert!(!blocked.is_legal(mv("e1", "c1", 0)));
    // Promotions need a piece; every promotion piece is its own move.
    let promo = pos("8/4P3/8/8/8/8/k7/4K3 w - - 0 1");
    assert!(!promo.is_legal(mv("e7", "e8", 0)));
    for t in [2, 3, 4, 5] {
        assert!(promo.is_legal(mv("e7", "e8", t)));
    }
    for t in [1, 6, 7] {
        assert!(!promo.is_legal(mv("e7", "e8", t)));
    }
    assert_eq!(promo.legal_moves().iter().filter(|&&m| move_from(m) == sq("e7")).count(), 4);
    // Flags returned by play().
    let e = pos("rnbqkbnr/ppp1p1pp/8/3pPp2/8/8/PPPP1PPP/RNBQKBNR w KQkq f6 0 3");
    assert_eq!(e.clone().play(mv("e5", "f6", 0)).map(MoveFlags::bits), Ok(1 | 2));
    assert_eq!(Position::start().play(mv("e2", "e4", 0)).map(MoveFlags::bits), Ok(16));
    assert_eq!(
        pos("k2r4/4P3/8/8/8/8/8/4K3 w - - 0 1").play(mv("e7", "d8", 5)).map(MoveFlags::bits),
        Ok(1 | 32 | 64)
    );
    let mut mate = pos("rnbqkbnr/pppp1ppp/8/4p3/6P1/5P2/PPPPP2P/RNBQKBNR b KQkq - 0 2");
    assert_eq!(mate.play(mv("d8", "h4", 0)).map(MoveFlags::bits), Ok(64 | 128));
    assert!(mate.is_checkmate());
    assert!(!mate.is_stalemate());
    assert!(pos("7k/5Q2/6K1/8/8/8/8/8 b - - 0 1").is_stalemate());
    assert!(!pos("7k/5Q2/6K1/8/8/8/8/8 b - - 0 1").has_legal_move());
}

#[test]
fn insufficient_material_and_can_color_mate() {
    let dead = |f: &str| pos(f).has_insufficient_material();
    assert!(dead("4k3/8/8/8/8/8/8/4K3 w - - 0 1"));
    assert!(dead("4k3/8/8/8/8/8/8/2B1K3 w - - 0 1"));
    assert!(dead("4k3/8/8/8/8/8/8/1N2K3 w - - 0 1"));
    assert!(dead("4k3/8/8/8/8/2b5/8/2B1K3 w - - 0 1")); // both bishops on dark squares
    assert!(dead("4k3/8/8/8/8/8/8/B1B1K3 w - - 0 1")); // a1, c1 both dark
    assert!(!dead("4k3/8/8/8/8/8/2b5/2B1K3 w - - 0 1")); // opposite colours
    assert!(!dead("4k3/8/8/8/8/8/2n5/2N1K3 w - - 0 1")); // knight v knight
    assert!(!dead("4k3/8/8/8/8/8/8/1NN1K3 w - - 0 1")); // two knights
    assert!(!dead("4k3/7p/8/8/8/8/8/2B1K3 w - - 0 1")); // bishop v pawn
    assert!(!dead("4k3/8/8/8/8/8/2n5/2B1K3 w - - 0 1")); // bishop v knight
    assert!(!dead("4k3/8/8/8/8/8/8/2B1KB2 w - - 0 1")); // c1 dark, f1 light
    let p = pos("4k3/8/8/8/8/8/8/4K2N w - - 0 1");
    assert!(!p.can_color_mate(Color::White)); // lone knight v bare king
    assert!(!p.can_color_mate(Color::Black)); // bare king
    let p = pos("4k3/7p/8/8/8/8/8/4K2N w - - 0 1");
    assert!(p.can_color_mate(Color::White)); // the pawn can block: helpmate exists
    assert!(p.can_color_mate(Color::Black));
    let p = pos("4k3/8/8/8/8/8/8/R3K3 w - - 0 1");
    assert!(p.can_color_mate(Color::White));
    assert!(!p.can_color_mate(Color::Black));
    assert!(pos("4k3/8/8/8/8/8/8/1NN1K3 w - - 0 1").can_color_mate(Color::White));
    let p = pos("4k3/8/8/8/8/8/2b5/2B1K3 w - - 0 1");
    assert!(p.can_color_mate(Color::White));
    assert!(p.can_color_mate(Color::Black));
}

#[test]
fn generation_order_is_the_games() {
    // Squares a1, b1 .. h8; pawn push, double push; knights in their step order; castling last.
    let order: Vec<String> = Position::start().legal_moves().into_iter().map(move_uci).collect();
    assert_eq!(
        order.join(" "),
        "b1c3 b1a3 g1h3 g1f3 a2a3 a2a4 b2b3 b2b4 c2c3 c2c4 d2d3 d2d4 e2e3 e2e4 f2f3 f2f4 g2g3 g2g4 h2h3 h2h4"
    );
    let castle: Vec<String> =
        pos("r3k2r/8/8/8/8/8/8/R3K2R w KQkq - 0 1").legal_moves().into_iter().map(move_uci).collect();
    assert_eq!(castle[castle.len() - 2..], ["e1g1", "e1c1"]);
    let promos: Vec<u16> = pos("8/4P3/8/8/8/8/k7/4K3 w - - 0 1")
        .legal_moves()
        .into_iter()
        .filter(|&m| move_from(m) == sq("e7"))
        .collect();
    assert_eq!(promos, [5, 4, 3, 2].map(|t| mv("e7", "e8", t)));
}

#[test]
fn hostile_counters_never_overflow() {
    // The largest counters a FEN can carry keep growing without overflow.
    let mut p = pos("4k3/8/8/8/8/8/8/R3K3 b - - 999999999 999999999");
    for u in ["e8d7", "a1a2", "d7e8", "a2a1"] {
        p.play(p.parse_uci(u).unwrap()).unwrap();
    }
    assert_eq!(p.fen(), "4k3/8/8/8/8/8/8/R3K3 b - - 1000000003 1000000001");
}
