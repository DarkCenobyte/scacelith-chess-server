// Wire helpers of the load generator: WebSocket client frames and a fast path for the messages
// of the hot loop (Move, C_Ping, C_Pong out; MoveMade, S_Ping, S_Pong in).
//
// The fast path writes and reads fixed-offset fields directly. The offsets are computed from
// src/protocol/schema.js (not hard-coded), and selfCheck() compares the fast encoders and readers
// with the codec (src/protocol/index.js) at start-up: when the schema changes shape in a way the
// fast path does not cover, the load generator falls back to the codec for everything. Every
// other message goes through the codec.

import * as schema from '../../src/protocol/schema.js';
import { MSG, encode, decode, enums, SCHEMA_HASH, PROTOCOL_VERSION, WS_SUBPROTOCOL } from '../../src/protocol/index.js';

export { MSG, encode, decode, enums, SCHEMA_HASH, PROTOCOL_VERSION, WS_SUBPROTOCOL };

const FIXED = { u8: 1, bool: 1, u16: 2, u32: 4, i32: 4, f64: 8, id53: 8 };

/** Offsets (type byte included) of the leading fixed-size fields of a message, and its size when all fields are fixed. */
function layout(name, dir) {
    const m = schema.messages.find((x) => x.name === name && x.dir === dir);
    if (!m) return null;
    const off = {};
    let o = 1, fixed = true;
    for (const [field, type] of m.fields) {
        const sz = type.startsWith('enum:') ? 1 : FIXED[type];
        if (!sz) { fixed = false; break; }
        off[field] = { o, type };
        o += sz;
    }
    return { id: m.id, off, size: fixed ? o : -1 };
}

const L = {
    Move: layout('Move', 'c2s'),
    C_Ping: layout('Ping', 'c2s'),
    C_Pong: layout('Pong', 'c2s'),
    MoveMade: layout('MoveMade', 's2c'),
    S_Ping: layout('Ping', 's2c'),
    S_Pong: layout('Pong', 's2c'),
};

function put(buf, base, f, v) {
    const o = base + f.o;
    switch (f.type) {
        case 'u8': case 'bool': buf[o] = +v; return;
        case 'u16': buf.writeUInt16LE(v, o); return;
        case 'u32': buf.writeUInt32LE(v >>> 0, o); return;
        case 'i32': buf.writeInt32LE(v | 0, o); return;
        case 'f64': buf.writeDoubleLE(v, o); return;
        case 'id53': buf.writeUInt32LE(v % 0x100000000, o); buf.writeUInt32LE(Math.floor(v / 0x100000000), o + 4); return;
        default: buf[o] = v;
    }
}

function get(buf, base, f) {
    const o = base + f.o;
    switch (f.type) {
        case 'u8': return buf[o];
        case 'bool': return buf[o] === 1;
        case 'u16': return buf.readUInt16LE(o);
        case 'u32': return buf.readUInt32LE(o);
        case 'i32': return buf.readInt32LE(o);
        case 'f64': return buf.readDoubleLE(o);
        case 'id53': return buf.readUInt32LE(o + 4) * 0x100000000 + buf.readUInt32LE(o);
        default: return buf[o];
    }
}

/**
 * Masks a client message into a WebSocket binary frame (FIN, opcode 2). Small messages only
 * (< 65536 bytes). `mask` is a 4-byte key.
 * @param {Buffer} payload
 * @param {Buffer} mask
 * @returns {Buffer}
 */
export function clientFrame(payload, mask) {
    const len = payload.length;
    const hl = len < 126 ? 2 : 4;
    const f = Buffer.allocUnsafe(hl + 4 + len);
    f[0] = 0x82;
    if (len < 126) f[1] = 0x80 | len;
    else { f[1] = 0x80 | 126; f.writeUInt16BE(len, 2); }
    f[hl] = mask[0]; f[hl + 1] = mask[1]; f[hl + 2] = mask[2]; f[hl + 3] = mask[3];
    const o = hl + 4;
    for (let i = 0; i < len; i++) f[o + i] = payload[i] ^ mask[i & 3];
    return f;
}

