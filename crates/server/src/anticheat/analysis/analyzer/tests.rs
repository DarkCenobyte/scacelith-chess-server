use std::collections::HashMap;
use std::future::{Future, ready};

use super::*;
use crate::anticheat::analysis::engine::EngineErrorKind;
use crate::anticheat::num::js_json;
use crate::clock::ManualClock;

// Distinct dummy moves (the analyser never checks legality: the engine does).
fn mv(i: usize) -> u16 {
    ((i % 64) | (((i * 7 + 3) % 64) << 6)) as u16
}

fn line(multipv: i64, mv: Option<&str>, cp: i64) -> PvLine {
    PvLine {
        multipv,
        depth: 10,
        cp: Some(cp),
        mate: None,
        bound: None,
        mv: mv.map(str::to_string),
        pv: mv.into_iter().map(str::to_string).collect(),
    }
}

fn mate_line(mate: i64) -> PvLine {
    PvLine { cp: None, mate: Some(mate), ..line(1, None, 0) }
}

fn result(lines: Vec<PvLine>, bestmove: Option<&str>) -> SearchResult {
    SearchResult { lines, bestmove: bestmove.map(str::to_string), node_limited: false }
}

/// What a scripted engine was asked.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Call {
    NewGame,
    ClearHash,
    /// Plies of the position, depth, MultiPV, node limit.
    Search(usize, u32, u32, Option<u64>),
}

// An engine whose answers come from a function of the position and the search.
struct Scripted<F> {
    name: &'static str,
    calls: Vec<Call>,
    answer: F,
}

impl<F: FnMut(&[String], &SearchOptions) -> SearchResult + Send> AnalysisEngine for Scripted<F> {
    fn name(&self) -> &str {
        self.name
    }

    fn net(&self) -> Option<String> {
        None
    }

    fn hash_mb(&self) -> Option<u32> {
        None
    }

    fn new_game(&mut self) -> impl Future<Output = Result<(), EngineError>> + Send {
        self.calls.push(Call::NewGame);
        ready(Ok(()))
    }

    fn clear_hash(&mut self) -> impl Future<Output = Result<(), EngineError>> + Send {
        self.calls.push(Call::ClearHash);
        ready(Ok(()))
    }

    fn analyse(
        &mut self,
        moves: &[String],
        opts: &SearchOptions,
    ) -> impl Future<Output = Result<SearchResult, EngineError>> + Send {
        self.calls.push(Call::Search(moves.len(), opts.depth, opts.multi_pv, opts.nodes));
        ready(Ok((self.answer)(moves, opts)))
    }
}

fn block_on<T>(f: impl Future<Output = T>) -> T {
    tokio::runtime::Builder::new_current_thread().build().expect("a runtime").block_on(f)
}

#[test]
fn line_cp_converts_mate_scores() {
    assert_eq!(line_cp(&line(1, None, 35)), 35);
    assert_eq!(line_cp(&mate_line(3)), MATE_CP - 30);
    assert_eq!(line_cp(&mate_line(-2)), -MATE_CP + 20);
    assert_eq!(line_cp(&mate_line(0)), -MATE_CP);
}

#[test]
fn analysis_profiles() {
    assert_eq!(
        analysis_profile("Stockfish 19", Some("nn-1a298aa575a0.nnue"), 9, 15, Some(32)),
        "Stockfish 19; nn-1a298aa575a0.nnue; depth 9/15; hash 32; analysis 2"
    );
    assert_eq!(analysis_profile("", None, 4, 12, Some(0)), "engine; depth 4/12; analysis 2");
    assert_eq!(
        analysis_profile("a|b", Some(""), 1, 2, None),
        "a/b; depth 1/2; analysis 2",
        "'|' separates key parts"
    );
}

