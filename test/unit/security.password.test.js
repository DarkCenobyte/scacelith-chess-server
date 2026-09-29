import test from 'node:test';
import assert from 'node:assert/strict';
import crypto from 'node:crypto';
import {
    checkPasswordPolicy, commonPasswords, createPasswordHasher, isCommonPassword, PASSWORD_MAX_BYTES,
} from '../../src/security/password.js';

const fast = createPasswordHasher({ scrypt: { logN: 10 }, argon2: false });

// A stand-in for crypto.argon2 (Node >= 24.7) so the argon2id code path runs on any Node: a
// deterministic function of every parameter.
function fakeArgon2(calls = []) {
    return (alg, p, cb) => {
        calls.push({ alg, ...p });
        const h = crypto.createHash('sha256').update(JSON.stringify([alg, p.memory, p.passes, p.parallelism, p.tagLength]))
            .update(p.nonce).update(p.message).digest();
        const out = Buffer.alloc(p.tagLength);
        for (let i = 0; i < out.length; i++) out[i] = h[i % 32] ^ i;
        setImmediate(() => cb(null, out));
    };
}

test('scrypt: self-describing format with the DESIGN parameters', async () => {
    const h = await createPasswordHasher({ argon2: false }).hash('a sufficiently long passphrase');
    assert.match(h, /^scrypt\$17\$8\$1\$[A-Za-z0-9_-]{22}\$[A-Za-z0-9_-]{86}$/);
});

test('hash and verify; wrong passwords and garbage hashes fail', async () => {
    const h = await fast.hash('correct horse battery');
    assert.deepEqual(await fast.verify(h, 'correct horse battery'), { ok: true, needsRehash: false });
    assert.equal((await fast.verify(h, 'correct horse batterY')).ok, false);
    assert.equal((await fast.verify('not a hash', 'x')).ok, false);
    assert.equal((await fast.verify(null, 'x')).ok, false);
    assert.notEqual(await fast.hash('same'), await fast.hash('same'), 'random salt');
});

test('passwords are compared after Unicode NFC normalisation', async () => {
    const h = await fast.hash('café au lait noir');
    assert.equal((await fast.verify(h, 'café au lait noir')).ok, true);
});

test('weaker scrypt parameters are reported for rehash after a successful check only', async () => {
    const old = await createPasswordHasher({ scrypt: { logN: 9 }, argon2: false }).hash('passphrase one two');
    assert.deepEqual(await fast.verify(old, 'passphrase one two'), { ok: true, needsRehash: true });
    assert.deepEqual(await fast.verify(old, 'wrong'), { ok: false, needsRehash: false });
});

test('argon2id is preferred when available; scrypt hashes are then upgraded', async () => {
    const calls = [];
    const a = createPasswordHasher({ scrypt: { logN: 10 }, argon2: fakeArgon2(calls), argon2Params: { memory: 1024, passes: 2, parallelism: 1 } });
    assert.equal(a.algorithm, 'argon2id');
    const h = await a.hash('an argon2 passphrase');
    assert.match(h, /^\$argon2id\$v=19\$m=1024,t=2,p=1\$[A-Za-z0-9+/]{22}\$[A-Za-z0-9+/]{43}$/);
    assert.equal(calls[0].alg, 'argon2id');
    assert.equal(calls[0].nonce.length, 16);
    assert.equal(calls[0].tagLength, 32);
    assert.deepEqual(await a.verify(h, 'an argon2 passphrase'), { ok: true, needsRehash: false });
    assert.equal((await a.verify(h, 'another')).ok, false);
    const legacy = await fast.hash('an argon2 passphrase');
    assert.deepEqual(await a.verify(legacy, 'an argon2 passphrase'), { ok: true, needsRehash: true });
    const stronger = createPasswordHasher({ argon2: fakeArgon2(), argon2Params: { memory: 2048, passes: 2, parallelism: 1 } });
    assert.deepEqual(await stronger.verify(h, 'an argon2 passphrase'), { ok: true, needsRehash: true });
    assert.equal((await a.verifyDummy('x')), false);
});

test('the real crypto.argon2 when the runtime has it', { skip: typeof crypto.argon2 !== 'function' && 'crypto.argon2 needs Node >= 24.7' }, async () => {
    const a = createPasswordHasher({ argon2Params: { memory: 8192, passes: 1, parallelism: 1 } });
    const h = await a.hash('real argon2 passphrase');
    assert.match(h, /^\$argon2id\$/);
    assert.equal((await a.verify(h, 'real argon2 passphrase')).ok, true);
});

test('unknown accounts cost the same work (dummy hash)', async () => {
    const h = createPasswordHasher({ scrypt: { logN: 12 }, argon2: false });
    await h.warmUp();
    const stored = await h.hash('whatever it is');
    const time = async (f) => { const t = process.hrtime.bigint(); await f(); return Number(process.hrtime.bigint() - t) / 1e6; };
    const real = [], dummy = [];
    for (let i = 0; i < 5; i++) {
        real.push(await time(() => h.verify(stored, 'wrong password here')));
        dummy.push(await time(() => h.verifyDummy('wrong password here')));
    }
    const med = (a) => a.sort((x, y) => x - y)[2];
    const ratio = med(real) / med(dummy);
    assert.ok(ratio > 0.5 && ratio < 2, `ratio ${ratio}`);
    assert.equal(await h.verifyDummy('x'), false);
});

