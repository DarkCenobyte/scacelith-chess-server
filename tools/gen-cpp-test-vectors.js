#!/usr/bin/env node
// Protocol test vectors for the game client's unit tests (tests/net_tests.cpp).
//
//   node dedicated-server/tools/gen-cpp-test-vectors.js   writes ../tests/data/net-protocol-vectors.json
//
// Every vector comes from the JavaScript codec (src/protocol/index.js), so the C++ codec is
// checked against it byte for byte:
//   valid[]      { name, msg (decoded object, schema field names), hex (encode() output) }
//   malformed[]  { name, hex, reason }: frames built by a raw writer below (no validation) that
//                the JS decode() refuses; the C++ decode() must refuse them too.
//   fnv1a32[]    { text, hash } from fnv1a32() (the posHash of Move).
// The generator stops with an error when the JS codec does not behave as expected (a valid
// vector that does not round-trip, a malformed one that decodes).

import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import * as schema from '../src/protocol/schema.js';
import * as P from '../src/protocol/index.js';

const here = path.dirname(fileURLToPath(import.meta.url));
const outPath = path.resolve(here, '../../tests/data/net-protocol-vectors.json');

// ---- raw writer: the wire format without any validation ----------------------------------------

const byName = new Map();
for (const m of schema.messages) byName.set(P.messageName(m.id), m);

function rawField(parts, type, v) {
    if (type.startsWith('list16:')) {
        const item = type.slice(7);
        const items = v ?? [];
        const b = Buffer.alloc(2);
        b.writeUInt16LE(items.length);
        parts.push(b);
        for (const x of items) rawField(parts, item, x);
        return;
    }
    if (type.startsWith('struct:')) {
        for (const [n, t] of schema.structs[type.slice(7)]) rawField(parts, t, v?.[n]);
        return;
    }
    const b = Buffer.alloc(8);
    switch (type.startsWith('enum:') ? 'u8' : type) {
        case 'u8': case 'bool': parts.push(Buffer.from([Number(v) & 0xff])); return;
        case 'u16': b.writeUInt16LE(v & 0xffff); parts.push(b.subarray(0, 2)); return;
        case 'u32': b.writeUInt32LE(v >>> 0); parts.push(b.subarray(0, 4)); return;
        case 'i32': b.writeInt32LE(v | 0); parts.push(b.subarray(0, 4)); return;
        case 'f64': b.writeDoubleLE(v); parts.push(b); return;
        case 'id53': b.writeBigUInt64LE(BigInt(v)); parts.push(b); return;
        case 'str8': {
            const s = Buffer.isBuffer(v) ? v : Buffer.from(v ?? '', 'utf8');
            parts.push(Buffer.from([s.length & 0xff]), s);
            return;
        }
    }
    throw new Error(`raw: type ${type}`);
}

function raw(name, obj) {
    const m = byName.get(name);
    if (!m) throw new Error(`raw: message ${name}`);
    const parts = [Buffer.from([m.id])];
    for (const [n, t] of m.fields) rawField(parts, t, obj[n]);
    return Buffer.concat(parts);
}

// ---- valid samples -----------------------------------------------------------------------------

const alice = { userId: 1, name: 'alice', rating: 1500, provisional: true };
const bob = { userId: 4294967295, name: 'Bob_ÉÈ漢字😀', rating: 65535, provisional: false };
const moves = [
    { move: 796, spentMs: 0, clockMs: 180000 },
    { move: 0x7fff, spentMs: 4294967295, clockMs: 0 },
    { move: P.encodeMove(52, 60, 5), spentMs: 1234, clockMs: 170000 },
];
const snapshot = {
    game: 9007199254740991, gseq: 17, category: '3+2', baseMs: 180000, incMs: 2000, rated: true,
    white: alice, black: bob, you: 1, moves, running: 0, whiteMs: 170000, blackMs: 165432,
    serverTime: 1727000000123.25, drawOffer: 2, status: 0, reason: 0, whiteConnected: true,
    blackConnected: false, graceMs: 30000, firstMoveMs: 0, startedAt: 1727000000000, rematch: 2, autoPress: true,
};

