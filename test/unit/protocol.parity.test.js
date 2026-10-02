// Differential tests: the generated codec (codec.gen.js) against the interpreted reference
// (codec.interpreted.js, the placeholder it replaced): same bytes, same decoded values, same
// acceptance of arbitrary inputs. A synthetic schema exercises type combinations that the real
// schema does not use yet (scalar lists, lists of variable-size structs, i32, sparse enums,
// nested structs, empty messages), through a codec generated on the fly.

import { test } from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { pathToFileURL } from 'node:url';
import * as gen from '../../src/protocol/codec.gen.js';
import * as ref from '../../src/protocol/codec.interpreted.js';
import * as schema from '../../src/protocol/schema.js';
import { generateCodec } from '../../tools/gen-protocol.js';
import { randomFields, mulberry32 } from '../../tools/gen-protocol-vectors.js';

const vectors = JSON.parse(fs.readFileSync(new URL('../fixtures/protocol-vectors.json', import.meta.url), 'utf8'));

// Outcome of decoding with one codec: { ok, value } or { ok: false, reason }; anything but the
// codec's ProtocolError is rethrown (a test failure).
function attempt(codec, buf, opts) {
    try {
        return { ok: true, value: codec.decode(buf, opts) };
    } catch (e) {
        if (!(e instanceof codec.ProtocolError)) throw e;
        return { ok: false, reason: e.reason };
    }
}

function mutations(buf, rnd, count) {
    const out = [];
    for (let k = 0; k < count; k++) {
        const b = Buffer.from(buf);
        const r = rnd();
        if (r < 0.4 && b.length > 1) {
            const i = 1 + Math.floor(rnd() * (b.length - 1));
            b[i] ^= 1 << Math.floor(rnd() * 8);
            out.push(b);
        } else if (r < 0.55) {
            out.push(b.subarray(0, Math.floor(rnd() * b.length)));
        } else if (r < 0.7) {
            const extra = Buffer.alloc(1 + Math.floor(rnd() * 6));
            for (let i = 0; i < extra.length; i++) extra[i] = Math.floor(rnd() * 256);
            out.push(Buffer.concat([b, extra]));
        } else if (r < 0.85 && b.length > 1) {
            const i = 1 + Math.floor(rnd() * (b.length - 1));
            b[i] = [0, 1, 2, 0x7f, 0x80, 0xff][Math.floor(rnd() * 6)];
            out.push(b);
        } else {
            const i = Math.floor(rnd() * (b.length + 1));
            out.push(Buffer.concat([b.subarray(0, i), Buffer.from([Math.floor(rnd() * 256)]), b.subarray(i)]));
        }
    }
    return out;
}

test('same constants and tables', () => {
    assert.equal(gen.SCHEMA_HASH, ref.SCHEMA_HASH);
    assert.equal(gen.PROTOCOL_VERSION, ref.PROTOCOL_VERSION);
    assert.equal(gen.PROTOCOL_MIN, ref.PROTOCOL_MIN);
    assert.equal(gen.WS_SUBPROTOCOL, ref.WS_SUBPROTOCOL);
    assert.deepEqual({ ...gen.MSG }, { ...ref.MSG });
    assert.deepEqual(Object.keys(gen.encode), Object.keys(ref.encode));
    assert.deepEqual(gen.enums, ref.enums);
    for (let t = 0; t < 256; t++) {
        assert.equal(gen.messageName(t), ref.messageName(t));
        assert.equal(gen.isClientType(t), ref.isClientType(t));
    }
});

test('identical bytes and values for every golden vector', () => {
    for (const v of vectors.valid) {
        const a = gen.encode[v.name](v.fields), b = ref.encode[v.name](v.fields);
        assert.ok(a.equals(b), `${v.name} (${v.note})`);
        assert.deepEqual(gen.decode(a), ref.decode(b));
    }
});

test('identical reasons for every malformed vector', () => {
    for (const v of vectors.malformed) {
        const buf = Buffer.from(v.hex, 'hex');
        const a = attempt(gen, buf, { dir: v.dir }), b = attempt(ref, buf, { dir: v.dir });
        assert.equal(a.ok, false);
        assert.equal(b.ok, false);
        assert.equal(a.reason, b.reason, v.note);
    }
});

