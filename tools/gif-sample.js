#!/usr/bin/env node
// Sample animated GIFs of games (src/gif/render.js) for visual review, with PNG pictures of chosen
// frames, and the measurements of the renderer (time and size per game length and picture size,
// memory of the rendering thread).
//
//   node tools/gif-sample.js [--out DIR] [--pgn FILE] [--size small|medium|large|all]
//        [--frames 0,20,last] [--zoom N] [--orientation white|black] [--delay MS] [--no-coords]
//   node tools/gif-sample.js --bench          # render times and sizes, pool memory
//
// Without --pgn: two famous games (Morphy's Opera game, Paris 1858; Kasparov - Topalov, Wijk aan
// Zee 1999) and two seeded random games of 80 and 300 plies. Output (default
// /home/user/gif-samples or ./gif-samples): <name>-<size>.gif, <name>-<size>-<ply>.png.

import { mkdirSync, readFileSync, writeFileSync, existsSync } from 'node:fs';
import { join } from 'node:path';
import { pathToFileURL } from 'node:url';
import { renderGame, SIZES } from '../src/gif/render.js';
import { indexedToPng } from '../src/gif/png.js';
import { createGifPool } from '../src/gif/pool.js';
import { readPgn } from '../src/chess/pgn.js';
import { Position } from '../src/chess/index.js';

const OPERA = `[Event "Paris"]
[Site "Paris FRA"]
[Date "1858.??.??"]
[White "Paul Morphy"]
[Black "Duke Karl / Count Isouard"]
[Result "1-0"]

1. e4 e5 2. Nf3 d6 3. d4 Bg4 4. dxe5 Bxf3 5. Qxf3 dxe5 6. Bc4 Nf6 7. Qb3 Qe7
8. Nc3 c6 9. Bg5 b5 10. Nxb5 cxb5 11. Bxb5+ Nbd7 12. O-O-O Rd8 13. Rxd7 Rxd7
14. Rd1 Qe6 15. Bxd7+ Nxd7 16. Qb8+ Nxb8 17. Rd8# 1-0`;

const KASPAROV_TOPALOV = `[Event "Hoogovens"]
[Site "Wijk aan Zee NED"]
[Date "1999.01.20"]
[White "Garry Kasparov"]
[Black "Veselin Topalov"]
[Result "1-0"]
[WhiteElo "2812"]
[BlackElo "2700"]

1. e4 d6 2. d4 Nf6 3. Nc3 g6 4. Be3 Bg7 5. Qd2 c6 6. f3 b5 7. Nge2 Nbd7 8. Bh6
Bxh6 9. Qxh6 Bb7 10. a3 e5 11. O-O-O Qe7 12. Kb1 a6 13. Nc1 O-O-O 14. Nb3 exd4
15. Rxd4 c5 16. Rd1 Nb6 17. g3 Kb8 18. Na5 Ba8 19. Bh3 d5 20. Qf4+ Ka7 21. Rhe1
d4 22. Nd5 Nbxd5 23. exd5 Qd6 24. Rxd4 cxd4 25. Re7+ Kb6 26. Qxd4+ Kxa5 27. b4+
Ka4 28. Qc3 Qxd5 29. Ra7 Bb7 30. Rxb7 Qc4 31. Qxf6 Kxa3 32. Qxa6+ Kxb4 33. c3+
Kxc3 34. Qa1+ Kd2 35. Qb2+ Kd1 36. Bf1 Rd2 37. Rd7 Rxd7 38. Bxc4 bxc4 39. Qxh8
Rd3 40. Qa8 c3 41. Qa4+ Ke1 42. f4 f5 43. Kc1 Rd2 44. Qa7 1-0`;

/** A seeded random legal game of exactly `plies` plies (retries with the next seed on an early end). */
export function randomGame(plies, seed = 1) {
    for (let s = seed; ; s++) {
        let x = s >>> 0 || 1;
        const rnd = () => {
            x ^= x << 13; x >>>= 0;
            x ^= x >>> 17;
            x ^= x << 5; x >>>= 0;
            return x / 4294967296;
        };
        const p = Position.start();
        const moves = [];
        while (moves.length < plies) {
            const legal = p.legalMoves();
            if (legal.length === 0) break;
            // Prefer captures a little: games that look less static.
            const captures = legal.filter((m) => p.pieceAt((m >> 6) & 63) !== 0);
            const pool = captures.length && rnd() < 0.35 ? captures : legal;
            const m = pool[Math.floor(rnd() * pool.length)];
            p.play(m);
            moves.push(m);
        }
        if (moves.length === plies && p.legalMoves().length > 0) return moves;
    }
}

function args(argv) {
    const o = { out: existsSync('/home/user') ? '/home/user/gif-samples' : 'gif-samples', size: 'all', frames: null, zoom: 1 };
    for (let i = 0; i < argv.length; i++) {
        const a = argv[i];
        const v = () => argv[++i];
        if (a === '--out') o.out = v();
        else if (a === '--pgn') o.pgn = v();
        else if (a === '--size') o.size = v();
        else if (a === '--frames') o.frames = v();
        else if (a === '--zoom') o.zoom = Math.max(1, Number(v()) | 0);
        else if (a === '--orientation') o.orientation = v();
        else if (a === '--delay') o.delayMs = Number(v());
        else if (a === '--no-coords') o.coords = false;
        else if (a === '--bench') o.bench = true;
        else if (a === '--help' || a === '-h') o.help = true;
        else throw new Error(`unknown argument ${a}`);
    }
    return o;
}

function tag(tags, name) {
    return tags.find(([n]) => n === name)?.[1];
}

