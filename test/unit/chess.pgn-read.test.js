// PGN reader (src/chess/pgn.js): the server's own PGN (fixtures of tests/data/server-pgn and
// ChessGame.pgn round trips), lichess and chess.com exports, lenient SAN, movetext structure,
// first game only, and hostile inputs (only PgnError, with its line and column).
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync, readdirSync, existsSync } from 'node:fs';

import { readPgn, parseSan, normalizeResult, PgnError, PGN_LIMITS } from '../../src/chess/pgn.js';
import { Position, ChessGame, EndReason, GameStatus, BLACK } from '../../src/chess/index.js';

const START = 'rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1';
const uci = (moves, fen) => {
    const p = fen ? Position.fromFEN(fen) : Position.start();
    return moves.map((m) => {
        const u = p.uci(m);
        p.play(m);
        return u;
    });
};

function pgnError(fn, { line, column, message } = {}) {
    let err = null;
    try {
        fn();
    } catch (e) {
        err = e;
    }
    assert.ok(err instanceof PgnError, `PgnError expected, got ${err && err.stack ? err.stack : err}`);
    assert.ok(Number.isInteger(err.line) && err.line >= 1 && Number.isInteger(err.column) && err.column >= 1);
    if (line !== undefined) assert.equal(err.line, line, `line of "${err.message}"`);
    if (column !== undefined) assert.equal(err.column, column, `column of "${err.message}"`);
    if (message !== undefined) assert.match(err.message, message);
    return err;
}

function rng(seed) {
    let x = seed >>> 0 || 1;
    return () => {
        x ^= x << 13; x >>>= 0;
        x ^= x >>> 17;
        x ^= x << 5; x >>>= 0;
        return x / 4294967296;
    };
}

test('the PGN files the server writes (tests/data/server-pgn)', (t) => {
    const dir = new URL('../../../tests/data/server-pgn/', import.meta.url);
    if (!existsSync(dir)) {
        t.skip('tests/data/server-pgn not in this checkout');
        return;
    }
    const index = JSON.parse(readFileSync(new URL('index.json', dir), 'utf8'));
    const files = readdirSync(dir).filter((f) => f.endsWith('.pgn'));
    assert.ok(files.length >= 8);
    for (const g of index.games) {
        const r = readPgn(readFileSync(new URL(g.file, dir), 'utf8'));
        assert.deepEqual(uci(r.moves), g.uci, g.file);
        assert.equal(r.result, g.result, g.file);
        assert.equal(r.startFen, null);
        const tag = (n) => r.tags.find(([k]) => k === n)?.[1];
        assert.equal(tag('White'), g.white);
        assert.equal(tag('Black'), g.black);
        assert.equal(tag('ScacelithGameId'), g.gameId);
        assert.equal(tag('Termination'), g.termination);
        assert.equal(r.tags[0][0], 'Event');
    }
});

test('ChessGame.pgn round trips: random games, comments, custom starts, every ending', () => {
    const r = rng(2026);
    const fens = [undefined, 'r3k2r/pppq1ppp/2npbn2/4p3/4P3/2NPBN2/PPPQ1PPP/R3K2R b KQkq - 4 9', '8/P6k/8/8/8/8/6Kp/8 w - - 0 60',
        '4k3/8/8/2pP4/8/8/8/4K3 w - c6 0 1'];
    for (let n = 0; n < 60; n++) {
        const fen = fens[n % fens.length];
        const g = new ChessGame(fen);
        const plies = Math.floor(r() * 140);
        while (g.ply < plies && !g.isOver) {
            const legal = g.position.legalMoves();
            g.play(legal[Math.floor(r() * legal.length)]);
        }
        if (!g.isOver) {
            const k = n % 4;
            if (k === 0) g.resign(g.position.side);
            else if (k === 1) g.agreeDraw();
            else if (k === 2) g.end(GameStatus.Aborted, EndReason.Aborted);
        }
        const comments = g.moves.map((_, i) => (i % 3 === 0 ? ['[%clk 0:02:58.3]', '[%emt 0:00:01.7]'] : i % 3 === 1 ? 'a } brace' : null));
        const text = g.pgn({ white: 'alice', black: 'deleted#42', event: 'Test "quoted" \\ event', date: '2026.10.01', comments,
            afterResult: [['WhiteElo', '1500']], extra: [['ScacelithGameId', String(n)]] });
        const back = readPgn(text);
        assert.deepEqual(back.moves, g.moves, text);
        assert.equal(back.result, g.resultString());
        assert.equal(back.startFen, fen === undefined ? null : new ChessGame(fen).startFen);
        assert.equal(back.tags.find(([k]) => k === 'Event')[1], 'Test "quoted" \\ event');
        assert.equal(back.tags.find(([k]) => k === 'ScacelithGameId')[1], String(n));
    }
});

