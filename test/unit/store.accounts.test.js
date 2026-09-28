import { test } from 'node:test';
import assert from 'node:assert/strict';
import crypto from 'node:crypto';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { setTimeout as sleep } from 'node:timers/promises';
import { Worker } from 'node:worker_threads';
import { openStore, migrate, StoreError, normalizeEmail } from '../../src/store/index.js';
import { testConfig } from '../../src/config.js';

const STORE_URL = new URL('../../src/store/index.js', import.meta.url).href;
const CONFIG_URL = new URL('../../src/config.js', import.meta.url).href;

function memStore() {
    const store = openStore(testConfig({ DB_PATH: ':memory:' }));
    migrate(store);
    return store;
}

const sha = (s) => crypto.createHash('sha256').update(s).digest();

test('users: create, lookups, uniqueness, normalization', () => {
    const store = memStore();
    const id = store.users.create({ username: 'Magnus', email: '  First.Last@GMail.com ', passwordHash: 'scrypt$x', emailVerified: false });
    assert.equal(typeof id, 'number');
    const u = store.users.byId(id);
    assert.equal(u.username, 'Magnus');
    assert.equal(u.email, 'First.Last@GMail.com');
    assert.equal(u.emailVerified, false);
    assert.equal(u.passwordHash, 'scrypt$x');
    assert.equal(u.status, 'active');
    assert.equal(u.acceptChallenges, true);
    assert.equal(u.mfaEnabled, false);
    assert.equal(u.mfaSecretEnc, null);
    assert.equal(u.lastLoginAt, null);
    assert.ok(u.createdAt > 0);

    assert.equal(store.users.byUsername('MAGNUS').id, id);
    assert.equal(store.users.byEmail('first.last@gmail.com').id, id);
    assert.equal(store.users.byEmail('firstlast@gmail.com'), null, 'Gmail dots are kept');
    assert.equal(store.users.byLogin('magnus').id, id);
    assert.equal(store.users.byLogin(' FIRST.LAST@gmail.COM').id, id);
    assert.equal(store.users.byLogin('nobody'), null);
    assert.equal(store.users.byId(9999), null);
    assert.equal(normalizeEmail(' A@B.c '), 'a@b.c');

    assert.throws(() => store.users.create({ username: 'magnus', email: 'other@example.org' }),
        (e) => e instanceof StoreError && e.code === 'username_taken');
    assert.throws(() => store.users.create({ username: 'Hikaru', email: 'FIRST.LAST@gmail.com' }),
        (e) => e instanceof StoreError && e.code === 'email_taken');
    // SSO-only account: no password hash.
    const sso = store.users.create({ username: 'Sso', email: 'sso@example.org', emailVerified: true });
    assert.equal(store.users.byId(sso).passwordHash, null);
    assert.equal(store.users.byId(sso).emailVerified, true);
    store.close();
});

