//! ChessGame: automatic endings, claims, resignation, agreement, flag fall, online endings, PGN
//! (the cases of the game's tests/chess_tests.cpp and of the former server's game tests).

mod common;

use common::{game_after, line, mv};
use scacelith_chess::{
    ChessGame, Color, EndReason, GameStatus, InvalidEnd, MoveFlags, PgnComment, PgnTags, PlayError,
    PlayResult,
};

const CYCLE: &str = "g1f3 g8f6 f3g1 f6g8";

fn uci(game: &ChessGame, u: &str) -> u16 {
    game.position().parse_uci(u).unwrap_or_else(|| panic!("{u} legal in {}", game.position().fen()))
}

fn owned(s: &str) -> Option<String> {
    Some(s.to_owned())
}

#[test]
fn checkmate_ends_the_game_and_is_recorded() {
    let mut g = game_after(None, "f2f3 e7e5 g2g4");
    let m = uci(&g, "d8h4");
    let r = g.play(m);
    assert_eq!(
        r,
        Ok(PlayResult {
            flags: MoveFlags::CHECK | MoveFlags::MATE,
            status: GameStatus::BlackWins,
            reason: EndReason::Checkmate
        })
    );
    assert_eq!(g.status(), GameStatus::BlackWins);
    assert_eq!(g.reason(), EndReason::Checkmate);
    assert!(g.is_over());
    assert_eq!(g.result_string(), "0-1");
    assert_eq!(g.san_moves(), ["f3", "e5", "g4", "Qh4#"]);
    assert_eq!(g.uci_moves(), ["f2f3", "e7e5", "g2g4", "d8h4"]);
    assert_eq!(g.ply(), 4);
    // Game over: nothing else is accepted.
    let a3 = mv("a2", "a3", 0);
    assert!(!g.is_legal(a3));
    assert_eq!(g.play(a3), Err(PlayError::GameOver));
    assert_eq!(g.moves().len(), 4);
    assert!(!g.resign(Color::White));
    assert!(!g.agree_draw());
    assert!(!g.flag_fall(Color::Black));
    assert!(!g.claim_draw());
    assert_eq!(g.end(GameStatus::Aborted, EndReason::ServerAborted), Ok(false));
    assert_eq!(g.status(), GameStatus::BlackWins);
    // Illegal moves change nothing.
    let mut h = ChessGame::default();
    assert_eq!(h.play(mv("e2", "e5", 0)), Err(PlayError::IllegalMove));
    assert_eq!(h.play(0xffff), Err(PlayError::IllegalMove));
    assert_eq!(h.play(796 | 0x8000), Err(PlayError::IllegalMove));
    assert_eq!(h.moves().len(), 0);
    assert_eq!(h.status(), GameStatus::Ongoing);
    assert_eq!(h.reason(), EndReason::None);
}

#[test]
fn stalemate() {
    let mut g = ChessGame::new(Some("7k/8/6K1/8/8/8/8/5Q2 w - - 0 1")).unwrap();
    let r = g.play(uci(&g, "f1f7")).unwrap();
    assert_eq!(r.flags, MoveFlags::NONE);
    assert_eq!(r.status, GameStatus::Draw);
    assert_eq!(r.reason, EndReason::Stalemate);
    assert_eq!(g.result_string(), "1/2-1/2");
    // A start position that is already over.
    assert_eq!(
        ChessGame::new(Some("7k/5Q2/6K1/8/8/8/8/8 b - - 0 1")).unwrap().reason(),
        EndReason::Stalemate
    );
    let mated =
        ChessGame::new(Some("rnb1kbnr/pppp1ppp/8/4p3/6Pq/5P2/PPPPP2P/RNBQKBNR w KQkq - 1 3")).unwrap();
    assert_eq!(mated.status(), GameStatus::BlackWins);
    assert_eq!(mated.reason(), EndReason::Checkmate);
}