test('lichess and chess.com exports', () => {
    const lichess = `[Event "Rated Blitz game"]
[Site "https://lichess.org/abcdefgh"]
[Date "2024.03.02"]
[White "alice"]
[Black "bob"]
[Result "0-1"]
[UTCDate "2024.03.02"]
[UTCTime "10:11:12"]
[WhiteElo "1850"]
[BlackElo "1902"]
[WhiteRatingDiff "-6"]
[BlackRatingDiff "+5"]
[Variant "Standard"]
[TimeControl "180+2"]
[ECO "B01"]
[Opening "Scandinavian Defense: Mieses-Kotroc Variation"]
[Termination "Normal"]
[Annotator "lichess.org"]

1. e4 { [%eval 0.36] [%clk 0:03:00] } 1... d5 { [%eval 0.59] [%clk 0:03:00] } 2. exd5 { [%eval 0.5] [%clk 0:03:01] } 2... Qxd5 { [%eval 0.62] [%clk 0:03:01] } 3. Nc3 { [%eval 0.45] [%clk 0:03:02] } 3... Qa5 { [%eval 0.57] [%clk 0:03:02] } 4. Bc4?! { (0.57 → -0.10) Inaccuracy. d4 was best. } { [%eval -0.1] [%clk 0:02:58] } (4. d4 Nf6 5. Nf3 c6) 4... Nf6 { [%eval 0.0] [%clk 0:02:59] } 5. Qf3?? { [%eval -3.2] [%clk 0:02:50] } 5... Qe5+ $6 6. Kd1 Bg4 7. Qxg4 Nxg4 { White resigns. } 0-1


`;
    const a = readPgn(lichess);
    assert.deepEqual(uci(a.moves), ['e2e4', 'd7d5', 'e4d5', 'd8d5', 'b1c3', 'd5a5', 'f1c4', 'g8f6', 'd1f3', 'a5e5', 'e1d1', 'c8g4', 'f3g4', 'f6g4']);
    assert.equal(a.result, '0-1');
    assert.equal(a.tags.length, 18);
    const chesscom = `[Event "Live Chess"]
[Site "Chess.com"]
[Date "2024.05.06"]
[Round "-"]
[White "Carol"]
[Black "Dave"]
[Result "1/2-1/2"]
[CurrentPosition "4k3/8/8/8/8/8/8/4K3 w - - 0 40"]
[Timezone "UTC"]
[ECO "C20"]
[ECOUrl "https://www.chess.com/openings/Kings-Pawn-Opening"]
[UTCDate "2024.05.06"]
[UTCTime "18:00:00"]
[WhiteElo "1200"]
[BlackElo "1210"]
[TimeControl "600"]
[Termination "Game drawn by agreement"]
[StartTime "18:00:00"]
[EndDate "2024.05.06"]
[EndTime "18:20:00"]
[Link "https://www.chess.com/game/live/123456789"]

1. e4 {[%clk 0:09:58.1]} 1... e5 {[%clk 0:09:57.3]} 2. Nf3 {[%clk 0:09:55]} 2... Nc6 {[%clk 0:09:50.2]} 3. Bb5 {[%clk 0:09:49]} 3... a6 {[%clk 0:09:45.9]} 4. Bxc6 {[%clk 0:09:44]} 4... dxc6 {[%clk 0:09:40]} 5. O-O {[%clk 0:09:39]} 5... f6 {[%clk 0:09:30]} 1/2-1/2
`;
    const b = readPgn(chesscom);
    assert.equal(b.moves.length, 10);
    assert.equal(uci(b.moves)[8], 'e1g1');
    assert.equal(b.result, '1/2-1/2');
    // Windows line ends, a byte order mark, old Mac line ends.
    assert.deepEqual(readPgn('\ufeff' + lichess.replace(/\n/g, '\r\n')).moves, a.moves);
    assert.deepEqual(readPgn(lichess.replace(/\n/g, '\r')).moves, a.moves);
});

