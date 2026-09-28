// Reference codec that interprets schema.js at run time: the placeholder that codec.gen.js
// replaced, kept for differential testing (test/unit/protocol.parity.test.js) and for
// `node tools/gen-protocol.js --bench`. Never import it from server code: it is several times
// slower than the generated codec.
//
// Same API and semantics as the original placeholder, with two changes:
//   * createCodec(schema) builds the same API for any schema object (tests use synthetic
//     schemas to exercise the generator on every type combination);
//   * strings are decoded with ignoreBOM: a leading U+FEFF is part of the string, as in the
//     generated JS codec and the C++ codec (the placeholder silently dropped it).

import * as schema from './schema.js';
import { computeSchemaHash } from './schema-hash.js';

export class ProtocolError extends Error {
    constructor(reason) { super(`protocol: ${reason}`); this.reason = reason; }
}

/**
 * Builds the codec API ({ MSG, encode, decode, messageName, isClientType, ... }) for a schema.
 * @param {object} s schema module-like object
 */
export function createCodec(s) {
    const shared = new Set();
    {
        const seen = new Map();
        for (const m of s.messages) seen.set(m.name, (seen.get(m.name) || 0) + 1);
        for (const [n, c] of seen) if (c > 1) shared.add(n);
    }
    const keyOf = (m) => (shared.has(m.name) ? (m.dir === 'c2s' ? 'C_' : 'S_') + m.name : m.name);

    const MSG = {};
    const byId = new Map();
    for (const m of s.messages) { MSG[keyOf(m)] = m.id; byId.set(m.id, m); }
    Object.freeze(MSG);

    const enumSets = {};
    for (const [n, e] of Object.entries(s.enums)) enumSets[n] = new Set(Object.values(e));
    const utf8 = new TextDecoder('utf-8', { fatal: true, ignoreBOM: true });

    function parseType(t) {
        if (t.startsWith('list16:')) return { kind: 'list', item: parseType(t.slice(7)) };
        if (t.startsWith('struct:')) return { kind: 'struct', name: t.slice(7), fields: s.structs[t.slice(7)].map(parseField) };
        if (t.startsWith('enum:')) return { kind: 'enum', name: t.slice(5) };
        return { kind: t };
    }
    function parseField([name, type, opts = {}]) { return { name, t: parseType(type), opts }; }
    const plans = new Map();
    for (const m of s.messages) plans.set(m.id, m.fields.map(parseField));

    function sizeOf(t, v, opts) {
        switch (t.kind) {
            case 'u8': case 'bool': case 'enum': return 1;
            case 'u16': return 2;
            case 'u32': case 'i32': return 4;
            case 'f64': case 'id53': return 8;
            case 'str8': return 1 + Buffer.byteLength(v ?? '', 'utf8');
            case 'struct': return t.fields.reduce((a, f) => a + sizeOf(f.t, v?.[f.name], f.opts), 0);
            case 'list': { let n = 2; for (const x of v ?? []) n += sizeOf(t.item, x, {}); return n; }
            default: throw new Error(`codec: type ${t.kind}`);
        }
    }

    function checkRange(name, v, lo, hi, opts) {
        if (!Number.isInteger(v) || v < lo || v > hi) throw new ProtocolError(`${name} out of range`);
        if (opts.min !== undefined && v < opts.min) throw new ProtocolError(`${name} below min`);
        if (opts.max !== undefined && v > opts.max) throw new ProtocolError(`${name} above max`);
    }

    function write(buf, o, t, v, opts, name) {
        switch (t.kind) {
            case 'u8': checkRange(name, v, 0, 0xff, opts); buf[o] = v; return o + 1;
            case 'bool': buf[o] = v ? 1 : 0; return o + 1;
            case 'enum': if (!enumSets[t.name].has(v)) throw new ProtocolError(`${name} not a ${t.name}`); buf[o] = v; return o + 1;
            case 'u16': checkRange(name, v, 0, 0xffff, opts); buf.writeUInt16LE(v, o); return o + 2;
            case 'u32': checkRange(name, v, 0, 0xffffffff, opts); buf.writeUInt32LE(v, o); return o + 4;
            case 'i32': checkRange(name, v, -0x80000000, 0x7fffffff, opts); buf.writeInt32LE(v, o); return o + 4;
            case 'f64': buf.writeDoubleLE(+v || 0, o); return o + 8;
            case 'id53': {
                const x = v || 0;
                if (!Number.isSafeInteger(x) || x < 0) throw new ProtocolError(`${name} not an id53`);
                buf.writeUInt32LE(x % 0x100000000, o); buf.writeUInt32LE(Math.floor(x / 0x100000000), o + 4); return o + 8;
            }
            case 'str8': {
                const str = v ?? '';
                const n = buf.write(str, o + 1, 'utf8');
                if (n > 255 || (opts.max !== undefined && n > opts.max) || (opts.min !== undefined && n < opts.min) || str.includes('\0')) throw new ProtocolError(`${name} bad length`);
                buf[o] = n; return o + 1 + n;
            }
            case 'struct': for (const f of t.fields) o = write(buf, o, f.t, v?.[f.name], f.opts, `${name}.${f.name}`); return o;
            case 'list': {
                const a = v ?? [];
                if (a.length > 0xffff || (opts.max !== undefined && a.length > opts.max)) throw new ProtocolError(`${name} too long`);
                buf.writeUInt16LE(a.length, o); o += 2;
                for (const x of a) o = write(buf, o, t.item, x, {}, name);
                return o;
            }
        }
        throw new Error('unreachable');
    }

    class Reader {
        constructor(buf) { this.b = buf; this.o = 1; }
        need(n) { if (this.o + n > this.b.length) throw new ProtocolError('truncated'); }
    }

    function read(r, t, opts, name) {
        const b = r.b;
        switch (t.kind) {
            case 'u8': { r.need(1); const v = b[r.o++]; checkRange(name, v, 0, 0xff, opts); return v; }
            case 'bool': { r.need(1); const v = b[r.o++]; if (v > 1) throw new ProtocolError(`${name} not a bool`); return v === 1; }
            case 'enum': { r.need(1); const v = b[r.o++]; if (!enumSets[t.name].has(v)) throw new ProtocolError(`${name} not a ${t.name}`); return v; }
            case 'u16': { r.need(2); const v = b.readUInt16LE(r.o); r.o += 2; checkRange(name, v, 0, 0xffff, opts); return v; }
            case 'u32': { r.need(4); const v = b.readUInt32LE(r.o); r.o += 4; checkRange(name, v, 0, 0xffffffff, opts); return v; }
            case 'i32': { r.need(4); const v = b.readInt32LE(r.o); r.o += 4; checkRange(name, v, -0x80000000, 0x7fffffff, opts); return v; }
            case 'f64': { r.need(8); const v = b.readDoubleLE(r.o); r.o += 8; if (!Number.isFinite(v)) throw new ProtocolError(`${name} not finite`); return v; }
            case 'id53': {
                r.need(8); const lo = b.readUInt32LE(r.o), hi = b.readUInt32LE(r.o + 4); r.o += 8;
                if (hi >= 0x200000) throw new ProtocolError(`${name} above 2^53`);
                return hi * 0x100000000 + lo;
            }
            case 'str8': {
                r.need(1); const n = b[r.o++];
                if ((opts.max !== undefined && n > opts.max) || (opts.min !== undefined && n < opts.min)) throw new ProtocolError(`${name} bad length`);
                r.need(n);
                const bytes = b.subarray(r.o, r.o + n); r.o += n;
                if (bytes.includes(0)) throw new ProtocolError(`${name} contains NUL`);
                try { return utf8.decode(bytes); } catch { throw new ProtocolError(`${name} not UTF-8`); }
            }
            case 'struct': { const o = {}; for (const f of t.fields) o[f.name] = read(r, f.t, f.opts, `${name}.${f.name}`); return o; }
            case 'list': {
                r.need(2); const n = b.readUInt16LE(r.o); r.o += 2;
                if (opts.max !== undefined && n > opts.max) throw new ProtocolError(`${name} too long`);
                const a = new Array(n);
                for (let i = 0; i < n; i++) a[i] = read(r, t.item, {}, name);
                return a;
            }
        }
        throw new Error('unreachable');
    }

    const encode = {};
    for (const m of s.messages) {
        const plan = plans.get(m.id);
        encode[keyOf(m)] = (obj) => {
            let size = 1;
            for (const f of plan) size += sizeOf(f.t, obj[f.name], f.opts);
            const buf = Buffer.allocUnsafe(size);
            buf[0] = m.id;
            let o = 1;
            for (const f of plan) o = write(buf, o, f.t, obj[f.name], f.opts, f.name);
            return buf;
        };
    }
    Object.freeze(encode);

    // Decodes one message. opts.dir: 'c2s' | 's2c' refuses the other direction's types.
    function decode(buf, opts = {}) {
        if (!buf || buf.length < 1) throw new ProtocolError('empty');
        const m = byId.get(buf[0]);
        if (!m) throw new ProtocolError('unknown type');
        if (opts && opts.dir && m.dir !== opts.dir) throw new ProtocolError('wrong direction');
        const r = new Reader(buf);
        const out = { type: m.id };
        for (const f of plans.get(m.id)) out[f.name] = read(r, f.t, f.opts, f.name);
        if (r.o !== buf.length) throw new ProtocolError('trailing bytes');
        return out;
    }

    return {
        PROTOCOL_VERSION: s.PROTOCOL_VERSION,
        PROTOCOL_MIN: s.PROTOCOL_MIN,
        WS_SUBPROTOCOL: s.WS_SUBPROTOCOL,
        SCHEMA_HASH: computeSchemaHash(s),
        enums: s.enums,
        MoveFlag: s.MoveFlag,
        CloseCode: s.CloseCode,
        ProtocolError,
        MSG,
        encode,
        decode,
        isClientType: (t) => t >= 0x01 && t <= 0x7f,
        messageName: (type) => { const m = byId.get(type); return m ? keyOf(m) : null; },
    };
}

const codec = createCodec(schema);

export const PROTOCOL_VERSION = codec.PROTOCOL_VERSION;
export const PROTOCOL_MIN = codec.PROTOCOL_MIN;
export const WS_SUBPROTOCOL = codec.WS_SUBPROTOCOL;
export const SCHEMA_HASH = codec.SCHEMA_HASH;
export const enums = codec.enums;
export const MoveFlag = codec.MoveFlag;
export const CloseCode = codec.CloseCode;
export const MSG = codec.MSG;
export const encode = codec.encode;
export const decode = codec.decode;
export const isClientType = codec.isClientType;
export const messageName = codec.messageName;