#[test]
fn skips_opening_forced_and_decided_moves_and_scores_the_others() {
    let moves: Vec<u16> = (0..24).map(mv).collect();
    let uci: Vec<String> = moves.iter().map(|&m| move_to_uci(m)).collect();
    let u = |i: usize| Some(uci[i].as_str());
    let other: Vec<String> = (0..5).map(|k| format!("h{k}h{k}")).collect(); // never a played move
    let o = |k: usize| Some(other[k].as_str());
    // Deep analyses by ply (position before move `ply`), from the side to move's view.
    let deep: HashMap<usize, Vec<PvLine>> = HashMap::from([
        (16, vec![line(1, u(16), 30), line(2, o(1), 20), line(3, o(2), -100)]), // white: T1, complex
        (17, vec![line(1, u(17), -25)]),                                        // black: forced
        (18, vec![line(1, o(1), 700), line(2, o(2), 650), line(3, u(18), 600)]), // white: decided
        (19, vec![line(1, o(1), 10), line(2, o(2), -150), line(3, o(3), -200)]), // black: not top 3
        (20, vec![line(1, o(1), 200), line(2, u(20), 150), line(3, o(3), 100)]), // white (black lost 210)
        (21, vec![line(1, u(21), -150), line(2, o(2), -300), line(3, o(3), -400)]), // black: T1, gap 150
        (22, vec![line(1, o(1), 160), line(2, u(22), 140), line(3, o(3), 0)]),  // white: top 2, complex
        (23, vec![line(1, o(1), -130), line(2, u(23), -500), line(3, o(3), -600)]), // black: blunder 370
        (24, vec![line(1, o(1), 480)]),                                         // final position
    ]);
    let fast_best: HashMap<usize, String> = HashMap::from([
        (16, uci[16].clone()),
        (19, other[1].clone()),
        (20, other[1].clone()),
        (21, other[4].clone()),
        (22, uci[22].clone()),
        (23, other[1].clone()),
    ]);
    let played = uci.clone();
    let mut engine = Scripted {
        name: "fake-engine",
        calls: Vec::new(),
        answer: |ms: &[String], opts: &SearchOptions| {
            assert_eq!(ms, &played[..ms.len()]);
            if opts.depth == 12 {
                let lines = deep[&ms.len()].clone();
                let best = lines[0].mv.clone();
                return SearchResult { lines, bestmove: best, node_limited: false };
            }
            let best = fast_best[&ms.len()].as_str();
            result(vec![line(1, Some(best), 0)], Some(best))
        },
    };
    let spent_ms: Vec<u32> = (0..24).map(|i| 1000 + i * 10).collect();
    let record = GameRecord {
        id: 42,
        category: "5+0".into(),
        rated: true,
        base_ms: 300_000,
        inc_ms: 0,
        white_id: 1,
        black_id: 2,
        white_rating: Some(1600),
        black_rating: Some(1550),
        ended_at: Some(5),
        moves,
        spent_ms: spent_ms.clone(),
    };
    let clock = ManualClock::new(0.0, 1_700_000_000_000);
    let context = [SideContext { rating_games: Some(40) }, SideContext::default()];
    let f = block_on(analyse_game(&mut engine, &record, Depths { fast: 4, deep: 12 }, context, &*clock))
        .expect("the analysis succeeds");

    let calls = &engine.calls;
    assert_eq!(calls.iter().filter(|c| **c == Call::NewGame).count(), 1);
    // Deep pass: plies 16..24; shallow pass only where a move is scored (not forced / decided).
    let plies = |depth: u32| -> Vec<usize> {
        calls
            .iter()
            .filter_map(|c| match c {
                Call::Search(p, d, _, _) if *d == depth => Some(*p),
                _ => None,
            })
            .collect()
    };
    assert_eq!(plies(12), [16, 17, 18, 19, 20, 21, 22, 23, 24]);
    assert_eq!(plies(4), [16, 19, 20, 21, 22, 23]);
    for c in calls {
        if let Call::Search(_, depth, multi_pv, nodes) = c {
            let deep = *depth == 12;
            assert_eq!((*multi_pv, *nodes), if deep { (3, Some(DEEP_NODE_LIMIT)) } else { (1, None) });
        }
    }
    assert_eq!(
        calls.iter().filter(|c| **c == Call::ClearHash).count(),
        6,
        "hash cleared before every shallow search"
    );

    assert_eq!((f.game_id, f.engine.as_str(), f.plies), (42, "fake-engine", 24));
    assert_eq!(f.profile, "fake-engine; depth 4/12; analysis 2");
    let (w, b) = (&f.white, &f.black);
    assert_eq!((w.user_id, w.rating_games), (1, Some(40)));
    assert_eq!(w.skipped, Skipped { opening: 8, forced: 0, decided: 1, missing: 0 });
    assert_eq!(b.skipped, Skipped { opening: 8, forced: 1, decided: 0, missing: 0 });
    // White scored plies 16, 20, 22: losses 0, 50, 20.
    assert_eq!(w.n, 3);
    assert_eq!(w.acpl, Some(js_round((0.0 + 50.0 + 20.0) / 3.0 * 10.0) / 10.0));
    assert_eq!(w.t1_deep, Some(js_round(1.0 / 3.0 * 1e4) / 1e4));
    assert_eq!(w.t1_fast, Some(js_round(2.0 / 3.0 * 1e4) / 1e4), "16 and 22 match the shallow choice");
    assert_eq!(w.top3, Some(1.0));
    assert_eq!(w.n_complex, 3, "16 (30/20), 20 (200/150) and 22 (160/140)");
    assert_eq!(w.t1_complex, Some(0.3333));
    // Black scored plies 19, 21, 23: losses 10-(-200)=210 (from the next position), 0, 370.
    assert_eq!(b.n, 3);
    assert_eq!(b.acpl, Some(js_round((210.0 + 0.0 + 370.0) / 3.0 * 10.0) / 10.0));
    assert_eq!(b.top3, Some(js_round(2.0 / 3.0 * 1e4) / 1e4));
    assert!(w.accuracy > b.accuracy);
    assert_eq!(w.time_corr, None, "too few timed moves for a correlation");
    assert_eq!(
        w.moves[0],
        ScoredMove {
            ply: 16,
            loss: 0,
            flags: flags::T1 | flags::FAST | flags::TOP3 | flags::COMPLEX,
            n_good: 2,
            spent_ms: 1160
        }
    );

    // The records as the former server wrote them (Node 22), black without the context it
    // was not given.
    let white = r#"{"userId":1,"rating":1600,"ratingGames":40,"n":3,"accuracy":92.27,"acpl":23.3,"wpl":1.95,"t1Deep":0.3333,"t1Fast":0.6667,"top3":1,"nComplex":3,"t1Complex":0.3333,"timeCorr":null,"timeCv":null,"nTimed":3,"meanSpentMs":1193,"skipped":{"opening":8,"forced":0,"decided":1,"missing":0},"moves":[[16,0,15,2,1160],[20,50,12,2,1200],[22,20,30,2,1220]]}"#;
    let black = r#"{"userId":2,"rating":1550,"n":3,"accuracy":53.36,"acpl":193.3,"wpl":14.37,"t1Deep":0.3333,"t1Fast":0,"top3":0.6667,"nComplex":0,"t1Complex":null,"timeCorr":null,"timeCv":null,"nTimed":3,"meanSpentMs":1210,"skipped":{"opening":8,"forced":1,"decided":0,"missing":0},"moves":[[19,210,0,1,1190],[21,0,21,1,1210],[23,370,4,1,1230]]}"#;
    assert_eq!(js_json(&w.to_json()), white);
    let mut b_json = b.to_json();
    assert_eq!(b_json["ratingGames"], Value::Null);
    b_json.as_object_mut().expect("an object").shift_remove("ratingGames");
    assert_eq!(js_json(&b_json), black);

    let record_json = f.to_json();
    let keys: Vec<&str> = record_json.as_object().expect("an object").keys().map(String::as_str).collect();
    assert_eq!(
        keys,
        [
            "v",
            "gameId",
            "category",
            "baseMs",
            "incMs",
            "plies",
            "endedAt",
            "engine",
            "net",
            "hashMb",
            "depthFast",
            "depthDeep",
            "profile",
            "analysedAt",
            "durationMs",
            "white",
            "black"
        ]
    );
    assert_eq!(
        (record_json["v"].as_u64(), record_json["analysedAt"].as_i64()),
        (Some(2), Some(1_700_000_000_000))
    );
    // The scoring reads the record back.
    let side = crate::anticheat::scoring::side_of(&record_json, 1).expect("white side");
    assert_eq!((side.n, side.base_ms, side.ended_at), (3.0, 300_000.0, 5.0));
}