function jobFromPgn(text, extra = {}) {
    const g = readPgn(text);
    const elo = (s) => (s && /^\d+$/.test(s) ? Number(s) : null);
    // The names of the famous games have spaces and slashes; server names never do.
    return {
        startFen: g.startFen,
        moves: g.moves,
        white: { name: (tag(g.tags, 'White') ?? 'White').replace(/[^A-Za-z0-9_.-]+/g, '_'), rating: elo(tag(g.tags, 'WhiteElo')) },
        black: { name: (tag(g.tags, 'Black') ?? 'Black').replace(/[^A-Za-z0-9_.-]+/g, '_'), rating: elo(tag(g.tags, 'BlackElo')) },
        result: g.result,
        ...extra,
    };
}

function bench() {
    const sizes = Object.keys(SIZES);
    const games = [['40 moves (80 plies)', randomGame(80, 7)], ['150 moves (300 plies)', randomGame(300, 11)]];
    console.log('Single-threaded renderGame, warm caches and JIT (median of 5 runs):');
    // Warm the sprites, squares and the JIT of every size first.
    for (const size of sizes) for (let k = 0; k < 2; k++) renderGame({ moves: games[1][1], options: { size } });
    for (const size of sizes) {
        for (const [label, moves] of games) {
            const times = [];
            let bytes = 0;
            for (let k = 0; k < 5; k++) {
                const t0 = performance.now();
                bytes = renderGame({ moves, white: { name: 'alice', rating: 1520 }, black: { name: 'bob', rating: 1497 }, result: '1/2-1/2', options: { size } }).length;
                times.push(performance.now() - t0);
            }
            times.sort((a, b) => a - b);
            console.log(`  ${size.padEnd(6)} ${label.padEnd(22)} ${times[2].toFixed(0).padStart(5)} ms  ${(bytes / 1024).toFixed(0).padStart(5)} KiB`);
        }
    }
    // Cold start: a fresh process would also rasterize the pieces (measured in a new thread below).
    return (async () => {
        const rss = () => process.memoryUsage().rss / 1048576;
        global.gc?.();
        const before = rss();
        const pool = createGifPool({ threads: 1, queueMax: 4, idleMs: 0 });
        const job = (size) => ({ moves: games[1][1], white: { name: 'alice' }, black: { name: 'bob' }, result: '*', options: { size } });
        let t0 = performance.now();
        await pool.render(job('medium'));
        const cold = performance.now() - t0;
        const afterFirst = rss();
        for (const size of sizes) await pool.render(job(size));
        const afterAll = rss();
        t0 = performance.now();
        await pool.render(job('large'));
        const warm = performance.now() - t0;
        await pool.close();
        await new Promise((r) => setTimeout(r, 200));
        global.gc?.();
        console.log('GIF pool (1 thread):');
        console.log(`  first render in a new thread (medium, 300 plies, cold caches): ${cold.toFixed(0)} ms`);
        console.log(`  warm render through the pool (large, 300 plies): ${warm.toFixed(0)} ms`);
        console.log(`  process RSS: ${before.toFixed(1)} MiB before, ${afterFirst.toFixed(1)} MiB with the thread after one render,`
            + ` ${afterAll.toFixed(1)} MiB after renders at all sizes, ${rss().toFixed(1)} MiB after close`);
    })();
}

async function main() {
    const o = args(process.argv.slice(2));
    if (o.help) {
        console.log('usage: node tools/gif-sample.js [--out DIR] [--pgn FILE] [--size small|medium|large|all] [--frames 0,20,last] [--zoom N] [--orientation white|black] [--delay MS] [--no-coords] | --bench');
        return;
    }
    if (o.bench) {
        await bench();
        return;
    }
    mkdirSync(o.out, { recursive: true });
    const games = o.pgn
        ? [['pgn', jobFromPgn(readFileSync(o.pgn))]]
        : [
            ['opera', jobFromPgn(OPERA, { footer: 'Checkmate' })],
            ['kasparov-topalov', jobFromPgn(KASPAROV_TOPALOV, { footer: 'Black resigned' })],
            ['random-80', { moves: randomGame(80, 7), white: { name: 'alice', rating: 1520 }, black: { name: 'bob_the.builder', rating: 1497 }, result: '1/2-1/2', footer: 'Draw by agreement' }],
            ['random-300', { moves: randomGame(300, 11), white: { name: 'deleted#4242', rating: 1680 }, black: { name: 'x', rating: 2105 }, result: '0-1', footer: 'Loss on time' }],
        ];
    const sizes = o.size === 'all' ? Object.keys(SIZES) : [o.size];
    for (const [name, job] of games) {
        for (const size of sizes) {
            const last = job.moves.length;
            const wanted = new Set((o.frames ?? `0,${Math.min(last, 9)},last`).split(',').map((f) => (f === 'last' ? last : Number(f))));
            const options = { size, orientation: o.orientation, delayMs: o.delayMs, coords: o.coords };
            const t0 = performance.now();
            const gif = renderGame({ ...job, options }, {
                onFrame(f) {
                    if (wanted.has(f.ply)) writeFileSync(join(o.out, `${name}-${size}-${f.ply}.png`), indexedToPng(f.width, f.height, f.pixels, f.palette, o.zoom));
                },
            });
            const ms = performance.now() - t0;
            writeFileSync(join(o.out, `${name}-${size}.gif`), gif);
            console.log(`${name}-${size}.gif: ${job.moves.length} plies, ${(gif.length / 1024).toFixed(1)} KiB, ${ms.toFixed(0)} ms (PNG frames included)`);
        }
    }
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
    main().catch((e) => {
        console.error(e && e.stack ? e.stack : e);
        process.exit(1);
    });
}
