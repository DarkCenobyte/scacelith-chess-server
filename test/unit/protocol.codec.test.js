// Generated codec: API surface (docs/DESIGN.md 5.1), encode-side validation, decode edge cases and
// the hand-written helpers of src/protocol/index.js.

import { test } from 'node:test';
import assert from 'node:assert/strict';
import * as gen from '../../src/protocol/codec.gen.js';
import * as P from '../../src/protocol/index.js';
import * as schema from '../../src/protocol/schema.js';
import { computeSchemaHash } from '../../src/protocol/schema-hash.js';

const isPE = (reason) => (e) => e instanceof P.ProtocolError && (reason === undefined || e.reason === reason);
const GAME = 123456789012345;

test('exports exactly the documented API', () => {
    assert.deepEqual(Object.keys(gen).sort(), ['CloseCode', 'MSG', 'MoveFlag', 'PROTOCOL_MIN', 'PROTOCOL_VERSION', 'ProtocolError',
        'SCHEMA_HASH', 'WS_SUBPROTOCOL', 'decode', 'encode', 'enums', 'isClientType', 'messageName'].sort());
    for (const k of ['encodeMove', 'decodeMove', 'fnv1a32', 'fenDigest', 'moveToUci', 'uciToMove', 'peekType', 'DECODE_C2S', 'DECODE_S2C']) {
        assert.ok(k in P, `index.js exports ${k}`);
    }
});

test('constants come from the schema', () => {
    assert.equal(P.PROTOCOL_VERSION, schema.PROTOCOL_VERSION);
    assert.equal(P.PROTOCOL_MIN, schema.PROTOCOL_MIN);
    assert.equal(P.WS_SUBPROTOCOL, 'scacelith.v1');
    assert.equal(P.SCHEMA_HASH, computeSchemaHash());
    assert.deepEqual(P.enums, schema.enums);
    assert.deepEqual(P.MoveFlag, schema.MoveFlag);
    assert.deepEqual(P.CloseCode, schema.CloseCode);
    assert.ok(Object.isFrozen(P.enums) && Object.isFrozen(P.enums.ErrorCode) && Object.isFrozen(P.MSG) && Object.isFrozen(P.encode));
});

test('MSG ids, shared names and messageName', () => {
    assert.equal(P.MSG.Move, 0x20);
    assert.equal(P.MSG.C_Ping, 0x02);
    assert.equal(P.MSG.S_Ping, 0x82);
    assert.equal(P.MSG.C_Pong, 0x03);
    assert.equal(P.MSG.S_Pong, 0x83);
    assert.equal(P.MSG.Ping, undefined);
    for (const m of schema.messages) {
        const key = ['Ping', 'Pong'].includes(m.name) ? (m.dir === 'c2s' ? 'C_' : 'S_') + m.name : m.name;
        assert.equal(P.MSG[key], m.id);
        assert.equal(P.messageName(m.id), key);
        assert.equal(typeof P.encode[key], 'function');
    }
    assert.equal(P.messageName(0), null);
    assert.equal(P.messageName(0x04), null);
    assert.equal(P.messageName(300), null);
    assert.equal(P.messageName('32'), null);
    assert.equal(P.messageName(undefined), null);
    assert.equal(P.isClientType(0x01), true);
    assert.equal(P.isClientType(0x7f), true);
    assert.equal(P.isClientType(0x80), false);
    assert.equal(P.isClientType(0), false);
});

test('DESIGN.md example: MoveMade encode/decode', () => {
    const f = { game: GAME, gseq: 3, ply: 2, move: P.encodeMove(6, 21), flags: 0, spentMs: 0, whiteMs: 180000, blackMs: 180000, serverTime: 1790000000000.5, drawOffer: false, firstMoveMs: 30000 };
    const buf = P.encode.MoveMade(f);
    assert.ok(Buffer.isBuffer(buf));
    assert.equal(buf.length, 43);
    assert.deepEqual(P.decode(buf), { type: 0xa1, ...f });
    assert.deepEqual(P.decode(buf, P.DECODE_S2C), { type: 0xa1, ...f });
    assert.throws(() => P.decode(buf, P.DECODE_C2S), isPE('wrong direction'));
});