#[test]
fn dead_positions() {
    let cases = [
        ("4k3/8/8/8/8/8/3q4/4K3 w - - 0 1", "e1d2"),     // K v K
        ("4k3/8/8/8/8/8/3n4/2B1K3 w - - 0 1", "c1d2"),   // K+B v K
        ("4k3/8/8/8/8/8/3b4/1N2K3 w - - 0 1", "b1d2"),   // K+N v K
        ("4k3/8/8/8/8/2b5/3n4/2B1K3 w - - 0 1", "c1d2"), // K+B v K+B, same colour
    ];
    for (fen, u) in cases {
        let mut g = ChessGame::new(Some(fen)).unwrap();
        assert_eq!(g.status(), GameStatus::Ongoing);
        let r = g.play(uci(&g, u)).unwrap();
        assert!(r.flags.contains(MoveFlags::CAPTURE));
        assert_eq!(r.status, GameStatus::Draw, "{fen}");
        assert_eq!(r.reason, EndReason::InsufficientMaterial, "{fen}");
    }
    assert_eq!(
        ChessGame::new(Some("4k3/8/8/8/8/8/8/4K3 w - - 0 1")).unwrap().reason(),
        EndReason::InsufficientMaterial
    );
    assert_eq!(
        ChessGame::new(Some("4k3/8/8/8/8/8/2b5/2B1K3 w - - 0 1")).unwrap().status(),
        GameStatus::Ongoing
    );
    assert_eq!(
        ChessGame::new(Some("4k3/8/8/8/8/8/2n5/2N1K3 w - - 0 1")).unwrap().status(),
        GameStatus::Ongoing
    );
}

#[test]
fn threefold_claim_and_fivefold_repetition() {
    let mut g = ChessGame::default();
    assert_eq!(g.repetition_count(), 1);
    line(&mut g, CYCLE);
    assert_eq!(g.repetition_count(), 2);
    assert!(!g.can_claim_threefold());
    line(&mut g, CYCLE);
    assert_eq!(g.repetition_count(), 3);
    assert!(g.can_claim_threefold());
    assert_eq!(g.status(), GameStatus::Ongoing); // threefold must be claimed
    line(&mut g, CYCLE);
    assert_eq!(g.repetition_count(), 4);
    assert_eq!(g.status(), GameStatus::Ongoing);
    line(&mut g, "g1f3 g8f6 f3g1");
    assert_eq!(g.status(), GameStatus::Ongoing);
    let r = g.play(uci(&g, "f6g8")).unwrap();
    assert_eq!(r.status, GameStatus::Draw);
    assert_eq!(r.reason, EndReason::FivefoldRepetition);

    // Claiming the threefold repetition.
    let mut c = game_after(None, CYCLE);
    assert!(!c.claim_draw()); // nothing to claim yet
    assert_eq!(c.status(), GameStatus::Ongoing);
    line(&mut c, CYCLE);
    assert!(c.claim_draw());
    assert_eq!(c.status(), GameStatus::Draw);
    assert_eq!(c.reason(), EndReason::ThreefoldClaim);
    assert!(!c.can_claim_threefold()); // game over

    // An en passant possibility makes positions different (FIDE 9.2.3.2).
    let mut e = game_after(None, "e2e4 b8c6 e4e5 d7d5");
    line(&mut e, "g1f3 c6b8 f3g1 b8c6");
    assert_eq!(e.repetition_count(), 1);
    line(&mut e, "g1f3 c6b8 f3g1 b8c6");
    assert_eq!(e.repetition_count(), 2);

    // Castling rights make positions different.
    let mut k = game_after(None, "g1f3 g8f6 h1g1 h8g8 g1h1 g8h8");
    assert_eq!(k.repetition_count(), 1);
    line(&mut k, "h1g1 h8g8 g1h1 g8h8");
    assert_eq!(k.repetition_count(), 2);

    // A double push without a possible en passant capture does not change the position identity.
    let d = game_after(None, "e2e4 e7e5 g1f3 g8f6 f3g1 f6g8");
    assert_eq!(d.repetition_count(), 2);
    assert_eq!(d.position().digest(), game_after(None, "e2e4 e7e5").position().digest());
}