test('lenient SAN', () => {
    const cases = [
        [START, ['e4', 'e2e4', 'e2-e4', 'e4!', 'e4!?', 'e4?!'], 'e2e4'],
        [START, ['Nf3', 'nf3', 'Ng1f3', 'Ng1-f3', 'Ngf3', 'N1f3', 'Nf3+', '\u2658f3', '\u265ef3', 'g1f3'], 'g1f3'],
        ['r3k2r/8/8/8/8/8/8/R3K2R w KQkq - 0 1', ['O-O', '0-0', 'o-o', 'OO', 'O-O+', 'e1g1', 'Kg1'], 'e1g1'],
        ['r3k2r/8/8/8/8/8/8/R3K2R w KQkq - 0 1', ['O-O-O', '0-0-0', 'OOO', 'e1c1'], 'e1c1'],
        ['r3k2r/8/8/8/8/8/8/R3K2R b KQkq - 0 1', ['O-O', '0-0'], 'e8g8'],
        ['8/4P3/8/8/8/8/k7/4K3 w - - 0 1', ['e8=Q', 'e8Q', 'e8(Q)', 'e8/Q', 'e8=q', 'e8=Q+', 'e7e8q', 'e7-e8=Q'], 'e7e8q'],
        ['8/4P3/8/8/8/8/k7/4K3 w - - 0 1', ['e8=N', 'e8N', 'e7e8n'], 'e7e8n'],
        ['3r4/4P3/8/8/8/8/k7/4K3 w - - 0 1', ['exd8=R', 'exd8R', 'ed8R', 'e7xd8=R', 'e7:d8=R'], 'e7d8r'],
        // b-pawn capture first, then a bishop: "bxc3" is the pawn, "Bxc3" the bishop.
        ['4k3/8/8/8/8/2n5/1P1B4/4K3 w - - 0 1', ['bxc3', 'b2c3', 'bc3', 'b2xc3'], 'b2c3'],
        ['4k3/8/8/8/8/2n5/1P1B4/4K3 w - - 0 1', ['Bxc3', 'Bdc3', 'Bd2xc3', 'B2c3'], 'd2c3'],
        // lower-case bishop only when no pawn move matches.
        ['4k3/8/8/8/8/8/8/4KB2 w - - 0 1', ['bc4', 'Bc4', 'bxc4'], 'f1c4'],
        // En passant with or without "e.p.".
        ['4k3/8/8/2pP4/8/8/8/4K3 w - c6 0 1', ['dxc6', 'dxc6e.p.', 'dxc6 e.p.', 'dc6', 'd5c6'], 'd5c6'],
        // Disambiguation; over-disambiguated forms.
        ['4k3/8/8/8/8/8/8/1N2KN2 w - - 0 1', ['Nbd2', 'N1d2'.length ? 'Nbd2' : '', 'Nb1d2', 'Nb1-d2'], 'b1d2'],
        ['4k3/8/8/8/8/8/8/1N2KN2 w - - 0 1', ['Nfd2', 'Nf1d2'], 'f1d2'],
    ];
    for (const [fen, texts, want] of cases) {
        for (const text of texts) {
            const p = Position.fromFEN(fen);
            const m = parseSan(p, text);
            assert.notEqual(m, -1, `${text} in ${fen}`);
            assert.equal(p.uci(m), want, `${text} in ${fen}`);
        }
    }
    const refused = [
        [START, ['e5', 'Ke2', 'O-O', 'Nf4', 'Pe4', 'xyz', '', 'e', 'Nf3f3', 'e2e5']],
        ['8/4P3/8/8/8/8/k7/4K3 w - - 0 1', ['e8', 'e8=K', 'e8=P', 'e7e8']],
        ['4k3/8/8/8/8/8/8/1N2KN2 w - - 0 1', ['Nd2', 'N1d2']],     // ambiguous
    ];
    for (const [fen, texts] of refused) {
        for (const text of texts) assert.equal(parseSan(Position.fromFEN(fen), text), -1, `${text} refused in ${fen}`);
    }
});