/** Masked close frame (code 1000). */
export function closeFrame(mask) {
    const p = Buffer.from([0x03, 0xe8]);
    const f = Buffer.allocUnsafe(8);
    f[0] = 0x88; f[1] = 0x82;
    mask.copy(f, 2, 0, 4);
    f[6] = p[0] ^ mask[0]; f[7] = p[1] ^ mask[1];
    return f;
}

let fast = false;

// Fast encoders: build the payload in place inside the frame (header 2 + mask 4), then mask.
function fastFrame(lay, values, mask) {
    const len = lay.size;
    const f = Buffer.allocUnsafe(6 + len);
    f[0] = 0x82; f[1] = 0x80 | len;
    f[2] = mask[0]; f[3] = mask[1]; f[4] = mask[2]; f[5] = mask[3];
    f[6] = lay.id;
    for (const k in values) put(f, 6, lay.off[k], values[k]);
    for (let i = 0; i < len; i++) f[6 + i] ^= mask[i & 3];
    return f;
}

// Hand-unrolled Move encoder (the hottest one); offsets come from the layout.
let MV = null;
function fastMove(seq, game, ply, move, posHash, thinkMs, mask) {
    const len = L.Move.size;
    const f = Buffer.allocUnsafe(6 + len);
    f[0] = 0x82; f[1] = 0x80 | len;
    f[2] = mask[0]; f[3] = mask[1]; f[4] = mask[2]; f[5] = mask[3];
    f[6] = L.Move.id;
    f.writeUInt32LE(seq >>> 0, 6 + MV.seq);
    f.writeUInt32LE(game % 0x100000000, 6 + MV.game);
    f.writeUInt32LE(Math.floor(game / 0x100000000), 6 + MV.game + 4);
    f.writeUInt16LE(ply, 6 + MV.ply);
    f.writeUInt16LE(move, 6 + MV.move);
    f.writeUInt32LE(posHash >>> 0, 6 + MV.posHash);
    f.writeUInt32LE(thinkMs >>> 0, 6 + MV.thinkMs);
    f[6 + MV.drawOffer] = 0;
    for (let i = 0; i < len; i++) f[6 + i] ^= mask[i & 3];
    return f;
}

/** Move frame. */
export function moveFrame(seq, game, ply, move, posHash, thinkMs, mask) {
    if (fast) return fastMove(seq, game, ply, move, posHash, thinkMs, mask);
    return clientFrame(encode.Move({ seq, game, ply, move, posHash, thinkMs, drawOffer: false }), mask);
}

/** C_Pong frame (answer to the server heartbeat). */
export function pongFrame(seq, nonce, mask) {
    if (fast) return fastFrame(L.C_Pong, { seq, nonce }, mask);
    return clientFrame(encode.C_Pong({ seq, nonce }), mask);
}

/** C_Ping frame (client round-trip measurement). */
export function pingFrame(seq, nonce, mask) {
    if (fast) return fastFrame(L.C_Ping, { seq, nonce }, mask);
    return clientFrame(encode.C_Ping({ seq, nonce }), mask);
}

/** Reads MoveMade { game, ply, move } from a payload at offset `o`. */
export function readMoveMade(buf, o, len, out) {
    if (fast && len === L.MoveMade.size) {
        out.game = get(buf, o, L.MoveMade.off.game);
        out.ply = get(buf, o, L.MoveMade.off.ply);
        out.move = get(buf, o, L.MoveMade.off.move);
        return out;
    }
    const m = decode(buf.subarray(o, o + len), { dir: 's2c' });
    out.game = m.game; out.ply = m.ply; out.move = m.move;
    return out;
}

/** Nonce of an S_Ping / S_Pong payload at offset `o`. */
export function readNonce(buf, o, len, layoutName = 'S_Ping') {
    if (fast && len === L[layoutName].size) return get(buf, o, L[layoutName].off.nonce);
    return decode(buf.subarray(o, o + len), { dir: 's2c' }).nonce;
}

