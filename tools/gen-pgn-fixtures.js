#!/usr/bin/env node
// Server PGN fixtures for the game's PGN reader: a handful of games exported exactly as
// GET /api/v1/games/:id/pgn exports them (src/http/routes/players.js gamePgn), written to the
// repository's tests/data/server-pgn/ where the game's C++ tests read them through chess::pgn and
// the game archive (moves, [%clk] / [%emt] clocks, tags).
//
//   node dedicated-server/tools/gen-pgn-fixtures.js           writes the files
//   node dedicated-server/tools/gen-pgn-fixtures.js --check   exits 1 when a file is stale
//
// Files: NN-<name>.pgn (one game each) and index.json, the values a reader must get back from
// each file: { file, gameId, white, black, result, termination, timeControl, plies, uci[],
// clockMs[], elapsedMs[] }, the clocks as the PGN writes them (tenths of a second, truncated;
// the game's reader keeps milliseconds). The games are legal (the generator replays them with the
// server's chess module and stops on an illegal move) and cover: checkmate, resignation, a flag
// fall, a flag fall drawn because the opponent cannot mate, abandonment, an aborted game (Result
// "*"), castling on both sides, en passant, promotions (queen and knights), a fair-play forfeit,
// a custom time control, an unknown rating ("-") and a deleted player ("deleted#<id>").
// Clocks follow the server's rules (src/game/clock.js): each side's first move runs no clock and
// adds no increment; later moves charge their time, then add the increment.
// Other .pgn files in the folder (made by hand) are left alone.

import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { ChessGame, GameStatus, EndReason } from '../src/chess/index.js';
import { gamePgn } from '../src/http/routes/players.js';

const here = path.dirname(fileURLToPath(import.meta.url));
export const FIXTURES_DIR = path.resolve(here, '../../tests/data/server-pgn');
export const FIXTURE_CONFIG = Object.freeze({ serverName: 'Scacelith Test Server', serverPublicHost: 'chess.example.org' });

const T0 = Date.UTC(2026, 8, 28, 18, 30, 5);
const MIN = 60000;

// Think times (ms) of the moves that run a clock (ply >= 2): a fixed spread with millisecond noise.
const casual = (i) => 800 + (Math.imul(i + 7, 2654435761) >>> 0) % 5200;