test('users: update, MFA fields, advanceMfaStep, anonymize', () => {
    const store = memStore();
    const id = store.users.create({ username: 'Judit', email: 'j@example.org', passwordHash: 'h1' });
    const other = store.users.create({ username: 'Other', email: 'o@example.org' });
    const secret = Buffer.from('0123456789abcdef0123456789abcdef');
    assert.equal(store.users.update(id, {
        emailVerified: true, passwordHash: 'h2', lastLoginAt: 1234.7, acceptChallenges: false, pendingMfaSecretEnc: secret,
    }), true);
    let u = store.users.byId(id);
    assert.equal(u.emailVerified, true);
    assert.equal(u.passwordHash, 'h2');
    assert.equal(u.lastLoginAt, 1234);
    assert.equal(u.acceptChallenges, false);
    assert.ok(Buffer.isBuffer(u.pendingMfaSecretEnc));
    assert.ok(u.pendingMfaSecretEnc.equals(secret));
    store.users.update(id, { mfaEnabled: true, mfaSecretEnc: secret, pendingMfaSecretEnc: null });
    u = store.users.byId(id);
    assert.equal(u.mfaEnabled, true);
    assert.ok(u.mfaSecretEnc.equals(secret));
    assert.equal(u.pendingMfaSecretEnc, null);
    assert.throws(() => store.users.update(id, { email: 'O@example.org' }), (e) => e.code === 'email_taken');
    assert.throws(() => store.users.update(id, { username: 'other' }), (e) => e.code === 'username_taken');
    assert.throws(() => store.users.update(id, { bogus: 1 }), (e) => e.code === 'invalid');
    assert.throws(() => store.users.update(id, { status: 'frozen' }), (e) => e.code === 'invalid');
    store.users.update(id, { email: 'New@Example.org' });
    assert.equal(store.users.byEmail('new@example.org').id, id);
    assert.equal(store.users.update(424242, { passwordHash: 'x' }), false);

    assert.equal(store.users.advanceMfaStep(id, 100), true);
    assert.equal(store.users.advanceMfaStep(id, 100), false, 'replayed step refused');
    assert.equal(store.users.advanceMfaStep(id, 99), false);
    assert.equal(store.users.advanceMfaStep(id, 101), true);
    assert.equal(store.users.byId(id).mfaLastStep, 101);

    // Anonymization erases personal data and revokes everything, keeps the id.
    const now = Date.now();
    store.sessions.create({ userId: id, tokenHash: sha('s1'), expiresAt: now + 1e6, idleExpiresAt: now + 1e6, ip: '203.0.113.9' });
    store.tokens.create({ kind: 'reset', tokenHash: sha('t1'), userId: id, expiresAt: now + 1e6 });
    store.sso.link(id, 'google', 'sub-1', 'j@gmail.com');
    store.mfa.replaceRecoveryCodes(id, ['a', 'b']);
    store.security.insertBatch([{ kind: 'login_failed', userId: id, ip: '203.0.113.9', at: now }]);
    const res = store.users.anonymize(id);
    assert.equal(res.tokenHashes.length, 1);
    assert.ok(res.tokenHashes[0].equals(sha('s1')));
    u = store.users.byId(id);
    assert.equal(u.status, 'deleted');
    assert.equal(u.username, `deleted#${id}`);
    assert.equal(u.email, null);
    assert.equal(u.passwordHash, null);
    assert.equal(u.mfaEnabled, false);
    assert.ok(u.deletedAt > 0);
    assert.equal(store.users.byEmail('new@example.org'), null);
    assert.equal(store.users.byUsername('judit'), null);
    assert.equal(store.sessions.byTokenHash(sha('s1')), null);
    assert.equal(store.tokens.get('reset', sha('t1')), null);
    assert.equal(store.sso.find('google', 'sub-1'), null);
    assert.equal(store.mfa.countRecoveryCodes(id), 0);
    assert.equal(store.security.forUser(id)[0].ip, null);
    // The name and e-mail are free again.
    assert.ok(store.users.create({ username: 'Judit', email: 'new@example.org' }) > other);
    assert.throws(() => store.users.anonymize(987654), (e) => e.code === 'not_found');
    store.close();
});

test('mfa recovery codes: replace, single use, count', () => {
    const store = memStore();
    const id = store.users.create({ username: 'Mfa', email: 'm@example.org' });
    store.mfa.replaceRecoveryCodes(id, ['h1', 'h2', 'h3']);
    assert.equal(store.mfa.countRecoveryCodes(id), 3);
    assert.equal(store.mfa.consumeRecoveryCode(id, 'h2'), true);
    assert.equal(store.mfa.consumeRecoveryCode(id, 'h2'), false);
    assert.equal(store.mfa.consumeRecoveryCode(id, 'zz'), false);
    assert.equal(store.mfa.countRecoveryCodes(id), 2);
    store.mfa.replaceRecoveryCodes(id, [sha('x'), sha('y')]);
    assert.equal(store.mfa.countRecoveryCodes(id), 2);
    assert.equal(store.mfa.consumeRecoveryCode(id, 'h1'), false, 'old codes are gone');
    assert.equal(store.mfa.consumeRecoveryCode(id, sha('x')), true);
    store.close();
});

