#!/usr/bin/env node
// Golden protocol vectors: test/fixtures/protocol-vectors.json, read by the JS tests
// (test/unit/protocol.*.test.js) and by the game client's C++ tests (tests/net_tests.cpp).
//
//   node tools/gen-protocol-vectors.js           writes the file
//   node tools/gen-protocol-vectors.js --check   exits 1 when the committed file is stale
//
// (`npm run gen:protocol` runs it after regenerating the codec.)
//
// File format (self-describing, deterministic):
//   valid[]:     { name, type, dir, note, fields, hex }
//                name = the message name of encode.<name> / the C++ struct (C_/S_ prefixes for
//                the names of both directions: Ping, Pong, Gesture), type = its id, fields =
//                every field by its schema name (enums as numbers, bools as true/false, id53 and
//                f64 as JSON numbers, structs as objects, lists as arrays), hex = the exact
//                encoding (lower-case). Decoding hex gives { type, ...fields } and encoding
//                fields gives hex.
//   malformed[]: { name, type, dir, note, reason, hex }
//                bytes that a decoder receiving them in direction `dir` must refuse; name/type
//                describe the type byte (null when unknown or empty); reason is the JS codec's
//                ProtocolError reason (each entry has exactly one defect, so every correct
//                decoder refuses it, whatever order it checks in).

import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import * as codec from '../src/protocol/codec.gen.js';
import * as schema from '../src/protocol/schema.js';

const here = path.dirname(fileURLToPath(import.meta.url));
export const VECTORS_PATH = path.resolve(here, '../test/fixtures/protocol-vectors.json');

const { encode, decode, MSG, enums, ProtocolError, SCHEMA_HASH } = codec;
const E = enums.ErrorCode;
const C = enums.Color;

// ---- deterministic helpers -----------------------------------------------------------------------

function mulberry32(seed) {
    let a = seed >>> 0;
    return () => {
        a = (a + 0x6d2b79f5) >>> 0;
        let t = a;
        t = Math.imul(t ^ (t >>> 15), t | 1);
        t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
        return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
    };
}
const sq = (s) => (s.charCodeAt(0) - 97) + 8 * (s.charCodeAt(1) - 49);
const mv = (uci) => sq(uci.slice(0, 2)) | (sq(uci.slice(2, 4)) << 6) | ((({ n: 2, b: 3, r: 4, q: 5 })[uci[4]] || 0) << 12);
function fnv1a32(s) {
    let h = 0x811c9dc5;
    for (let i = 0; i < s.length; i++) { h ^= s.charCodeAt(i) & 0xff; h = Math.imul(h, 0x01000193) >>> 0; }
    return h >>> 0;
}
const START_HASH = fnv1a32('rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq -');

// A realistic game id (src/util/ids.js layout: ms since 2026-01-01 << 12 | shard << 6 | seq).
const GAME = (Date.UTC(2026, 8, 28, 12, 0, 0) - Date.UTC(2026, 0, 1)) * 4096 + 3 * 64 + 5;
const T0 = 1790000000123.5;               // a server wall-clock time (epoch ms, with a fraction)
const TOKEN = 'sct_' + Buffer.alloc(32, 0).map((_, i) => (i * 37 + 11) & 0xff).toString('base64url');
const MAX53 = Number.MAX_SAFE_INTEGER;    // 2^53 - 1

const player = (userId, name, rating, provisional = false) => ({ userId, name, rating, provisional });
const RUY = ['e2e4', 'e7e5', 'g1f3', 'b8c6', 'f1b5', 'a7a6', 'b5a4', 'g8f6', 'e1g1', 'f8e7'].map(mv);

function moveList(n, seed) {
    const rnd = mulberry32(seed);
    const out = [];
    let white = 180000, black = 180000;
    for (let i = 0; i < n; i++) {
        const from = Math.floor(rnd() * 64), to = Math.floor(rnd() * 64);
        const promo = rnd() < 0.02 ? 2 + Math.floor(rnd() * 4) : 0;
        const spentMs = i < 2 ? 0 : Math.floor(rnd() * 9000);
        const clockMs = i % 2 === 0 ? (white = Math.max(0, white - spentMs + 2000)) : (black = Math.max(0, black - spentMs + 2000));
        out.push({ move: from | (to << 6) | (promo << 12), spentMs, clockMs });
    }
    return out;
}

function snapshot(over = {}) {
    let white = 180000, black = 180000;
    const moves = RUY.map((move, i) => {
        const spentMs = i < 2 ? 0 : 1500 + i * 311;
        const clockMs = i % 2 === 0 ? (white = white - spentMs + (i < 2 ? 0 : 2000)) : (black = black - spentMs + (i < 2 ? 0 : 2000));
        return { move, spentMs, clockMs };
    });
    return {
        game: GAME, gseq: 12, category: '3+2', baseMs: 180000, incMs: 2000, rated: true,
        white: player(1017, 'Łukasz', 1532), black: player(2048, 'ユキ', 1498, true), you: C.White,
        moves, running: C.White, whiteMs: white, blackMs: black, serverTime: T0, drawOffer: C.None,
        status: enums.GameStatus.Ongoing, reason: enums.EndReason.None, whiteConnected: true, blackConnected: true,
        graceMs: 18000, firstMoveMs: 0, startedAt: T0 - 95000.25, rematch: C.None, autoPress: true, ...over,
    };
}