test('warmUp measures the slowest verification: the preferred algorithm, and the legacy scrypt hashes when argon2id is preferred', async () => {
    const time = async (f) => { const t = performance.now(); await f(); return performance.now() - t; };
    const med = (a) => [...a].sort((x, y) => x - y)[2];
    // argon2id preferred (an instant stand-in): the warm-up also times a scrypt verification with
    // the configured scrypt parameters, the slowest hash the database can still hold.
    const a = createPasswordHasher({ scrypt: { logN: 14 }, argon2: fakeArgon2(), argon2Params: { memory: 1024, passes: 1, parallelism: 1 } });
    const legacy = await createPasswordHasher({ scrypt: { logN: 14 }, argon2: false }).hash('a legacy passphrase');
    const scryptMs = [];
    for (let i = 0; i < 5; i++) scryptMs.push(await time(() => a.verify(legacy, 'a wrong passphrase')));
    const dummyMs = [];
    for (let i = 0; i < 5; i++) dummyMs.push(await time(() => a.verifyDummy('a wrong passphrase')));
    const warm = await a.warmUp();
    assert.ok(Number.isFinite(warm) && warm >= 0.5 * med(scryptMs),
        `warm-up measured ${warm.toFixed(1)} ms; a scrypt check takes ${med(scryptMs).toFixed(1)} ms, the argon2 dummy ${med(dummyMs).toFixed(1)} ms`);
    // scrypt preferred: the dummy hash is the slowest kind; a second warm-up times a dummy check.
    const s = createPasswordHasher({ scrypt: { logN: 14 }, argon2: false });
    const first = await s.warmUp();
    const again = await s.warmUp();
    const own = [];
    for (let i = 0; i < 5; i++) own.push(await time(() => s.verifyDummy('x')));
    for (const w of [first, again]) assert.ok(w >= 0.5 * med(own), `warm-up ${w.toFixed(1)} ms, dummy check ${med(own).toFixed(1)} ms`);
});

test('an argon2id hash on a runtime without argon2 fails after the dummy work, not at once', async () => {
    const h = createPasswordHasher({ scrypt: { logN: 13 }, argon2: false });
    await h.warmUp();
    const stored = await createPasswordHasher({ argon2: fakeArgon2(), argon2Params: { memory: 1024, passes: 1, parallelism: 1 } }).hash('whatever it is');
    assert.match(stored, /^\$argon2id\$/);
    const time = async (f) => { const t = process.hrtime.bigint(); await f(); return Number(process.hrtime.bigint() - t) / 1e6; };
    const argon = [], dummy = [];
    for (let i = 0; i < 5; i++) {
        argon.push(await time(async () => assert.deepEqual(await h.verify(stored, 'whatever it is'), { ok: false, needsRehash: false })));
        dummy.push(await time(() => h.verifyDummy('whatever it is')));
    }
    const med = (a) => a.sort((x, y) => x - y)[2];
    const ratio = med(argon) / med(dummy);
    assert.ok(ratio > 0.5 && ratio < 2, `median ${med(argon).toFixed(1)} ms against ${med(dummy).toFixed(1)} ms for the dummy`);
});

test('policy: length, bytes, username, e-mail, common passwords', () => {
    const opts = { minLength: 10, username: 'Magnus_C', email: 'grandpatzer@example.com' };
    assert.equal(checkPasswordPolicy('short', opts).reason, 'too_short');
    assert.equal(checkPasswordPolicy('é'.repeat(9), opts).reason, 'too_short');
    assert.equal(checkPasswordPolicy('x'.repeat(PASSWORD_MAX_BYTES + 1), opts).reason, 'too_long');
    assert.equal(checkPasswordPolicy('é'.repeat(129), opts).reason, 'too_long', '258 bytes');
    assert.equal(checkPasswordPolicy('my magnus_c secret!', opts).reason, 'contains_username');
    assert.equal(checkPasswordPolicy('GRANDPATZER forever', opts).reason, 'contains_email');
    assert.equal(checkPasswordPolicy('qwertyuiop', opts).reason, 'too_common');
    assert.equal(checkPasswordPolicy('PASSWORD1234', opts).reason, 'too_common');
    assert.equal(checkPasswordPolicy('1q2w3e4r5t', opts).reason, 'too_common');
    assert.equal(checkPasswordPolicy('ivory rook takes e5', opts), null);
    assert.equal(checkPasswordPolicy('x'.repeat(PASSWORD_MAX_BYTES), { minLength: 10 }), null);
});

test('the embedded common-password list is large and lower case', () => {
    const set = commonPasswords();
    assert.ok(set.size >= 1000, `size ${set.size}`);
    for (const p of set) assert.equal(p, p.toLowerCase());
    assert.ok(isCommonPassword('123456') && isCommonPassword('Password') && isCommonPassword('iloveyou'));
    assert.ok(!isCommonPassword('a very unusual passphrase 42'));
});
