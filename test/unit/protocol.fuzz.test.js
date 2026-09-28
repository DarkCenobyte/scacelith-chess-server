// Fuzzing the generated codec: random buffers and mutated valid messages never make decode throw
// anything but ProtocolError, never hang, and whatever it accepts re-encodes to the same bytes
// (the encoding is canonical). Random valid messages round-trip exactly.

import { test } from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import * as P from '../../src/protocol/index.js';
import * as schema from '../../src/protocol/schema.js';
import { randomFields, mulberry32 } from '../../tools/gen-protocol-vectors.js';

const vectors = JSON.parse(fs.readFileSync(new URL('../fixtures/protocol-vectors.json', import.meta.url), 'utf8'));
const TYPES = schema.messages.map((m) => m.id);
const DIRS = [undefined, { dir: 'c2s' }, { dir: 's2c' }];

function hasNegativeZero(v) {
    if (Object.is(v, -0)) return true;
    if (v && typeof v === 'object') return Object.values(v).some(hasNegativeZero);
    return false;
}

// Decodes; returns the message or null; fails the test on any other exception type. An accepted
// buffer must re-encode to exactly the same bytes (f64 -0 aside: encode normalises it to +0).
function check(buf, opts) {
    let msg;
    try {
        msg = P.decode(buf, opts);
    } catch (e) {
        if (!(e instanceof P.ProtocolError)) throw new Error(`decode threw ${e && e.name}: ${e && e.message} on ${buf.toString('hex')}`);
        assert.equal(typeof e.reason, 'string');
        return null;
    }
    assert.equal(msg.type, buf[0]);
    if (opts) assert.equal(P.isClientType(msg.type), opts.dir === 'c2s');
    const { type, ...fields } = msg;
    const again = P.encode[P.messageName(type)](fields);
    if (!hasNegativeZero(fields)) assert.ok(again.equals(buf), `re-encoding differs for ${buf.toString('hex')}`);
    else assert.deepEqual(P.decode(again), JSON.parse(JSON.stringify(msg)));
    return msg;
}

test('200k random buffers: only ProtocolError, never a hang', { timeout: 120000 }, () => {
    const rnd = mulberry32(0xf022);
    let accepted = 0;
    const scratch = Buffer.alloc(4096);
    const t0 = performance.now();
    for (let i = 0; i < 200000; i++) {
        const r = rnd();
        const len = r < 0.2 ? Math.floor(rnd() * 9) : r < 0.7 ? 9 + Math.floor(rnd() * 56) : r < 0.95 ? 65 + Math.floor(rnd() * 236) : 301 + Math.floor(rnd() * 3700);
        for (let k = 0; k < len; k++) scratch[k] = (rnd() * 256) | 0;
        if (len > 0 && rnd() < 0.75) scratch[0] = TYPES[Math.floor(rnd() * TYPES.length)];
        // Plausible structure now and then: small string lengths and list counts.
        if (len > 16 && rnd() < 0.3) for (let k = 1; k < len; k += 1 + Math.floor(rnd() * 8)) scratch[k] = Math.floor(rnd() * 8);
        if (check(Buffer.from(scratch.subarray(0, len)), DIRS[i % 3])) accepted++;
    }
    const ms = performance.now() - t0;
    assert.ok(ms < 60000, `took ${ms} ms`);
    assert.ok(accepted > 0, 'some random buffers are valid messages');
});

test('mutated valid messages (bit flips, truncation, extension, insertion, deletion)', { timeout: 120000 }, () => {
    const rnd = mulberry32(99);
    let total = 0, accepted = 0;
    const bases = vectors.valid.map((v) => Buffer.from(v.hex, 'hex'));
    for (const m of schema.messages) for (let i = 0; i < 20; i++) bases.push(P.encode[P.messageName(m.id)](randomFields(schema, m.fields, rnd)));
    for (const base of bases) {
        const n = base.length;
        // Every truncation.
        for (let len = 0; len < n; len++) { check(base.subarray(0, len)); total++; }
        // Every single-bit flip (a sample of them for long messages).
        const bits = n * 8;
        const step = bits > 4000 ? Math.ceil(bits / 4000) : 1;
        for (let bit = 0; bit < bits; bit += step) {
            const b = Buffer.from(base);
            b[bit >> 3] ^= 1 << (bit & 7);
            if (check(b, DIRS[bit % 3])) accepted++;
            total++;
        }
        // Extensions, insertions, deletions, byte overwrites.
        for (let k = 0; k < 40; k++) {
            const extra = Buffer.alloc(1 + Math.floor(rnd() * 16));
            for (let j = 0; j < extra.length; j++) extra[j] = Math.floor(rnd() * 256);
            const at = Math.floor(rnd() * (n + 1));
            const del = Math.floor(rnd() * n);
            const over = Buffer.from(base);
            over[Math.floor(rnd() * n)] = [0, 0xff, 0x80, 0x7f, 1, 2][k % 6];
            for (const b of [Buffer.concat([base, extra]), Buffer.concat([base.subarray(0, at), extra, base.subarray(at)]),
                Buffer.concat([base.subarray(0, del), base.subarray(del + 1)]), over]) {
                if (check(b)) accepted++;
                total++;
            }
        }
    }
    assert.ok(total > 200000, `${total} mutations`);
    assert.ok(accepted > 1000, `${accepted} mutations still valid`);
});

test('random valid messages round-trip exactly', () => {
    const rnd = mulberry32(2026);
    for (const m of schema.messages) {
        const key = P.messageName(m.id);
        for (let i = 0; i < 400; i++) {
            const fields = randomFields(schema, m.fields, rnd);
            const buf = P.encode[key](fields);
            assert.deepEqual(P.decode(buf, { dir: m.dir }), { type: m.id, ...fields });
        }
    }
});

test('hostile list counts and string lengths are bounded by the buffer (no allocation bomb)', () => {
    // GameSnapshot claiming 1200 moves with none present; strings claiming 255 bytes.
    const snap = Buffer.from(vectors.valid.find((v) => v.name === 'GameSnapshot' && v.fields.moves.length === 0).hex, 'hex');
    const withCount = Buffer.from(snap);
    const countAt = snap.length - 41;            // moves count sits before the 39-byte tail (see PROTOCOL.md)
    assert.equal(withCount.readUInt16LE(countAt), 0);
    withCount.writeUInt16LE(1200, countAt);
    assert.throws(() => P.decode(withCount), (e) => e instanceof P.ProtocolError && e.reason === 'truncated');
    withCount.writeUInt16LE(0xffff, countAt);
    assert.throws(() => P.decode(withCount), (e) => e instanceof P.ProtocolError && e.reason === 'moves too long');
    const hello = Buffer.from([0x01, 1, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0xff]);
    assert.throws(() => P.decode(hello), (e) => e instanceof P.ProtocolError && e.reason === 'client bad length');
});