// ---- typical value of each message ---------------------------------------------------------------

const TYPICAL = {
    Hello: { seq: 1, proto: 2, schema: SCHEMA_HASH, client: 'Scacelith/1.4.0 (Windows x64)', token: TOKEN },
    C_Ping: { seq: 7, nonce: 123456 },
    C_Pong: { seq: 8, nonce: 0xdeadbeef },
    QueueJoin: { seq: 2, category: '3+2', rated: true },
    QueueLeave: { seq: 3 },
    ChallengeCreate: { seq: 4, target: 'yuki', baseSec: 300, incSec: 3, rated: true, color: enums.ColorPref.White },
    ChallengeAccept: { seq: 5, id: 77 },
    ChallengeDecline: { seq: 5, id: 78 },
    ChallengeCancel: { seq: 6, id: 79 },
    ChallengeJoinCode: { seq: 4, code: 'K7QX-9M2P' },
    Move: { seq: 12, game: GAME, ply: 0, move: mv('e2e4'), posHash: START_HASH, thinkMs: 2345, drawOffer: false },
    Resign: { seq: 20, game: GAME },
    DrawOffer: { seq: 21, game: GAME },
    DrawAnswer: { seq: 22, game: GAME, accept: true },
    DrawClaim: { seq: 23, game: GAME },
    Abort: { seq: 9, game: GAME },
    Resync: { seq: 10, game: GAME },
    Rematch: { seq: 30, game: GAME, accept: true },
    C_Gesture: { seq: 31, game: GAME, ply: 6, touch: 5, aim: 26, placed: 0, flags: 0, yaw: -212, pitch: -598, lean: 35 },
    Welcome: { proto: 2, serverTime: T0, userId: 1017, username: 'Łukasz', serverName: 'Scacelith Community Server', heartbeatMs: 10000, clientPingMs: 10000, maxMsgPerSec: 20, activeGame: 0, gestureRate: 4, gestureBurst: 8 },
    Error: { ref: 12, code: E.IllegalMove, fatal: false, game: GAME },
    S_Ping: { nonce: 991, serverTime: T0 },
    S_Pong: { nonce: 123456, serverTime: T0 + 12.25 },
    Ack: { ref: 3 },
    Notice: { code: enums.NoticeCode.ServerShutdown, arg: 30000 },
    QueueStatus: { category: '3+2', rated: true, state: enums.QueueState.Searching, waitMs: 12500, window: 150, queued: 42 },
    ChallengeReceived: { id: 77, from: player(2048, 'ユキ', 1612), baseSec: 300, incSec: 3, rated: true, yourColor: enums.ColorPref.Black, expiresMs: 60000 },
    ChallengeStatus: { id: 78, state: enums.ChallengeState.Pending, target: '', code: 'K7QX-9M2P', baseSec: 600, incSec: 5, rated: false },
    GameSnapshot: snapshot(),
    MoveMade: { game: GAME, gseq: 5, ply: 4, move: mv('f1b5'), flags: 0, spentMs: 2744, whiteMs: 177256, blackMs: 176100, serverTime: T0, drawOffer: false, firstMoveMs: 0 },
    MoveRejected: { game: GAME, ply: 6, move: mv('b5a4'), code: E.Desync },
    GameEvent: { game: GAME, gseq: 9, kind: enums.GameEventKind.PlayerDisconnected, color: C.Black, arg: 18000 },
    GameEnd: { game: GAME, gseq: 61, status: enums.GameStatus.WhiteWins, reason: enums.EndReason.Checkmate, whiteMs: 41250, blackMs: 3999, serverTime: T0 + 600000 },
    S_Gesture: { game: GAME, ply: 6, touch: 5, aim: 26, placed: 0, flags: 0, yaw: -212, pitch: -598, lean: 35 },
    RatingUpdate: {
        game: GAME, category: '3+2',
        white: { before: 1532, after: 1548, games: 31, provisional: false },
        black: { before: 1498, after: 1482, games: 12, provisional: true },
    },
};

// Generic typical values for messages added to the schema later (the explicit table wins).
function sampleOf(type, opts = {}, name = '') {
    if (type.startsWith('list16:')) return [sampleOf(type.slice(7), {}, name)];
    if (type.startsWith('struct:')) return Object.fromEntries(schema.structs[type.slice(7)].map(([n, t, o]) => [n, sampleOf(t, o, n)]));
    if (type.startsWith('enum:')) return Object.values(schema.enums[type.slice(5)])[0];
    switch (type) {
        case 'bool': return true;
        case 'f64': return T0;
        case 'id53': return GAME;
        case 'str8': return 'x'.repeat(Math.max(opts.min ?? 0, Math.min(opts.max ?? 8, 8)));
        default: {
            if (name === 'seq') return 1;
            const lo = opts.min ?? 0;
            const hi = opts.max ?? ({ u8: 255, u16: 65535, u32: 4294967295, i32: 2147483647 })[type];
            return Math.min(hi, Math.max(lo, 42));
        }
    }
}

// ---- random valid values (tests: round trips and differential fuzzing) ---------------------------

export { mulberry32 };

