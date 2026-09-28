// Move generation: perft node counts (chessprogramming.org) and speed.
import { test } from 'node:test';
import assert from 'node:assert/strict';

import { Position } from '../../src/chess/index.js';

const START = 'rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1';
const KIWIPETE = 'r3k2r/p1ppqpb1/bn2pnp1/3PN3/1p2P3/2N2Q1p/PPPBBPPP/R3K2R w KQkq - 0 1';
const POS3 = '8/2p5/3p4/KP5r/1R3p1k/8/4P1P1/8 w - - 0 1';
const POS4 = 'r3k2r/Pppp1ppp/1b3nbN/nP6/BBP1P3/q4N2/Pp1P2PP/R2Q1RK1 w kq - 0 1';
const POS4_MIRROR = 'r2q1rk1/pP1p2pp/Q4n2/bbp1p3/Np6/1B3NBn/pPPP1PPP/R3K2R b KQ - 0 1';
const POS5 = 'rnbq1k1r/pp1Pbppp/2p5/8/2B5/8/PPP1NnPP/RNBQK2R w KQ - 1 8';
const POS6 = 'r4rk1/1pp1qppp/p1np1n2/2b1p1B1/2B1P1b1/P1NP1N2/1PP1QPPP/R4RK1 w - - 0 10';

let totalNodes = 0, totalMs = 0;

function checkPerft(name, fen, counts) {
    const p = Position.fromFEN(fen);
    assert.ok(p, fen);
    const before = p.fen();
    for (let d = 1; d <= counts.length; d++) {
        const t0 = performance.now();
        const n = p.perft(d);
        const ms = performance.now() - t0;
        assert.equal(n, counts[d - 1], `${name} perft(${d})`);
        if (d === counts.length) {
            totalNodes += n;
            totalMs += ms;
            console.log(`${name}: perft(${d}) = ${n} in ${ms.toFixed(0)} ms (${(n / ms / 1000).toFixed(1)} M nodes/s)`);
        }
    }
    assert.equal(p.fen(), before, 'perft leaves the position unchanged');
    assert.equal(p.perft(0), 1);
}

test('perft: start position to depth 5', () => {
    checkPerft('start', START, [20, 400, 8902, 197281, 4865609]);
});

test('perft: Kiwipete to depth 4', () => {
    checkPerft('kiwipete', KIWIPETE, [48, 2039, 97862, 4085603]);
});

test('perft: position 3 to depth 6', () => {
    checkPerft('position 3', POS3, [14, 191, 2812, 43238, 674624, 11030083]);
});

test('perft: position 4 and its mirror to depth 5', () => {
    checkPerft('position 4', POS4, [6, 264, 9467, 422333, 15833292]);
    checkPerft('position 4 mirrored', POS4_MIRROR, [6, 264, 9467, 422333, 15833292]);
});

test('perft: position 5 to depth 4', () => {
    checkPerft('position 5', POS5, [44, 1486, 62379, 2103487]);
});

test('perft: position 6 to depth 4', () => {
    checkPerft('position 6', POS6, [46, 2079, 89890, 3894594]);
});

test('perft: edge cases (illegal en passant, castling, promotions, stalemates)', () => {
    const cases = [
        ['3k4/3p4/8/K1P4r/8/8/8/8 b - - 0 1', 6, 1134888],        // illegal ep #1
        ['8/8/4k3/8/2p5/8/B2P2K1/8 w - - 0 1', 6, 1015133],       // illegal ep #2
        ['8/8/1k6/2b5/2pP4/8/5K2/8 b - d3 0 1', 6, 1440467],      // ep capture checks opponent
        ['5k2/8/8/8/8/8/8/4K2R w K - 0 1', 6, 661072],            // short castling gives check
        ['3k4/8/8/8/8/8/8/R3K3 w Q - 0 1', 6, 803711],            // long castling gives check
        ['r3k2r/1b4bq/8/8/8/8/7B/R3K2R w KQkq - 0 1', 4, 1274206], // castle rights
        ['r3k2r/8/3Q4/8/8/5q2/8/R3K2R b KQkq - 0 1', 4, 1720476], // castling prevented
        ['2K2r2/4P3/8/8/8/8/8/3k4 w - - 0 1', 6, 3821001],        // promote out of check
        ['8/8/1P2K3/8/2n5/1q6/8/5k2 b - - 0 1', 5, 1004658],      // discovered check
        ['4k3/1P6/8/8/8/8/K7/8 w - - 0 1', 6, 217342],            // promote to give check
        ['8/P1k5/K7/8/8/8/8/8 w - - 0 1', 6, 92683],              // under-promote to give check
        ['K1k5/8/P7/8/8/8/8/8 w - - 0 1', 6, 2217],               // self stalemate
        ['8/k1P5/8/1K6/8/8/8/8 w - - 0 1', 7, 567584],            // stalemate and checkmate
        ['8/8/2k5/5q2/5n2/8/5K2/8 b - - 0 1', 4, 23527],          // stalemate and checkmate
    ];
    for (const [fen, depth, nodes] of cases) {
        assert.equal(Position.fromFEN(fen).perft(depth), nodes, `perft(${depth}) of ${fen}`);
    }
});

test('perft: throughput', () => {
    assert.ok(totalNodes > 0);
    console.log(`perft total: ${totalNodes} nodes, ${(totalNodes / totalMs / 1000).toFixed(1)} M nodes/s`);
});
