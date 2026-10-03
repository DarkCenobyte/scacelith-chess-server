use std::time::Duration;

use super::*;

fn info(line: &str) -> InfoLine {
    parse_info_line(line).expect("a scored info line")
}

#[test]
fn parses_multipv_info_lines_with_cp_scores_and_pv() {
    let l = info(
        "info depth 18 seldepth 25 multipv 2 score cp -35 nodes 1234567 nps 900000 hashfull 120 tbhits 0 time 1371 pv e7e5 g1f3 b8c6",
    );
    let want = InfoLine {
        depth: 18,
        seldepth: 25,
        multipv: 2,
        cp: Some(-35),
        mate: None,
        bound: None,
        wdl: None,
        pv: vec!["e7e5".into(), "g1f3".into(), "b8c6".into()],
    };
    assert_eq!(l, want);
}

#[test]
fn parses_mate_scores_bounds_and_wdl() {
    let m = info("info depth 12 seldepth 14 multipv 1 score mate -3 nodes 100 pv h2h3 d8h4");
    assert_eq!((m.mate, m.cp), (Some(-3), None));
    let lb =
        info("info depth 20 seldepth 30 multipv 1 score cp 57 lowerbound nodes 999 nps 1 time 3001 pv d2d4");
    assert_eq!((lb.bound, lb.cp), (Some(Bound::Lower), Some(57)));
    let ub = info("info depth 20 multipv 1 score cp 12 upperbound wdl 100 800 100 pv d2d4 d7d5");
    assert_eq!(ub.bound, Some(Bound::Upper));
    assert_eq!(ub.wdl, Some([Some(100), Some(800), Some(100)]));
    assert_eq!(ub.pv, ["d2d4", "d7d5"]);
    let mate0 = info("info depth 0 score mate 0");
    assert_eq!(mate0.mate, Some(0));
    assert!(mate0.pv.is_empty());
    assert_eq!(Bound::Upper.as_str(), "upperbound");
}

#[test]
fn ignores_lines_without_a_score_and_non_info_lines() {
    for line in [
        "info depth 5 currmove e2e4 currmovenumber 1",
        "info string NNUE evaluation using nn-5af11540bbfe.nnue enabled",
        "bestmove e2e4 ponder e7e5",
        "info depth 3 score xx 5",
        "info depth 3 score cp",
        "",
    ] {
        assert_eq!(parse_info_line(line), None, "{line}");
    }
    // Keys of other engines are skipped; numbers are read as parseInt reads them.
    let l = info("info depth 7x custom 3 multipv 2 score cp +15 refutation e2e4 e7e5");
    assert_eq!((l.depth, l.multipv, l.cp, l.pv.len()), (7, 2, Some(15), 0));
}

#[test]
fn merges_iterations_deepest_line_per_multipv_exact_beats_bound_at_equal_depth() {
    let mut acc = BTreeMap::new();
    for s in [
        "info depth 9 multipv 1 score cp 20 pv e2e4",
        "info depth 9 multipv 2 score cp 10 pv d2d4",
        "info depth 9 multipv 3 score cp 5 pv c2c4",
        "info depth 10 multipv 1 score cp 40 lowerbound pv e2e4",
        "info depth 10 multipv 1 score cp 25 pv e2e4 e7e5",
        "info depth 10 multipv 1 score cp 99 upperbound pv g1f3",
        "info depth 10 multipv 2 score cp 15 pv d2d4 d7d5",
        "info depth 10 multipv 3 score cp 8 pv g1f3",
    ] {
        merge_info(&mut acc, info(s));
    }
    let lines: Vec<_> =
        final_lines(&acc).into_iter().map(|l| (l.multipv, l.depth, l.cp, l.mv, l.bound)).collect();
    assert_eq!(
        lines,
        [
            (1, 10, Some(25), Some("e2e4".to_string()), None),
            (2, 10, Some(15), Some("d2d4".to_string()), None),
            (3, 10, Some(8), Some("g1f3".to_string()), None),
        ]
    );
    // A line of an iteration older than the previous one was not searched again: dropped.
    let mut acc = BTreeMap::new();
    for s in ["info depth 4 multipv 2 score cp 1 pv a2a3", "info depth 9 multipv 1 score cp 2 pv e2e4"] {
        merge_info(&mut acc, info(s));
    }
    assert_eq!(final_lines(&acc).len(), 1);
    assert!(final_lines(&BTreeMap::new()).is_empty());
}