function unmask(frame) {
    const len = frame[1] & 0x7f;
    const o = len < 126 ? 2 : 4;
    const mask = frame.subarray(o, o + 4);
    const p = Buffer.allocUnsafe(frame.length - o - 4);
    for (let i = 0; i < p.length; i++) p[i] = frame[o + 4 + i] ^ mask[i & 3];
    return p;
}

/**
 * Enables the fast path when it produces exactly the codec's bytes and values.
 * @returns {{ fast: boolean, reason?: string }}
 */
export function selfCheck() {
    fast = false;
    try {
        for (const k of Object.keys(L)) if (!L[k] || L[k].size < 0 || L[k].size > 125) return { fast, reason: `layout of ${k} not fixed` };
        MV = {};
        for (const k of ['seq', 'game', 'ply', 'move', 'posHash', 'thinkMs', 'drawOffer']) {
            if (!L.Move.off[k]) return { fast, reason: `Move has no ${k}` };
            MV[k] = L.Move.off[k].o;
        }
        if (Object.keys(L.Move.off).length !== 7) return { fast, reason: 'Move has other fields' };
        const mask = Buffer.from([0x12, 0x34, 0x56, 0x78]);
        const mv = { seq: 123456, game: 2 ** 40 + 12345, ply: 77, move: 0x1234, posHash: 0xdeadbeef, thinkMs: 1500, drawOffer: false };
        const a = unmask(fastMove(mv.seq, mv.game, mv.ply, mv.move, mv.posHash, mv.thinkMs, mask));
        if (!a.equals(encode.Move(mv))) return { fast, reason: 'Move bytes differ from the codec' };
        const pg = unmask(fastFrame(L.C_Pong, { seq: 9, nonce: 0xabcdef01 }, mask));
        if (!pg.equals(encode.C_Pong({ seq: 9, nonce: 0xabcdef01 }))) return { fast, reason: 'C_Pong bytes differ' };
        const pi = unmask(fastFrame(L.C_Ping, { seq: 10, nonce: 7 }, mask));
        if (!pi.equals(encode.C_Ping({ seq: 10, nonce: 7 }))) return { fast, reason: 'C_Ping bytes differ' };
        const mm = encode.MoveMade({ game: 2 ** 41 + 99, gseq: 5, ply: 12, move: 0x0abc, flags: 0, spentMs: 10, whiteMs: 1000, blackMs: 2000, serverTime: Date.now(), drawOffer: false, firstMoveMs: 0 });
        fast = true;
        const r = readMoveMade(mm, 0, mm.length, {});
        const sp = encode.S_Ping({ nonce: 0x01020304, serverTime: 1 });
        const ok = r.game === 2 ** 41 + 99 && r.ply === 12 && r.move === 0x0abc && mm.length === L.MoveMade.size
            && readNonce(sp, 0, sp.length, 'S_Ping') === 0x01020304 && sp.length === L.S_Ping.size;
        if (!ok) { fast = false; return { fast, reason: 'MoveMade/S_Ping reader differs from the codec' }; }
        return { fast };
    } catch (e) {
        fast = false;
        return { fast, reason: e.message };
    }
}

/** Type bytes. */
export const T = {
    Welcome: MSG.Welcome, Error: MSG.Error, S_Ping: MSG.S_Ping, S_Pong: MSG.S_Pong, Ack: MSG.Ack, Notice: MSG.Notice,
    QueueStatus: MSG.QueueStatus, ChallengeReceived: MSG.ChallengeReceived, ChallengeStatus: MSG.ChallengeStatus,
    GameSnapshot: MSG.GameSnapshot, MoveMade: MSG.MoveMade, MoveRejected: MSG.MoveRejected, GameEvent: MSG.GameEvent,
    GameEnd: MSG.GameEnd, RatingUpdate: MSG.RatingUpdate,
};
