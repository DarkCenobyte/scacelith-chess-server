// Writes the golden fixtures of the GIF renderer tests (games.json) from the former Node.js
// server, the reference of the byte-identical output: the replayed positions of each game (what the
// renderer draws per ply) and, per job, the size and SHA-256 of the GIF that Node renders.
//
//   git worktree add /tmp/node-ref 7531830      # the last commit with the Node.js server
//   NODE_REF=/tmp/node-ref/dedicated-server node crates/gif/tests/fixtures/gen-fixtures.mjs
//
// Node 22 or later. The output is deterministic.

import { createHash } from 'node:crypto';
import { writeFileSync } from 'node:fs';
import { join } from 'node:path';
import { pathToFileURL } from 'node:url';

const REF = process.env.NODE_REF || '/home/user/rsw/node-ref/dedicated-server';
const load = (p) => import(pathToFileURL(join(REF, p)).href);
const { renderGame } = await load('src/gif/render.js');
const { pieceSprite, PIECE_CODES } = await load('src/gif/pieces.js');
const { Position, WHITE } = await load('src/chess/index.js');

const sha256 = (b) => createHash('sha256').update(b).digest('hex');

function uci(list, fen) {
    const p = fen ? Position.fromFEN(fen) : Position.start();
    return list.split(' ').filter(Boolean).map((u) => {
        const m = p.parseUCI(u);
        if (m === -1) throw new Error(`bad move ${u}`);
        p.play(m);
        return m;
    });
}

// Seeded random legal game (tools/gif-sample.js randomGame, without the retry on an early end).
function randomGame(plies, seed) {
    let x = seed >>> 0 || 1;
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
        const captures = legal.filter((m) => p.pieceAt((m >> 6) & 63) !== 0);
        const pool = captures.length && rnd() < 0.35 ? captures : legal;
        const m = pool[Math.floor(rnd() * pool.length)];
        p.play(m);
        moves.push(m);
    }
    return moves;
}

// The replay of src/gif/render.js: what each frame shows, one line per position:
// "<64 hex digits: piece code per square, a1 first> <from> <to> <check> <side> <number> <san>".
function replay(startFen, moves) {
    const pos = startFen ? Position.fromFEN(startFen) : Position.start();
    const board = (p) => Array.from({ length: 64 }, (_, sq) => p.pieceAt(sq).toString(16)).join('');
    const check = (p) => (p.inCheck() ? p.kingSquare(p.side) : -1);
    const states = [`${board(pos)} -1 -1 ${check(pos)} ${pos.side}  `];
    for (const m of moves) {
        if (!pos.isLegal(m)) throw new Error('illegal move');
        const san = pos.san(m);
        const number = pos.side === WHITE ? `${pos.fullmove}.` : `${pos.fullmove}...`;
        pos.play(m);
        states.push(`${board(pos)} ${m & 63} ${(m >> 6) & 63} ${check(pos)} ${pos.side} ${number} ${san}`);
    }
    return {
        states,
        checkmate: pos.isCheckmate(), stalemate: pos.isStalemate(), insufficientMaterial: pos.hasInsufficientMaterial(),
    };
}

const OPERA = 'e2e4 e7e5 g1f3 d7d6 d2d4 c8g4 d4e5 g4f3 d1f3 d6e5 f1c4 g8f6 f3b3 d8e7 b1c3 c7c6 c1g5 b7b5 c3b5 c6b5 '
    + 'c4b5 b8d7 e1c1 a8d8 d1d7 d8d7 h1d1 e7e6 b5d7 f6d7 b3b8 d7b8 d1d8';
const PROMO_FEN = '8/P7/8/8/8/8/k7/4K3 w - - 0 1';
const BARE_FEN = '8/8/8/3k4/8/3K4/2q5/8 w - - 0 1';
const CASTLE_FEN = 'r3k2r/ppp2ppp/5q2/3pP3/8/8/PPPQ1PPP/R3K2R w KQkq d6 0 12';
const UNDER_FEN = '8/1P4k1/8/8/8/8/5Kp1/8 b - - 3 70';
const games = {
    opera: { startFen: null, moves: uci(OPERA) },
    empty: { startFen: null, moves: [] },
    fools: { startFen: null, moves: uci('f2f3 e7e5 g2g4 d8h4') },
    promotion: { startFen: PROMO_FEN, moves: uci('a7a8q a2b2 a8b8', PROMO_FEN) },
    // Sam Loyd's ten-move stalemate, and a draw by bare kings (insufficient material).
    stalemate: { startFen: null, moves: uci('e2e3 a7a5 d1h5 a8a6 h5a5 h7h5 h2h4 a6h6 a5c7 f7f6 c7d7 e8f7 d7b7 d8d3 b7b8 d3h7 b8c8 f7g6 c8e6') },
    bareKings: { startFen: BARE_FEN, moves: uci('d3c2 d5e4', BARE_FEN) },
    // En passant and both castlings from a position with a move number; promotions to knights
    // from a position with Black to move.
    castles: { startFen: CASTLE_FEN, moves: uci('e5d6 e8c8 e1g1 f6d6 d2d6 d8d6', CASTLE_FEN) },
    underPromotion: { startFen: UNDER_FEN, moves: uci('g2g1n f2g1 g7f6 b7b8n', UNDER_FEN) },
    random80: { startFen: null, moves: randomGame(80, 7) },
    random300: { startFen: null, moves: randomGame(300, 11) },
    random1200: { startFen: null, moves: randomGame(1200, 5) },
};