test('encode validates ranges, bounds, enums, strings and lists', () => {
    const move = { seq: 1, game: GAME, ply: 0, move: 1804, posHash: 1, thinkMs: 0, drawOffer: false };
    assert.throws(() => P.encode.Move({ ...move, seq: -1 }), isPE('seq out of range'));
    assert.throws(() => P.encode.Move({ ...move, seq: 2 ** 32 }), isPE('seq out of range'));
    assert.throws(() => P.encode.Move({ ...move, seq: 1.5 }), isPE('seq out of range'));
    assert.throws(() => P.encode.Move({ ...move, seq: '1' }), isPE('seq out of range'));
    assert.throws(() => P.encode.Move({ ...move, seq: undefined }), isPE('seq out of range'));
    assert.throws(() => P.encode.Move({ ...move, seq: NaN }), isPE('seq out of range'));
    assert.throws(() => P.encode.Move({ ...move, ply: 1200 }), isPE('ply above max'));
    assert.throws(() => P.encode.Move({ ...move, move: 0x8000 }), isPE('move above max'));
    assert.throws(() => P.encode.Move({ ...move, game: 2 ** 53 }), isPE('game not an id53'));
    assert.throws(() => P.encode.Move({ ...move, game: -1 }), isPE('game not an id53'));
    assert.throws(() => P.encode.Move({ ...move, game: 1.5 }), isPE('game not an id53'));
    assert.throws(() => P.encode.ChallengeCreate({ seq: 1, target: '', baseSec: 14, incSec: 0, rated: false, color: 0 }), isPE('baseSec below min'));
    assert.throws(() => P.encode.ChallengeCreate({ seq: 1, target: '', baseSec: 60, incSec: 181, rated: false, color: 0 }), isPE('incSec above max'));
    assert.throws(() => P.encode.ChallengeCreate({ seq: 1, target: '', baseSec: 60, incSec: 0, rated: false, color: 3 }), isPE('color not a ColorPref'));
    assert.throws(() => P.encode.Error({ ref: 1, code: 0, fatal: false, game: 0 }), isPE('code not a ErrorCode'));
    assert.throws(() => P.encode.Error({ ref: 1, code: 300, fatal: false, game: 0 }), isPE('code not a ErrorCode'));
    assert.throws(() => P.encode.Error({ ref: 1, code: '1', fatal: false, game: 0 }), isPE('code not a ErrorCode'));
    assert.throws(() => P.encode.GameEvent({ game: 1, gseq: 1, kind: 0, color: 0, arg: 0 }), isPE('kind not a GameEventKind'));
    assert.throws(() => P.encode.QueueJoin({ seq: 1, category: '3+', rated: true }), isPE('category bad length'));
    assert.throws(() => P.encode.QueueJoin({ seq: 1, category: '12345678', rated: true }), isPE('category bad length'));
    assert.throws(() => P.encode.QueueJoin({ seq: 1, category: 'ab\0c', rated: true }), isPE('category contains NUL'));
    assert.throws(() => P.encode.QueueJoin({ seq: 1, category: 32, rated: true }), isPE('category not a string'));
    assert.throws(() => P.encode.Welcome({ proto: 1, serverTime: 0, userId: 1, username: 'ユキユキユキユキa', serverName: '', heartbeatMs: 1, maxMsgPerSec: 1, activeGame: 0 }), isPE('username bad length'));
    assert.throws(() => P.encode.S_Ping({ nonce: 1, serverTime: Infinity }), isPE('serverTime not finite'));
    assert.throws(() => P.encode.Notice({ code: 1, arg: -Infinity }), isPE('arg not finite'));
    const snap = { game: 1, gseq: 1, category: '3+2', baseMs: 1, incMs: 0, rated: false, white: { userId: 1, name: 'a', rating: 1, provisional: false },
        black: { userId: 2, name: 'b', rating: 1, provisional: false }, you: 0, moves: [], running: 2, whiteMs: 0, blackMs: 0, serverTime: 0,
        drawOffer: 2, status: 0, reason: 0, whiteConnected: true, blackConnected: true, graceMs: 0, firstMoveMs: 0, startedAt: 0, rematch: 2 };
    assert.equal(P.decode(P.encode.GameSnapshot(snap)).white.name, 'a');
    assert.throws(() => P.encode.GameSnapshot({ ...snap, moves: new Array(1201).fill({ move: 0, spentMs: 0, clockMs: 0 }) }), isPE('moves too long'));
    assert.throws(() => P.encode.GameSnapshot({ ...snap, moves: 'e2e4' }), isPE('moves not an array'));
    assert.throws(() => P.encode.GameSnapshot({ ...snap, moves: [{ move: 0x8000, spentMs: 0, clockMs: 0 }] }), isPE('moves.move above max'));
    assert.throws(() => P.encode.GameSnapshot({ ...snap, moves: [{ move: 1, spentMs: -1, clockMs: 0 }] }), isPE('moves.spentMs out of range'));
    assert.throws(() => P.encode.GameSnapshot({ ...snap, white: { ...snap.white, name: '' } }), isPE('white.name bad length'));
    assert.throws(() => P.encode.GameSnapshot({ ...snap, black: undefined }), (e) => e instanceof P.ProtocolError && /^black\./.test(e.reason));
    assert.throws(() => P.encode.GameSnapshot({ ...snap, you: 3 }), isPE('you not a Color'));
    assert.throws(() => P.encode.Ack(null), isPE());
    assert.throws(() => P.encode.Ack(7), isPE());
});