#[test]
fn time_features_think_time_following_complexity_gives_a_high_rank_correlation() {
    let n = 60;
    let moves: Vec<u16> = (0..n).map(mv).collect();
    let uci: Vec<String> = moves.iter().map(|&m| move_to_uci(m)).collect();
    let mut pos = Positions::default();
    let mut spent = vec![0u32; n];
    for p in 16..=n {
        let k = p % 3; // 0: one clear best move, 1: two good moves, 2: three good moves
        let cps = match k {
            0 => [50, -150, -300],
            1 => [40, 20, -200],
            _ => [30, 25, 10],
        };
        let best = (p < n).then(|| uci[p].as_str());
        let first = best.unwrap_or("a1a1");
        let lines =
            vec![line(1, Some(first), cps[0]), line(2, Some("b1b1"), cps[1]), line(3, Some("c1c1"), cps[2])];
        pos.deep.insert(p, result(lines, best));
        if p < n {
            pos.fast.insert(p, result(Vec::new(), best));
            spent[p] = 2000 + 4000 * k as u32 + (p as u32 % 5) * 100;
        }
    }
    let [w, b] = compute_features(&moves, &spent, &pos);
    assert!(w.time_corr.expect("a correlation") > 0.8, "white {:?}", w.time_corr);
    assert!(b.time_corr.expect("a correlation") > 0.8);
    assert!(w.time_cv.expect("a variation") > 0.3);
    assert_eq!((w.t1_deep, w.acpl, w.accuracy), (Some(1.0), Some(0.0), Some(100.0)));
    // The same numbers as the former server.
    let strip = |s: &SideFeatures| {
        let mut v = s.to_json();
        let m = v.as_object_mut().expect("an object");
        for k in ["userId", "rating", "ratingGames", "moves"] {
            m.shift_remove(k);
        }
        js_json(&v)
    };
    assert_eq!(
        strip(&w),
        r#"{"n":22,"accuracy":100,"acpl":0,"wpl":0,"t1Deep":1,"t1Fast":1,"top3":1,"nComplex":15,"t1Complex":1,"timeCorr":0.9449,"timeCv":0.5216,"nTimed":22,"meanSpentMs":6200,"skipped":{"opening":8,"forced":0,"decided":0,"missing":0}}"#
    );
    assert_eq!(
        strip(&b),
        r#"{"n":22,"accuracy":100,"acpl":0,"wpl":0,"t1Deep":1,"t1Fast":1,"top3":1,"nComplex":15,"t1Complex":1,"timeCorr":0.9449,"timeCv":0.5321,"nTimed":22,"meanSpentMs":6391,"skipped":{"opening":8,"forced":0,"decided":0,"missing":0}}"#
    );
    // Constant think time: no correlation, no variation.
    let flat = compute_features(&moves, &vec![3000; n], &pos);
    assert_eq!((flat[0].time_corr, flat[0].time_cv), (None, Some(0.0)));
    // Without clock times: no time features, and -1 in the scored moves.
    let untimed = compute_features(&moves, &[], &pos);
    assert_eq!((untimed[0].n_timed, untimed[0].mean_spent_ms, untimed[0].moves[0].spent_ms), (0, None, -1));
}