test('sessions: create, lookup, touch, revoke, list, limit', () => {
    const store = memStore();
    const uid = store.users.create({ username: 'Sess', email: 's@example.org' });
    const other = store.users.create({ username: 'Other', email: 'o@example.org' });
    const t0 = 1_800_000_000_000;
    const ids = [];
    for (let i = 0; i < 5; i++) {
        ids.push(store.sessions.create({ userId: uid, tokenHash: sha(`tok${i}`), createdAt: t0 + i, expiresAt: t0 + 90 * 86400000,
            idleExpiresAt: t0 + 30 * 86400000, clientLabel: `pc${i}`, ip: '198.51.100.1' }));
    }
    const s = store.sessions.byTokenHash(sha('tok2'));
    assert.deepEqual(s, { id: ids[2], userId: uid, createdAt: t0 + 2, lastSeenAt: t0 + 2, expiresAt: t0 + 90 * 86400000,
        idleExpiresAt: t0 + 30 * 86400000, revokedAt: null });
    assert.equal(store.sessions.byTokenHash(sha('nope')), null);
    assert.throws(() => store.sessions.create({ userId: uid, tokenHash: sha('tok0'), expiresAt: t0, idleExpiresAt: t0 }),
        (e) => e.code === 'duplicate');
    store.sessions.touch(ids[2], t0 + 1000, t0 + 1000 + 86400000);
    assert.equal(store.sessions.byTokenHash(sha('tok2')).lastSeenAt, t0 + 1000);
    assert.equal(store.sessions.byTokenHash(sha('tok2')).idleExpiresAt, t0 + 1000 + 86400000);

    assert.equal(store.sessions.revoke(ids[0], other), null, 'not the owner');
    assert.ok(store.sessions.revoke(ids[0], uid, t0 + 5).equals(sha('tok0')));
    assert.equal(store.sessions.revoke(ids[0]), null, 'already revoked');
    assert.equal(store.sessions.byTokenHash(sha('tok0')).revokedAt, t0 + 5);
    const list = store.sessions.listForUser(uid);
    assert.equal(list.length, 4);
    assert.equal(list[0].id, ids[2], 'most recently seen first');
    assert.equal(list[0].clientLabel, 'pc2');

    // Keep the 2 newest live sessions.
    const revoked = store.sessions.enforceLimit(uid, 2, t0 + 2000);
    assert.deepEqual(revoked.map((h) => h.toString('hex')).sort(), [sha('tok1'), sha('tok2')].map((h) => h.toString('hex')).sort());
    assert.deepEqual(store.sessions.listForUser(uid).map((x) => x.id).sort(), [ids[3], ids[4]].sort());

    const all = store.sessions.revokeAllForUser(uid, ids[4]);
    assert.equal(all.length, 1);
    assert.ok(all[0].equals(sha('tok3')));
    assert.deepEqual(store.sessions.listForUser(uid).map((x) => x.id), [ids[4]]);
    assert.equal(store.sessions.revokeAllForUser(uid).length, 1);
    assert.equal(store.sessions.listForUser(uid).length, 0);

    // String hashes work as well (stored and compared as given).
    const sid = store.sessions.create({ userId: other, tokenHash: 'abc123hex', expiresAt: t0 + 1, idleExpiresAt: t0 + 1 });
    assert.equal(store.sessions.byTokenHash('abc123hex').id, sid);
    assert.equal(store.sessions.revoke(sid), 'abc123hex');
    store.close();
});

test('tokens: create, get, update, single-use consume, expiry', () => {
    const store = memStore();
    const uid = store.users.create({ username: 'Tok', email: 't@example.org' });
    const now = 1_800_000_000_000;
    store.tokens.create({ kind: 'verify', tokenHash: sha('v'), userId: uid, data: { email: 't@example.org' }, expiresAt: now + 1000, createdAt: now });
    store.tokens.create({ kind: 'sso', tokenHash: sha('v'), data: { status: 'pending' }, expiresAt: now + 1000, createdAt: now });
    assert.throws(() => store.tokens.create({ kind: 'verify', tokenHash: sha('v'), expiresAt: now }), (e) => e.code === 'duplicate');
    assert.deepEqual(store.tokens.get('sso', sha('v')).data, { status: 'pending' });
    assert.equal(store.tokens.get('sso', sha('v')).userId, null);
    assert.equal(store.tokens.update('sso', sha('v'), { status: 'done', userId: 5 }), true);
    assert.deepEqual(store.tokens.get('sso', sha('v')).data, { status: 'done', userId: 5 });
    assert.equal(store.tokens.update('sso', sha('nope'), {}), false);

    const row = store.tokens.consume('verify', sha('v'), now + 10);
    assert.equal(row.userId, uid);
    assert.equal(row.kind, 'verify');
    assert.deepEqual(row.data, { email: 't@example.org' });
    assert.equal(row.consumedAt, now + 10);
    assert.equal(store.tokens.consume('verify', sha('v'), now + 11), null, 'single use');
    assert.equal(store.tokens.get('verify', sha('v')).consumedAt, now + 10);
    assert.equal(store.tokens.consume('sso', sha('v'), now + 1000), null, 'expired');
    assert.equal(store.tokens.consume('reset', sha('v'), now), null, 'kind matters');
    store.close();
});

