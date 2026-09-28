// Cross-check of the server rules against the game's own C++ rules: replays
// test/fixtures/chess-crosscheck.json (tools/gen-chess-crosscheck.sh) and requires every FEN,
// digest, legal move list, SAN, move flag, draw/claim predicate and final result to match.
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';

import { Position, ChessGame, GameStatus } from '../../src/chess/index.js';

const fixture = JSON.parse(readFileSync(fileURLToPath(new URL('../fixtures/chess-crosscheck.json', import.meta.url)), 'utf8'));

function fnv1a32(s) {
    let h = 0x811c9dc5;
    for (let i = 0; i < s.length; i++) {
        h ^= s.charCodeAt(i) & 0xff;
        h = Math.imul(h, 0x01000193) >>> 0;
    }
    return h >>> 0;
}

function decodeLegal(b64) {
    const buf = Buffer.from(b64, 'base64');
    const out = [];
    for (let i = 0; i + 1 < buf.length; i += 2) out.push(buf[i] | (buf[i + 1] << 8));
    return out;
}

function sortedLegal(pos) {
    return pos.legalMoves().sort((a, b) => a - b);
}

function describe(list, pos) {
    return list.map((m) => pos.uci(m)).join(' ');
}

/** Info bits as written by the generator. */
function infoOf(game) {
    const p = game.position;
    let info = 0;
    if (p.inCheck()) info |= 1;
    if (p.hasInsufficientMaterial()) info |= 2;
    if (p.canColorMate(0)) info |= 4;
    if (p.canColorMate(1)) info |= 8;
    if (game.canClaimThreefold()) info |= 16;
    if (game.canClaimFiftyMove()) info |= 32;
    info |= game.repetitionCount() << 8;
    return info;
}

/**
 * isLegal() over candidate u16 values must accept exactly the legal list. full: every from/to
 * pair with promo 0, plus promo 1..7 from every own pawn; otherwise from the side's pieces only.
 */
function checkIsLegalSet(pos, legal, full, where) {
    const legalSet = new Set(legal);
    let accepted = 0;
    for (let from = 0; from < 64; from++) {
        const pc = pos.pieceAt(from);
        const own = pc !== 0 && (pc >> 3) === pos.side;
        if (!full && !own) continue;
        const pawn = own && (pc & 7) === 1;
        const pawnNearEnd = pawn && ((from >> 3) === (pos.side === 0 ? 6 : 1));
        for (let to = 0; to < 64; to++) {
            const maxPromo = (full && pawn) || pawnNearEnd ? 7 : 0;
            for (let promo = 0; promo <= maxPromo; promo++) {
                const m = from | (to << 6) | (promo << 12);
                const ok = pos.isLegal(m);
                if (ok !== legalSet.has(m)) {
                    assert.fail(`${where}: isLegal(${pos.uci(m)} = ${m}) = ${ok} in ${pos.fen()}`);
                }
                if (ok) accepted++;
            }
        }
    }
    assert.equal(accepted, legal.length, `${where}: isLegal accepted count`);
    // Values that are never moves.
    for (const bad of [-1, 0x8000 | legal[0], 1.5, NaN, '12', null, undefined, 0x10000]) {
        assert.equal(pos.isLegal(bad), false, `${where}: isLegal(${bad})`);
    }
}

function checkPosition(game, fen, digest, legalB64, where) {
    const pos = game.position;
    assert.equal(pos.fen(), fen, `${where}: fen`);
    assert.equal(pos.digest(), digest, `${where}: digest`);
    assert.equal(pos.digest(), fnv1a32(fen.split(' ').slice(0, 4).join(' ')), `${where}: digest vs fen`);
    const expected = decodeLegal(legalB64);
    const got = sortedLegal(pos);
    if (got.length !== expected.length || got.some((m, i) => m !== expected[i])) {
        assert.fail(`${where}: legal moves in ${fen}\n  game:   ${describe(expected, pos)}\n  server: ${describe(got, pos)}`);
    }
    // FEN round trip reproduces the same position and repetition key.
    const again = Position.fromFEN(fen);
    assert.ok(again, `${where}: reparse`);
    assert.equal(again.fen(), fen);
    assert.equal(again.repetitionKey(), pos.repetitionKey(), `${where}: incremental vs fresh key`);
    assert.equal(again.digest(), digest);
    return got;
}

