// Proof of work (DESIGN.md section 8): SHA-256(challenge + ':' + nonce) with `bits` leading zero bits.

import { test } from 'node:test';
import assert from 'node:assert/strict';
import crypto from 'node:crypto';
import { solvePow, checkPow } from '../../src/client/pow.js';

function leadingZeroBits(buf) {
    let n = 0;
    for (const b of buf) {
        if (b === 0) { n += 8; continue; }
        return n + Math.clz32(b) - 24;
    }
    return n;
}
const sha = (s) => crypto.createHash('sha256').update(s, 'utf8').digest();

test('solvePow returns the smallest valid nonce around every SHA-256 block boundary', () => {
    // challenge + ':' + nonce crosses 55/56/64-byte limits: one or two final blocks, carried lengths.
    const lengths = [0, 1, 7, 40, 50, 52, 53, 54, 55, 56, 57, 62, 63, 64, 65, 100, 117, 118, 119, 120, 127, 128, 129, 250];
    for (const len of lengths) {
        const ch = 'q'.repeat(len);
        for (const bits of [0, 1, 3, 8, 9]) {
            const nonce = solvePow(ch, bits);
            let k = 0;
            while (leadingZeroBits(sha(`${ch}:${k}`)) < bits) k++;
            assert.equal(nonce, String(k), `length ${len}, ${bits} bits`);
        }
    }
});

test('solutions verify with node:crypto, multi-byte challenges included', () => {
    for (const [ch, bits] of [['abc', 16], ['Łukasz-ユキ-🐴:challenge', 14], ['x'.repeat(61), 17], [crypto.randomBytes(40).toString('base64url'), 18]]) {
        const nonce = solvePow(ch, bits);
        assert.match(nonce, /^(0|[1-9][0-9]*)$/);
        assert.ok(leadingZeroBits(sha(`${ch}:${nonce}`)) >= bits);
        assert.equal(checkPow(ch, nonce, bits), true);
    }
});

test('nonces grow past digit-count changes (9 -> 10, 99 -> 100)', () => {
    // Force the search through length changes with a start just below them.
    for (const start of [9, 99, 999999]) {
        const nonce = solvePow('carry', 4, { start });
        assert.ok(Number(nonce) >= start);
        let k = start;
        while (leadingZeroBits(sha(`carry:${k}`)) < 4) k++;
        assert.equal(nonce, String(k));
    }
});

test('checkPow refuses wrong nonces and malformed input', () => {
    const nonce = solvePow('chal', 12);
    assert.equal(checkPow('chal', nonce, 12), true);
    assert.equal(checkPow('chal', nonce, 0), true);
    assert.equal(checkPow('chal2', nonce, 12), leadingZeroBits(sha(`chal2:${nonce}`)) >= 12);
    assert.equal(checkPow('chal', String(Number(nonce) + 1), 12), leadingZeroBits(sha(`chal:${Number(nonce) + 1}`)) >= 12);
    assert.equal(checkPow('chal', '-1', 1), false);
    assert.equal(checkPow('chal', '1e3', 1), false);
    assert.equal(checkPow('chal', '', 0), false);
    assert.equal(checkPow('chal', '1'.repeat(20), 0), true);
    assert.equal(checkPow('chal', '1'.repeat(21), 0), false);         // the server's limit
    assert.equal(checkPow('chal', 12, 0), false);
    assert.equal(checkPow('x', '0', 256), false);
    assert.throws(() => checkPow('x', '0', -1), RangeError);
    assert.throws(() => solvePow('x', 257), RangeError);
    assert.throws(() => solvePow('x', 1.5), RangeError);
    assert.throws(() => solvePow('x', 40, { maxAttempts: 1000 }), /no solution within 1000 attempts/);
    assert.equal(solvePow('anything', 0), '0');
});

test('18 bits (the default difficulty) solves quickly', () => {
    const t0 = performance.now();
    const nonce = solvePow('perf-' + 'z'.repeat(80), 18);
    const ms = performance.now() - t0;
    assert.equal(checkPow('perf-' + 'z'.repeat(80), nonce, 18), true);
    assert.ok(ms < 10000, `${ms} ms`);
});