test('both codecs refuse ±Infinity in an f64 on encode and write NaN as 0', () => {
    for (const c of [gen, ref]) {
        for (const v of [Infinity, -Infinity]) assert.throws(() => c.encode.S_Pong({ nonce: 1, serverTime: v }), (e) => e.reason === 'serverTime not finite');
    }
    assert.ok(gen.encode.S_Pong({ nonce: 1, serverTime: NaN }).equals(ref.encode.S_Pong({ nonce: 1, serverTime: NaN })));
});

test('random valid messages: identical bytes, decoded back unchanged', () => {
    const rnd = mulberry32(0x5eed);
    for (const m of schema.messages) {
        const key = gen.messageName(m.id);
        for (let i = 0; i < 150; i++) {
            const fields = randomFields(schema, m.fields, rnd);
            const a = gen.encode[key](fields), b = ref.encode[key](fields);
            assert.ok(a.equals(b), `${key}: ${JSON.stringify(fields)}`);
            assert.deepEqual(gen.decode(a), { type: m.id, ...fields });
        }
    }
});

test('differential decoding of mutated messages', () => {
    const rnd = mulberry32(42);
    let accepted = 0, total = 0;
    const bases = vectors.valid.filter((v) => v.hex.length < 4000).map((v) => Buffer.from(v.hex, 'hex'));
    for (const base of bases) {
        for (const buf of mutations(base, rnd, 250)) {
            const opts = [undefined, { dir: 'c2s' }, { dir: 's2c' }][Math.floor(rnd() * 3)];
            const a = attempt(gen, buf, opts), b = attempt(ref, buf, opts);
            total++;
            assert.equal(a.ok, b.ok, `${buf.toString('hex')}: generated ${a.ok ? 'accepts' : a.reason}, reference ${b.ok ? 'accepts' : b.reason}`);
            if (a.ok) { accepted++; assert.deepEqual(a.value, b.value); }
        }
    }
    assert.ok(total > 10000 && accepted > 100, `${accepted}/${total}`);
});

// ---- synthetic schema -----------------------------------------------------------------------------

const synthetic = {
    PROTOCOL_VERSION: 3,
    PROTOCOL_MIN: 2,
    WS_SUBPROTOCOL: 'synthetic.v3',
    enums: {
        Sparse: { A: 0, B: 7, C: 200, D: 255 },
        Offset: { X: 3, Y: 4, Z: 5 },
        Solo: { Only: 9 },
    },
    MoveFlag: { One: 1 },
    GestureFlag: { Two: 2 },
    CloseCode: { Normal: 1000 },
    structs: {
        Fixed: [['a', 'u8', { min: 1, max: 9 }], ['b', 'f64'], ['c', 'bool'], ['d', 'enum:Offset']],
        Nested: [['f', 'struct:Fixed'], ['g', 'u32', { max: 70000 }], ['h', 'id53']],
        Inner: [['i', 'i32', { min: -5, max: 1000 }], ['tag', 'str8', { max: 4 }], ['e', 'enum:Sparse']],
        Outer: [['id', 'id53'], ['inner', 'struct:Inner'], ['names', 'list16:str8', { max: 3 }], ['w', 'u16', { min: 10 }], ['fx', 'list16:struct:Fixed', { max: 2 }]],
    },
    messages: [
        { id: 0x01, name: 'Ping', dir: 'c2s', fields: [['seq', 'u32']] },
        { id: 0x81, name: 'Ping', dir: 's2c', fields: [] },
        { id: 0x05, name: 'Lists', dir: 'c2s', fields: [['seq', 'u32'], ['u8s', 'list16:u8'], ['i32s', 'list16:i32', { max: 5 }], ['ids', 'list16:id53'],
            ['fs', 'list16:f64'], ['bs', 'list16:bool'], ['es', 'list16:enum:Sparse'], ['strs', 'list16:str8'], ['tail', 'u16']] },
        { id: 0x06, name: 'Signed', dir: 'c2s', fields: [['seq', 'u32'], ['x', 'i32'], ['y', 'i32', { min: -100, max: 100 }], ['z', 'u32', { min: 5, max: 70000 }], ['q', 'u8', { min: 3 }], ['s', 'enum:Solo']] },
        { id: 0x90, name: 'Deep', dir: 's2c', fields: [['o', 'struct:Outer'], ['outs', 'list16:struct:Outer', { max: 4 }], ['nest', 'list16:struct:Nested'],
            ['n', 'struct:Nested'], ['sp', 'enum:Sparse'], ['last', 'str8', { min: 2, max: 10 }]] },
        { id: 0xa0, name: 'Strings', dir: 's2c', fields: [['a', 'str8'], ['b', 'str8', { min: 1 }], ['c', 'str8', { max: 0 }], ['d', 'bool']] },
    ],
};