/** The fixture games, as store.games.byId returns them (plus `file`). */
export function fixtureGames() {
    const specs = [
        {
            file: '01-checkmate.pgn', category: '3+2', rated: true, baseMs: 3 * MIN, incMs: 2000,
            white: 'Alice', black: 'Bob', whiteRating: 1500, blackRating: 1520, changes: [11, -11],
            uci: 'e2e4 e7e5 f1c4 b8c6 d1h5 g8f6 h5f7', status: GameStatus.WhiteWins, reason: EndReason.Checkmate,
        },
        {
            file: '02-resignation-castling.pgn', category: '5+3', rated: true, baseMs: 5 * MIN, incMs: 3000,
            white: 'Carol', black: 'Dave', whiteRating: 1610, blackRating: 1580, changes: [9, -9],
            uci: 'e2e4 e7e5 g1f3 b8c6 f1c4 f8c5 e1g1 d7d6 d2d3 c8g4 b1c3 d8d7 c1e3 e8c8 e3c5 d6c5 c3d5',
            status: GameStatus.WhiteWins, reason: EndReason.Resignation,
        },
        {
            // White's flag falls on its 10th move: Black wins on time.
            file: '03-flag-fall.pgn', category: '1+0', rated: true, baseMs: MIN, incMs: 0,
            white: 'Erin', black: 'Finn', whiteRating: 1702, blackRating: 1688, changes: [-8, 8],
            uci: 'd2d4 d7d5 c2c4 e7e6 b1c3 g8f6 c1g5 f8e7 e2e3 e8g8 g1f3 b8d7 a1c1 c7c6 f1d3 d5c4 d3c4 f6d5',
            think: (i) => (i % 2 === 0 ? [7310, 6905, 8120, 7655, 6420, 7880, 7215, 7123][(i - 2) / 2] : 900 + (i * 131) % 700),
            status: GameStatus.BlackWins, reason: EndReason.Timeout,
        },
        {
            // White's flag falls, but Black has a bare king: drawn ("time forfeit" all the same).
            file: '04-flag-fall-draw.pgn', category: '3+0', rated: true, baseMs: 3 * MIN, incMs: 0,
            white: 'Gina', black: 'Hugo', whiteRating: 1500, blackRating: 1500, changes: [0, 0],
            uci: 'a2a4 a7a5 g2g3 a8a7 h2h4 g7g5 h4g5 e7e6 h1h7 g8h6 h7h8 a7a6 g5h6 a6a8 h8f8 e8e7 f8d8 b7b6 d8c8 c7c5 '
                + 'c8b8 f7f5 b8a8 b6b5 a8a5 c5c4 a4b5 e7d6 a5a2 d6d5 a2a5 d5e4 b1a3 c4c3 b2c3 d7d5 a3c4 e6e5 c4e5 d5d4 '
                + 'c3d4 e4d4 f2f4 d4c5 a5a6 c5b4 e5c6 b4b5 c6e5 b5c5 c2c3 c5d5 f1g2 d5c5 g2h3 c5b5 h3f5 b5c5',
            think: (i) => (i % 2 === 0 ? 6250 + (i * 37) % 160 : 1100 + (i * 53) % 900),
            status: GameStatus.Draw, reason: EndReason.TimeoutVsInsufficient,
        },
        {
            // En passant (exd6), then Black disconnects for too long.
            file: '05-abandonment-en-passant.pgn', category: '10+5', rated: true, baseMs: 10 * MIN, incMs: 5000,
            white: 'Ivan', black: 'Jade', whiteRating: 1455, blackRating: 1490, changes: [12, -12],
            uci: 'e2e4 a7a6 e4e5 d7d5 e5d6 c7d6 d2d4 g8f6 g1f3',
            status: GameStatus.WhiteWins, reason: EndReason.Abandonment,
        },
        {
            // A custom time control, unrated, no ratings shown; Black never played its first move.
            file: '06-aborted.pgn', category: 'custom', rated: false, baseMs: 90000, incMs: 5000,
            white: 'Kim', black: 'Lou', whiteRating: null, blackRating: null, changes: null,
            uci: 'e2e4', status: GameStatus.Aborted, reason: EndReason.NoShow,
        },
        {
            // Promotion to a queen; Black's account was deleted since (anonymized name).
            file: '07-promotion-deleted-player.pgn', category: '5+0', rated: true, baseMs: 5 * MIN, incMs: 0,
            white: 'Mia', black: 'deleted#42', whiteRating: 1530, blackRating: 1450, changes: [7, -7],
            uci: 'e2e4 d7d5 e4d5 c7c6 d5c6 g8f6 c6b7 b8d7 b7a8q', status: GameStatus.WhiteWins, reason: EndReason.Resignation,
        },
        {
            // Two promotions to a knight, then White is forfeited by the fair-play checks.
            file: '08-forfeit-underpromotion.pgn', category: '3+2', rated: true, baseMs: 3 * MIN, incMs: 2000,
            white: 'Ned', black: 'Ola', whiteRating: 1800, blackRating: 1795, changes: [-9, 9],
            uci: 'a2a4 h7h5 a4a5 h5h4 a5a6 h4h3 a6b7 h3g2 b7a8n g2h1n', status: GameStatus.BlackWins, reason: EndReason.Forfeit,
        },
    ];
    return specs.map((s, n) => {
        const replay = new ChessGame();
        const moves = [];
        for (const u of s.uci.split(' ')) {
            const m = replay.position.parseUCI(u);
            if (m < 0 || !replay.play(m).ok) throw new Error(`${s.file}: illegal move ${u} in ${replay.position.fen()}`);
            moves.push(m);
        }
        const think = s.think || casual;
        const clocks = [s.baseMs, s.baseMs];
        const spentMs = [], clockMs = [];
        for (let i = 0; i < moves.length; i++) {
            const side = i % 2;
            const spent = i < 2 ? 0 : think(i);
            if (i >= 2) clocks[side] = clocks[side] - spent + s.incMs;
            if (!(clocks[side] > 0)) throw new Error(`${s.file}: the clock of ply ${i} ran out (${clocks[side]} ms)`);
            spentMs.push(spent);
            clockMs.push(clocks[side]);
        }
        const startedAt = T0 + n * 3600000;
        const endedAt = startedAt + spentMs.reduce((a, b) => a + b, 0) + 4000;
        const ratingChanges = s.changes ? {
            white: { before: s.whiteRating, after: s.whiteRating + s.changes[0] },
            black: { before: s.blackRating, after: s.blackRating + s.changes[1] },
        } : null;
        return {
            file: s.file, id: 4_100_000_000_000 + n * 1013 + 7, category: s.category, rated: s.rated, baseMs: s.baseMs, incMs: s.incMs,
            whiteId: 100 + 2 * n, blackId: 101 + 2 * n, whiteName: s.white, blackName: s.black, whiteRating: s.whiteRating,
            blackRating: s.blackRating, startedAt, endedAt, status: s.status, reason: s.reason, plyCount: moves.length,
            rematchOf: null, flags: 0, ratingChanges, moves: Uint16Array.from(moves), spentMs: Uint32Array.from(spentMs),
            clockMs: Uint32Array.from(clockMs), uci: s.uci.split(' '),
        };
    });
}

