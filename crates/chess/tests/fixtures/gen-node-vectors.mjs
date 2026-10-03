// Reference vectors of the former Node.js server's chess module for tests/node_vectors.rs:
// the PGN reader on mutated inputs (text, bytes, lowered limits), lenient SAN on random and
// mutated strings, and the PGN writer on random games (unicode tags, long and odd comments,
// every ending). The games are drawn from the legal move lists, so the Rust test also checks the
// generation order.
//
// The Node.js sources are those of the last commit before the Rust rewrite:
//   git worktree add /tmp/scacelith-node 46d51dd^
//   /opt/node22/bin/node dedicated-server/crates/chess/tests/fixtures/gen-node-vectors.mjs \
//       /tmp/scacelith-node/dedicated-server
// writes node-vectors.json next to this script.
import { readFileSync, writeFileSync } from 'node:fs';
import { pathToFileURL } from 'node:url';
import path from 'node:path';

const serverDir = process.argv[2];
if (!serverDir) {
    console.error('usage: gen-node-vectors.mjs <former Node.js dedicated-server directory>');
    process.exit(2);
}
const load = (rel) => import(pathToFileURL(path.resolve(serverDir, rel)).href);
const { readPgn, parseSan, PgnError } = await load('src/chess/pgn.js');
const { Position, ChessGame, GameStatus, EndReason } = await load('src/chess/index.js');

// xorshift32 as a float in [0, 1) (the generator of the former PGN tests).
function rng(seed) {
    let x = seed >>> 0 || 1;
    return () => {
        x ^= x << 13; x >>>= 0;
        x ^= x >>> 17;
        x ^= x << 5; x >>>= 0;
        return x / 4294967296;
    };
}

// FNV-1a 32 of the UTF-8 bytes, as 8 hex digits.
function fnv1a(text) {
    let h = 0x811c9dc5;
    for (const b of Buffer.from(text, 'utf8')) h = Math.imul(h ^ b, 0x01000193) >>> 0;
    return h.toString(16).padStart(8, '0');
}

// The low 16 bits of the hash: enough to tell outcomes apart in a test, and compact.
const short = (text) => fnv1a(text).slice(4);

// A reader outcome as one line: the game (result, start, UCI moves, tags) or the error.
function describe(read) {
    try {
        const g = read();
        const p = g.startFen ? Position.fromFEN(g.startFen) : Position.start();
        const ucis = g.moves.map((m) => {
            const u = p.uci(m);
            p.play(m);
            return u;
        });
        const tags = g.tags.map(([k, v]) => `${k}\u0001${v}`).join('\u0002');
        return `ok ${g.result} ${g.startFen ?? '-'} ${ucis.join(',')} ${tags}`;
    } catch (e) {
        if (!(e instanceof PgnError)) throw e;
        return `err ${e.line}:${e.column} ${e.message}`;
    }
}

// The mutation of the former fuzz test: 1 to 6 deletions, insertions or replacements of items.
function mutate(items, alphabet, r) {
    const out = items.slice();
    const edits = 1 + Math.floor(r() * 6);
    for (let e = 0; e < edits; e++) {
        const at = Math.floor(r() * (out.length + 1));
        const c = alphabet[Math.floor(r() * alphabet.length)];
        const op = r();
        if (op < 0.4) out.splice(at, 1);
        else if (op < 0.7) out.splice(at, 0, c);
        else out[at] = c;
    }
    return out;
}

const SERVER_02 = readFileSync(new URL('../../../../../tests/data/server-pgn/02-resignation-castling.pgn', import.meta.url), 'utf8');
const FUZZ_ALPHABET = '[]{}()"\\;%$!?+-=*/.:0123456789 \n\rabcdefghKQRBNOxo#\u00bd\u2026\u0000\u00e9';

const UNICODE_BASE = '\ufeff[Event "Unicode \u2654 test \ud83d\ude00"]\r\n[White "J\u00f6rg \ud83d\ude00\ud83d\ude00"]\r\n'
    + '[Black "\u540d\u4eba"]\r\n[Result "0-1"]\r\n\r\n'
    + '1. e4 {\ud83d\ude00 smile \u2028 line} 1... e5 $2 2. \u2658f3 \u265ec6 ; \u00e9t\u00e9 \ud83d\ude00\r\n'
    + '3. Bb5 a6 (3... Nf6 4. O-O) 4. Bxc6 dxc6 5. O-O f6 6. d4 exd4 7. Nxd4 c5 8. Nb3 Qxd1 9. Rxd1 Bg4 '
    + '10. f3 Be6 11. Nc3 Bd6 12. Be3 b6 13. a4 O-O-O \u2026 0-1\r\n';
const UNICODE_ALPHABET = FUZZ_ALPHABET + '\ufeff\ud83d\ude00\u2658\t\u00a0\u2028\u0085\u3000\u2654\u265e';

