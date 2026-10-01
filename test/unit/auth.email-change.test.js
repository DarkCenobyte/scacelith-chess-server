// E-mail change (POST /account/email, GET/POST /confirm-email-change; auth/accounts.js): with and
// without REQUIRE_EMAIL_VERIFICATION, enumeration resistance, the confirmation page, the notices
// and their masked addresses, and what a change or a new password cancels.

import test from 'node:test';
import assert from 'node:assert/strict';
import { base32Decode, totp } from '../../src/security/totp.js';
import { maskEmail } from '../../src/auth/identity.js';
import { templates } from '../../src/mail/templates.js';
import { linkIn, startTestServer } from './helpers/auth-fakes.js';

const PW = 'correct horse battery';
const EMAIL = '/api/v1/account/email';
const FORM = 'application/x-www-form-urlencoded';

async function setup(env = {}) {
    const s = await startTestServer({ env });
    const u = await s.createUser({ username: 'alice', email: 'alice@example.com', password: PW });
    const { token } = await s.login('alice', PW);
    return { s, u, token };
}

/** Mails sent since `from` (an index into mailer.sent), once the queue is empty. */
async function mailsSince(s, from = 0) {
    await s.mailer.idle();
    return s.mailer.sent.slice(from);
}

function linkToken(mail) {
    const url = new URL(linkIn(mail.text));
    assert.equal(url.pathname, '/confirm-email-change');
    return url.searchParams.get('token');
}

const page = (s, token) => s.request('GET', `/confirm-email-change?token=${encodeURIComponent(token)}`);
const confirm = (s, token) => s.request('POST', '/confirm-email-change', { raw: `token=${encodeURIComponent(token)}`, contentType: FORM });
const me = async (s, token) => (await s.request('GET', '/api/v1/account/me', { token })).json.user;
const events = (s, kind) => { s.auth.events.flush(); return s.store._raw.securityEvents.filter((e) => e.kind === kind); };

test('maskEmail keeps the first character and the domain', () => {
    assert.equal(maskEmail('nora@example.org'), 'n***@example.org');
    assert.equal(maskEmail('n@example.org'), 'n***@example.org');
    assert.equal(maskEmail('bad'), '***');
    assert.equal(maskEmail(''), '***');
});