const INT_BOUNDS = { u8: [0, 0xff], u16: [0, 0xffff], u32: [0, 0xffffffff], i32: [-0x80000000, 0x7fffffff] };
// Characters of 1, 2, 3 and 4 UTF-8 bytes (and a BOM, which must survive a round trip).
const CHARS = [['a', 'Z', '0', ' ', '+', '-', '~'], ['Ł', 'é', 'ß', 'ж', 'م', 'ُ'], ['ユ', 'キ', '♞', '€', '﻿', '中'], ['🐴', '😀', '𝄞']];

function randomString(rnd, min, max) {
    const target = min + Math.floor(rnd() * (max - min + 1));
    let s = '', n = 0;
    while (n < target) {
        const room = target - n;
        const w = 1 + Math.floor(rnd() * Math.min(4, room));
        const pool = CHARS[w - 1];
        s += pool[Math.floor(rnd() * pool.length)];
        n += w;
    }
    return s;
}

function randomDouble(rnd) {
    const r = rnd();
    if (r < 0.1) return 0;
    if (r < 0.3) return Math.floor(rnd() * 2e12) + Math.floor(rnd() * 4) / 4;   // epoch ms
    if (r < 0.4) return (rnd() - 0.5) * 1e6;
    for (;;) {                                                                    // any finite bit pattern
        const b = Buffer.alloc(8);
        for (let i = 0; i < 8; i++) b[i] = Math.floor(rnd() * 256);
        const v = b.readDoubleLE(0);
        if (Number.isFinite(v) && !Object.is(v, -0)) return v;
    }
}

function randomInt(rnd, lo, hi) {
    const r = rnd();
    if (r < 0.1) return lo;
    if (r < 0.2) return hi;
    return lo + Math.floor(rnd() * (hi - lo + 1));
}

/**
 * A random valid value of a schema type.
 * @param {object} s schema module-like object
 * @param {string} type schema type ('u16', 'str8', 'list16:struct:MoveRec'...)
 * @param {{min?: number, max?: number}} opts
 * @param {() => number} rnd uniform in [0, 1)
 */
export function randomValue(s, type, opts, rnd) {
    if (type.startsWith('list16:')) {
        const max = opts.max ?? 0xffff;
        const n = rnd() < 0.05 ? max : Math.floor(rnd() * Math.min(max, 12) + (rnd() < 0.5 ? 0 : 0.999));
        const a = [];
        for (let i = 0; i < Math.min(n, max); i++) a.push(randomValue(s, type.slice(7), {}, rnd));
        return a;
    }
    if (type.startsWith('struct:')) return randomFields(s, s.structs[type.slice(7)], rnd);
    if (type.startsWith('enum:')) { const v = Object.values(s.enums[type.slice(5)]); return v[Math.floor(rnd() * v.length)]; }
    switch (type) {
        case 'bool': return rnd() < 0.5;
        case 'f64': return randomDouble(rnd);
        case 'id53': return randomInt(rnd, 0, Number.MAX_SAFE_INTEGER);
        case 'str8': return randomString(rnd, opts.min ?? 0, opts.max ?? 255);
        default: {
            const [lo, hi] = INT_BOUNDS[type];
            return randomInt(rnd, Math.max(lo, opts.min ?? lo), Math.min(hi, opts.max ?? hi));
        }
    }
}

/** Random valid values for a field list ([name, type, opts?][]). */
export function randomFields(s, fields, rnd) {
    const o = {};
    for (const [name, type, opts = {}] of fields) o[name] = randomValue(s, type, opts, rnd);
    return o;
}

// ---- builders ----------------------------------------------------------------------------------

const keyOf = (m) => codec.messageName(m.id);
const messageByKey = new Map(schema.messages.map((m) => [keyOf(m), m]));

function canonical(key, fields) {
    const buf = encode[key](fields);
    const { type, ...back } = decode(buf);
    if (type !== MSG[key]) throw new Error(`vectors: ${key} decodes as type ${type}`);
    return { buf, fields: back };
}

// Byte offset of a (nested) field in an encoding: path like 'white.name' or 'moves.3.move'.
function locate(key, fields, fieldPath) {
    const parts = fieldPath.split('.');
    let off = 1;
    function sizeOf(type, v) {
        if (type.startsWith('list16:')) return 2 + v.reduce((a, x) => a + sizeOf(type.slice(7), x), 0);
        if (type.startsWith('struct:')) return schema.structs[type.slice(7)].reduce((a, [n, t]) => a + sizeOf(t, v[n]), 0);
        if (type.startsWith('enum:')) return 1;
        return { u8: 1, bool: 1, u16: 2, u32: 4, i32: 4, f64: 8, id53: 8 }[type] ?? 1 + Buffer.byteLength(v, 'utf8');
    }
    function walk(list, obj, i) {
        for (const [n, t] of list) {
            if (n !== parts[i]) { off += sizeOf(t, obj[n]); continue; }
            if (i === parts.length - 1) return off;
            if (t.startsWith('struct:')) return walk(schema.structs[t.slice(7)], obj[n], i + 1);
            if (t.startsWith('list16:')) {
                const idx = Number(parts[i + 1]);
                const item = t.slice(7);
                off += 2;
                for (let k = 0; k < idx; k++) off += sizeOf(item, obj[n][k]);
                if (i + 1 === parts.length - 1) return off;
                return walk(schema.structs[item.slice(7)], obj[n][idx], i + 2);
            }
            throw new Error(`vectors: cannot descend into ${n}`);
        }
        throw new Error(`vectors: no field ${parts[i]} in ${key}`);
    }
    return walk(messageByKey.get(key).fields, fields, 0);
}