const samples = [
    ['Hello', { seq: 1, proto: 2, schema: P.SCHEMA_HASH, client: 'Scacelith/0.1.0 win64', token: 'sct_' + 'A'.repeat(43) }],
    ['Hello', { seq: 4294967295, proto: 65535, schema: 0, client: '', token: 'x'.repeat(160) }],
    ['C_Ping', { seq: 2, nonce: 0 }],
    ['C_Pong', { seq: 3, nonce: 4294967295 }],
    ['QueueJoin', { seq: 4, category: '3+2', rated: true }],
    ['QueueJoin', { seq: 5, category: '180+180', rated: false }],
    ['QueueLeave', { seq: 6 }],
    ['ChallengeCreate', { seq: 7, target: 'bob', baseSec: 15, incSec: 0, rated: false, color: 0 }],
    ['ChallengeCreate', { seq: 8, target: '', baseSec: 10800, incSec: 180, rated: true, color: 2 }],
    ['ChallengeAccept', { seq: 9, id: 123456 }],
    ['ChallengeDecline', { seq: 10, id: 0 }],
    ['ChallengeCancel', { seq: 11, id: 4294967295 }],
    ['ChallengeJoinCode', { seq: 12, code: 'AB12' }],
    ['ChallengeJoinCode', { seq: 13, code: 'ABCDEFGHJKLM' }],
    ['Move', { seq: 14, game: 1, ply: 0, move: P.encodeMove(12, 28, 0), posHash: P.fnv1a32('rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq -'), thinkMs: 1500, drawOffer: false }],
    ['Move', { seq: 15, game: 9007199254740991, ply: 1199, move: 0x7fff, posHash: 4294967295, thinkMs: 4294967295, drawOffer: true }],
    ['Resign', { seq: 16, game: 4294967296 }],
    ['DrawOffer', { seq: 17, game: 77 }],
    ['DrawAnswer', { seq: 18, game: 77, accept: true }],
    ['DrawClaim', { seq: 19, game: 77 }],
    ['Abort', { seq: 20, game: 77 }],
    ['Resync', { seq: 21, game: 0 }],
    ['Rematch', { seq: 22, game: 77, accept: false }],
    ['C_Gesture', { seq: 23, game: 77, ply: 12, touch: 6, aim: 21, placed: 0, flags: 1, yaw: -3142, pitch: 1571, lean: 0 }],
    ['C_Gesture', { seq: 24, game: 9007199254740991, ply: 1199, touch: 64, aim: 64, placed: 0x7fff, flags: 7, yaw: 3142, pitch: -1571, lean: 100 }],
    ['Welcome', { proto: 2, serverTime: 1727000000000.5, userId: 42, username: 'alice', serverName: 'Scacelith official ♔', heartbeatMs: 15000, clientPingMs: 10000, maxMsgPerSec: 20, activeGame: 0, gestureRate: 4, gestureBurst: 8 }],
    ['Welcome', { proto: 2, serverTime: -1.5e300, userId: 0, username: '', serverName: '', heartbeatMs: 0, clientPingMs: 60000, maxMsgPerSec: 0, activeGame: 281474976710657, gestureRate: 0, gestureBurst: 0 }],
    ['Error', { ref: 5, code: 102, fatal: false, game: 77 }],
    ['Error', { ref: 0, code: 1, fatal: true, game: 0 }],
    ['Error', { ref: 1, code: 209, fatal: false, game: 0 }],
    ['S_Ping', { nonce: 99, serverTime: 1727000000000 }],
    ['S_Pong', { nonce: 100, serverTime: 0.125 }],
    ['Ack', { ref: 7 }],
    ['Notice', { code: 1, arg: 30000 }],
    ['Notice', { code: 2, arg: 1727999999999 }],
    ['QueueStatus', { category: '10+5', rated: true, state: 1, waitMs: 12000, window: 150, queued: 37 }],
    ['ChallengeReceived', { id: 5, from: bob, baseSec: 600, incSec: 5, rated: false, yourColor: 1, expiresMs: 60000 }],
    ['ChallengeStatus', { id: 6, state: 0, target: '', code: 'K7Q2ZP', baseSec: 300, incSec: 3, rated: false }],
    ['ChallengeStatus', { id: 6, state: 5, target: 'bob', code: '', baseSec: 300, incSec: 3, rated: true }],
    ['GameSnapshot', snapshot],
    ['GameSnapshot', { ...snapshot, game: 2, moves: [], running: 2, status: 3, reason: 26, you: 2, drawOffer: 0, rematch: 1, white: { ...alice, name: 'w' }, autoPress: false }],
    ['MoveMade', { game: 77, gseq: 2, ply: 1, move: P.encodeMove(52, 36, 0), flags: 16, spentMs: 0, whiteMs: 180000, blackMs: 180000, serverTime: 1727000000500, drawOffer: false, firstMoveMs: 30000 }],
    ['MoveMade', { game: 77, gseq: 9, ply: 1199, move: 0xffff, flags: 255, spentMs: 4294967295, whiteMs: 0, blackMs: 4294967295, serverTime: 1e15, drawOffer: true, firstMoveMs: 0 }],
    ['MoveRejected', { game: 77, ply: 4, move: 796, code: 104 }],
    ['GameEvent', { game: 77, gseq: 10, kind: 3, color: 1, arg: 30000 }],
    ['GameEvent', { game: 77, gseq: 11, kind: 6, color: 2, arg: 0 }],
    ['GameEnd', { game: 77, gseq: 12, status: 1, reason: 1, whiteMs: 1000, blackMs: 0, serverTime: 1727000009999.75 }],
    ['S_Gesture', { game: 77, ply: 3, touch: 52, aim: 36, placed: P.encodeMove(52, 36, 0), flags: 4, yaw: 700, pitch: -300, lean: 55 }],
    ['S_Gesture', { game: 1, ply: 0, touch: 64, aim: 64, placed: 0, flags: 0, yaw: 0, pitch: 0, lean: 0 }],
    ['RatingUpdate', { game: 77, category: '3+2', white: { before: 1500, after: 1516, games: 1, provisional: true }, black: { before: 1600, after: 1584, games: 31, provisional: false } }],
];