test('with e-mail verification: 202, link to the new address, notice to the current one, page then confirmation', async (t) => {
    const { s, u, token } = await setup();
    t.after(s.close);
    const other = await s.login('alice', PW);
    const before = s.mailer.sent.length;

    const r = await s.request('POST', EMAIL, { token, body: { newEmail: ' Nora@Example.ORG ', password: PW } });
    assert.deepEqual([r.status, r.json], [202, { status: 'verification_sent' }]);
    // Nothing changes before the link is used; the pending address shows.
    let view = await me(s, token);
    assert.equal(view.email, 'alice@example.com');
    assert.equal(view.pendingEmail, 'nora@example.org');
    assert.equal(s.store.users.byId(u.id).email, 'alice@example.com');

    const mails = await mailsSince(s, before);
    assert.deepEqual(mails.map((m) => m.to).sort(), ['alice@example.com', 'nora@example.org']);
    const link = mails.find((m) => m.to === 'nora@example.org');
    assert.match(link.subject, /Confirm your new e-mail address for/);
    assert.ok(link.text.includes('Hello alice'));
    assert.equal(new URL(linkIn(link.text)).origin, 'http://chess.example.org:8443');
    const notice = mails.find((m) => m.to === 'alice@example.com');
    assert.match(notice.subject, /A change of your .* e-mail address was requested/);
    assert.ok(notice.text.includes('n***@example.org'), 'the new address, masked');
    assert.ok(!notice.text.includes('nora@example.org'), 'never in full');
    assert.ok(!notice.text.includes('confirm-email-change'), 'no link in the notice');
    assert.equal(events(s, 'email_change_requested').length, 1);

    // GET shows the address and a button, and does not consume the token (link scanners).
    const tk = linkToken(link);
    for (let i = 0; i < 2; i++) {
        const p = await page(s, tk);
        assert.equal(p.status, 200);
        assert.match(p.headers['content-type'], /^text\/html/);
        assert.match(p.headers['content-security-policy'], /default-src 'none'/);
        assert.ok(p.text.includes('nora@example.org') && p.text.includes('alice'));
        assert.match(p.text, /<form method="post" action="\/confirm-email-change">/);
    }
    assert.equal((await me(s, token)).pendingEmail, 'nora@example.org');

    const sentBefore = s.mailer.sent.length;
    const done = await confirm(s, tk);
    assert.equal(done.status, 200);
    assert.match(done.text, /E-mail address changed/);
    assert.ok(done.text.includes('nora@example.org'));
    const row = s.store.users.byId(u.id);
    assert.equal(row.email, 'nora@example.org');
    assert.equal(row.emailVerified, true);

    // Both sessions stay signed in and read the new address (the cached sessions are dropped everywhere).
    for (const tok of [token, other.token]) {
        view = await me(s, tok);
        assert.equal(view.email, 'nora@example.org');
        assert.equal(view.pendingEmail, null);
    }
    assert.ok(s.primary.calls.some((c) => c.type === 'session.revoked' && c.payload.userId === u.id && c.payload.tokenHashes.length === 0));
    assert.equal(s.store._raw.sessions.size, 2);
    assert.ok([...s.store._raw.sessions.values()].every((x) => !x.revokedAt), 'no session revoked');

    // The former address is told, with the new one masked.
    const after = await mailsSince(s, sentBefore);
    assert.equal(after.length, 1);
    assert.equal(after[0].to, 'alice@example.com');
    assert.match(after[0].subject, /e-mail address was changed/);
    assert.ok(after[0].text.includes('n***@example.org') && !after[0].text.includes('nora@example.org'));
    assert.equal(events(s, 'email_changed').length, 1);

    // Single use; the new address logs in, the old one no longer does.
    const again = await confirm(s, tk);
    assert.equal(again.status, 400);
    assert.match(again.text, /invalid, was already used, or has expired/);
    assert.equal((await page(s, tk)).status, 400);
    assert.equal((await s.request('POST', '/api/v1/auth/login', { body: { login: 'nora@example.org', password: PW } })).status, 200);
    assert.equal((await s.request('POST', '/api/v1/auth/login', { body: { login: 'alice@example.com', password: PW } })).status, 401);
});

test('a taken address gets the same answer; its owner gets the throttled notice, never a link', async (t) => {
    const { s, u, token } = await setup();
    t.after(s.close);
    const bob = await s.createUser({ username: 'bob', email: 'bob@example.org', password: PW });
    const free = await s.request('POST', EMAIL, { token, body: { newEmail: 'free@example.org', password: PW } });
    const freeView = await me(s, token);
    const before = s.mailer.sent.length;
    const taken = await s.request('POST', EMAIL, { token, body: { newEmail: 'BOB@example.org', password: PW } });
    assert.deepEqual([taken.status, taken.json], [free.status, free.json]);
    assert.deepEqual(taken.json, { status: 'verification_sent' });
    const view = await me(s, token);
    assert.equal(view.pendingEmail, 'bob@example.org', 'pendingEmail as for a free address');
    assert.deepEqual(Object.keys(view), Object.keys(freeView));

    let mails = await mailsSince(s, before);
    assert.deepEqual(mails.map((m) => m.to).sort(), ['alice@example.com', 'bob@example.org']);
    const toBob = mails.find((m) => m.to === 'bob@example.org');
    assert.match(toBob.subject, /Someone tried to use your e-mail address/);
    assert.ok(toBob.text.includes('Hello bob'));
    assert.ok(!toBob.text.includes('alice'), 'the requester is not named');
    assert.equal(linkIn(toBob.text), null, 'no link to the owner of the address');
    const toAlice = mails.find((m) => m.to === 'alice@example.com');
    assert.match(toAlice.subject, /was requested/);
    assert.ok(toAlice.text.includes('b***@example.org'));
    assert.equal(s.store.users.byId(bob.id).email, 'bob@example.org');
    assert.equal(s.store.users.byId(u.id).email, 'alice@example.com');

    // The requester's events are the same whatever the address; the owner's has no IP.
    assert.equal(events(s, 'email_change_requested').filter((e) => e.userId === u.id).length, 2);
    const owner = events(s, 'email_change_existing_email');
    assert.equal(owner.length, 1);
    assert.equal(owner[0].userId, bob.id);
    assert.equal(owner[0].ip ?? null, null);

    // Within the hour, no second notice to the owner; the requester's notice still goes.
    const before2 = s.mailer.sent.length;
    const again = await s.request('POST', EMAIL, { token, body: { newEmail: 'bob@example.org', password: PW } });
    assert.equal(again.status, 202);
    mails = await mailsSince(s, before2);
    assert.deepEqual(mails.map((m) => m.to), ['alice@example.com']);
});