#[test]
fn seventy_five_move_rule_and_fifty_move_claim() {
    let mut g = ChessGame::new(Some("7k/8/6K1/8/8/8/8/R7 w - - 149 100")).unwrap();
    let r = g.play(uci(&g, "a1a2")).unwrap();
    assert_eq!(r.status, GameStatus::Draw);
    assert_eq!(r.reason, EndReason::SeventyFiveMoves);
    // Checkmate on the 75th move takes precedence.
    let mut g = ChessGame::new(Some("7k/8/6K1/8/8/8/8/R7 w - - 149 100")).unwrap();
    let r = g.play(uci(&g, "a1a8")).unwrap();
    assert_eq!(r.status, GameStatus::WhiteWins);
    assert_eq!(r.reason, EndReason::Checkmate);
    // Already over at the start.
    assert_eq!(
        ChessGame::new(Some("7k/8/6K1/8/8/8/8/R7 w - - 150 100")).unwrap().reason(),
        EndReason::SeventyFiveMoves
    );
    // Fifty-move claim.
    let mut g = ChessGame::new(Some("7k/8/6K1/8/8/8/8/R7 w - - 98 60")).unwrap();
    assert!(!g.can_claim_fifty_move());
    line(&mut g, "a1a2");
    assert!(!g.can_claim_fifty_move());
    line(&mut g, "h8g8");
    assert!(g.can_claim_fifty_move());
    assert_eq!(g.status(), GameStatus::Ongoing);
    assert!(g.claim_draw());
    assert_eq!(g.status(), GameStatus::Draw);
    assert_eq!(g.reason(), EndReason::FiftyMoveClaim);
    // A pawn move resets the counter.
    let mut g = ChessGame::new(Some("7k/8/6K1/8/8/8/P7/R7 w - - 99 60")).unwrap();
    line(&mut g, "a2a3");
    assert_eq!(g.position().halfmove(), 0);
    assert!(!g.can_claim_fifty_move());
}

#[test]
fn flag_fall_loss_on_time_or_draw_when_the_opponent_cannot_mate() {
    let mut g = ChessGame::default();
    assert!(g.flag_fall(Color::White));
    assert_eq!(g.status(), GameStatus::BlackWins);
    assert_eq!(g.reason(), EndReason::Timeout);
    let mut g = ChessGame::new(Some("4k3/8/8/8/8/8/8/R3K3 w - - 0 1")).unwrap();
    g.flag_fall(Color::White); // Black has a bare king
    assert_eq!(g.status(), GameStatus::Draw);
    assert_eq!(g.reason(), EndReason::TimeoutVsInsufficient);
    let mut g = ChessGame::new(Some("4k3/8/8/8/8/8/8/R3K3 b - - 0 1")).unwrap();
    g.flag_fall(Color::Black);
    assert_eq!(g.status(), GameStatus::WhiteWins);
    let mut g = ChessGame::new(Some("4k3/8/8/8/8/8/8/4KN2 b - - 0 1")).unwrap();
    assert!(g.is_over()); // K+N v K is already dead
    assert!(!g.flag_fall(Color::Black));
    assert_eq!(g.reason(), EndReason::InsufficientMaterial);
    let mut g = ChessGame::new(Some("4k3/8/8/8/8/8/1p6/4K3 w - - 0 1")).unwrap();
    g.flag_fall(Color::Black); // White has a bare king
    assert_eq!(g.status(), GameStatus::Draw);
    assert_eq!(g.reason(), EndReason::TimeoutVsInsufficient);
    let mut g = ChessGame::new(Some("4k3/7p/8/8/8/8/8/4KN2 b - - 0 1")).unwrap();
    g.flag_fall(Color::Black); // K+N v K+P: a mate is possible
    assert_eq!(g.status(), GameStatus::WhiteWins);
    assert_eq!(g.reason(), EndReason::Timeout);
    assert!(ChessGame::new(Some("4k3/8/8/8/8/8/8/2B1K3 b - - 0 1")).unwrap().is_over()); // K+B v K
    let mut g = ChessGame::new(Some("4k3/8/8/8/5n2/8/8/2B1K3 w - - 0 1")).unwrap();
    g.flag_fall(Color::Black); // lone bishop v knight: helpmate exists
    assert_eq!(g.status(), GameStatus::WhiteWins);
}