test('encode keeps the placeholder defaults for omitted bool, f64, id53, string and list fields', () => {
    const e = P.decode(P.encode.Error({ ref: 4, code: P.enums.ErrorCode.RateLimited }));
    assert.deepEqual(e, { type: P.MSG.Error, ref: 4, code: 5, fatal: false, game: 0 });
    const n = P.decode(P.encode.Notice({ code: P.enums.NoticeCode.SessionRevoked }));
    assert.equal(n.arg, 0);
    const q = P.decode(P.encode.QueueStatus({ rated: 1, state: 1, waitMs: 0, window: 0, queued: 0 }));
    assert.equal(q.category, '');
    assert.equal(q.rated, true);
    const f = P.decode(P.encode.S_Pong({ nonce: 1, serverTime: '12.5' }));
    assert.equal(f.serverTime, 12.5);
    assert.ok(Object.is(P.decode(P.encode.S_Pong({ nonce: 1, serverTime: -0 })).serverTime, 0));
});

test('strings: UTF-8 round trip, BOM kept, lone surrogates become U+FFFD', () => {
    for (const s of ['Łukasz', 'ユキ', 'مُحَمَّد', '🐴♞', '﻿bom', 'aÿb']) {
        const b = P.encode.Welcome({ proto: 1, serverTime: 0, userId: 1, username: s, serverName: s, heartbeatMs: 1, maxMsgPerSec: 1, activeGame: 0 });
        const m = P.decode(b);
        assert.equal(m.username, s);
        assert.equal(m.serverName, s);
        assert.equal(b[15], Buffer.byteLength(s));
    }
    const lone = P.decode(P.encode.Ack({ ref: 1 }));
    assert.equal(lone.ref, 1);
    const b = P.encode.QueueStatus({ category: 'a\ud800b', rated: false, state: 0, waitMs: 0, window: 0, queued: 0 });
    assert.equal(b[1], 5);
    assert.equal(P.decode(b).category, 'a�b');
});