test('invalid and same address are refused before the password is checked', async (t) => {
    const { s, token } = await setup();
    t.after(s.close);
    let r = await s.request('POST', EMAIL, { token, body: { newEmail: 'not-an-address', password: 'wrong wrong wrong' } });
    assert.deepEqual([r.status, r.json.error], [400, 'invalid_email']);
    r = await s.request('POST', EMAIL, { token, body: { newEmail: ' ALICE@example.com', password: 'wrong wrong wrong' } });
    assert.deepEqual([r.status, r.json.error], [400, 'same_email']);
    assert.equal(events(s, 'reauth_failed').length, 0);
    r = await s.request('POST', EMAIL, { token, body: { newEmail: 'nora@example.org', password: 'wrong wrong wrong' } });
    assert.deepEqual([r.status, r.json.error], [403, 'invalid_password']);
    assert.equal(events(s, 'reauth_failed').length, 1);
    r = await s.request('POST', EMAIL, { token, body: { newEmail: 'nora@example.org' } });
    assert.deepEqual([r.status, r.json.error], [400, 'invalid_request']);
    r = await s.request('POST', EMAIL, { body: { newEmail: 'nora@example.org', password: PW } });
    assert.equal(r.status, 401);
    assert.equal((await me(s, token)).pendingEmail, null);
});

test('two-step verification: a code or a recovery code is required; the reauth failure counter is shared with deletion', async (t) => {
    const { s, token } = await setup({ AUTH_FAILURES_PER_ACCOUNT: '3' });
    t.after(s.close);
    const setupR = await s.request('POST', '/api/v1/account/mfa/totp/setup', { token, body: { password: PW } });
    const secret = base32Decode(setupR.json.secret);
    const en = await s.request('POST', '/api/v1/account/mfa/totp/enable', { token, body: { code: totp(secret, s.now()) } });
    s.now.advance(30000);
    let r = await s.request('POST', EMAIL, { token, body: { newEmail: 'nora@example.org', password: PW } });
    assert.deepEqual([r.status, r.json.error], [403, 'mfa_code_required']);
    r = await s.request('POST', EMAIL, { token, body: { newEmail: 'nora@example.org', password: PW, code: totp(secret, s.now()) } });
    assert.equal(r.status, 202);
    r = await s.request('POST', EMAIL, { token, body: { newEmail: 'nina@example.org', password: PW, recoveryCode: en.json.recoveryCodes[0] } });
    assert.equal(r.status, 202);
    assert.equal(s.store.mfa.countRecoveryCodes(1), 9, 'the recovery code is used up');
    assert.equal((await me(s, token)).pendingEmail, 'nina@example.org');

    // Wrong passwords here and at deletion count together (one FailureCounter, accounts.js reauth).
    for (let i = 0; i < 2; i++) {
        r = await s.request('POST', EMAIL, { token, body: { newEmail: 'x@example.org', password: 'nope nope nope' } });
        assert.equal(r.json.error, 'invalid_password');
    }
    r = await s.request('POST', '/api/v1/account/delete', { token, body: { password: 'nope nope nope' } });
    assert.equal(r.json.error, 'invalid_password');
    r = await s.request('POST', EMAIL, { token, body: { newEmail: 'x@example.org', password: PW, code: '123456' } });
    assert.deepEqual([r.status, r.json.error], [429, 'too_many_attempts']);
});

