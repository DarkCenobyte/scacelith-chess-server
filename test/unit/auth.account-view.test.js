// GET /account/me and PUT /account/preferences on the real SQLite store: the account view's
// lastLoginAt and pendingEmail, acceptChallenges as 'all' | 'none' over the store's boolean, and
// the e-mail change against the real unique index.

import test from 'node:test';
import assert from 'node:assert/strict';
import { linkIn } from './helpers/auth-fakes.js';
import { startReal } from './helpers/real-auth.js';

const PW = 'correct horse battery';
const FORM = 'application/x-www-form-urlencoded';

async function setup(t, env) {
    const s = await startReal(t, env);
    const id = s.store.users.create({ username: 'alice', email: 'alice@example.org', passwordHash: await s.hasher.hash(PW), emailVerified: true });
    const r = await s.request('POST', '/api/v1/auth/login', { body: { login: 'alice', password: PW } });
    assert.equal(r.status, 200, r.text);
    return { s, id, token: r.json.token, login: r.json };
}

async function linkOf(s, to) {
    await s.mailer.idle();
    const m = s.mailer.sent.filter((x) => x.to === to).at(-1);
    return new URL(linkIn(m.text)).searchParams.get('token');
}

test('GET /account/me keeps every field and adds lastLoginAt and pendingEmail; the login answer too', async (t) => {
    const { s, id, token, login } = await setup(t);
    const loginAt = s.now();
    assert.equal(login.user.lastLoginAt, loginAt);
    assert.equal(login.user.pendingEmail, null);
    s.now.advance(5000);
    let me = await s.request('GET', '/api/v1/account/me', { token });
    assert.equal(me.status, 200);
    assert.deepEqual(Object.keys(me.json.user).sort(), ['acceptChallenges', 'createdAt', 'email', 'emailVerified', 'googleLinked', 'hasPassword', 'id',
        'lastLoginAt', 'mfaEnabled', 'pendingEmail', 'username']);
    assert.equal(me.json.user.id, id);
    assert.equal(me.json.user.lastLoginAt, loginAt);
    assert.equal(me.json.user.acceptChallenges, 'all');
    assert.equal(me.json.user.pendingEmail, null);
    assert.ok(me.json.user.createdAt > 0);
    for (const k of ['ratings', 'sanctions', 'ban']) assert.ok(k in me.json);

    const r = await s.request('POST', '/api/v1/account/email', { token, body: { newEmail: 'Nora@Example.org', password: PW } });
    assert.equal(r.status, 202);
    me = await s.request('GET', '/api/v1/account/me', { token });
    assert.equal(me.json.user.pendingEmail, 'nora@example.org');
    assert.equal(me.json.user.email, 'alice@example.org');
});

test('preferences: \'none\' is stored as false and shown as \'none\'; challenges then refused by the primary\'s check', async (t) => {
    const { s, id, token } = await setup(t);
    let r = await s.request('PUT', '/api/v1/account/preferences', { token, body: { acceptChallenges: 'none' } });
    assert.deepEqual([r.status, r.json], [200, { preferences: { acceptChallenges: 'none' } }]);
    assert.equal(s.store.users.byId(id).acceptChallenges, false, 'primary-main.js acceptsChallenges reads acceptChallenges !== false');
    assert.equal((await s.request('GET', '/api/v1/account/me', { token })).json.user.acceptChallenges, 'none');
    r = await s.request('PUT', '/api/v1/account/preferences', { token, body: { acceptChallenges: 'all' } });
    assert.equal(r.status, 200);
    assert.equal(s.store.users.byId(id).acceptChallenges, true);
    assert.equal((await s.request('GET', '/api/v1/account/me', { token })).json.user.acceptChallenges, 'all');
});

test('e-mail change on the real store: the new address logs in; the unique index refuses an address taken meanwhile', async (t) => {
    const { s, id, token } = await setup(t);
    const post = (tk) => s.request('POST', '/confirm-email-change', { raw: `token=${tk}`, contentType: FORM });

    let r = await s.request('POST', '/api/v1/account/email', { token, body: { newEmail: 'nora@example.org', password: PW } });
    assert.equal(r.status, 202);
    let tk = await linkOf(s, 'nora@example.org');
    assert.equal((await s.request('GET', `/confirm-email-change?token=${tk}`)).status, 200);
    r = await post(tk);
    assert.equal(r.status, 200, r.text);
    assert.equal(s.store.users.byId(id).email, 'nora@example.org');
    assert.equal(s.store.users.byEmail('NORA@example.org').id, id, 'email_normalized follows');
    assert.equal(s.store.tokens.liveForUser(id, 'email_change', s.now()), null);
    const l = await s.request('POST', '/api/v1/auth/login', { body: { login: 'nora@example.org', password: PW } });
    assert.equal(l.status, 200);

    // Taken between the request and the confirmation, unseen by the lookup: the UNIQUE index decides.
    r = await s.request('POST', '/api/v1/account/email', { token, body: { newEmail: 'zoe@example.org', password: PW } });
    assert.equal(r.status, 202);
    tk = await linkOf(s, 'zoe@example.org');
    s.store.users.create({ username: 'zoe', email: 'Zoe@Example.org', passwordHash: 'x', emailVerified: true });
    const byEmail = s.store.users.byEmail;
    s.store.users.byEmail = () => null;
    try {
        r = await post(tk);
    } finally {
        s.store.users.byEmail = byEmail;
    }
    assert.equal(r.status, 409);
    assert.equal(s.store.users.byId(id).email, 'nora@example.org');
});