const LIMITS_BASE = '[Event "limits"]\n[Site "a site"]\n[Date "2026.10.03"]\n[Round "1"]\n[White "w"]\n[Black "b"]\n'
    + '[Result "1-0"]\n\n1. e4 e5 (1... c5 2. Nf3 (2. c3 d5 (2... Nf6)) 2... d6) 2. Nf3 Nc6 3. Bc4 Bc5 '
    + '4. c3 Nf6 5. d4 exd4 6. cxd4 Bb4+ 7. Bd2 Bxd2+ 8. Nbxd2 d5 9. exd5 Nxd5 10. Qb3 Na5 1-0\n';
// The base fits these caps exactly; most mutations stay near them.
const LIMITS = { maxBytes: Buffer.byteLength(LIMITS_BASE) + 2, maxPlies: 20, maxTags: 7, maxTagName: 6, maxTagValue: 10,
    maxDepth: 3, maxToken: 6 };

const BYTES_BASE = Buffer.from('[White "J\u00f6rg \ud83d\ude00"]\n[Black "\u00bd"]\n1. e4 {\u00e9} e5 2. Nf3 \u2026 *\n', 'utf8');
const BYTE_ALPHABET = [0x00, 0x0a, 0x0d, 0x20, 0x22, 0x5b, 0x5d, 0x7b, 0x7d, 0x28, 0x29, 0x2e, 0x31, 0x65, 0x34,
    0x80, 0x9f, 0xa0, 0xa9, 0xbb, 0xbd, 0xbf, 0xc3, 0xe2, 0xef, 0xf0, 0xf4, 0xff];

function readerSet(name, seed, count, base, alphabet, limits, asBytes) {
    const r = rng(seed);
    const items = asBytes ? [...base] : [...base];
    const hashes = [];
    const samples = [];
    let games = 0;
    for (let n = 0; n < count; n++) {
        const mutated = mutate(items, alphabet, r);
        const input = asBytes ? Uint8Array.from(mutated) : mutated.join('');
        const line = describe(() => readPgn(input, limits ?? {}));
        hashes.push(short(line));
        if (line.startsWith('ok')) games++;
        if (n < 8) samples.push(line);
    }
    return { name, seed, count, games, limits: limits ?? null, hashes: hashes.join(''), samples };
}

const reader = [
    { ...readerSet('server-pgn 02', 7, 3000, SERVER_02, FUZZ_ALPHABET, null, false), base: 'server-pgn/02-resignation-castling.pgn', alphabet: FUZZ_ALPHABET },
    { ...readerSet('unicode', 11, 2000, UNICODE_BASE, [...UNICODE_ALPHABET], null, false), base: UNICODE_BASE, alphabet: UNICODE_ALPHABET },
    { ...readerSet('limits', 13, 1500, LIMITS_BASE, FUZZ_ALPHABET, LIMITS, false), base: LIMITS_BASE, alphabet: FUZZ_ALPHABET },
    { ...readerSet('bytes', 17, 1500, BYTES_BASE, BYTE_ALPHABET, null, true), base: [...BYTES_BASE], alphabet: BYTE_ALPHABET },
];
// The unmodified bases too.
const bases = [
    describe(() => readPgn(SERVER_02)), describe(() => readPgn(UNICODE_BASE)),
    describe(() => readPgn(LIMITS_BASE, LIMITS)), describe(() => readPgn(BYTES_BASE)),
];

// ---- Lenient SAN -----------------------------------------------------------------------------

const SAN_FENS = [
    'rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1',
    'r3k2r/p1ppqpb1/bn2pnp1/3PN3/1p2P3/2N2Q1p/PPPBBPPP/R3K2R w KQkq - 0 1',
    'r3k2r/Pppp1ppp/1b3nbN/nP6/BBP1P3/q4N2/Pp1P2PP/R2Q1RK1 w kq - 0 1',
    'rnbqkbnr/ppp1pppp/8/3p4/2PP4/8/PP2PPPP/RNBQKBNR b KQkq - 0 2',
    '4k3/1P6/8/2pP4/8/8/5p2/RN2K1NR w KQ c6 0 1',
    '1r2k3/2P5/8/8/8/2N3N1/8/R3K2R w KQ - 0 1',
];
const SAN_ALPHABET = [...'abcdefghKQRBNPkqrbnpx:-=+#!?/()12345678O0o.e \u00a0\u2658\u265e\u2654\u2655\ud83d\ude00'];

const san = SAN_FENS.map((fen, i) => {
    const r = rng(101 + i);
    const p = Position.fromFEN(fen);
    const legal = p.legalMoves();
    const forms = legal.flatMap((m) => [p.san(m), p.uci(m)]);
    const results = [];
    for (let n = 0; n < 1000; n++) {
        let text;
        if (n % 2 === 0) {
            const len = 1 + Math.floor(r() * 8);
            text = '';
            for (let k = 0; k < len; k++) text += SAN_ALPHABET[Math.floor(r() * SAN_ALPHABET.length)];
        } else {
            text = mutate([...forms[Math.floor(r() * forms.length)]], SAN_ALPHABET, r).join('');
        }
        const m = parseSan(p, text);
        results.push(m < 0 ? '-' : p.uci(m));
    }
    return { fen, seed: 101 + i, alphabet: SAN_ALPHABET.join(''), results: results.join(' ') };
});