#[test]
fn reads_the_nodes_and_the_network_of_a_line() {
    assert_eq!(nodes_of("info depth 1 seldepth 1 multipv 1 score cp 20 nodes 20 pv e2e4"), Some(20));
    assert_eq!(nodes_of("info depth 1 tbnodes 5 nodes x nodes 7"), Some(7));
    assert_eq!(nodes_of("info depth 1"), None);
    assert_eq!(
        net_of("info string NNUE evaluation using nn-1a298aa575a0.nnue (109MiB)"),
        Some("nn-1a298aa575a0.nnue")
    );
    assert_eq!(net_of("info string Using 1 thread"), None);
}

#[test]
fn parses_where_stockfish_keeps_a_replica_of_its_network_and_nothing_else() {
    let r = |replica, memory, error: Option<&str>| NetworkReplica {
        replica,
        memory,
        error: error.map(str::to_string),
    };
    assert_eq!(
        parse_network_replica("info string Network replica 1: Shared memory."),
        Some(r(1, ReplicaMemory::Shared, None))
    );
    assert_eq!(
        parse_network_replica(
            "info string Network replica 2: Local memory. Shared memory not supported by the OS. Local allocation fallback."
        ),
        Some(r(
            2,
            ReplicaMemory::Local,
            Some("Shared memory not supported by the OS. Local allocation fallback.")
        ))
    );
    assert_eq!(
        parse_network_replica("info string Network replica 2: No allocation."),
        Some(r(2, ReplicaMemory::NoAllocation, None))
    );
    // A status of a later version: kept as the explanation.
    assert_eq!(
        parse_network_replica("info string Network replica 1: Unknown status."),
        Some(r(1, ReplicaMemory::Unknown, Some("Unknown status.")))
    );
    for other in [
        "info string NNUE evaluation using nn-1a298aa575a0.nnue (109MiB, (86896, 1024, 32, 32, 1))",
        "info string Using 1 thread",
        "info string Available processors: 0-3",
        "info depth 1 seldepth 1 multipv 1 score cp 20 pv e2e4",
        "bestmove e2e4",
        "",
    ] {
        assert_eq!(parse_network_replica(other), None, "{other}");
    }
}

#[test]
fn a_network_is_shared_when_every_allocated_replica_is() {
    let r =
        |memory, error: Option<&str>| NetworkReplica { replica: 1, memory, error: error.map(str::to_string) };
    use NetworkMemory::{Local, Shared};
    use ReplicaMemory as M;
    assert_eq!(network_memory(&[]), (None, None), "not reported (Stockfish 16, another engine)");
    assert_eq!(network_memory(&[r(M::Shared, None)]), (Some(Shared), None));
    assert_eq!(
        network_memory(&[r(M::Shared, None), r(M::NoAllocation, None)]),
        (Some(Shared), None),
        "a NUMA node without threads needs no replica"
    );
    assert_eq!(
        network_memory(&[r(M::Shared, None), r(M::Local, Some("why"))]),
        (Some(Local), Some("why".into()))
    );
    assert_eq!(
        network_memory(&[r(M::Unknown, Some("Unknown status."))]),
        (Some(Local), Some("Unknown status.".into()))
    );
    assert_eq!(network_memory(&[r(M::NoAllocation, None)]).0, Some(Local));
    assert_eq!(NetworkMemory::label(None), "not reported");
    assert_eq!(NetworkMemory::label(Some(Shared)), "shared memory");
}

// ---- process tests ----------------------------------------------------------------------------

// A minimal UCI engine run by /bin/sh: it answers `uci` and `isready`, answers every `go` with a
// one-line search of e2e4 (depth 1 in 20 nodes) preceded by the info strings given as arguments
// (what Stockfish reports when a search starts), and quits on `quit`. `on_deep` is the shell code
// run instead for a `go depth 7`, and `on_stop` the one run for `stop`.
fn fake_script(on_deep: &str, on_stop: &str) -> String {
    format!(
        r#"while IFS= read -r line; do
  case "${{line%% *}}" in
    uci) printf 'id name Fake Engine 1\nuciok\n' ;;
    isready) printf 'readyok\n' ;;
    go)
      case "$line" in
        *"depth 7"*) {on_deep} ;;
        *) for s in "$@"; do printf 'info string %s\n' "$s"; done
           printf 'info depth 1 seldepth 1 multipv 1 score cp 20 nodes 20 pv e2e4\nbestmove e2e4\n' ;;
      esac ;;
    stop) {on_stop} ;;
    quit) exit 0 ;;
  esac