test('a new request replaces the pending one; an expired link does nothing', async (t) => {
    const { s, u, token } = await setup();
    t.after(s.close);
    let from = s.mailer.sent.length;
    await s.request('POST', EMAIL, { token, body: { newEmail: 'first@example.org', password: PW } });
    const first = linkToken((await mailsSince(s, from)).find((m) => m.to === 'first@example.org'));
    from = s.mailer.sent.length;
    await s.request('POST', EMAIL, { token, body: { newEmail: 'second@example.org', password: PW } });
    const second = linkToken((await mailsSince(s, from)).find((m) => m.to === 'second@example.org'));
    assert.equal((await me(s, token)).pendingEmail, 'second@example.org');
    assert.equal([...s.store._raw.tokens.values()].filter((r) => r.kind === 'email_change').length, 1, 'one pending change per account');
    assert.equal((await page(s, first)).status, 400);
    assert.equal((await confirm(s, first)).status, 400);
    assert.equal(s.store.users.byId(u.id).email, 'alice@example.com');

    s.now.advance(24 * 3600000 + 1);
    assert.equal((await me(s, token)).pendingEmail, null, 'expired after 24 h');
    assert.equal((await page(s, second)).status, 400);
    assert.equal((await confirm(s, second)).status, 400);
    assert.equal(s.store.users.byId(u.id).email, 'alice@example.com');
    assert.equal((await page(s, 'A'.repeat(43))).status, 400);
    assert.equal((await confirm(s, 'garbage')).status, 400);
});

test('the address taken between the request and the confirmation: refused, nothing changes', async (t) => {
    const { s, u, token } = await setup();
    t.after(s.close);
    const from = s.mailer.sent.length;
    await s.request('POST', EMAIL, { token, body: { newEmail: 'nora@example.org', password: PW } });
    const tk = linkToken((await mailsSince(s, from)).find((m) => m.to === 'nora@example.org'));
    await s.createUser({ username: 'nora', email: 'nora@example.org', password: PW });
    assert.equal((await page(s, tk)).status, 200, 'the page itself does not tell');
    const before = s.mailer.sent.length;
    const r = await confirm(s, tk);
    assert.equal(r.status, 409);
    assert.match(r.text, /Another account now uses this e-mail address/);
    assert.equal(s.store.users.byId(u.id).email, 'alice@example.com');
    assert.equal((await mailsSince(s, before)).length, 0);
    assert.equal(events(s, 'email_change_refused').length, 1);
    assert.equal((await confirm(s, tk)).status, 400, 'the link is used up');
});

test('the store\'s unique index decides when the address is taken at the last moment', async (t) => {
    const { s, u, token } = await setup();
    t.after(s.close);
    const from = s.mailer.sent.length;
    await s.request('POST', EMAIL, { token, body: { newEmail: 'nora@example.org', password: PW } });
    const tk = linkToken((await mailsSince(s, from)).find((m) => m.to === 'nora@example.org'));
    // byEmail does not see it yet, the update refuses it (another process won the race).
    const realByEmail = s.store.users.byEmail;
    s.store.users.byEmail = (e) => (String(e).toLowerCase() === 'nora@example.org' ? null : realByEmail(e));
    await s.createUser({ username: 'nora', email: 'nora@example.org', password: PW });
    t.after(() => { s.store.users.byEmail = realByEmail; });
    const r = await confirm(s, tk);
    assert.equal(r.status, 409);
    assert.equal(s.store.users.byId(u.id).email, 'alice@example.com');
});

test('a new password cancels a pending change; a change ends the links sent to the former address', async (t) => {
    const { s, u, token } = await setup();
    t.after(s.close);
    let from = s.mailer.sent.length;
    await s.request('POST', EMAIL, { token, body: { newEmail: 'nora@example.org', password: PW } });
    const tk = linkToken((await mailsSince(s, from)).find((m) => m.to === 'nora@example.org'));
    const NEW_PW = 'a brand new passphrase';
    const r = await s.request('POST', '/api/v1/account/password', { token, body: { currentPassword: PW, newPassword: NEW_PW } });
    assert.equal(r.status, 200);
    assert.equal((await me(s, token)).pendingEmail, null);
    assert.equal((await confirm(s, tk)).status, 400);
    assert.equal(s.store.users.byId(u.id).email, 'alice@example.com');

    // A reset link sent to the former address dies with the change.
    from = s.mailer.sent.length;
    await s.request('POST', '/api/v1/auth/password/forgot', { body: { email: 'alice@example.com' } });
    const reset = new URL(linkIn((await mailsSince(s, from))[0].text)).searchParams.get('token');
    from = s.mailer.sent.length;
    await s.request('POST', EMAIL, { token, body: { newEmail: 'nora@example.org', password: NEW_PW } });
    const tk2 = linkToken((await mailsSince(s, from)).find((m) => m.to === 'nora@example.org'));
    assert.equal((await confirm(s, tk2)).status, 200);
    const rr = await s.request('POST', '/api/v1/auth/password/reset', { body: { token: reset, newPassword: 'attacker chooses this one' } });
    assert.deepEqual([rr.status, rr.json.error], [400, 'invalid_token']);

    // A password reset cancels a pending change too.
    await s.request('POST', EMAIL, { token, body: { newEmail: 'zoe@example.org', password: NEW_PW } });
    from = s.mailer.sent.length;
    await s.request('POST', '/api/v1/auth/password/forgot', { body: { email: 'nora@example.org' } });
    const reset2 = new URL(linkIn((await mailsSince(s, from))[0].text)).searchParams.get('token');
    assert.equal((await s.request('POST', '/api/v1/auth/password/reset', { body: { token: reset2, newPassword: 'yet another passphrase' } })).status, 200);
    assert.ok(![...s.store._raw.tokens.values()].some((x) => x.kind === 'email_change'), 'the pending change is gone');
});

