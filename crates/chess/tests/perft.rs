//! Move generation: perft node counts (chessprogramming.org) and speed.
//!
//! The default run checks every position to a depth that stays quick in a debug build; the full
//! depths of the former server's suite run with `--ignored` (fast in release:
//! `cargo test --release -p scacelith-chess --test perft -- --ignored --nocapture`).

mod common;

use std::time::Instant;

use common::pos;
use scacelith_chess::{MAX_PERFT_DEPTH, PerftDepthError, Position, START_FEN};

const KIWIPETE: &str = "r3k2r/p1ppqpb1/bn2pnp1/3PN3/1p2P3/2N2Q1p/PPPBBPPP/R3K2R w KQkq - 0 1";
const POS3: &str = "8/2p5/3p4/KP5r/1R3p1k/8/4P1P1/8 w - - 0 1";
const POS4: &str = "r3k2r/Pppp1ppp/1b3nbN/nP6/BBP1P3/q4N2/Pp1P2PP/R2Q1RK1 w kq - 0 1";
const POS4_MIRROR: &str = "r2q1rk1/pP1p2pp/Q4n2/bbp1p3/Np6/1B3NBn/pPPP1PPP/R3K2R b KQ - 0 1";
const POS5: &str = "rnbq1k1r/pp1Pbppp/2p5/8/2B5/8/PPP1NnPP/RNBQK2R w KQ - 1 8";
const POS6: &str = "r4rk1/1pp1qppp/p1np1n2/2b1p1B1/2B1P1b1/P1NP1N2/1PP1QPPP/R4RK1 w - - 0 10";

/// Name, FEN, node counts from depth 1.
const SUITE: [(&str, &str, &[u64]); 7] = [
    ("start", START_FEN, &[20, 400, 8902, 197_281, 4_865_609]),
    ("kiwipete", KIWIPETE, &[48, 2039, 97_862, 4_085_603]),
    ("position 3", POS3, &[14, 191, 2812, 43_238, 674_624, 11_030_083]),
    ("position 4", POS4, &[6, 264, 9467, 422_333, 15_833_292]),
    ("position 4 mirrored", POS4_MIRROR, &[6, 264, 9467, 422_333, 15_833_292]),
    ("position 5", POS5, &[44, 1486, 62_379, 2_103_487]),
    ("position 6", POS6, &[46, 2079, 89_890, 3_894_594]),
];

/// Illegal en passant, castling, promotions, stalemates: FEN, depth, nodes.
const EDGE_CASES: [(&str, u32, u64); 14] = [
    ("3k4/3p4/8/K1P4r/8/8/8/8 b - - 0 1", 6, 1_134_888), // illegal ep #1
    ("8/8/4k3/8/2p5/8/B2P2K1/8 w - - 0 1", 6, 1_015_133), // illegal ep #2
    ("8/8/1k6/2b5/2pP4/8/5K2/8 b - d3 0 1", 6, 1_440_467), // ep capture checks opponent
    ("5k2/8/8/8/8/8/8/4K2R w K - 0 1", 6, 661_072),      // short castling gives check
    ("3k4/8/8/8/8/8/8/R3K3 w Q - 0 1", 6, 803_711),      // long castling gives check
    ("r3k2r/1b4bq/8/8/8/8/7B/R3K2R w KQkq - 0 1", 4, 1_274_206), // castle rights
    ("r3k2r/8/3Q4/8/8/5q2/8/R3K2R b KQkq - 0 1", 4, 1_720_476), // castling prevented
    ("2K2r2/4P3/8/8/8/8/8/3k4 w - - 0 1", 6, 3_821_001), // promote out of check
    ("8/8/1P2K3/8/2n5/1q6/8/5k2 b - - 0 1", 5, 1_004_658), // discovered check
    ("4k3/1P6/8/8/8/8/K7/8 w - - 0 1", 6, 217_342),      // promote to give check
    ("8/P1k5/K7/8/8/8/8/8 w - - 0 1", 6, 92_683),        // under-promote to give check
    ("K1k5/8/P7/8/8/8/8/8 w - - 0 1", 6, 2217),          // self stalemate
    ("8/k1P5/8/1K6/8/8/8/8 w - - 0 1", 7, 567_584),      // stalemate and checkmate
    ("8/8/2k5/5q2/5n2/8/5K2/8 b - - 0 1", 4, 23_527),    // stalemate and checkmate
];