// ---- Writer ----------------------------------------------------------------------------------

const GAME_FENS = [null, 'r3k2r/pppq1ppp/2npbn2/4p3/4P3/2NPBN2/PPPQ1PPP/R3K2R b KQkq - 4 9', '8/P6k/8/8/8/8/6Kp/8 w - - 0 60',
    '4k3/8/8/2pP4/8/8/8/4K3 w - c6 0 1', 'rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 5 1', '8/8/8/4k3/8/8/4K3/7R b - - 99 120'];
const LONG = 'x'.repeat(100);
const COMMENTS = [
    '\ud83d\ude00'.repeat(30) + ' after',
    'a } brace { and {more}',
    `${LONG} short`,
    ['[%clk 0:02:58.3]', '[%emt 0:00:01.7]'],
    ['', '  ', '{}'],
    '\u0007bell\u0000 tab\tin word \u0085next \u00a0nbsp\u00a0 \u3000ideo',
    ['\ud83d\ude00 \ud83d\ude00 \ud83d\ude00 \ud83d\ude00 \ud83d\ude00 \ud83d\ude00 \ud83d\ude00 \ud83d\ude00 \ud83d\ude00 \ud83d\ude00 \ud83d\ude00 \ud83d\ude00 \ud83d\ude00 \ud83d\ude00 \ud83d\ude00 \ud83d\ude00 \ud83d\ude00 \ud83d\ude00 \ud83d\ude00 \ud83d\ude00'],
    null,
    '',
    'line\nbreak\r\nand \u2028 separator',
    ['\u00e9t\u00e9', 'na\u00efve \u540d\u4eba'],
];
const TAG_SETS = [
    { white: 'J\u00f6rg \ud83d\ude00', black: '\u540d\u4eba', event: 'Line\nbreak "q" \\ x\r', site: '', date: '2026.10.03', round: '3.1',
        timeControl: '', afterResult: [['WhiteElo', '\ud83d\ude00'], ['UTCTime', '12:00:00']], extra: [['Annotator', 'a\rb'], ['PlyCount', '0']] },
    { white: 'alice', black: 'deleted#42', date: '2026.01.02', timeControl: '180+2' },
    { event: '', round: '', white: '', black: '', date: '1999.12.31', extra: [['Empty', '']] },
    { date: '2026.10.03', site: '\u2028sep\u2029', afterResult: [['A', '"'], ['B', '\\']] },
];

const writer = [];
{
    const r = rng(99);
    for (let n = 0; n < 42; n++) {
        const fen = GAME_FENS[n % GAME_FENS.length];
        const g = new ChessGame(fen ?? undefined);
        const plies = Math.floor(r() * 120);
        while (g.ply < plies && !g.isOver) {
            const legal = g.position.legalMoves();
            g.play(legal[Math.floor(r() * legal.length)]);
        }
        const actions = ['resign', 'agreeDraw', 'abort', 'flagFall', 'abandon', 'none', 'claim'];
        const action = actions[n % actions.length];
        if (!g.isOver) {
            if (action === 'resign') g.resign(g.position.side);
            else if (action === 'agreeDraw') g.agreeDraw();
            else if (action === 'abort') g.end(GameStatus.Aborted, EndReason.Aborted);
            else if (action === 'flagFall') g.flagFall(g.position.side);
            else if (action === 'abandon') g.end(GameStatus.WhiteWins, EndReason.Abandonment);
            else if (action === 'claim') g.claimDraw();
        }
        // Ply i gets COMMENTS[(i + n) % COMMENTS.length]; game n the tags TAG_SETS[n % TAG_SETS.length].
        const comments = g.moves.map((_, i) => COMMENTS[(i + n) % COMMENTS.length]);
        const text = g.pgn({ ...TAG_SETS[n % TAG_SETS.length], comments });
        writer.push({ start: fen, uci: g.uciMoves().join(' '), action, status: g.status, reason: g.reason,
            pgnHash: fnv1a(text), pgn: n < 6 ? text : undefined });
    }
}

const out = {
    generator: 'dedicated-server/crates/chess/tests/fixtures/gen-node-vectors.mjs (the Node.js server of 46d51dd^)',
    reader,
    bases,
    san,
    comments: COMMENTS,
    tagSets: TAG_SETS,
    writer,
};
writeFileSync(new URL('node-vectors.json', import.meta.url), JSON.stringify(out) + '\n');
console.log(`reader sets ${reader.length}, san positions ${san.length}, games ${writer.length}`);