test('generator on a synthetic schema: generated and interpreted codecs agree', async () => {
    const file = path.join(os.tmpdir(), `scacelith-synthetic-codec-${process.pid}-${Date.now()}.mjs`);
    fs.writeFileSync(file, generateCodec(synthetic));
    let g;
    try { g = await import(pathToFileURL(file).href); } finally { fs.rmSync(file, { force: true }); }
    const r = ref.createCodec(synthetic);
    assert.equal(g.SCHEMA_HASH, r.SCHEMA_HASH);
    assert.deepEqual({ ...g.MSG }, { C_Ping: 0x01, S_Ping: 0x81, Lists: 0x05, Signed: 0x06, Deep: 0x90, Strings: 0xa0 });
    assert.deepEqual(g.decode(g.encode.S_Ping({})), { type: 0x81 });
    assert.throws(() => g.decode(Buffer.from([0x81, 0])), (e) => e.reason === 'trailing bytes');

    const rnd = mulberry32(7);
    let accepted = 0;
    for (const m of synthetic.messages) {
        const key = g.messageName(m.id);
        for (let i = 0; i < 300; i++) {
            const fields = randomFields(synthetic, m.fields, rnd);
            const a = g.encode[key](fields), b = r.encode[key](fields);
            assert.ok(a.equals(b), `${key} ${JSON.stringify(fields)}`);
            assert.deepEqual(g.decode(a), { type: m.id, ...fields });
            assert.deepEqual(r.decode(b), { type: m.id, ...fields });
            for (const buf of mutations(a, rnd, 20)) {
                const x = attempt(g, buf), y = attempt(r, buf);
                assert.equal(x.ok, y.ok, `${key} ${buf.toString('hex')}: ${x.reason} / ${y.reason}`);
                if (x.ok) { accepted++; assert.deepEqual(x.value, y.value); }
            }
        }
    }
    assert.ok(accepted > 100);
    // Bounds of the synthetic types are enforced on both sides.
    assert.throws(() => g.encode.Signed({ seq: 1, x: 2 ** 31, y: 0, z: 5, q: 3, s: 9 }), (e) => e.reason === 'x out of range');
    assert.throws(() => g.encode.Signed({ seq: 1, x: 0, y: -101, z: 5, q: 3, s: 9 }), (e) => e.reason === 'y below min');
    assert.throws(() => g.encode.Signed({ seq: 1, x: 0, y: 0, z: 70001, q: 3, s: 9 }), (e) => e.reason === 'z above max');
    assert.throws(() => g.encode.Signed({ seq: 1, x: 0, y: 0, z: 5, q: 2, s: 9 }), (e) => e.reason === 'q below min');
    assert.throws(() => g.encode.Signed({ seq: 1, x: 0, y: 0, z: 5, q: 3, s: 8 }), (e) => e.reason === 's not a Solo');
    assert.throws(() => g.encode.Lists({ seq: 1, i32s: [1, 2, 3, 4, 5, 6], tail: 0 }), (e) => e.reason === 'i32s too long');
    assert.throws(() => g.encode.Lists({ seq: 1, es: [7, 8], tail: 0 }), (e) => e.reason === 'es not a Sparse');
    assert.throws(() => g.encode.Lists({ seq: 1, strs: ['ok', 'a\0b'], tail: 0 }), (e) => e.reason === 'strs contains NUL');
    assert.throws(() => g.encode.Lists({ seq: 1, fs: [1, NaN, Infinity], tail: 0 }), (e) => e.reason === 'fs not finite');
    assert.throws(() => r.encode.Lists({ seq: 1, fs: [1, NaN, Infinity], tail: 0 }), (e) => e.reason === 'fs not finite');
    assert.throws(() => g.encode.Strings({ a: '', b: '', c: '', d: true }), (e) => e.reason === 'b bad length');
    assert.throws(() => g.encode.Strings({ a: '', b: 'x', c: 'y', d: true }), (e) => e.reason === 'c bad length');
    assert.throws(() => g.decode(Buffer.from([0x06, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 5, 0, 0, 0, 3, 8])), (e) => e.reason === 's not a Solo');
});