done"#
    )
}

fn fake_options(script: String, strings: &[&str]) -> EngineOptions {
    let mut args = vec!["-c".to_string(), script, "fake-engine".to_string()];
    args.extend(strings.iter().map(|s| s.to_string()));
    EngineOptions { args, ..EngineOptions::new("/bin/sh") }
}

fn fake_engine(strings: &[&str]) -> UciEngine {
    UciEngine::new(fake_options(fake_script(":", ":"), strings))
}

fn moves(m: &[&str]) -> Vec<String> {
    m.iter().map(|s| s.to_string()).collect()
}

const SF19_NET: &str = "NNUE evaluation using nn-1a298aa575a0.nnue (109MiB, (86896, 1024, 32, 32, 1))";

#[tokio::test]
async fn an_engine_that_cannot_start_reports_a_spawn_error() {
    let mut e = UciEngine::new(EngineOptions::new("/nonexistent/engine-binary"));
    let err = e.start().await.expect_err("no such engine");
    assert_eq!(err.kind(), EngineErrorKind::Spawn);
    assert_eq!(err.code(), "spawn");
    assert!(err.to_string().starts_with("engine spawn: cannot start engine"), "{err}");
    let err = UciEngine::new(EngineOptions::new("")).start().await.expect_err("no path");
    assert_eq!(err.kind(), EngineErrorKind::Spawn);
}

#[tokio::test]
async fn an_engine_that_hangs_is_killed_and_the_error_says_timeout() {
    // `cat` echoes the commands but never says uciok.
    let opts = EngineOptions {
        timeout: Duration::from_millis(200),
        handshake_timeout: Duration::from_millis(200),
        ..EngineOptions::new("/bin/cat")
    };
    let mut e = UciEngine::new(opts);
    let err = e.start().await.expect_err("no handshake");
    assert_eq!(err.kind(), EngineErrorKind::Timeout);
    assert_eq!(err.message(), "engine did not answer \"uci\"");
    assert!(!err.recovered());
    assert!(!e.is_alive());
    assert_eq!(e.pid(), None);
    e.close().await;
}

#[tokio::test]
async fn the_engine_learns_at_start_where_its_network_lives_among_its_other_info_strings() {
    let mut sf19 = fake_engine(&[
        "Available processors: 0-3",
        "Using 1 thread",
        SF19_NET,
        "Network replica 1: Shared memory.",
    ]);
    sf19.start().await.expect("the fake engine starts");
    assert_eq!(sf19.name(), "Fake Engine 1");
    assert_eq!(sf19.net().as_deref(), Some("nn-1a298aa575a0.nnue"));
    assert_eq!(
        (sf19.net_memory(), sf19.net_memory_error(), sf19.starts()),
        (Some(NetworkMemory::Shared), None, 1)
    );
    assert!(sf19.is_alive());
    assert!(sf19.pid().is_some());
    // A search still reads its score line.
    let res = sf19.analyse(&moves(&["e2e4"]), &SearchOptions::depth(5)).await.expect("a search");
    assert_eq!(res.lines[0].cp, Some(20));
    assert_eq!(res.bestmove.as_deref(), Some("e2e4"));
    assert_eq!(sf19.net_memory(), Some(NetworkMemory::Shared));
    sf19.close().await;

    let mut fallback = fake_engine(&[
        SF19_NET,
        "Network replica 1: Local memory. Shared memory is not serving to other processes",
    ]);
    fallback.start().await.expect("the fake engine starts");
    assert_eq!(fallback.net_memory(), Some(NetworkMemory::Local));
    assert_eq!(fallback.net_memory_error(), Some("Shared memory is not serving to other processes"));
    fallback.close().await;

    let mut sf16 = fake_engine(&["NNUE evaluation using nn-5af11540bbfe.nnue enabled"]);
    sf16.start().await.expect("the fake engine starts");
    assert_eq!(sf16.net().as_deref(), Some("nn-5af11540bbfe.nnue"));
    assert_eq!(
        (sf16.net_memory(), sf16.net_memory_error()),
        (None, None),
        "Stockfish 16 says nothing: not reported"
    );
    sf16.close().await;

    let mut two = fake_engine(&[
        "NNUE evaluation using big.nnue",
        "NNUE evaluation using small.nnue",
        "NNUE evaluation using big.nnue",
    ]);
    two.start().await.expect("the fake engine starts");
    assert_eq!(two.nets(), ["big.nnue", "small.nnue"]);
    assert_eq!(two.net().as_deref(), Some("big.nnue+small.nnue"));
    two.close().await;
}