const hexOf = (b) => Buffer.from(b).toString('hex');

export function buildVectors() {
    const valid = [];
    const seenKeys = new Set();
    const ok = (key, fields, note) => {
        const m = messageByKey.get(key);
        if (!m) throw new Error(`vectors: unknown message ${key}`);
        const c = canonical(key, fields);
        valid.push({ name: key, type: m.id, dir: m.dir, note, fields: c.fields, hex: hexOf(c.buf) });
        seenKeys.add(key);
        return c;
    };

    // Typical values, one per message, in schema order.
    for (const m of schema.messages) {
        const key = keyOf(m);
        const fields = TYPICAL[key] ?? Object.fromEntries(m.fields.map(([n, t, o]) => [n, sampleOf(t, o, n)]));
        ok(key, fields, 'typical');
    }

    // Edge cases.
    const ascii = (n, c = 'a') => c.repeat(n);
    ok('Hello', { ...TYPICAL.Hello, client: ascii(48, 'c'), token: 'sct_' + ascii(156, 'T') }, 'client and token at their maximum length (48 and 160 bytes)');
    ok('Hello', { ...TYPICAL.Hello, client: '', token: 'sct_0123456789ab' }, 'empty client, token at its minimum length (16 bytes)');
    ok('Hello', { ...TYPICAL.Hello, seq: 0xffffffff, proto: 0xffff, schema: 0, client: '♞ Scacelith 🐴 Łódź' }, 'u32/u16 maxima, schema 0, 3- and 4-byte UTF-8');
    ok('C_Ping', { seq: 0, nonce: 0 }, 'zeros');
    ok('C_Pong', { seq: 0xffffffff, nonce: 0xffffffff }, 'u32 maxima');
    ok('QueueJoin', { seq: 2, category: '1+0', rated: false }, 'category at its minimum length (3 bytes), casual');
    ok('QueueJoin', { seq: 2, category: '180+180', rated: true }, 'category at its maximum length (7 bytes)');
    ok('ChallengeCreate', { seq: 4, target: '', baseSec: 15, incSec: 0, rated: false, color: enums.ColorPref.Random }, 'private game (empty target), baseSec at its minimum');
    ok('ChallengeCreate', { seq: 4, target: 'مُحَمَّد', baseSec: 10800, incSec: 180, rated: false, color: enums.ColorPref.Black }, 'Arabic name with diacritics (16 bytes), baseSec and incSec at their maximum');
    ok('ChallengeCreate', { seq: 4, target: 'ユキユキユキユキ', baseSec: 180, incSec: 2, rated: true, color: enums.ColorPref.White }, 'target at its maximum length in 3-byte characters (24 bytes)');
    ok('ChallengeJoinCode', { seq: 4, code: 'AB12' }, 'code at its minimum length (4)');
    ok('ChallengeJoinCode', { seq: 4, code: 'ABCD-EFGH-JK' }, 'code at its maximum length (12)');
    ok('Move', { seq: 99, game: MAX53, ply: 1199, move: 0x7fff, posHash: 0xffffffff, thinkMs: 0xffffffff, drawOffer: true }, 'game = 2^53 - 1, ply and move at their maximum, drawOffer');
    ok('Move', { seq: 13, game: 0x100000000, ply: 57, move: mv('e7e8q'), posHash: 0, thinkMs: 0, drawOffer: false }, 'game = 2^32, promotion e7e8=Q');
    ok('Move', { seq: 14, game: 0xffffffff, ply: 8, move: mv('e1g1'), posHash: 0x12345678, thinkMs: 812, drawOffer: false }, 'game = 2^32 - 1, castling e1g1');
    ok('Resign', { seq: 1, game: 1 }, 'smallest game id');
    ok('Rematch', { seq: 31, game: MAX53 - 1, accept: false }, 'decline, game = 2^53 - 2');
    ok('Welcome', { proto: 2, serverTime: 0, userId: 0xffffffff, username: 'ユキユキユキユキ', serverName: '♜'.repeat(21) + 'x', heartbeatMs: 0, clientPingMs: 0xffffffff, maxMsgPerSec: 0xffff, activeGame: MAX53, gestureRate: 60, gestureBurst: 120 },
        'username at its maximum (24 bytes of 3-byte characters), serverName at its maximum (64 bytes), activeGame = 2^53 - 1, serverTime 0');
    ok('Welcome', { proto: 2, serverTime: -123456.789, userId: 1, username: 'مُحَمَّد', serverName: '', heartbeatMs: 10000, clientPingMs: 0, maxMsgPerSec: 20, activeGame: GAME, gestureRate: 0, gestureBurst: 0 },
        'negative f64, Arabic username, empty serverName');
    ok('Error', { ref: 0, code: E.CheatDetected, fatal: true, game: 0 }, 'fatal, no request, no game');
    ok('Error', { ref: 1, code: E.Malformed, fatal: true, game: 0 }, 'smallest ErrorCode');
    ok('Error', { ref: 0xffffffff, code: E.SlowConsumer, fatal: false, game: MAX53 }, 'largest ErrorCode');
    ok('S_Ping', { nonce: 0xffffffff, serverTime: Number.MAX_VALUE }, 'largest finite f64');
    ok('S_Pong', { nonce: 1, serverTime: 0.1 }, 'f64 0.1 (inexact in binary)');
    ok('Notice', { code: enums.NoticeCode.Banned, arg: T0 + 86400000 }, 'ban end time');
    ok('Notice', { code: enums.NoticeCode.MatchmakingCooldown, arg: -1e-300 }, 'tiny negative f64');
    ok('Notice', { code: enums.NoticeCode.SessionRevoked, arg: 0 }, 'no argument');
    ok('QueueStatus', { category: '', rated: false, state: enums.QueueState.Left, waitMs: 0, window: 0, queued: 0 }, 'left, empty category');
    ok('QueueStatus', { category: '180+180', rated: true, state: enums.QueueState.Matched, waitMs: 0xffffffff, window: 0xffff, queued: 0xffffffff }, 'maxima');
    ok('ChallengeReceived', { id: 0xffffffff, from: player(1, 'Łukasz', 0, true), baseSec: 0xffff, incSec: 255, rated: false, yourColor: enums.ColorPref.Random, expiresMs: 0 }, 'Polish name, u16/u8 maxima without schema bounds');
    ok('ChallengeReceived', { id: 1, from: player(3, 'x', 65535), baseSec: 15, incSec: 0, rated: true, yourColor: enums.ColorPref.White, expiresMs: 5000 }, 'one-byte name (minimum), rating 65535');
    ok('ChallengeStatus', { id: 5, state: enums.ChallengeState.Unavailable, target: 'ABCDEFGHIJKLMNOPQRSTUVWX', code: '', baseSec: 0, incSec: 0, rated: true }, 'target at its maximum length (24), last ChallengeState');
    ok('GameSnapshot', snapshot({ gseq: 0, moves: [], running: C.None, whiteMs: 180000, blackMs: 180000, firstMoveMs: 30000, you: C.Black, startedAt: T0 }), 'game start: empty move list, clocks not running, first-move timer');
    ok('GameSnapshot', snapshot({ category: 'custom', rated: false, baseMs: 0, incMs: 0, gseq: 0xffffffff, drawOffer: C.Black, rematch: C.White,
        status: enums.GameStatus.Draw, reason: enums.EndReason.BothDisconnected, running: C.None, whiteConnected: false, blackConnected: false,
        white: player(0xffffffff, 'مُحَمَّد', 0), black: player(0, 'ユキユキユキユキ', 65535, true), you: C.None }),
    'finished game, largest EndReason, spectator (you = None), multi-byte names, custom category');
    ok('GameSnapshot', snapshot({ gseq: 80, moves: moveList(80, 80), whiteMs: 54321, blackMs: 43210 }), '80 moves (the benchmark message)');
    ok('GameSnapshot', snapshot({ gseq: 1200, moves: moveList(1200, 1200), status: enums.GameStatus.Draw, reason: enums.EndReason.SeventyFiveMoves, running: C.None }), 'move list at its maximum (1200)');
    ok('MoveMade', { game: MAX53, gseq: 0xffffffff, ply: 0xffff, move: 0xffff, flags: 0xff, spentMs: 0xffffffff, whiteMs: 0xffffffff, blackMs: 0, serverTime: 1e300, drawOffer: true, firstMoveMs: 0xffffffff },
        'maxima (MoveMade.move and ply have no schema bound), f64 1e300');
    ok('MoveMade', { game: GAME, gseq: 1, ply: 0, move: mv('e2e4'), flags: codec.MoveFlag.DoublePush, spentMs: 0, whiteMs: 180000, blackMs: 180000, serverTime: T0, drawOffer: false, firstMoveMs: 30000 },
        'first move: no clock charge, first-move timer for Black');
    ok('MoveRejected', { game: GAME, ply: 1199, move: 0x7fff, code: E.FlagFell }, 'FlagFell');
    ok('GameEvent', { game: GAME, gseq: 3, kind: enums.GameEventKind.AbortAvailable, color: C.None, arg: 0xffffffff }, 'largest GameEventKind, color None');
    ok('GameEnd', { game: GAME, gseq: 2, status: enums.GameStatus.Aborted, reason: enums.EndReason.NoShow, whiteMs: 180000, blackMs: 180000, serverTime: T0 }, 'aborted (no-show)');
    ok('RatingUpdate', { game: MAX53, category: '', white: { before: 0, after: 65535, games: 0xffffffff, provisional: true }, black: { before: 65535, after: 0, games: 0, provisional: false } }, 'extremes');

    for (const m of schema.messages) if (!seenKeys.has(keyOf(m))) throw new Error(`vectors: no vector for ${keyOf(m)}`);

    // ---- malformed inputs (exactly one defect each) ----
    const malformed = [];
    const bad = (bytes, dir, reason, note) => {
        const buf = Buffer.from(bytes);
        let err = null;
        try { decode(buf, { dir }); } catch (e) { err = e; }
        if (!(err instanceof ProtocolError)) throw new Error(`vectors: "${note}" was accepted`);
        if (err.reason !== reason) throw new Error(`vectors: "${note}" fails with "${err.reason}", expected "${reason}"`);
        const t = buf.length ? buf[0] : null;
        const name = t === null ? null : codec.messageName(t);
        malformed.push({ name, type: name ? t : null, dir, note, reason, hex: hexOf(buf) });
    };
    const enc = (key, fields) => Buffer.from(encode[key](fields));
    const patch = (key, fields, fieldPath, write) => {
        const b = enc(key, fields);
        write(b, locate(key, fields, fieldPath));
        return b;
    };
    const u8 = (v) => (b, o) => { b[o] = v; };
    const u16 = (v) => (b, o) => { b.writeUInt16LE(v, o); };
    const u32at = (d, v) => (b, o) => { b.writeUInt32LE(v, o + d); };
    const f64bits = (hi, lo) => (b, o) => { b.writeUInt32LE(lo, o); b.writeUInt32LE(hi, o + 4); };
    // Replaces the payload of the str8 at fieldPath (same length) with raw bytes.
    const strBytes = (key, fields, fieldPath, raw) => {
        const f = { ...fields };
        let target = f;
        const parts = fieldPath.split('.');
        for (let i = 0; i < parts.length - 1; i++) { target[parts[i]] = { ...target[parts[i]] }; target = target[parts[i]]; }
        target[parts[parts.length - 1]] = 'z'.repeat(raw.length);
        return patch(key, f, fieldPath, (b, o) => { Buffer.from(raw).copy(b, o + 1); });
    };
    // Replaces the str8 at fieldPath by a length byte and raw payload of any length.
    const strSplice = (key, fields, fieldPath, raw) => {
        const b = enc(key, fields);
        const o = locate(key, fields, fieldPath);
        return Buffer.concat([b.subarray(0, o), Buffer.from([raw.length]), Buffer.from(raw), b.subarray(o + 1 + b[o])]);
    };
    const T = TYPICAL;

    bad([], 'c2s', 'empty', 'empty message');
    bad([0x00], 'c2s', 'unknown type', 'type 0x00');
    bad([0x04, 1, 0, 0, 0], 'c2s', 'unknown type', 'unassigned client type 0x04');
    bad([0x7f, 1, 0, 0, 0], 'c2s', 'unknown type', 'unassigned client type 0x7F');
    bad([0xa7, 0], 's2c', 'unknown type', 'unassigned server type 0xA7');
    bad([0xff], 's2c', 'unknown type', 'type 0xFF');
    bad(enc('Welcome', T.Welcome), 'c2s', 'wrong direction', 'server message (Welcome) received by the server');
    bad(enc('S_Ping', T.S_Ping), 'c2s', 'wrong direction', 'server Ping received by the server');
    bad(enc('Move', T.Move), 's2c', 'wrong direction', 'client message (Move) received by the client');
    bad(enc('C_Pong', T.C_Pong), 's2c', 'wrong direction', 'client Pong received by the client');

    const move = enc('Move', T.Move);
    bad(move.subarray(0, move.length - 1), 'c2s', 'truncated', 'Move missing its last byte');
    bad(move.subarray(0, 1), 'c2s', 'truncated', 'Move: type byte only');
    bad(enc('QueueLeave', T.QueueLeave).subarray(0, 3), 'c2s', 'truncated', 'QueueLeave with a 2-byte seq');
    const hello = enc('Hello', T.Hello);
    bad(hello.subarray(0, hello.length - 10), 'c2s', 'truncated', 'Hello cut inside the token');
    bad(hello.subarray(0, locate('Hello', T.Hello, 'token')), 'c2s', 'truncated', 'Hello without the token length byte');
    const mm = enc('MoveMade', T.MoveMade);
    bad(mm.subarray(0, locate('MoveMade', T.MoveMade, 'serverTime') + 5), 's2c', 'truncated', 'MoveMade cut inside serverTime');
    const snap = enc('GameSnapshot', T.GameSnapshot);
    bad(snap.subarray(0, locate('GameSnapshot', T.GameSnapshot, 'moves.4')), 's2c', 'truncated', 'GameSnapshot cut inside the move list');
    bad(snap.subarray(0, snap.length - 3), 's2c', 'truncated', 'GameSnapshot cut inside startedAt');
    bad(snap.subarray(0, locate('GameSnapshot', T.GameSnapshot, 'white.name') + 3), 's2c', 'truncated', 'GameSnapshot cut inside white.name');
    bad(Buffer.concat([move, Buffer.from([0])]), 'c2s', 'trailing bytes', 'Move with one extra byte');
    bad(Buffer.concat([enc('Ack', T.Ack), Buffer.from([0xff, 0xff, 0xff])]), 's2c', 'trailing bytes', 'Ack with three extra bytes');
    bad(Buffer.concat([hello, Buffer.from([0])]), 'c2s', 'trailing bytes', 'Hello with one extra byte');
    bad(Buffer.concat([snap, Buffer.from([0])]), 's2c', 'trailing bytes', 'GameSnapshot with one extra byte');

    bad(patch('Error', T.Error, 'code', u8(0)), 's2c', 'code not a ErrorCode', 'Error.code = 0');
    bad(patch('Error', T.Error, 'code', u8(12)), 's2c', 'code not a ErrorCode', 'Error.code = 12 (gap after EmailUnverified)');
    bad(patch('Error', T.Error, 'code', u8(244)), 's2c', 'code not a ErrorCode', 'Error.code = 244 (after SlowConsumer)');
    bad(patch('MoveRejected', T.MoveRejected, 'code', u8(99)), 's2c', 'code not a ErrorCode', 'MoveRejected.code = 99');
    bad(patch('ChallengeCreate', T.ChallengeCreate, 'color', u8(3)), 'c2s', 'color not a ColorPref', 'ChallengeCreate.color = 3');
    bad(patch('GameSnapshot', T.GameSnapshot, 'you', u8(3)), 's2c', 'you not a Color', 'GameSnapshot.you = 3');
    bad(patch('GameSnapshot', T.GameSnapshot, 'reason', u8(14)), 's2c', 'reason not a EndReason', 'GameSnapshot.reason = 14 (gap before Abandonment)');
    bad(patch('GameEvent', T.GameEvent, 'kind', u8(0)), 's2c', 'kind not a GameEventKind', 'GameEvent.kind = 0');
    bad(patch('GameEvent', T.GameEvent, 'kind', u8(8)), 's2c', 'kind not a GameEventKind', 'GameEvent.kind = 8');
    bad(patch('Notice', T.Notice, 'code', u8(0xff)), 's2c', 'code not a NoticeCode', 'Notice.code = 255');
    bad(patch('QueueStatus', T.QueueStatus, 'state', u8(3)), 's2c', 'state not a QueueState', 'QueueStatus.state = 3');
    bad(patch('GameEnd', T.GameEnd, 'status', u8(5)), 's2c', 'status not a GameStatus', 'GameEnd.status = 5');

    bad(patch('Move', T.Move, 'drawOffer', u8(2)), 'c2s', 'drawOffer not a bool', 'Move.drawOffer = 2');
    bad(patch('QueueJoin', T.QueueJoin, 'rated', u8(2)), 'c2s', 'rated not a bool', 'QueueJoin.rated = 2');
    bad(patch('ChallengeReceived', T.ChallengeReceived, 'from.provisional', u8(2)), 's2c', 'from.provisional not a bool', 'ChallengeReceived.from.provisional = 2');
    bad(patch('GameSnapshot', T.GameSnapshot, 'whiteConnected', u8(0xff)), 's2c', 'whiteConnected not a bool', 'GameSnapshot.whiteConnected = 255');

    bad(strBytes('Hello', T.Hello, 'client', [0x41, 0xc3, 0x28]), 'c2s', 'client not UTF-8', 'invalid continuation byte (C3 28)');
    bad(strBytes('Hello', T.Hello, 'client', [0xc0, 0xaf]), 'c2s', 'client not UTF-8', 'overlong encoding of "/" (C0 AF)');
    bad(strBytes('Hello', T.Hello, 'client', [0xe0, 0x80, 0xaf]), 'c2s', 'client not UTF-8', 'overlong 3-byte encoding (E0 80 AF)');
    bad(strBytes('Hello', T.Hello, 'client', [0xed, 0xa0, 0x80]), 'c2s', 'client not UTF-8', 'encoded UTF-16 surrogate U+D800 (ED A0 80)');
    bad(strBytes('Hello', T.Hello, 'client', [0x61, 0xe3, 0x81]), 'c2s', 'client not UTF-8', 'multi-byte sequence cut at the end of the string (E3 81)');
    bad(strBytes('Hello', T.Hello, 'client', [0xf4, 0x90, 0x80, 0x80]), 'c2s', 'client not UTF-8', 'code point above U+10FFFF (F4 90 80 80)');
    bad(strBytes('Hello', T.Hello, 'client', [0xff]), 'c2s', 'client not UTF-8', 'byte FF');
    bad(strBytes('Hello', T.Hello, 'client', [0x80, 0x61]), 'c2s', 'client not UTF-8', 'lone continuation byte (80)');
    bad(strBytes('ChallengeReceived', T.ChallengeReceived, 'from.name', [0xe3, 0x82]), 's2c', 'from.name not UTF-8', 'PlayerInfo.name with a cut sequence');
    bad(strBytes('Hello', T.Hello, 'client', [0x61, 0x62, 0x00, 0x63]), 'c2s', 'client contains NUL', 'NUL inside a string');
    bad(strBytes('Welcome', T.Welcome, 'username', [0x00]), 's2c', 'username contains NUL', 'username = NUL');

    bad(strSplice('Hello', T.Hello, 'client', Buffer.from(ascii(49, 'c'))), 'c2s', 'client bad length', 'Hello.client of 49 bytes (max 48)');
    bad(strSplice('Hello', T.Hello, 'token', Buffer.from(ascii(161, 't'))), 'c2s', 'token bad length', 'Hello.token of 161 bytes (max 160)');
    bad(strSplice('Hello', T.Hello, 'token', Buffer.from(ascii(15, 't'))), 'c2s', 'token bad length', 'Hello.token of 15 bytes (min 16)');
    bad(strSplice('QueueJoin', T.QueueJoin, 'category', Buffer.from('3+')), 'c2s', 'category bad length', 'QueueJoin.category of 2 bytes (min 3)');
    bad(strSplice('QueueJoin', T.QueueJoin, 'category', Buffer.from('180+1800')), 'c2s', 'category bad length', 'QueueJoin.category of 8 bytes (max 7)');
    bad(strSplice('ChallengeJoinCode', T.ChallengeJoinCode, 'code', Buffer.from('ABC')), 'c2s', 'code bad length', 'code of 3 bytes (min 4)');
    bad(strSplice('Welcome', T.Welcome, 'username', Buffer.from('ユキユキユキユキa')), 's2c', 'username bad length', 'username of 25 bytes (max 24)');
    bad(strSplice('GameSnapshot', T.GameSnapshot, 'black.name', Buffer.alloc(0)), 's2c', 'black.name bad length', 'empty PlayerInfo.name (min 1)');
    bad(strSplice('GameSnapshot', T.GameSnapshot, 'white.name', Buffer.from(ascii(25, 'w'))), 's2c', 'white.name bad length', 'PlayerInfo.name of 25 bytes (max 24)');

    {
        const f = snapshot({ moves: moveList(1200, 1201) });
        const b = enc('GameSnapshot', f);
        const o = locate('GameSnapshot', f, 'moves');
        const extra = Buffer.alloc(10);
        extra.writeUInt16LE(mv('a2a3'), 0);
        const end = o + 2 + 1200 * 10;
        const over = Buffer.concat([b.subarray(0, end), extra, b.subarray(end)]);
        over.writeUInt16LE(1201, o);
        bad(over, 's2c', 'moves too long', 'GameSnapshot with 1201 moves (max 1200)');
    }
    bad(patch('Move', T.Move, 'game', u32at(4, 0x200000)), 'c2s', 'game above 2^53', 'Move.game = 2^53');
    bad(patch('Move', T.Move, 'game', u32at(4, 0xffffffff)), 'c2s', 'game above 2^53', 'Move.game with every high bit set');
    bad(patch('Welcome', T.Welcome, 'activeGame', u32at(4, 0x80000000)), 's2c', 'activeGame above 2^53', 'Welcome.activeGame = 2^63');
    bad(patch('Move', T.Move, 'move', u16(0x8000 | mv('e2e4'))), 'c2s', 'move above max', 'Move.move with bit 15 set');
    bad(patch('GameSnapshot', T.GameSnapshot, 'moves.3.move', u16(0xffff)), 's2c', 'moves.move above max', 'MoveRec.move with bit 15 set');
    bad(patch('Move', T.Move, 'ply', u16(1200)), 'c2s', 'ply above max', 'Move.ply = 1200 (max 1199)');
    bad(patch('ChallengeCreate', T.ChallengeCreate, 'baseSec', u16(14)), 'c2s', 'baseSec below min', 'ChallengeCreate.baseSec = 14 (min 15)');
    bad(patch('ChallengeCreate', T.ChallengeCreate, 'baseSec', u16(10801)), 'c2s', 'baseSec above max', 'ChallengeCreate.baseSec = 10801 (max 10800)');
    bad(patch('ChallengeCreate', T.ChallengeCreate, 'incSec', u8(181)), 'c2s', 'incSec above max', 'ChallengeCreate.incSec = 181 (max 180)');
    bad(patch('Welcome', T.Welcome, 'serverTime', f64bits(0x7ff80000, 0)), 's2c', 'serverTime not finite', 'Welcome.serverTime = NaN');
    bad(patch('S_Ping', T.S_Ping, 'serverTime', f64bits(0x7ff00000, 0)), 's2c', 'serverTime not finite', 'Ping.serverTime = +Infinity');
    bad(patch('Notice', T.Notice, 'arg', f64bits(0xfff00000, 0)), 's2c', 'arg not finite', 'Notice.arg = -Infinity');
    bad(patch('GameEnd', T.GameEnd, 'serverTime', f64bits(0x7ff00000, 1)), 's2c', 'serverTime not finite', 'GameEnd.serverTime = signalling NaN');

    return {
        about: 'Scacelith protocol golden vectors, generated by dedicated-server/tools/gen-protocol-vectors.js from src/protocol/schema.js (do not edit). '
            + 'valid[]: encoding `fields` gives `hex` and decoding `hex` gives { type, ...fields }. '
            + 'malformed[]: every decoder receiving `hex` in direction `dir` must refuse it; `reason` is the JS codec\'s ProtocolError reason.',
        protocolVersion: codec.PROTOCOL_VERSION,
        protocolMin: codec.PROTOCOL_MIN,
        schemaHash: SCHEMA_HASH,
        schemaHashHex: '0x' + SCHEMA_HASH.toString(16).padStart(8, '0'),
        subprotocol: codec.WS_SUBPROTOCOL,
        valid,
        malformed,
    };
}

