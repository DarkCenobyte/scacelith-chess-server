#!/usr/bin/env node
// Benchmark of the server chess rules (src/chess): per-move validation cost as the game host
// uses it, auxiliary calls, and perft throughput.
// Usage: node tools/bench-chess.js [games=200]
import { Position, ChessGame } from '../src/chess/index.js';

const GAMES = Number(process.argv[2]) || 200;

// Deterministic pseudo-random games (moves only), long enough to reach endgames.
let seed = 0x5cac;
const rnd = (n) => {
    seed = (Math.imul(seed, 1103515245) + 12345) >>> 0;
    return (seed >>> 8) % n;
};
const games = [];
let totalMoves = 0;
for (let i = 0; i < GAMES; i++) {
    const g = new ChessGame();
    while (!g.isOver && g.ply < 300) {
        const legal = g.position.legalMoves();
        g.play(legal[rnd(legal.length)]);
    }
    games.push(Uint16Array.from(g.moves));
    totalMoves += g.moves.length;
}

function time(label, perGame, perMove) {
    let n = 0;
    const once = () => {
        for (const moves of games) {
            const ctx = perGame();
            for (let i = 0; i < moves.length; i++) {
                perMove(ctx, moves[i]);
                n++;
            }
        }
    };
    once();     // warm-up (JIT)
    n = 0;
    const t0 = performance.now();
    let rounds = 0;
    while (performance.now() - t0 < 1000 || rounds < 2) {
        once();
        rounds++;
    }
    const us = ((performance.now() - t0) * 1000) / n;
    console.log(`${label.padEnd(44)} ${us.toFixed(3).padStart(8)} us/move`);
}

console.log(`${GAMES} games, ${totalMoves} moves, Node ${process.version}`);
time('Position isLegal + play', () => Position.start(), (p, m) => {
    if (!p.isLegal(m)) throw new Error('illegal');
    p.play(m);
});
time('ChessGame isLegal + play (+ endings)', () => new ChessGame(), (g, m) => {
    if (!g.position.isLegal(m)) throw new Error('illegal');
    g.play(m);
});
time('ChessGame digest + isLegal + play', () => new ChessGame(), (g, m) => {
    g.position.digest();
    if (!g.position.isLegal(m)) throw new Error('illegal');
    g.play(m);
});
time('legalMoves() (array) + play', () => Position.start(), (p, m) => {
    p.legalMoves();
    p.play(m);
});
time('san() + play', () => Position.start(), (p, m) => {
    p.san(m);
    p.play(m);
});
time('fen() + play', () => Position.start(), (p, m) => {
    p.fen();
    p.play(m);
});

const perfts = [
    ['start d5', 'rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1', 5],
    ['kiwipete d4', 'r3k2r/p1ppqpb1/bn2pnp1/3PN3/1p2P3/2N2Q1p/PPPBBPPP/R3K2R w KQkq - 0 1', 4],
    ['position 3 d6', '8/2p5/3p4/KP5r/1R3p1k/8/4P1P1/8 w - - 0 1', 6],
];
for (const [name, fen, depth] of perfts) {
    const p = Position.fromFEN(fen);
    const t0 = performance.now();
    const n = p.perft(depth);
    const ms = performance.now() - t0;
    console.log(`perft ${name.padEnd(38)} ${String(n).padStart(9)} nodes ${ms.toFixed(0).padStart(5)} ms ${(n / ms / 1000).toFixed(1)} M nodes/s`);
}
