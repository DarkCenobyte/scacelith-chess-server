// Golden vectors (test/fixtures/protocol-vectors.json): byte-exact encoding, decoding, and
// rejection of every malformed input with a ProtocolError carrying the recorded reason.

import { test } from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import * as P from '../../src/protocol/index.js';
import * as schema from '../../src/protocol/schema.js';

const vectors = JSON.parse(fs.readFileSync(new URL('../fixtures/protocol-vectors.json', import.meta.url), 'utf8'));

test('vector file header matches the codec', () => {
    assert.equal(vectors.protocolVersion, P.PROTOCOL_VERSION);
    assert.equal(vectors.protocolMin, P.PROTOCOL_MIN);
    assert.equal(vectors.schemaHash, P.SCHEMA_HASH);
    assert.equal(vectors.schemaHashHex, '0x' + P.SCHEMA_HASH.toString(16).padStart(8, '0'));
    assert.equal(vectors.subprotocol, P.WS_SUBPROTOCOL);
});

test('every message has at least one valid vector', () => {
    const names = new Set(vectors.valid.map((v) => v.name));
    for (const name of Object.keys(P.MSG)) assert.ok(names.has(name), `no vector for ${name}`);
    assert.equal(schema.messages.length, Object.keys(P.MSG).length);
});

test('valid vectors: encode(fields) is byte-exact and decode(hex) gives the fields back', () => {
    for (const v of vectors.valid) {
        assert.equal(v.type, P.MSG[v.name], v.name);
        assert.equal(P.messageName(v.type), v.name);
        const bytes = P.encode[v.name](v.fields);
        assert.equal(bytes.toString('hex'), v.hex, `${v.name} (${v.note})`);
        const msg = P.decode(Buffer.from(v.hex, 'hex'));
        assert.deepEqual(msg, { type: v.type, ...v.fields }, `${v.name} (${v.note})`);
        // Key order: type first, then schema order (JSON of a decoded message is stable).
        assert.deepEqual(Object.keys(msg), ['type', ...Object.keys(v.fields)]);
        // Accepted in its own direction, refused in the other one.
        assert.deepEqual(P.decode(Buffer.from(v.hex, 'hex'), { dir: v.dir }), msg);
        const other = v.dir === 'c2s' ? 's2c' : 'c2s';
        assert.throws(() => P.decode(Buffer.from(v.hex, 'hex'), { dir: other }), (e) => e instanceof P.ProtocolError && e.reason === 'wrong direction');
        assert.equal(P.isClientType(v.type), v.dir === 'c2s');
    }
});

test('malformed vectors are refused with a ProtocolError and the recorded reason', () => {
    assert.ok(vectors.malformed.length >= 60);
    for (const v of vectors.malformed) {
        const buf = Buffer.from(v.hex, 'hex');
        let err = null;
        try { P.decode(buf, { dir: v.dir }); } catch (e) { err = e; }
        assert.ok(err instanceof P.ProtocolError, `${v.note}: ${err}`);
        assert.equal(err.reason, v.reason, v.note);
        assert.equal(err.message, `protocol: ${v.reason}`);
        if (v.type !== null) assert.equal(P.messageName(v.type), v.name);
    }
});

test('malformed vectors cover the required defect classes', () => {
    const reasons = vectors.malformed.map((v) => v.reason);
    const has = (re) => reasons.some((r) => re.test(r));
    for (const re of [/^empty$/, /^truncated$/, /^trailing bytes$/, /^unknown type$/, /^wrong direction$/, /not a ErrorCode$/,
        /not a bool$/, /not UTF-8$/, /contains NUL$/, /bad length$/, /too long$/, /above 2\^53$/, /^move above max$/, /not finite$/]) {
        assert.ok(has(re), `no malformed vector for ${re}`);
    }
});

test('decoding the valid vectors from other views: Uint8Array, ArrayBuffer, offset slices', () => {
    for (const v of vectors.valid.slice(0, 40)) {
        const bytes = Buffer.from(v.hex, 'hex');
        const expected = { type: v.type, ...v.fields };
        assert.deepEqual(P.decode(new Uint8Array(bytes)), expected);
        const ab = new ArrayBuffer(bytes.length);
        new Uint8Array(ab).set(bytes);
        assert.deepEqual(P.decode(ab), expected);
        const big = Buffer.alloc(bytes.length + 7, 0xee);
        bytes.copy(big, 3);
        assert.deepEqual(P.decode(big.subarray(3, 3 + bytes.length)), expected);
    }
});