/** Serialises the vectors: header keys pretty-printed, one vector per line (stable, diff-friendly). */
export function renderVectors(v = buildVectors()) {
    const head = Object.entries(v).filter(([k]) => k !== 'valid' && k !== 'malformed')
        .map(([k, x]) => `  ${JSON.stringify(k)}: ${JSON.stringify(x)}`);
    const list = (a) => a.map((x) => '    ' + JSON.stringify(x)).join(',\n');
    return `{\n${head.join(',\n')},\n  "valid": [\n${list(v.valid)}\n  ],\n  "malformed": [\n${list(v.malformed)}\n  ]\n}\n`;
}

function main(argv) {
    const text = renderVectors();
    const current = fs.existsSync(VECTORS_PATH) ? fs.readFileSync(VECTORS_PATH, 'utf8') : null;
    const rel = path.relative(process.cwd(), VECTORS_PATH);
    if (argv.includes('--check')) {
        if (current !== text) { console.error(`${rel} is stale: run npm run gen:protocol`); return 1; }
        return 0;
    }
    if (current === text) { console.log(`${rel} up to date`); return 0; }
    fs.mkdirSync(path.dirname(VECTORS_PATH), { recursive: true });
    fs.writeFileSync(VECTORS_PATH, text);
    console.log(`wrote ${rel}`);
    return 0;
}

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
    process.exitCode = main(process.argv.slice(2));
}