test('without e-mail verification: changed at once, 409 email_taken, the former address told', async (t) => {
    const { s, u, token } = await setup({ REQUIRE_EMAIL_VERIFICATION: '0' });
    t.after(s.close);
    const bob = await s.createUser({ username: 'bob', email: 'bob@example.org', password: PW });
    let from = s.mailer.sent.length;
    let r = await s.request('POST', EMAIL, { token, body: { newEmail: 'Bob@Example.org', password: PW } });
    assert.deepEqual([r.status, r.json.error], [409, 'email_taken']);
    let mails = await mailsSince(s, from);
    assert.deepEqual(mails.map((m) => m.to), ['bob@example.org'], 'the owner still gets the notice');
    assert.match(mails[0].subject, /Someone tried to use your e-mail address/);

    from = s.mailer.sent.length;
    r = await s.request('POST', EMAIL, { token, body: { newEmail: 'Nora@Example.org', password: PW } });
    assert.deepEqual([r.status, r.json], [200, { status: 'email_changed', email: 'nora@example.org' }]);
    const view = await me(s, token);
    assert.equal(view.email, 'nora@example.org');
    assert.equal(view.emailVerified, true);
    assert.equal(view.pendingEmail, null);
    mails = await mailsSince(s, from);
    assert.deepEqual(mails.map((m) => m.to), ['alice@example.com']);
    assert.match(mails[0].subject, /e-mail address was changed/);
    assert.ok(mails[0].text.includes('n***@example.org') && !mails[0].text.includes('nora@example.org'));
    assert.ok(!s.mailer.sent.some((m) => linkIn(m.text)?.includes('confirm-email-change')), 'no confirmation link');
    assert.equal(s.store.users.byId(bob.id).email, 'bob@example.org');
    assert.equal(events(s, 'email_changed').filter((e) => e.userId === u.id).length, 1);
});

test('an account without a password (Google only) is told to set one first', async (t) => {
    const s = await startTestServer();
    t.after(s.close);
    const u = await s.createUser({ username: 'googler', password: null });
    const created = s.auth._svc.sessions.create(s.store.users.byId(u.id), {});
    const r = await s.request('POST', EMAIL, { token: created.token, body: { newEmail: 'other@example.org', password: 'anything at all' } });
    assert.deepEqual([r.status, r.json.error], [400, 'password_not_set']);
});

test('the three mail templates are plain English and carry what they promise', () => {
    const when = new Date(Date.UTC(2026, 9, 1, 12));
    const c = templates.emailChangeConfirm({ serverName: 'Club', username: 'alice', link: 'https://h/confirm-email-change?token=T', hours: 24 });
    assert.ok(c.text.includes('https://h/confirm-email-change?token=T') && c.text.includes('24 hours'));
    const q = templates.emailChangeRequested({ serverName: 'Club', username: 'alice', maskedEmail: 'n***@example.org', when, hours: 24 });
    assert.ok(q.text.includes('n***@example.org') && q.text.includes(when.toUTCString()) && q.text.includes('Forgot password'));
    const d = templates.emailChanged({ serverName: 'Club', username: 'alice', maskedEmail: 'n***@example.org', when });
    assert.ok(d.text.includes('n***@example.org') && d.text.includes('administrator of\nClub'));
    for (const m of [c, q, d]) {
        assert.ok(/^[\x20-\x7e\n]*$/.test(m.text + m.subject), 'ASCII text');
        assert.ok(m.text.includes('replies are not read'));
    }
});