test('movetext: numbers, comments, escapes, NAGs, glyphs, evaluations, variations, results', () => {
    const text = `[White "a \\"quoted\\" \\\\ name"]
[Black "b"]
% an escape line of another program: 1. d4
1. e4 ; a comment to the end of the line 1. d4
{ a comment
  over lines (1. d4) } 1... e5 $1 2.Nf3 !? Nc6 +- 3.Bb5 (3. Bc4 Bc5 (3... Nf6 4. Ng5 (4. d3)) 4. c3) 3...a6 ± 4.Ba4 <reserved> Nf6 5.O-O Be7 = 6.Re1 b5
7.Bb3 d6 8.c3 O-O ½-½`;
    const r = readPgn(text);
    assert.equal(r.tags[0][1], 'a "quoted" \\ name');
    assert.deepEqual(uci(r.moves), ['e2e4', 'e7e5', 'g1f3', 'b8c6', 'f1b5', 'a7a6', 'b5a4', 'g8f6', 'e1g1', 'f8e7', 'f1e1', 'b7b5',
        'a4b3', 'd7d6', 'c2c3', 'e8g8']);
    assert.equal(r.result, '1/2-1/2');
    // Black to move first, "1..." numbering, FEN with SetUp.
    const b = readPgn('[SetUp "1"]\n[FEN "rnbqkbnr/pppppppp/8/8/4P3/8/PPPP1PPP/RNBQKBNR b KQkq - 0 1"]\n\n1... c5 2. Nf3 *');
    assert.deepEqual(uci(b.moves, 'rnbqkbnr/pppppppp/8/8/4P3/8/PPPP1PPP/RNBQKBNR b KQkq - 0 1'), ['c7c5', 'g1f3']);
    assert.equal(b.startFen, 'rnbqkbnr/pppppppp/8/8/4P3/8/PPPP1PPP/RNBQKBNR b KQkq - 0 1');
    assert.equal(b.result, '*');
    // SetUp "0" ignores the FEN; a FEN equal to the start position gives null.
    assert.equal(readPgn('[SetUp "0"]\n[FEN "8/8/8/8/8/8/k7/K7 w - - 0 1"]\n1. e4 *').moves.length, 1);
    assert.equal(readPgn(`[FEN "${START}"]\n1. e4 *`).startFen, null);
    // Chess960 tags with a standard setup.
    assert.equal(readPgn(`[Variant "Chess960"]\n[FEN "${START}"]\n1. e4 *`).moves.length, 1);
    // Result: the movetext's, else the Result tag, else '*'.
    assert.equal(readPgn('[Result "0-1"]\n1. e4 e5').result, '0-1');
    assert.equal(readPgn('[Result "garbage"]\n1. e4 e5').result, '*');
    assert.equal(readPgn('1. e4 e5 1-0').result, '1-0');
    assert.equal(readPgn('[Result "0.5-0.5"]\n1. e4 e5').result, '1/2-1/2');
    // A game of tags only.
    const tagsOnly = readPgn('[Event "x"]\n[Result "1-0"]\n');
    assert.deepEqual(tagsOnly.moves, []);
    assert.equal(tagsOnly.result, '1-0');
    assert.equal(normalizeResult('1/2'), '1/2-1/2');
    assert.equal(normalizeResult('2-0'), '');
});