#[test]
fn resignation_agreement_and_online_endings() {
    let mut g = ChessGame::default();
    assert!(g.resign(Color::White));
    assert_eq!(g.status(), GameStatus::BlackWins);
    assert_eq!(g.reason(), EndReason::Resignation);
    assert!(!g.agree_draw()); // no effect after the end
    assert_eq!(g.status(), GameStatus::BlackWins);
    let mut g = ChessGame::default();
    g.resign(Color::Black);
    assert_eq!(g.status(), GameStatus::WhiteWins);
    let mut g = ChessGame::default();
    assert!(g.agree_draw());
    assert_eq!(g.status(), GameStatus::Draw);
    assert_eq!(g.reason(), EndReason::Agreement);
    let mut g = ChessGame::default();
    assert_eq!(g.end(GameStatus::Aborted, EndReason::NoShow), Ok(true));
    assert_eq!(g.status(), GameStatus::Aborted);
    assert_eq!(g.reason(), EndReason::NoShow);
    assert_eq!(g.result_string(), "*");
    assert_eq!(g.play(796), Err(PlayError::GameOver));
    let mut g = ChessGame::default();
    assert_eq!(g.end(GameStatus::WhiteWins, EndReason::Forfeit), Ok(true));
    assert_eq!(
        ChessGame::default().end(GameStatus::Ongoing, EndReason::None),
        Err(InvalidEnd(GameStatus::Ongoing))
    );
    // The status is checked even after the end.
    assert_eq!(g.end(GameStatus::Ongoing, EndReason::None), Err(InvalidEnd(GameStatus::Ongoing)));
    assert_eq!(InvalidEnd(GameStatus::Ongoing).to_string(), "bad status 0");
    let err = ChessGame::new(Some("not a fen")).unwrap_err();
    assert_eq!(err.to_string(), "invalid FEN: not a fen");
    assert!(ChessGame::new(Some("")).is_err());
}

#[test]
fn journal_replay_with_from_moves() {
    let g = game_after(None, "e2e4 e7e5 g1f3 b8c6 f1b5 a7a6 b5c6 d7c6 e1g1");
    let r = ChessGame::from_moves(None, g.moves()).unwrap();
    assert_eq!(r.position().fen(), g.position().fen());
    assert_eq!(r.moves(), g.moves());
    let mut longer = g.moves().to_vec();
    longer.push(mv("e2", "e5", 0));
    assert!(ChessGame::from_moves(None, &longer).is_none());
    assert!(ChessGame::from_moves(Some("bad fen"), &[]).is_none());
    // A move after the end of the game.
    let mate = game_after(None, "f2f3 e7e5 g2g4 d8h4");
    let mut after = mate.moves().to_vec();
    after.push(mv("a2", "a3", 0));
    assert!(ChessGame::from_moves(None, &after).is_none());
    let custom = ChessGame::from_moves(Some("4k3/8/8/8/8/8/8/R3K3 w Q - 0 1"), &[4 | 2 << 6]).unwrap();
    assert_eq!(custom.position().fen(), "4k3/8/8/8/8/8/8/2KR4 b - - 1 1");
    assert_eq!(custom.start_fen(), "4k3/8/8/8/8/8/8/R3K3 w Q - 0 1");
}

fn rated_tags() -> PgnTags {
    PgnTags {
        event: owned("Rated 3+2"),
        site: owned("Scacelith online"),
        date: owned("2026.09.28"),
        round: owned("-"),
        white: owned("Alice \"A\""),
        black: owned("Bob\\B"),
        time_control: owned("180+2"),
        ..PgnTags::default()
    }
}