function sameJson(a, b) { return JSON.stringify(a) === JSON.stringify(b); }

const valid = [];
for (const [name, msg] of samples) {
    const buf = P.encode[name](msg);
    const back = P.decode(buf);
    const { type, ...fields } = back;
    if (type !== byName.get(name).id) throw new Error(`${name}: type`);
    if (!raw(name, msg).equals(buf)) throw new Error(`${name}: raw writer differs from encode()`);
    if (!P.encode[name](fields).equals(buf)) throw new Error(`${name}: decode/encode not stable`);
    valid.push({ name, msg: fields, hex: buf.toString('hex') });
}
const seen = new Set(valid.map((v) => v.name));
for (const m of schema.messages) if (!seen.has(P.messageName(m.id))) throw new Error(`no valid sample for ${P.messageName(m.id)}`);

// ---- malformed frames --------------------------------------------------------------------------

const malformed = [];
function bad(name, buf, reason) {
    try {
        P.decode(buf);
    } catch (e) {
        if (!(e instanceof P.ProtocolError)) throw e;
        malformed.push({ name, hex: buf.toString('hex'), reason: e.reason });
        return;
    }
    throw new Error(`malformed vector '${name}' was accepted by the JS codec (${reason})`);
}
const sample = (name, i = 0) => samples.filter(([n]) => n === name)[i][1];
const mod = (name, patch, i = 0) => raw(name, { ...sample(name, i), ...patch });