/// Checks the counts of a position up to `max_nodes` leaves; returns (nodes, seconds) of the
/// deepest depth run.
fn check(name: &str, fen: &str, counts: &[u64], max_nodes: u64) -> (u64, f64) {
    let p = pos(fen);
    let mut last = (0, 0.0);
    for (d, &want) in (1..).zip(counts) {
        if want > max_nodes {
            break;
        }
        let t0 = Instant::now();
        let n = p.perft(d).unwrap();
        last = (n, t0.elapsed().as_secs_f64());
        assert_eq!(n, want, "{name} perft({d})");
    }
    assert_eq!(p.fen(), fen, "perft leaves the position unchanged");
    assert_eq!(p.perft(0), Ok(1));
    last
}

#[test]
fn perft_standard_positions() {
    for (name, fen, counts) in SUITE {
        check(name, fen, counts, 500_000);
    }
}

#[test]
fn perft_edge_cases() {
    for (fen, depth, nodes) in EDGE_CASES {
        let p = pos(fen);
        // The full depth when it is small enough, else two plies less (the counts of the deep
        // ones are checked by `perft_full_depths`).
        let shallow = if nodes <= 600_000 { depth } else { depth - 2 };
        let n = p.perft(shallow).unwrap();
        if shallow == depth {
            assert_eq!(n, nodes, "perft({depth}) of {fen}");
        } else {
            assert!(n > 0 && n < nodes, "perft({shallow}) of {fen}");
        }
    }
}

#[test]
fn perft_depth_limits() {
    let p = Position::start();
    assert_eq!(p.perft(0), Ok(1));
    assert_eq!(p.perft(MAX_PERFT_DEPTH + 1), Err(PerftDepthError(MAX_PERFT_DEPTH + 1)));
    assert_eq!(p.perft(u32::MAX), Err(PerftDepthError(u32::MAX)));
    // A mated side has no moves at any depth.
    let mated = pos("rnb1kbnr/pppp1ppp/8/4p3/6Pq/5P2/PPPPP2P/RNBQKBNR w KQkq - 1 3");
    assert_eq!(mated.perft(1), Ok(0));
    assert_eq!(mated.perft(3), Ok(0));
}

/// The full depths of the former server's suite (about 75 M leaves).
#[test]
#[ignore = "slow in a debug build; run with --release -- --ignored"]
fn perft_full_depths() {
    for (name, fen, counts) in SUITE {
        check(name, fen, counts, u64::MAX);
    }
    for (fen, depth, nodes) in EDGE_CASES {
        assert_eq!(pos(fen).perft(depth), Ok(nodes), "perft({depth}) of {fen}");
    }
}

/// Throughput of the deepest depth of each standard position (leaf nodes per second, bulk
/// counting at the last ply).
#[test]
#[ignore = "timing; run with --release -- --ignored --nocapture"]
fn perft_timing() {
    let (mut nodes, mut secs) = (0u64, 0.0f64);
    for (name, fen, counts) in SUITE {
        let (n, s) = check(name, fen, counts, u64::MAX);
        println!(
            "{name}: perft({}) = {n} in {:.0} ms ({:.1} M nodes/s)",
            counts.len(),
            s * 1e3,
            n as f64 / s / 1e6
        );
        nodes += n;
        secs += s;
    }
    println!("perft total: {nodes} nodes in {:.2} s, {:.1} M nodes/s", secs, nodes as f64 / secs / 1e6);
}