test('only the first game is read', () => {
    const two = '[Event "one"]\n1. e4 e5 1-0\n\n[Event "two"]\n1. d4 d5 0-1\n';
    const r = readPgn(two);
    assert.equal(r.tags[0][1], 'one');
    assert.equal(r.moves.length, 2);
    assert.equal(r.result, '1-0');
    // Without a termination marker: the next tag pair ends the game.
    const r2 = readPgn('[Event "one"]\n1. e4 e5\n[Event "two"]\n1. d4 Ke7 garbage {');
    assert.equal(r2.moves.length, 2);
    assert.equal(r2.result, '*');
    // A broken tag after the movetext opens the next game too.
    assert.equal(readPgn('1. e4 e5\n[Event broken\n1. d4').moves.length, 2);
    // Moves after an automatic ending are kept (only legality matters).
    const shuffle = 'Nf3 Nf6 Ng1 Ng8 '.repeat(5);
    assert.equal(readPgn(`${shuffle} e4 *`).moves.length, 21);
});

test('bytes: UTF-8, Latin-1', () => {
    const text = '[White "J\u00f6rg"]\n1. e4 *';
    assert.equal(readPgn(new TextEncoder().encode(text)).tags[0][1], 'J\u00f6rg');
    assert.equal(readPgn(Buffer.from(text, 'latin1')).tags[0][1], 'J\u00f6rg');
});