#[test]
fn pgn_export() {
    let g = game_after(None, "f2f3 e7e5 g2g4 d8h4");
    assert_eq!(
        g.pgn(&rated_tags()),
        [
            "[Event \"Rated 3+2\"]",
            "[Site \"Scacelith online\"]",
            "[Date \"2026.09.28\"]",
            "[Round \"-\"]",
            "[White \"Alice \\\"A\\\"\"]",
            "[Black \"Bob\\\\B\"]",
            "[Result \"0-1\"]",
            "[TimeControl \"180+2\"]",
            "[Termination \"normal\"]",
            "",
            "1. f3 e5 2. g4 Qh4# {Checkmate} 0-1",
            "",
        ]
        .join("\n")
    );
    let mut custom = game_after(Some("4k3/8/8/8/8/8/8/R3K3 b Q - 3 20"), "e8d7");
    custom.end(GameStatus::WhiteWins, EndReason::Abandonment).unwrap();
    let pgn = custom.pgn(&PgnTags {
        date: owned("2026.09.28"),
        white: owned("W"),
        black: owned("B"),
        extra: vec![("WhiteElo".to_owned(), "1500".to_owned())],
        ..PgnTags::default()
    });
    assert!(pgn.contains("[SetUp \"1\"]\n[FEN \"4k3/8/8/8/8/8/8/R3K3 b Q - 3 20\"]\n"), "{pgn}");
    assert!(pgn.contains("[Termination \"abandoned\"]\n[WhiteElo \"1500\"]\n"), "{pgn}");
    assert!(pgn.ends_with("\n20... Kd7 {Abandoned (disconnected for too long)} 1-0\n"), "{pgn}");
    // Defaults: today's date, "?" players, no reason comment for an ongoing game.
    let fresh = ChessGame::default().pgn(&PgnTags::default());
    let date = fresh.lines().nth(2).unwrap();
    assert!(date.len() == 19 && date.starts_with("[Date \"") && date.ends_with("\"]"), "{date}");
    assert!(date[7..17].bytes().enumerate().all(|(i, b)| if i == 4 || i == 7 {
        b == b'.'
    } else {
        b.is_ascii_digit()
    }));
    assert!(fresh.starts_with("[Event \"Casual game\"]\n[Site \"Scacelith\"]\n"));
    assert!(
        fresh.contains("[Round \"-\"]\n[White \"?\"]\n[Black \"?\"]\n[Result \"*\"]\n[TimeControl \"?\"]\n")
    );
    assert!(fresh.ends_with("[Termination \"unterminated\"]\n\n*\n"), "{fresh}");
    // Empty values: an empty event is kept, an empty date or time control takes the default.
    let empty = ChessGame::default().pgn(&PgnTags {
        event: owned(""),
        date: owned(""),
        time_control: owned(""),
        ..PgnTags::default()
    });
    assert!(empty.starts_with("[Event \"\"]\n"));
    assert!(!empty.contains("[Date \"\"]"));
    assert!(empty.contains("[TimeControl \"?\"]"));
    // Long games wrap at 80 columns.
    let mut long = ChessGame::default();
    for i in 0..120 {
        if long.is_over() {
            break;
        }
        let mut legal = long.position().legal_moves();
        legal.sort_unstable();
        long.play(legal[(i * 7919) % legal.len()]).unwrap();
    }
    let text = long.pgn(&rated_tags());
    let lines: Vec<&str> = text.split('\n').collect();
    assert!(lines.len() > 15);
    for l in lines {
        assert!(l.len() <= 79, "{l}");
    }
}