test('decode never throws anything but ProtocolError on hostile inputs', () => {
    for (const x of [undefined, null, 0, '', 'abc', 42, {}, [], [0x20], new Uint16Array(4), new DataView(new ArrayBuffer(4)), Buffer.alloc(0), new ArrayBuffer(0)]) {
        assert.throws(() => P.decode(x), isPE(), String(x));
    }
    assert.throws(() => P.decode(Buffer.alloc(0)), isPE('empty'));
    assert.throws(() => P.decode(null), isPE('empty'));
    assert.throws(() => P.decode('abc'), isPE('not a buffer'));
    // Options: null / unknown direction.
    const ack = P.encode.Ack({ ref: 1 });
    assert.equal(P.decode(ack, null).ref, 1);
    assert.equal(P.decode(ack, {}).ref, 1);
    assert.throws(() => P.decode(ack, { dir: 'sideways' }), isPE('wrong direction'));
});

test('decode errors carry no stack frames, encode errors keep theirs', () => {
    let d = null, e = null;
    try { P.decode(Buffer.from([0x20])); } catch (x) { d = x; }
    try { P.encode.Ack({ ref: -1 }); } catch (x) { e = x; }
    assert.ok(d instanceof P.ProtocolError && d instanceof Error);
    assert.equal(d.name, 'ProtocolError');
    assert.ok(!/\n\s+at /.test(d.stack));
    assert.ok(/\n\s+at /.test(e.stack));
    assert.ok(Error.stackTraceLimit > 0, 'the global stack limit is restored');
});

test('decoded integers, id53 and f64 are exact', () => {
    const cases = [0, 1, 0xffffffff, 0x100000000, 2 ** 52 + 1, Number.MAX_SAFE_INTEGER];
    for (const game of cases) assert.equal(P.decode(P.encode.Resign({ seq: 1, game })).game, game);
    for (const t of [0, 0.1, -1.5, 1790000000123.5, Number.MAX_VALUE, Number.MIN_VALUE, -Number.MAX_VALUE, 2 ** -1074]) {
        assert.equal(P.decode(P.encode.S_Ping({ nonce: 1, serverTime: t })).serverTime, t);
    }
});

test('helpers: moves, UCI, FNV-1a, FEN digest, peekType', () => {
    assert.equal(P.encodeMove(12, 28), 1804);
    assert.equal(P.encodeMove(52, 60, 5), 0x5f34);
    assert.deepEqual(P.decodeMove(0x5f34), { from: 52, to: 60, promo: 5 });
    assert.equal(P.moveToUci(1804), 'e2e4');
    assert.equal(P.moveToUci(0x5f34), 'e7e8q');
    assert.equal(P.moveToUci(P.encodeMove(4, 6)), 'e1g1');
    assert.equal(P.uciToMove('e2e4'), 1804);
    assert.equal(P.uciToMove('E7E8Q'), 0x5f34);
    assert.equal(P.uciToMove('a7a8n'), P.encodeMove(48, 56, 2));
    assert.equal(P.uciToMove('e2e9'), -1);
    assert.equal(P.uciToMove('e2e4k'), -1);
    for (let m = 0; m < 0x6000; m += 7) {
        const { promo } = P.decodeMove(m);
        if (promo === 1 || promo > 5) continue;
        assert.equal(P.uciToMove(P.moveToUci(m)), m);
    }
    // FNV-1a 32 reference values.
    assert.equal(P.fnv1a32(''), 0x811c9dc5);
    assert.equal(P.fnv1a32('a'), 0xe40c292c);
    assert.equal(P.fnv1a32('foobar'), 0xbf9cf968);
    const start = 'rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq -';
    assert.equal(P.fenDigest(start + ' 0 1'), P.fnv1a32(start));
    assert.equal(P.fenDigest('  ' + start.replace(/ /g, '   ') + ' 0 1 '), P.fnv1a32(start));
    assert.equal(P.peekType(P.encode.Ack({ ref: 1 })), P.MSG.Ack);
    assert.equal(P.peekType(Buffer.alloc(0)), -1);
});