test('hostile and broken inputs: PgnError with line and column', () => {
    pgnError(() => readPgn(''), { line: 1, column: 1, message: /no game/ });
    pgnError(() => readPgn('  \n { only a comment } \n'), { message: /no game/ });
    pgnError(() => readPgn(42), { message: /text expected/ });
    pgnError(() => readPgn(null));
    pgnError(() => readPgn('1. e4 e5\n2. Ke3 *'), { line: 2, column: 4, message: /illegal move 'Ke3'/ });
    pgnError(() => readPgn('[White "\u00e9\u00e9"] 1. e4 e5 2. Qh5 Nc6 3. Bc4 Nf6 4. Qxf7# Ke7'), { line: 1, column: 54, message: /illegal move/ });
    pgnError(() => readPgn('1. e4 -- 2. d4'), { message: /null moves/ });
    pgnError(() => readPgn('1. e4 Z0'), { message: /null moves/ });
    pgnError(() => readPgn('[FEN "4k3/8/8/8/8/8/8/1N2KN2 w - - 0 1"]\n1. Nd2'), { line: 2, column: 4, message: /illegal move 'Nd2'/ });
    pgnError(() => readPgn('[White "a"]\n[FEN "8/8/8 w - - 0 1"]\n1. e4'), { line: 2, column: 1, message: /invalid FEN/ });
    pgnError(() => readPgn('[Variant "Crazyhouse"]\n1. e4'), { line: 1, column: 1, message: /variant 'Crazyhouse' is not supported/ });
    pgnError(() => readPgn('[Variant "Chess960"]\n1. e4'), { message: /Chess960 game without a FEN/ });
    pgnError(() => readPgn('[Variant "Chess960"]\n[FEN "bqnbrkrn/pppppppp/8/8/8/8/PPPPPPPP/BQNBRKRN w GEge - 0 1"]\n1. e4'), { message: /Chess960/ });
    pgnError(() => readPgn('1. e4 { never closed'), { line: 1, column: 7, message: /unterminated comment/ });
    pgnError(() => readPgn('1. e4 { runs into\n[Event "next"]\n1. d4'), { message: /unterminated comment/ });
    pgnError(() => readPgn('[White "never closed]\n1. e4'), { line: 1, column: 1, message: /unterminated value/ });
    pgnError(() => readPgn('[White]\n1. e4'), { message: /quoted value expected/ });
    pgnError(() => readPgn('[ "x"]'), { message: /tag name expected/ });
    pgnError(() => readPgn('[White "x" junk]'), { message: /']' expected/ });
    pgnError(() => readPgn('1. e4 (1. d4 d5'), { line: 1, column: 7, message: /unterminated variation/ });
    pgnError(() => readPgn('1. e4 (1. d4 1-0'), { message: /unterminated variation before the result/ });
    pgnError(() => readPgn('1. e4 ) e5'), { line: 1, column: 7, message: /without a variation/ });
    pgnError(() => readPgn('1. e4 \u0001 e5'), { line: 1, column: 7, message: /control character/ });
    pgnError(() => readPgn('1. e4 # e5'), { message: /unexpected character '#'/ });
    pgnError(() => readPgn('1. e4 $999'), { message: /malformed NAG/ });
    pgnError(() => readPgn('1. e4 < unclosed'), { message: /'<' without '>'/ });
    pgnError(() => readPgn(`1. e4 ${'x'.repeat(41)}`), { message: /token too long/ });
    // Caps.
    pgnError(() => readPgn('1. e4 *', { maxBytes: 4 }), { message: /too large/ });
    pgnError(() => readPgn(new Uint8Array(100), { maxBytes: 99 }), { message: /too large/ });
    pgnError(() => readPgn('\u00e9'.repeat(60), { maxBytes: 100 }), { message: /too large/ });
    pgnError(() => readPgn('Nf3 Nf6 Ng1 Ng8 '.repeat(10), { maxPlies: 39 }), { line: 1, column: 157, message: /too many moves \(more than 39\)/ });
    assert.equal(readPgn('Nf3 Nf6 Ng1 Ng8 '.repeat(10), { maxPlies: 40 }).moves.length, 40);
    pgnError(() => readPgn('[A "1"]\n'.repeat(PGN_LIMITS.maxTags + 1)), { line: PGN_LIMITS.maxTags + 1, message: /too many tags/ });
    pgnError(() => readPgn(`[${'A'.repeat(65)} "x"]`), { message: /tag name too long/ });
    pgnError(() => readPgn(`[A "${'x'.repeat(2049)}"]`), { message: /value of tag A too long/ });
    pgnError(() => readPgn(`1. e4 ${'('.repeat(65)}`), { message: /nested too deeply/ });
    assert.equal(readPgn(`1. e4 ${'( d4 '.repeat(64)}${')'.repeat(64)} e5 *`).moves.length, 2);
});

test('fuzz: mutated PGN gives a game or a PgnError, never anything else', () => {
    const base = readFileSync(new URL('../../../tests/data/server-pgn/02-resignation-castling.pgn', import.meta.url), { encoding: 'utf8', flag: 'r' });
    const r = rng(7);
    const alphabet = '[]{}()"\\;%$!?+-=*/.:0123456789 \n\rabcdefghKQRBNOxo#\u00bd\u2026\u0000\u00e9';
    for (let n = 0; n < 3000; n++) {
        const chars = [...base];
        const edits = 1 + Math.floor(r() * 6);
        for (let e = 0; e < edits; e++) {
            const at = Math.floor(r() * (chars.length + 1));
            const c = alphabet[Math.floor(r() * alphabet.length)];
            const op = r();
            if (op < 0.4) chars.splice(at, 1);
            else if (op < 0.7) chars.splice(at, 0, c);
            else chars[at] = c;
        }
        const text = chars.join('');
        try {
            const g = readPgn(text);
            const p = g.startFen ? Position.fromFEN(g.startFen) : Position.start();
            for (const m of g.moves) p.play(m);      // every move legal
            assert.ok(['1-0', '0-1', '1/2-1/2', '*'].includes(g.result));
        } catch (e) {
            assert.ok(e instanceof PgnError, `${e && e.stack}\n--- input ---\n${text}`);
        }
    }
});