#[test]
fn pgn_tags_after_result_per_ply_comments_and_online_terminations() {
    let mut g = game_after(None, "e2e4 e7e5 g1f3");
    g.resign(Color::Black);
    let pair = |k: &str, v: &str| (k.to_owned(), v.to_owned());
    let pgn = g.pgn(&PgnTags {
        event: owned("E"),
        site: owned("S"),
        date: owned("2026.09.28"),
        white: owned("W"),
        black: owned("B"),
        time_control: owned("180+2"),
        after_result: vec![pair("UTCDate", "2026.09.28"), pair("UTCTime", "12:00:00")],
        extra: vec![pair("PlyCount", "3")],
        comments: vec![
            Some(PgnComment::Words(vec!["[%clk 0:03:00.0]".to_owned(), "[%emt 0:00:00.0]".to_owned()])),
            Some(PgnComment::Text("nice {move}\n".to_owned())),
            None,
            Some(PgnComment::Words(vec!["ignored: no such ply".to_owned()])),
        ],
        ..PgnTags::default()
    });
    assert_eq!(
        pgn,
        [
            "[Event \"E\"]",
            "[Site \"S\"]",
            "[Date \"2026.09.28\"]",
            "[Round \"-\"]",
            "[White \"W\"]",
            "[Black \"B\"]",
            "[Result \"1-0\"]",
            "[UTCDate \"2026.09.28\"]",
            "[UTCTime \"12:00:00\"]",
            "[TimeControl \"180+2\"]",
            "[Termination \"normal\"]",
            "[PlyCount \"3\"]",
            "",
            "1. e4 {[%clk 0:03:00.0] [%emt 0:00:00.0]} 1... e5 {nice move} 2. Nf3",
            "{Resignation} 1-0",
            "",
        ]
        .join("\n")
    );
    // An empty comment is no comment (and the Black move keeps its plain form).
    let plain = game_after(None, "e2e4 e7e5").pgn(&PgnTags {
        comments: vec![
            Some(PgnComment::Text(String::new())),
            Some(PgnComment::Words(Vec::new())),
            Some(PgnComment::Text(" ".to_owned())),
        ],
        ..PgnTags::default()
    });
    assert!(plain.ends_with("\n1. e4 e5 *\n"), "{plain}");
    // Termination of the online endings (PGN standard values).
    let term = |status, reason| {
        let mut x = ChessGame::default();
        x.end(status, reason).unwrap();
        let pgn = x.pgn(&PgnTags::default());
        let start = pgn.find("[Termination \"").unwrap() + 14;
        pgn[start..start + pgn[start..].find('"').unwrap()].to_owned()
    };
    assert_eq!(term(GameStatus::Aborted, EndReason::Aborted), "unterminated");
    assert_eq!(term(GameStatus::Aborted, EndReason::NoShow), "unterminated");
    assert_eq!(term(GameStatus::Aborted, EndReason::ServerAborted), "unterminated");
    assert_eq!(term(GameStatus::Aborted, EndReason::BothDisconnected), "unterminated");
    assert_eq!(term(GameStatus::WhiteWins, EndReason::Abandonment), "abandoned");
    assert_eq!(term(GameStatus::Draw, EndReason::AbandonmentVsInsufficient), "abandoned");
    assert_eq!(term(GameStatus::BlackWins, EndReason::Forfeit), "rules infraction");
    assert_eq!(term(GameStatus::WhiteWins, EndReason::IllegalMoves), "rules infraction");
    assert_eq!(term(GameStatus::Draw, EndReason::TimeoutVsInsufficient), "time forfeit");
    assert_eq!(term(GameStatus::BlackWins, EndReason::Timeout), "time forfeit");
    assert_eq!(term(GameStatus::Draw, EndReason::Agreement), "normal");
    let mut aborted = ChessGame::default();
    aborted.end(GameStatus::Aborted, EndReason::NoShow).unwrap();
    let text = aborted.pgn(&PgnTags::default());
    assert!(text.contains("[Result \"*\"]"));
    assert!(text.ends_with("\n{Aborted: first move not played in time} *\n"), "{text}");
    // Long games with clocks still wrap under 80 columns, a [%command] never broken.
    let mut long = ChessGame::default();
    let mut comments = Vec::new();
    for i in 0..160 {
        if long.is_over() {
            break;
        }
        let mut legal = long.position().legal_moves();
        legal.sort_unstable();
        long.play(legal[(i * 7919) % legal.len()]).unwrap();
        comments.push(Some(PgnComment::Words(vec![
            format!("[%clk 1:{:02}:00.{}]", 59 - (i % 60), i % 10),
            "[%emt 0:00:01.5]".to_owned(),
        ])));
    }
    let text = long.pgn(&PgnTags { comments, ..PgnTags::default() });
    for l in text.split('\n') {
        assert!(l.len() <= 79, "{l}");
        assert!(!l.ends_with("[%clk") && !l.ends_with("[%emt"), "a command split: {l}");
        let clock_chars = l.bytes().take_while(|&b| b.is_ascii_digit() || b == b':' || b == b'.').count();
        assert!(clock_chars == 0 || l.as_bytes().get(clock_chars) != Some(&b']'), "a command split: {l}");
    }
    assert_eq!(text.matches("[%clk ").count(), long.ply());
}