test('crosscheck: FEN parsing and normalisation match chess::Position::setFEN', () => {
    assert.ok(fixture.fens.length > 50);
    for (const [input, fen] of fixture.fens) {
        const p = Position.fromFEN(input);
        if (fen === null) assert.equal(p, null, `refused by the game: ${JSON.stringify(input)}`);
        else {
            assert.ok(p, `accepted by the game: ${JSON.stringify(input)}`);
            assert.equal(p.fen(), fen, JSON.stringify(input));
        }
    }
});

test('crosscheck: SAN of every legal move of hand-picked positions', () => {
    assert.ok(fixture.san.length > 20);
    for (const { fen, moves } of fixture.san) {
        const p = Position.fromFEN(fen);
        assert.ok(p, fen);
        assert.deepEqual(sortedLegal(p), moves.map(([m]) => m), `legal moves of ${fen}`);
        for (const [m, san] of moves) {
            assert.equal(p.san(m), san, `${fen} ${p.uci(m)}`);
            assert.equal(p.parseUCI(p.uci(m)), m);
        }
        assert.equal(p.fen(), fen, 'san() leaves the position unchanged');
    }
});

test('crosscheck: games replayed move by move', () => {
    assert.ok(fixture.games.length >= 250, 'a few hundred games');
    let plies = 0, fullScans = 0, pgns = 0;
    for (let gi = 0; gi < fixture.games.length; gi++) {
        const g = fixture.games[gi];
        const game = new ChessGame(g.start ?? undefined);
        for (let i = 0; i < g.plies.length; i++) {
            const [fen, digest, legalB64, move, san, flags, info] = g.plies[i];
            const where = `game ${gi} ply ${i}`;
            assert.equal(game.status, GameStatus.Ongoing, `${where}: status`);
            const legal = checkPosition(game, fen, digest, legalB64, where);
            assert.equal(infoOf(game), info, `${where}: info bits in ${fen}`);
            const full = (plies + i) % 16 === 0;
            if (full) fullScans++;
            checkIsLegalSet(game.position, legal, full, where);
            assert.equal(game.position.san(move), san, `${where}: SAN in ${fen}`);
            assert.equal(game.position.parseUCI(game.position.uci(move)), move);
            const r = game.play(move);
            assert.equal(r.ok, true, `${where}: play`);
            assert.equal(r.flags, flags, `${where}: flags of ${san} in ${fen}`);
            assert.equal(r.status, game.status);
            assert.equal(r.reason, game.reason);
        }
        plies += g.plies.length;
        const [fen, digest, legalB64, info] = g.final;
        const where = `game ${gi} final`;
        checkPosition(game, fen, digest, legalB64, where);
        assert.equal(infoOf(game), info, `${where}: info bits in ${fen}`);
        assert.equal(game.status, g.status, `${where}: status in ${fen}`);
        assert.equal(game.reason, g.reason, `${where}: reason in ${fen}`);
        assert.equal(game.position.repetitionKey(), g.hash, `${where}: Zobrist key equals chess::Position::hash()`);
        assert.deepEqual(game.sanMoves(), g.plies.map((p) => p[4]));
        if (g.pgn) {
            assert.equal(game.pgn(fixture.pgnTags), g.pgn, `${where}: pgn`);
            pgns++;
        }
        // The same game rebuilt from its move list (journal replay).
        const replay = ChessGame.fromMoves(g.start ?? undefined, game.moves);
        assert.ok(replay);
        assert.equal(replay.position.fen(), fen);
        assert.equal(replay.status, g.status);
    }
    assert.ok(plies > 10000, `plies checked: ${plies}`);
    assert.ok(fullScans > 500);
    assert.ok(pgns > 20);
});

test('crosscheck: the fixture exercises the special rules', () => {
    const seen = { ep: 0, castleK: 0, castleQ: 0, underPromo: 0, promo: 0, mate: 0, stalemate: 0, fivefold: 0, seventyFive: 0, dead: 0, rep3: 0, fifty: 0 };
    for (const g of fixture.games) {
        for (const p of g.plies) {
            const f = p[5];
            if (f & 2) seen.ep++;
            if (f & 4) seen.castleK++;
            if (f & 8) seen.castleQ++;
            if (f & 32) {
                seen.promo++;
                if ((p[3] >> 12) !== 5) seen.underPromo++;
            }
            if ((p[6] >> 8) >= 3) seen.rep3++;
            if (p[6] & 32) seen.fifty++;
        }
        if (g.reason === 1) seen.mate++;
        if (g.reason === 5) seen.stalemate++;
        if (g.reason === 6) seen.dead++;
        if (g.reason === 8) seen.fivefold++;
        if (g.reason === 9) seen.seventyFive++;
    }
    for (const [k, v] of Object.entries(seen)) assert.ok(v >= 3, `${k}: ${v}`);
});