const MORPHY = { name: 'Morphy', rating: 2690 }, DUKE = { name: 'Duke_Karl', rating: null };
const jobs = [];
for (const size of ['small', 'medium', 'large']) {
    for (const coords of [true, false]) {
        for (const orientation of ['white', 'black']) {
            jobs.push({ name: `opera ${size} ${coords ? 'coords' : 'nocoords'} ${orientation}`, game: 'opera',
                white: MORPHY, black: DUKE, result: '1-0', footer: 'Checkmate', options: { size, coords, orientation, delayMs: 500 } });
        }
    }
}
jobs.push({ name: 'empty defaults', game: 'empty', options: {} });
jobs.push({ name: 'opera odd names', game: 'opera', white: { name: 'x'.repeat(200), rating: 'abc\u0001' },
    black: { name: 'é中' }, result: '1/2-1/2', options: { size: 'small', coords: true, orientation: 'white', delayMs: 500 } });
jobs.push({ name: 'fools mate', game: 'fools', white: { name: 'a', rating: 1500 }, black: { name: 'b', rating: 1600 },
    result: '0-1', options: { size: 'medium', delayMs: 250 } });
jobs.push({ name: 'promotion', game: 'promotion', white: { name: 'w' }, black: { name: 'b' }, result: '*', options: { size: 'small' } });
jobs.push({ name: 'stalemate large black', game: 'stalemate', white: { name: 'Loyd', rating: 2100 }, black: { name: 'Victim · ½' },
    result: '1/2-1/2', options: { size: 'large', orientation: 'black', delayMs: 3000 } });
jobs.push({ name: 'bare kings', game: 'bareKings', white: { name: 'Kingsley' }, black: { name: 'Queenie', rating: 1999 },
    result: '½-½', options: { size: 'small', coords: false, delayMs: 100 } });
jobs.push({ name: 'aborted with footer', game: 'castles', white: { name: '  spaced  ' }, black: { name: '' },
    result: '*', footer: 'Game aborted', options: { size: 'medium', coords: false, orientation: 'black', delayMs: 505 } });
jobs.push({ name: 'long footer', game: 'underPromotion', white: { name: 'W'.repeat(40), rating: 2800 }, black: { name: 'B', rating: 3000 },
    result: '0-1', footer: 'A very long explanation of how this game ended, '.repeat(4), options: { size: 'large', delayMs: 1e9 } });
jobs.push({ name: 'random 80 medium', game: 'random80', white: { name: 'alice', rating: 1520 }, black: { name: 'bob', rating: 1497 },
    result: '*', options: { size: 'medium', delayMs: 1 } });
for (const size of ['small', 'medium', 'large']) {
    jobs.push({ name: `random 300 ${size}`, game: 'random300', white: { name: 'Random A' }, black: { name: 'Random B', rating: 1 },
        result: '1-0', footer: 'Time forfeit', options: { size, orientation: size === 'medium' ? 'black' : 'white', delayMs: 700 } });
}
jobs.push({ name: 'random 1200 small', game: 'random1200', white: { name: 'Marathon' }, black: { name: 'Runner' },
    result: '1/2-1/2', footer: 'Fifty-move rule', options: { size: 'small', coords: false } });

for (const j of jobs) {
    const g = games[j.game];
    const job = { startFen: g.startFen, moves: g.moves, white: j.white, black: j.black, result: j.result, footer: j.footer, options: j.options };
    const gif = renderGame(job);
    j.bytes = gif.length;
    j.sha256 = sha256(gif);
}

const sprites = {};
for (const size of [16, 32, 45, 48, 72, 100]) {
    sprites[size] = Object.fromEntries(PIECE_CODES.map((c) => {
        const s = pieceSprite(c, size);
        return [c, sha256(Buffer.from(s.buffer, s.byteOffset, s.byteLength))];
    }));
}

const out = {
    comment: 'Generated by gen-fixtures.mjs from the Node.js server (commit 7531830); do not edit.',
    games: Object.fromEntries(Object.entries(games).map(([k, g]) => [k, { startFen: g.startFen, moves: g.moves, ...replay(g.startFen, g.moves) }])),
    jobs,
    sprites,
};
// One line per move list.
const text = JSON.stringify(out, null, 1).replace(/"moves": \[([\s\d,]*)\]/g, (_, list) => `"moves": [${list.replace(/\s+/g, '')}]`);
writeFileSync(new URL('./games.json', import.meta.url), text + '\n');
console.log(jobs.map((j) => `${j.name}: ${j.bytes} ${j.sha256}`).join('\n'));