bad('empty', Buffer.alloc(0));
for (const t of [0x00, 0x04, 0x0f, 0x29, 0x7f, 0x80 - 1, 0x86, 0xa7, 0xff]) bad(`unknown type 0x${t.toString(16)}`, Buffer.from([t, 0, 0, 0, 0]));
for (const v of valid) {
    const buf = Buffer.from(v.hex, 'hex');
    bad(`${v.name}: truncated by one byte`, buf.subarray(0, buf.length - 1));
    bad(`${v.name}: trailing byte`, Buffer.concat([buf, Buffer.from([0])]));
    if (buf.length > 2) bad(`${v.name}: type byte only`, buf.subarray(0, 1));
}
// strings: bounds, UTF-8, NUL
bad('Hello: token below min', mod('Hello', { token: 'x'.repeat(15) }));
bad('Hello: token above max', mod('Hello', { token: 'x'.repeat(161) }));
bad('Hello: client above max', mod('Hello', { client: 'c'.repeat(49) }));
bad('Hello: token with NUL', mod('Hello', { token: Buffer.from('sct_aaaaaaaaaaaa\0aaaa') }));
bad('Hello: client invalid UTF-8', mod('Hello', { client: Buffer.from([0x41, 0xc3, 0x28]) }));
bad('Hello: client overlong', mod('Hello', { client: Buffer.from([0xc0, 0xaf]) }));
bad('Hello: client overlong 3 bytes', mod('Hello', { client: Buffer.from([0xe0, 0x80, 0xaf]) }));
bad('Hello: client surrogate', mod('Hello', { client: Buffer.from([0xed, 0xa0, 0x80]) }));
bad('Hello: client above U+10FFFF', mod('Hello', { client: Buffer.from([0xf4, 0x90, 0x80, 0x80]) }));
bad('Hello: client truncated sequence', mod('Hello', { client: Buffer.from([0x61, 0xe2, 0x82]) }));
bad('Hello: client lone continuation', mod('Hello', { client: Buffer.from([0x80]) }));
bad('Hello: client 0xFF byte', mod('Hello', { client: Buffer.from([0xff]) }));
bad('QueueJoin: category below min', mod('QueueJoin', { category: '3+' }));
bad('QueueJoin: category above max', mod('QueueJoin', { category: '180+1800' }));
bad('ChallengeJoinCode: code below min', mod('ChallengeJoinCode', { code: 'ABC' }));
bad('ChallengeJoinCode: code above max', mod('ChallengeJoinCode', { code: 'ABCDEFGHJKLMN' }));
bad('ChallengeCreate: target above max', mod('ChallengeCreate', { target: 't'.repeat(25) }));
bad('Welcome: serverName above max', mod('Welcome', { serverName: 's'.repeat(65) }));
bad('GameSnapshot: white name empty (min 1)', mod('GameSnapshot', { white: { ...alice, name: '' } }));
bad('GameSnapshot: black name above max', mod('GameSnapshot', { black: { ...bob, name: 'n'.repeat(25) } }));
bad('ChallengeReceived: from name with NUL', mod('ChallengeReceived', { from: { ...bob, name: Buffer.from('a\0b') } }));
// numeric opts
bad('Move: ply above max', mod('Move', { ply: 1200 }));
bad('Move: move bit 15', mod('Move', { move: 0x8000 }));
bad('ChallengeCreate: baseSec below min', mod('ChallengeCreate', { baseSec: 14 }));
bad('ChallengeCreate: baseSec above max', mod('ChallengeCreate', { baseSec: 10801 }));
bad('ChallengeCreate: incSec above max', mod('ChallengeCreate', { incSec: 181 }));
bad('GameSnapshot: MoveRec move bit 15', mod('GameSnapshot', { moves: [{ move: 0x8000, spentMs: 0, clockMs: 0 }] }));
// bools
bad('Move: drawOffer = 2', mod('Move', { drawOffer: 2 }));
bad('QueueJoin: rated = 255', mod('QueueJoin', { rated: 255 }));
bad('GameSnapshot: white provisional = 2', mod('GameSnapshot', { white: { ...alice, provisional: 2 } }));
bad('RatingUpdate: black provisional = 3', mod('RatingUpdate', { black: { before: 1, after: 2, games: 3, provisional: 3 } }));
// enums
bad('ChallengeCreate: color = 3', mod('ChallengeCreate', { color: 3 }));
bad('GameSnapshot: you = 3', mod('GameSnapshot', { you: 3 }));
bad('GameSnapshot: status = 5', mod('GameSnapshot', { status: 5 }));
bad('GameSnapshot: reason = 14 (gap)', mod('GameSnapshot', { reason: 14 }));
bad('GameSnapshot: reason = 27', mod('GameSnapshot', { reason: 27 }));
bad('GameEvent: kind = 0', mod('GameEvent', { kind: 0 }));
bad('GameEvent: kind = 8', mod('GameEvent', { kind: 8 }));
bad('Notice: code = 0', mod('Notice', { code: 0 }));
bad('Error: code = 0', mod('Error', { code: 0 }));
bad('Error: code = 12', mod('Error', { code: 12 }));
bad('Error: code = 45 (301 & 0xFF)', mod('Error', { code: 45 }));
bad('QueueStatus: state = 3', mod('QueueStatus', { state: 3 }));
bad('ChallengeStatus: state = 6', mod('ChallengeStatus', { state: 6 }));
// id53, f64
bad('Move: game = 2^53', mod('Move', { game: 2n ** 53n }));
bad('Move: game = 2^64 - 1', mod('Move', { game: 2n ** 64n - 1n }));
bad('Welcome: activeGame = 2^53', mod('Welcome', { activeGame: 2n ** 53n }));
bad('MoveMade: serverTime NaN', mod('MoveMade', { serverTime: NaN }));
bad('MoveMade: serverTime +Inf', mod('MoveMade', { serverTime: Infinity }));
bad('Notice: arg -Inf', mod('Notice', { arg: -Infinity }));
bad('GameSnapshot: startedAt NaN', mod('GameSnapshot', { startedAt: NaN }));
// lists
bad('GameSnapshot: 1201 moves', mod('GameSnapshot', { moves: Array.from({ length: 1201 }, () => ({ move: 796, spentMs: 1, clockMs: 2 })) }));
{
    // count says 3 moves, only 2 present
    const buf = mod('GameSnapshot', { moves: moves.slice(0, 2) });
    const at = 1 + 8 + 4 + 1 + 3 + 4 + 4 + 1 + (4 + 1 + 5 + 2 + 1) + (4 + 1 + Buffer.byteLength(bob.name) + 2 + 1) + 1;
    if (buf.readUInt16LE(at) !== 2) throw new Error('list count offset');
    buf.writeUInt16LE(3, at);
    bad('GameSnapshot: list count beyond the data', buf);
}
// direction does not matter for decode() without opts; C++ decode is per type, so a frame of
// another type must be refused by each typed decoder (checked in the C++ test itself).

// ---- FNV-1a (posHash) --------------------------------------------------------------------------

const fnvTexts = [
    '',
    'rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq -',
    'rnbqkbnr/pppppppp/8/8/4P3/8/PPPP1PPP/RNBQKBNR b KQkq -',
    'r3k2r/8/8/3pP3/8/8/8/R3K2R w KQkq d6',
    '8/8/8/8/8/8/8/K6k w - -',
];
const fnv1a32 = fnvTexts.map((text) => ({ text, hash: P.fnv1a32(text) }));

const out = {
    generator: 'dedicated-server/tools/gen-cpp-test-vectors.js',
    codec: 'dedicated-server/src/protocol/index.js',
    protocolVersion: P.PROTOCOL_VERSION,
    schemaHash: P.SCHEMA_HASH,
    valid,
    malformed,
    fnv1a32,
};
fs.mkdirSync(path.dirname(outPath), { recursive: true });
fs.writeFileSync(outPath, JSON.stringify(out, null, 1) + '\n');
console.log(`wrote ${path.relative(process.cwd(), outPath)}: ${valid.length} valid, ${malformed.length} malformed, ${fnv1a32.length} fnv1a32`);
