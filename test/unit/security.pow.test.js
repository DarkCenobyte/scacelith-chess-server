import test from 'node:test';
import assert from 'node:assert/strict';
import crypto from 'node:crypto';
import { checkWork, createPow, leadingZeroBits, solvePow, POW_TTL_MS } from '../../src/security/pow.js';
import { createLocalControl } from '../../src/security/ratelimit.js';
import { createClock } from './helpers/auth-fakes.js';

function setup() {
    const now = createClock();
    const control = createLocalControl({ now });
    const pow = createPow({
        key: crypto.randomBytes(32), ipKeyHash: crypto.randomBytes(32), now,
        once: async (key, ttlMs) => (await control.request('once.consume', { key, ttlMs })).fresh,
    });
    return { now, pow };
}

test('leading zero bits', () => {
    assert.equal(leadingZeroBits(Buffer.from([0x80])), 0);
    assert.equal(leadingZeroBits(Buffer.from([0x01])), 7);
    assert.equal(leadingZeroBits(Buffer.from([0x00, 0x00, 0x3f])), 18);
    assert.equal(leadingZeroBits(Buffer.from([0x00, 0x00])), 16);
});

test('the answer follows DESIGN 8: SHA-256(challenge + ":" + nonce), decimal nonce', () => {
    const challenge = 'abc';
    const nonce = solvePow(challenge, 10);
    assert.match(nonce, /^[0-9]+$/);
    const h = crypto.createHash('sha256').update(`${challenge}:${nonce}`).digest();
    assert.ok(leadingZeroBits(h) >= 10);
    assert.ok(checkWork(challenge, nonce, 10));
});

test('a solved challenge is accepted once', async () => {
    const { pow } = setup();
    const c = pow.issue({ ip: '203.0.113.5', endpoint: 'register', bits: 10 });
    assert.equal(c.bits, 10);
    assert.equal(c.expiresAt, Date.UTC(2026, 8, 28, 12, 0, 0) + POW_TTL_MS);
    assert.match(c.challenge, /^[A-Za-z0-9_-]+\.[A-Za-z0-9_-]{43}$/);
    const nonce = solvePow(c.challenge, 10);
    const args = { ip: '203.0.113.5', endpoint: 'register', bits: 10, challenge: c.challenge, nonce };
    assert.deepEqual(await pow.verify(args), { ok: true });
    assert.deepEqual(await pow.verify(args), { ok: false, reason: 'replayed' });
});

test('a challenge is single use whatever the spelling of its signature: the other base64url spellings are refused', async () => {
    const { pow } = setup();
    const c = pow.issue({ ip: '203.0.113.5', endpoint: 'register', bits: 4 });
    // The 43rd character of the signature carries 4 bits; its 2 low bits are ignored by a decoder,
    // so 3 other characters decode to the same 32 bytes.
    const B64URL = 'ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_';
    const i = B64URL.indexOf(c.challenge.at(-1));
    const variants = [0, 1, 2, 3].map((k) => B64URL[(i & ~3) | k]).filter((ch) => ch !== c.challenge.at(-1))
        .map((ch) => c.challenge.slice(0, -1) + ch);
    assert.equal(variants.length, 3);
    for (const v of variants) assert.ok(Buffer.from(v.split('.')[1], 'base64url').equals(Buffer.from(c.challenge.split('.')[1], 'base64url')));
    const args = (challenge) => ({ ip: '203.0.113.5', endpoint: 'register', bits: 4, challenge, nonce: solvePow(challenge, 4) });
    for (const v of variants) assert.deepEqual(await pow.verify(args(v)), { ok: false, reason: 'signature' });
    assert.deepEqual(await pow.verify(args(c.challenge)), { ok: true });
});

test('expired, other network, other endpoint, tampered, lazy and malformed answers are refused', async () => {
    const { pow, now } = setup();
    const c = pow.issue({ ip: '203.0.113.5', endpoint: 'register', bits: 8 });
    const nonce = solvePow(c.challenge, 8);
    const base = { ip: '203.0.113.5', endpoint: 'register', bits: 8, challenge: c.challenge, nonce };
    assert.equal((await pow.verify({ ...base, ip: '198.51.100.7' })).reason, 'network');
    assert.equal((await pow.verify({ ...base, endpoint: 'login' })).reason, 'endpoint');
    assert.equal((await pow.verify({ ...base, bits: 12 })).reason, 'bits');
    const [part, sig] = c.challenge.split('.');
    const forged = JSON.parse(Buffer.from(part, 'base64url').toString());
    forged.b = 1;
    const forgedPart = Buffer.from(JSON.stringify(forged)).toString('base64url');
    assert.equal((await pow.verify({ ...base, challenge: `${forgedPart}.${sig}` })).reason, 'signature');
    assert.equal((await pow.verify({ ...base, nonce: '12x' })).reason, 'malformed');
    assert.equal((await pow.verify({ ...base, challenge: 'short' })).reason, 'malformed');
    let bad = 0;
    while (checkWork(c.challenge, String(bad), 8)) bad++;
    assert.equal((await pow.verify({ ...base, nonce: String(bad) })).reason, 'work');
    now.advance(POW_TTL_MS + 1);
    assert.equal((await pow.verify(base)).reason, 'expired');
});

test('the network binding is per /64 for IPv6 and accepts IPv4-mapped addresses', async () => {
    const { pow } = setup();
    const c = pow.issue({ ip: '2001:db8:1:2::10', endpoint: 'login', bits: 4 });
    const nonce = solvePow(c.challenge, 4);
    assert.deepEqual(await pow.verify({ ip: '2001:db8:1:2:ffff::1', endpoint: 'login', bits: 4, challenge: c.challenge, nonce }), { ok: true });
    const d = pow.issue({ ip: '192.0.2.1', endpoint: 'login', bits: 4 });
    assert.deepEqual(await pow.verify({ ip: '::ffff:192.0.2.1', endpoint: 'login', bits: 4, challenge: d.challenge, nonce: solvePow(d.challenge, 4) }), { ok: true });
});