test('sso: find, link, re-link, conflict', () => {
    const store = memStore();
    const a = store.users.create({ username: 'A', email: 'a@example.org' });
    const b = store.users.create({ username: 'B', email: 'b@example.org' });
    assert.equal(store.sso.find('google', '123'), null);
    store.sso.link(a, 'google', '123', 'a@gmail.com');
    assert.deepEqual(store.sso.find('google', '123'), { userId: a, email: 'a@gmail.com' });
    store.sso.link(a, 'google', '123', 'a2@gmail.com');
    assert.equal(store.sso.find('google', '123').email, 'a2@gmail.com');
    assert.throws(() => store.sso.link(b, 'google', '123', 'b@gmail.com'), (e) => e.code === 'sso_taken');
    assert.equal(store.sso.find('google', '123').userId, a);
    store.sso.link(b, 'google', '456', null);
    assert.equal(store.sso.forUser(b).length, 1);
    assert.equal(store.sso.forUser(b)[0].subject, '456');
    store.close();
});

// Several threads, each with its own connection to the same file, race to consume the same
// tokens: each token must be consumed exactly once.
function runWorker(code, workerData) {
    return new Promise((resolve, reject) => {
        const w = new Worker(`
            const { workerData, parentPort } = require('node:worker_threads');
            (async () => { ${code} })().then((r) => parentPort.postMessage({ ok: true, r }),
                (e) => parentPort.postMessage({ ok: false, e: String((e && e.stack) || e) }));
        `, { eval: true, workerData });
        w.once('message', (m) => (m.ok ? resolve(m.r) : reject(new Error(m.e))));
        w.once('error', reject);
    });
}

test('tokens.consume is single-use across connections racing on the same file', async () => {
    const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'scacelith-race-'));
    const file = path.join(dir, 'race.db');
    const store = openStore(testConfig({ DB_PATH: file }));
    migrate(store);
    const uid = store.users.create({ username: 'Race', email: 'r@example.org' });
    const hashes = [];
    for (let i = 0; i < 300; i++) {
        const h = sha(`race${i}`).toString('hex');
        hashes.push(h);
        store.tokens.create({ kind: 'reset', tokenHash: h, userId: uid, expiresAt: Date.now() + 3600000 });
    }
    const sab = new SharedArrayBuffer(8);
    const ready = new Int32Array(sab, 0, 1);
    const go = new Int32Array(sab, 4, 1);
    const WORKERS = 4;
    const code = `
        const { openStore } = await import(${JSON.stringify(STORE_URL)});
        const { testConfig } = await import(${JSON.stringify(CONFIG_URL)});
        const store = openStore(testConfig({ DB_PATH: workerData.file }));
        const ready = new Int32Array(workerData.sab, 0, 1), go = new Int32Array(workerData.sab, 4, 1);
        Atomics.add(ready, 0, 1);
        Atomics.wait(go, 0, 0);
        const won = [];
        for (const h of workerData.hashes) if (store.tokens.consume('reset', h, Date.now())) won.push(h);
        store.close();
        return won;`;
    const runs = [];
    for (let i = 0; i < WORKERS; i++) runs.push(runWorker(code, { file, sab, hashes }));
    while (Atomics.load(ready, 0) < WORKERS) await sleep(5);
    Atomics.store(go, 0, 1);
    Atomics.notify(go, 0);
    const results = await Promise.all(runs);
    const all = results.flat();
    assert.equal(all.length, hashes.length, 'every token consumed exactly once');
    assert.equal(new Set(all).size, hashes.length);
    for (const h of hashes) assert.ok(store.tokens.get('reset', h).consumedAt > 0);
    store.close();
    fs.rmSync(dir, { recursive: true, force: true });
});