#[tokio::test]
async fn a_restarted_engine_reports_where_its_network_lives_anew() {
    let mut e = fake_engine(&["Network replica 1: Shared memory."]);
    e.start().await.expect("the fake engine starts");
    assert_eq!(e.net_memory(), Some(NetworkMemory::Shared));
    // The restarted process says nothing (another build).
    e.opts.args.truncate(3);
    // A crash.
    let p = e.proc.as_mut().expect("a running engine");
    p.child.start_kill().expect("the engine can be killed");
    p.child.wait().await.expect("the engine exits");
    e.start().await.expect("a new process starts");
    assert_eq!((e.starts(), e.net_memory()), (2, None));
    e.close().await;
}

#[tokio::test]
async fn a_search_within_a_node_limit_says_whether_the_limit_stopped_it() {
    let mut e = fake_engine(&[]);
    e.start().await.expect("the fake engine starts");
    e.sent.clear();
    let search = |depth, nodes| SearchOptions { nodes, ..SearchOptions::depth(depth) };
    let e2e4 = moves(&["e2e4"]);
    // The fake engine completes depth 1 in 20 nodes, and never goes deeper.
    assert!(!e.analyse(&e2e4, &search(1, None)).await.expect("a search").node_limited, "no limit");
    assert!(
        !e.analyse(&e2e4, &search(1, Some(1000))).await.expect("a search").node_limited,
        "the depth completed within the limit"
    );
    assert!(
        e.analyse(&e2e4, &search(1, Some(20))).await.expect("a search").node_limited,
        "the limit reached"
    );
    assert!(
        e.analyse(&e2e4, &search(5, Some(1000))).await.expect("a search").node_limited,
        "stopped before the depth was complete"
    );
    let go: Vec<&str> = e.sent.iter().map(String::as_str).filter(|c| c.starts_with("go")).collect();
    assert_eq!(go, ["go depth 1", "go depth 1 nodes 1000", "go depth 1 nodes 20", "go depth 5 nodes 1000"]);
    assert_eq!(e.searches(), 4);
    e.close().await;
}

#[tokio::test]
async fn commands_are_exact_and_options_are_sent_once_per_process() {
    let mut e = fake_engine(&[]);
    e.new_game().await.expect("a new game");
    let opts = SearchOptions {
        multi_pv: 3,
        fen: Some("8/8/8/8/8/8/8/K6k w - - 0 1".into()),
        ..SearchOptions::depth(1)
    };
    e.analyse(&moves(&["a1a2", "h1h2"]), &opts).await.expect("a search");
    e.analyse(&[], &opts).await.expect("a search");
    e.clear_hash().await.expect("the hash is cleared");
    assert_eq!(
        e.sent,
        [
            "uci",
            "setoption name Threads value 1",
            "setoption name Hash value 16",
            "isready",
            "position startpos",
            "go depth 1",
            "ucinewgame",
            "isready",
            "setoption name MultiPV value 3",
            "position fen 8/8/8/8/8/8/8/K6k w - - 0 1 moves a1a2 h1h2",
            "go depth 1",
            "position fen 8/8/8/8/8/8/8/K6k w - - 0 1",
            "go depth 1",
            "setoption name Clear Hash",
            "isready",
        ]
    );
    e.close().await;
    let err = e.new_game().await.expect_err("a closed engine refuses calls");
    assert_eq!(err.kind(), EngineErrorKind::Closed);
}

