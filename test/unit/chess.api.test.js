// Contract of src/chess/index.js: enums equal to the protocol schema, u16 moves, digest, speed.
import { test } from 'node:test';
import assert from 'node:assert/strict';

import * as chess from '../../src/chess/index.js';

const { Position, ChessGame, GameStatus, EndReason, MoveFlag, PieceType, WHITE, BLACK } = chess;

async function importOptional(rel) {
    try {
        return await import(new URL(rel, import.meta.url));
    } catch (e) {
        if (e && e.code === 'ERR_MODULE_NOT_FOUND') return null;
        throw e;
    }
}

test('enums are the protocol schema values', async (t) => {
    assert.equal(WHITE, 0);
    assert.equal(BLACK, 1);
    assert.deepEqual({ ...PieceType }, { None: 0, Pawn: 1, Knight: 2, Bishop: 3, Rook: 4, Queen: 5, King: 6 });
    const schema = await importOptional('../../src/protocol/schema.js');
    if (!schema) {
        t.skip('src/protocol/schema.js not present in this checkout');
        return;
    }
    assert.deepEqual({ ...GameStatus }, schema.enums.GameStatus);
    assert.deepEqual({ ...EndReason }, schema.enums.EndReason);
    assert.deepEqual({ ...MoveFlag }, schema.MoveFlag);
});

test('digest is the protocol posHash (fnv1a32 of the first four FEN fields)', async () => {
    const protocol = await importOptional('../../src/protocol/index.js');
    const fnv = protocol?.fnv1a32 ?? ((s) => {
        let h = 0x811c9dc5;
        for (let i = 0; i < s.length; i++) h = Math.imul(h ^ (s.charCodeAt(i) & 0xff), 0x01000193) >>> 0;
        return h >>> 0;
    });
    const g = new ChessGame();
    for (let i = 0; i < 60 && !g.isOver; i++) {
        const p = g.position;
        assert.equal(p.digest(), fnv(p.fen().split(' ').slice(0, 4).join(' ')));
        assert.equal(p.digest(), p.digest());   // cached value
        const legal = p.legalMoves().sort((a, b) => a - b);
        g.play(legal[(i * 31) % legal.length]);
    }
    if (protocol?.encodeMove) {
        const p = Position.start();
        assert.ok(p.isLegal(protocol.encodeMove(12, 28, 0)));
        const q = Position.fromFEN('8/4P3/8/8/8/8/k7/4K3 w - - 0 1');
        assert.ok(q.isLegal(protocol.encodeMove(52, 60, PieceType.Knight)));
    }
});

test('moves are u16: from | to << 6 | promo << 12, castling as the king move', () => {
    const p = Position.fromFEN('r3k2r/8/8/8/8/8/8/R3K2R w KQkq - 0 1');
    const legal = new Set(p.legalMoves());
    assert.ok(legal.has(4 | (6 << 6)));      // e1g1
    assert.ok(legal.has(4 | (2 << 6)));      // e1c1
    assert.ok(!legal.has(4 | (7 << 6)));     // e1h1 is not castling
    const promo = Position.fromFEN('8/4P3/8/8/8/8/k7/4K3 w - - 0 1');
    const promos = promo.legalMoves().filter((m) => (m & 63) === 52).sort((a, b) => a - b);
    assert.deepEqual(promos, [2, 3, 4, 5].map((t) => 52 | (60 << 6) | (t << 12)));
    for (const m of [...legal, ...promos]) assert.ok(m >= 0 && m <= 0x7fff);
});

test('play() applies only legal moves and reports MoveFlag bits', () => {
    const p = Position.start();
    const before = p.fen();
    assert.throws(() => p.play(0));
    assert.throws(() => p.play(12 | (28 << 6) | (5 << 12)));
    assert.equal(p.fen(), before);
    assert.equal(p.play(12 | (28 << 6)), MoveFlag.DoublePush);
    const g = new ChessGame();
    const r = g.play(12 | (28 << 6));
    assert.deepEqual(r, { ok: true, flags: MoveFlag.DoublePush, status: GameStatus.Ongoing, reason: EndReason.None });
});

test('clone() is independent', () => {
    const p = Position.start();
    const q = p.clone();
    q.play(12 | (28 << 6));
    assert.equal(p.fen(), 'rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1');
    assert.notEqual(p.repetitionKey(), q.repetitionKey());
    assert.notEqual(p.digest(), q.digest());
});

test('speed: isLegal + play well under 20 us per move', () => {
    // Deterministic pseudo-random games, replayed as the server does: isLegal, then play.
    const games = [];
    let seed = 12345;
    const rnd = (n) => {
        seed = (Math.imul(seed, 1103515245) + 12345) >>> 0;
        return (seed >>> 8) % n;
    };
    for (let i = 0; i < 40; i++) {
        const g = new ChessGame();
        while (!g.isOver && g.ply < 200) {
            const legal = g.position.legalMoves();
            g.play(legal[rnd(legal.length)]);
        }
        games.push(g.moves.slice());
    }
    let moves = 0;
    const run = () => {
        for (const list of games) {
            const g = new ChessGame();
            for (const m of list) {
                if (!g.position.isLegal(m)) throw new Error('replay');
                g.play(m);
                moves++;
            }
        }
    };
    run();  // warm-up
    moves = 0;
    const t0 = performance.now();
    run();
    run();
    const us = ((performance.now() - t0) * 1000) / moves;
    console.log(`ChessGame isLegal + play: ${us.toFixed(2)} us per move over ${moves} moves`);
    assert.ok(us < 20, `${us} us per move`);
});
