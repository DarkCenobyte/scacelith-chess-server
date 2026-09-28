import test from 'node:test';
import assert from 'node:assert/strict';
import crypto from 'node:crypto';
import {
    base32Decode, base32Encode, createSecretBox, generateRecoveryCodes, generateTotpSecret, hashRecoveryCode, hotp,
    normalizeRecoveryCode, otpauthUri, totp, totpStep, verifyTotp,
} from '../../src/security/totp.js';

// RFC 6238 appendix B, SHA-1 column (seed "12345678901234567890", 8 digits).
const RFC_SECRET = Buffer.from('12345678901234567890', 'ascii');
const RFC_VECTORS = [
    [59, '94287082'], [1111111109, '07081804'], [1111111111, '14050471'],
    [1234567890, '89005924'], [2000000000, '69279037'], [20000000000, '65353130'],
];

test('RFC 6238 appendix B vectors (SHA-1, 8 digits)', () => {
    for (const [t, code] of RFC_VECTORS) assert.equal(hotp(RFC_SECRET, totpStep(t * 1000), 8), code, `T=${t}`);
});

test('6-digit codes are the last 6 digits of the RFC vectors', () => {
    for (const [t, code] of RFC_VECTORS) assert.equal(totp(RFC_SECRET, t * 1000), code.slice(2));
});

test('RFC 4226 appendix D HOTP values', () => {
    const expect = ['755224', '287082', '359152', '969429', '338314', '254676', '287922', '162583', '399871', '520489'];
    expect.forEach((c, i) => assert.equal(hotp(RFC_SECRET, i), c));
});

test('base32 round trip and RFC 4648 vectors', () => {
    assert.equal(base32Encode(Buffer.from('foobar')), 'MZXW6YTBOI');
    assert.equal(base32Encode(Buffer.from('f')), 'MY');
    assert.equal(base32Decode('mzxw6ytboi').toString(), 'foobar');
    assert.equal(base32Decode('MZXW 6YTB OI======').toString(), 'foobar');
    assert.equal(base32Decode('MZ1'), null);
    for (let i = 0; i < 50; i++) {
        const b = crypto.randomBytes(i);
        assert.deepEqual(base32Decode(base32Encode(b)), b);
    }
    const s = generateTotpSecret();
    assert.equal(s.length, 20);
    assert.equal(base32Encode(s).length, 32);
});

test('verification accepts the current step and +-1, refuses +-2', () => {
    const secret = generateTotpSecret();
    const now = Date.UTC(2026, 8, 28, 12, 0, 10);
    const step = totpStep(now);
    for (const d of [-1, 0, 1]) assert.equal(verifyTotp(secret, hotp(secret, step + d), { now }), step + d);
    for (const d of [-2, 2]) assert.equal(verifyTotp(secret, hotp(secret, step + d), { now }), -1);
});

test('verification refuses already used steps (replay) and malformed codes', () => {
    const secret = generateTotpSecret();
    const now = Date.UTC(2026, 8, 28, 12, 0, 10);
    const step = totpStep(now);
    const code = hotp(secret, step);
    assert.equal(verifyTotp(secret, code, { now, lastStep: step }), -1);
    assert.equal(verifyTotp(secret, code, { now, lastStep: step - 1 }), step);
    assert.equal(verifyTotp(secret, hotp(secret, step - 1), { now, lastStep: step }), -1);
    for (const bad of ['', '12345', '1234567', 'abcdef', ' 123456', null, 123456]) assert.equal(verifyTotp(secret, bad, { now }), -1);
});

test('otpauth URI carries the URL-encoded issuer and the parameters', () => {
    const secret = Buffer.from('12345678901234567890');
    const uri = otpauthUri({ issuer: 'Scacelith Community Server', account: 'alice', secret });
    assert.equal(uri, 'otpauth://totp/Scacelith%20Community%20Server:alice?secret=GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ' +
        '&issuer=Scacelith%20Community%20Server&algorithm=SHA1&digits=6&period=30');
    assert.match(otpauthUri({ issuer: 'A&B', account: 'bob', secret }), /^otpauth:\/\/totp\/A%26B:bob\?.*issuer=A%26B&/);
});

test('secret box: round trip, bound to its associated data, tamper-evident', () => {
    const box = createSecretBox(crypto.randomBytes(32));
    const secret = generateTotpSecret();
    const sealed = box.seal(secret, 'mfa:1');
    assert.match(sealed, /^v1\.[A-Za-z0-9_-]+$/);
    assert.deepEqual(box.open(sealed, 'mfa:1'), secret);
    assert.equal(box.open(sealed, 'mfa:2'), null);
    const raw = Buffer.from(sealed.slice(3), 'base64url');
    raw[14] ^= 1;
    assert.equal(box.open('v1.' + raw.toString('base64url'), 'mfa:1'), null);
    assert.equal(createSecretBox(crypto.randomBytes(32)).open(sealed, 'mfa:1'), null);
    assert.equal(box.open('garbage', 'mfa:1'), null);
    assert.notEqual(box.seal(secret, 'mfa:1'), sealed, 'random IV');
    assert.throws(() => createSecretBox(Buffer.alloc(16)));
});

test('recovery codes: format, alphabet, uniqueness, normalisation', () => {
    const codes = generateRecoveryCodes();
    assert.equal(codes.length, 10);
    assert.equal(new Set(codes).size, 10);
    for (const c of codes) {
        assert.match(c, /^[0-9a-hjkmnp-tv-z]{4}-[0-9a-hjkmnp-tv-z]{4}-[0-9a-hjkmnp-tv-z]{2}$/);
        assert.equal(normalizeRecoveryCode(c), c.replace(/-/g, ''));
        assert.equal(normalizeRecoveryCode(c.toUpperCase().replace(/-/g, ' ')), c.replace(/-/g, ''));
    }
    assert.equal(normalizeRecoveryCode('abcd-efgh-jk'), 'abcdefghjk');
    assert.equal(normalizeRecoveryCode('OOOO-IIII-LL'), '0000111111');
    assert.equal(normalizeRecoveryCode('abcd-efgh-j'), null);
    assert.equal(normalizeRecoveryCode('abcd-efgh-ju'), null, 'u is not in the alphabet');
    assert.equal(normalizeRecoveryCode(42), null);
    // 10 symbols of a 32-symbol alphabet: 50 bits.
    const symbols = new Set(generateRecoveryCodes(200).join('').replace(/-/g, ''));
    assert.ok(symbols.size <= 32 && symbols.size >= 28);
});

test('recovery-code hashes are peppered and bound to the user', () => {
    const pepper = crypto.randomBytes(32);
    const n = normalizeRecoveryCode('abcd-efgh-jk');
    const h1 = hashRecoveryCode(pepper, 1, n);
    assert.match(h1, /^[0-9a-f]{64}$/);
    assert.equal(hashRecoveryCode(pepper, 1, n), h1);
    assert.notEqual(hashRecoveryCode(pepper, 2, n), h1);
    assert.notEqual(hashRecoveryCode(crypto.randomBytes(32), 1, n), h1);
});