#[tokio::test]
async fn a_search_past_its_timeout_is_stopped_and_an_engine_that_answers_stays() {
    let script = fake_script(":", "printf 'info depth 3 score cp 5 pv e2e4\\nbestmove e2e4\\n'");
    let opts = EngineOptions { timeout: Duration::from_millis(300), ..fake_options(script, &[]) };
    let mut e = UciEngine::new(opts);
    e.start().await.expect("the fake engine starts");
    let pid = e.pid();
    let err = e.analyse(&[], &SearchOptions::depth(7)).await.expect_err("too slow");
    assert_eq!(err.kind(), EngineErrorKind::Timeout);
    assert!(err.recovered(), "the engine answered stop");
    assert_eq!(err.message(), "search of depth 7 exceeded 300 ms");
    assert!(e.sent.iter().any(|c| c == "stop"));
    assert_eq!((e.restarts(), e.pid()), (0, pid), "the same process goes on");
    assert!(e.is_alive());
    assert_eq!(e.analyse(&[], &SearchOptions::depth(1)).await.expect("a search").lines[0].cp, Some(20));
    e.close().await;
}

#[tokio::test]
async fn a_search_that_ignores_stop_is_killed_and_the_next_call_restarts_the_engine() {
    let opts = EngineOptions {
        timeout: Duration::from_millis(200),
        stop_grace: Duration::from_millis(200),
        ..fake_options(fake_script(":", ":"), &[])
    };
    let mut e = UciEngine::new(opts);
    e.start().await.expect("the fake engine starts");
    let err = e.analyse(&[], &SearchOptions::depth(7)).await.expect_err("never answers");
    assert_eq!(err.kind(), EngineErrorKind::Timeout);
    assert!(!err.recovered());
    assert_eq!(err.message(), "engine did not answer \"go\"");
    assert_eq!(e.restarts(), 1);
    assert!(!e.is_alive());
    let res = e.analyse(&[], &SearchOptions::depth(1)).await.expect("a fresh process searches");
    assert_eq!(res.bestmove.as_deref(), Some("e2e4"));
    assert_eq!((e.starts(), e.restarts()), (2, 1));
    e.close().await;
}

#[tokio::test]
async fn a_crash_during_a_search_reports_the_exit_status() {
    let mut e = UciEngine::new(fake_options(fake_script("exit 3", ":"), &[]));
    e.start().await.expect("the fake engine starts");
    let err = e.analyse(&[], &SearchOptions::depth(7)).await.expect_err("the engine exits");
    assert_eq!(err.kind(), EngineErrorKind::Crashed);
    assert_eq!(err.to_string(), "engine crashed: engine exited (code 3, signal null)");
    assert_eq!(e.restarts(), 1);
    // The options are sent again to the new process.
    e.sent.clear();
    e.analyse(&[], &SearchOptions::depth(1)).await.expect("a fresh process searches");
    assert!(e.sent.iter().any(|c| c == "setoption name MultiPV value 1"));
    assert_eq!(e.starts(), 2);
    e.close().await;
}

#[tokio::test]
async fn an_engine_left_in_the_middle_of_a_command_is_replaced() {
    let mut e = fake_engine(&[]);
    e.start().await.expect("the fake engine starts");
    let first = e.pid();
    // The caller gives up on a search (its future is dropped): the engine's state is unknown.
    let gave_up =
        tokio::time::timeout(Duration::from_millis(100), e.analyse(&[], &SearchOptions::depth(7))).await;
    assert!(gave_up.is_err());
    assert!(!e.is_alive());
    e.new_game().await.expect("a new process");
    assert_eq!(e.starts(), 2);
    assert_ne!(e.pid(), first);
    e.close().await;
}

#[test]
fn line_reader_splits_lines_and_drops_runaway_output() {
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("a runtime");
    rt.block_on(async {
        let mut child = Command::new("/bin/sh")
            .args(["-c", "printf 'a\\r\\nbc\\n\\nlast'"])
            .stdout(Stdio::piped())
            .spawn()
            .expect("sh starts");
        let mut r = LineReader::new(child.stdout.take().expect("piped"));
        assert_eq!(r.next_line().await.as_deref(), Some("a"));
        assert_eq!(r.next_line().await.as_deref(), Some("bc"));
        assert_eq!(r.next_line().await.as_deref(), Some(""));
        assert_eq!(r.next_line().await, None, "an unterminated last line is not a line");
        child.wait().await.expect("sh exits");
    });
}