const tenths = (ms) => Math.floor(ms / 100) * 100;

/** The files to write: { name: text }. */
export function renderFixtures(games = fixtureGames()) {
    const out = {};
    const index = [];
    for (const g of games) {
        const text = gamePgn(g, FIXTURE_CONFIG);
        if (text === null) throw new Error(`${g.file}: gamePgn refused the record`);
        out[g.file] = text;
        const tag = (name) => new RegExp(`^\\[${name} "([^"]*)"\\]$`, 'm').exec(text)[1];
        index.push({
            file: g.file, gameId: String(g.id), white: g.whiteName, black: g.blackName, result: tag('Result'),
            termination: tag('Termination'), timeControl: tag('TimeControl'), plies: g.moves.length, uci: g.uci,
            clockMs: [...g.clockMs].map(tenths), elapsedMs: [...g.spentMs].map(tenths),
        });
    }
    const list = index.map((x) => '    ' + JSON.stringify(x)).join(',\n');
    out['index.json'] = `{\n  "generator": "dedicated-server/tools/gen-pgn-fixtures.js (GET /api/v1/games/:id/pgn)",\n  "games": [\n${list}\n  ]\n}\n`;
    return out;
}

function main(argv) {
    const files = renderFixtures();
    const rel = path.relative(process.cwd(), FIXTURES_DIR) || '.';
    const stale = Object.entries(files).filter(([name, text]) => {
        const p = path.join(FIXTURES_DIR, name);
        // Line endings are normalised: a checkout with CRLF (Windows) is not stale.
        return !fs.existsSync(p) || fs.readFileSync(p, 'utf8').replace(/\r\n/g, '\n') !== text;
    });
    if (argv.includes('--check')) {
        if (stale.length) { console.error(`${rel}: ${stale.map(([n]) => n).join(', ')} stale: run node dedicated-server/tools/gen-pgn-fixtures.js`); return 1; }
        return 0;
    }
    if (!stale.length) { console.log(`${rel} up to date`); return 0; }
    fs.mkdirSync(FIXTURES_DIR, { recursive: true });
    for (const [name, text] of stale) fs.writeFileSync(path.join(FIXTURES_DIR, name), text);
    console.log(`wrote ${stale.map(([n]) => path.join(rel, n)).join(', ')}`);
    return 0;
}

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
    process.exitCode = main(process.argv.slice(2));
}