#[test]
fn short_games_give_empty_features() {
    let mut engine = Scripted {
        name: "x",
        calls: Vec::new(),
        answer: |_: &[String], _: &SearchOptions| -> SearchResult { panic!("not called") },
    };
    let record = GameRecord {
        id: 1,
        white_id: 1,
        black_id: 2,
        moves: vec![mv(1), mv(2)],
        spent_ms: vec![0, 0],
        ..GameRecord::default()
    };
    let clock = ManualClock::new(0.0, 0);
    let f = block_on(analyse_game(
        &mut engine,
        &record,
        Depths { fast: 2, deep: 4 },
        Default::default(),
        &*clock,
    ))
    .expect("nothing to analyse");
    assert!(engine.calls.is_empty(), "no new game, no search");
    assert_eq!(f.white.n, 0);
    assert_eq!(f.black.accuracy, None);
    assert_eq!(f.white.skipped.opening, 1);
}

#[test]
fn a_deep_search_stopped_by_the_node_limit_is_repeated_from_an_empty_hash_without_the_limit() {
    let uci: Vec<String> = (0..20).map(|i| move_to_uci(mv(i))).collect();
    let next = uci.clone();
    let mut engine = Scripted {
        name: "fake-engine",
        calls: Vec::new(),
        answer: move |ms: &[String], opts: &SearchOptions| {
            // The search of ply 18 blows up on the hash: the limit stops it, with an unfinished
            // answer.
            let node_limited = opts.depth == 12 && ms.len() == 18 && opts.nodes.is_some();
            let cp = if node_limited { 900 } else { 10 };
            let best = next.get(ms.len()).map(String::as_str);
            let lines =
                vec![line(1, best, cp), line(2, Some("h1h1"), cp - 20), line(3, Some("h2h2"), cp - 40)];
            SearchResult { node_limited, ..result(lines, best) }
        },
    };
    let pos = block_on(analyse_positions(&mut engine, &uci, 4, 12)).expect("the analysis succeeds");
    let deep_calls: Vec<Call> = engine
        .calls
        .into_iter()
        .filter(|c| matches!(c, Call::ClearHash | Call::Search(_, 12, _, _)))
        .collect();
    let limited = |p| Call::Search(p, 12, 3, Some(DEEP_NODE_LIMIT));
    assert_eq!(
        deep_calls,
        [
            limited(16),
            limited(17),
            limited(18),
            Call::ClearHash,
            Call::Search(18, 12, 3, None),
            limited(19),
            limited(20),
            // The shallow pass clears the hash before each of its searches.
            Call::ClearHash,
            Call::ClearHash,
            Call::ClearHash,
            Call::ClearHash,
        ]
    );
    assert_eq!(pos.deep[&18].lines[0].cp, Some(10), "the answer of the repeated search");
    const { assert!(DEEP_NODE_LIMIT >= 10_000_000, "far above a normal deep search") };
}

#[test]
fn an_engine_error_fails_the_game() {
    struct Failing;
    impl AnalysisEngine for Failing {
        fn name(&self) -> &str {
            "failing"
        }
        fn net(&self) -> Option<String> {
            None
        }
        fn hash_mb(&self) -> Option<u32> {
            None
        }
        fn new_game(&mut self) -> impl Future<Output = Result<(), EngineError>> + Send {
            ready(Ok(()))
        }
        fn clear_hash(&mut self) -> impl Future<Output = Result<(), EngineError>> + Send {
            ready(Ok(()))
        }
        fn analyse(
            &mut self,
            _moves: &[String],
            _opts: &SearchOptions,
        ) -> impl Future<Output = Result<SearchResult, EngineError>> + Send {
            ready(Err(EngineError::new(EngineErrorKind::Crashed, "engine exited")))
        }
    }
    let uci: Vec<String> = (0..20).map(|i| move_to_uci(mv(i))).collect();
    let err = block_on(analyse_positions(&mut Failing, &uci, 4, 12)).expect_err("the engine fails");
    assert_eq!(err.code(), "crashed");
}