test('large inputs are read in linear time', () => {
    // 1 MiB of comments and variations around a short game.
    const filler = '{ ' + 'a comment that goes on and on '.repeat(20) + '} (1. d4 d5 2. c4 e6) ';
    const big = `[Event "big"]\n1. e4 ${filler.repeat(Math.floor((1 << 20) / filler.length) - 1)} e5 *`;
    assert.ok(big.length < (1 << 20) && big.length > 900000);
    const t0 = performance.now();
    const g = readPgn(big);
    assert.equal(g.moves.length, 2);
    assert.ok(performance.now() - t0 < 2000, `${performance.now() - t0} ms`);
    // 1500 plies of shuffling knights (no automatic ending stops the reader).
    const long = 'Nf3 Nf6 Ng1 Ng8 '.repeat(375);
    const t1 = performance.now();
    assert.equal(readPgn(long).moves.length, 1500);
    assert.ok(performance.now() - t1 < 2000, `${performance.now() - t1} ms`);
    // A line full of quotes inside a tag value does not cost one pass per quote.
    const quotes = `[A "${'"'.repeat(3000)}"]\n1. e4 *`;
    pgnError(() => readPgn(quotes), { message: /too long/ });
    assert.equal(readPgn(`[A "${'"'.repeat(2000)}"]\n1. e4 *`).tags[0][1], '"'.repeat(2000));
    const t2 = performance.now();
    readPgn(`[A "${'" '.repeat(1000)}"]\n1. e4 *`);
    assert.ok(performance.now() - t2 < 500);
    assert.equal(BLACK, 1);
});

test('many tag pairs on one long line are read in linear time', () => {
    // Every tag pair looks for the last closing quote of its line: once per line, not once per
    // tag (128 tags in front of 1 MiB on the same line took about a second).
    const tags = Array.from({ length: PGN_LIMITS.maxTags + 1 }, (_, i) => `[T${i} "v"] `).join('');
    const tooMany = tags + 'x'.repeat((1 << 20) - tags.length - 1);
    assert.ok(tooMany.length < (1 << 20) && tooMany.length > 1000000);
    const t0 = performance.now();
    pgnError(() => readPgn(tooMany), { line: 1, message: /too many tags/ });
    const ms0 = performance.now() - t0;
    assert.ok(ms0 < 300, `${ms0} ms`);
    // A valid game: 128 tags on its first line, then a comment of almost 1 MiB on the same line.
    const head = Array.from({ length: PGN_LIMITS.maxTags }, (_, i) => `[T${i} "v"] `).join('') + '{ ';
    const valid = head + 'c'.repeat((1 << 20) - head.length - 16) + ' } 1. e4 e5 *';
    assert.ok(valid.length < (1 << 20) && valid.length > 1000000);
    const t1 = performance.now();
    const g = readPgn(valid);
    const ms1 = performance.now() - t1;
    assert.equal(g.tags.length, PGN_LIMITS.maxTags);
    assert.equal(g.moves.length, 2);
    assert.ok(ms1 < 300, `${ms1} ms`);
    // The same answers as one tag per line (the cache of the line's last quote changes nothing).
    const lines = tags.replace(/\] /g, ']\n');
    pgnError(() => readPgn(lines), { line: PGN_LIMITS.maxTags + 1, message: /too many tags/ });
    assert.deepEqual(readPgn('[A "x"y"] [B "a"b"]\n1. e4 *').tags, [['A', 'x"y'], ['B', 'a"b']]);
    assert.deepEqual(readPgn('[A "x" ] [B "y"]   [C "z\\"w"]\n1. e4 *').tags, [['A', 'x'], ['B', 'y'], ['C', 'z"w']]);
});
