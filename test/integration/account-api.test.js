// The account API end to end on the real server (bin/scacelith-server.js, TLS, SQLite), with
// e-mail confirmation on and the mails read from the log transport: register and confirm, log
// in, play a rated game to its end, the player's history (/account/games), the caller-aware game
// record (/games/:id: you, reportable), its PGN, an e-mail change through its link, the data
// export, the deletion of the account and a password reset (both close the live connection of
// the revoked sessions). Then the default port: a server started without the right to bind 443
// logs how to fix it and its worker exits non-zero. Needs the openssl command line (skipped
// without it).
import test, { before, after } from 'node:test';
import assert from 'node:assert/strict';
import { execFileSync, spawn } from 'node:child_process';
import fs from 'node:fs';
import https from 'node:https';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { DatabaseSync } from 'node:sqlite';
import { startServer, haveOpenssl } from './helpers/harness.js';
import { PASSWORD, connect, challengeGame, Table, closeAll } from './helpers/players.js';
import { ApiClient } from '../../src/client/index.js';
import { CloseCode, enums } from '../../src/protocol/index.js';

const skip = !haveOpenssl() && 'openssl not available';
const FORM = 'application/x-www-form-urlencoded';

let srv;
before(async () => {
    if (skip) return;
    srv = await startServer({ workers: 1, env: { REQUIRE_EMAIL_VERIFICATION: 'true', FIRST_MOVE_TIMEOUT_MS: '20000', DB_COMMIT_MS: '20' } });
});
after(async () => { if (srv) await srv.stop(); });

/** The latest mail the log transport wrote to `to` whose subject matches, once it is there. */
async function mailTo(to, subject, { after: seen = 0 } = {}) {
    const deadline = Date.now() + 5000;
    for (;;) {
        const all = srv.lines.filter((r) => r.msg === 'mail (log transport)' && r.to === to && subject.test(r.subject));
        if (all.length > seen) return all[all.length - 1];
        if (Date.now() > deadline) throw new Error(`no mail to ${to} matching ${subject}`);
        await new Promise((r) => setTimeout(r, 50));
    }
}
const mailsTo = (to) => srv.lines.filter((r) => r.msg === 'mail (log transport)' && r.to === to);
const linkPath = (text) => { const u = new URL(/(https:\/\/\S+)/.exec(text)[1]); return u.pathname + u.search; };

/** An HTML page of the server (the e-mail links), GET or POST with a form body. */
function page(method, p, form) {
    return new Promise((resolve, reject) => {
        const body = form ? Buffer.from(new URLSearchParams(form).toString()) : null;
        const headers = body ? { 'Content-Type': FORM, 'Content-Length': body.length } : {};
        const req = https.request({ host: '127.0.0.1', port: srv.apiPort, method, path: p, headers, ca: srv.ca, servername: 'localhost', agent: false }, (res) => {
            let text = '';
            res.setEncoding('utf8');
            res.on('data', (c) => { text += c; });
            res.on('end', () => resolve({ status: res.statusCode, headers: res.headers, text }));
        });
        req.on('error', reject);
        req.end(body || undefined);
    });
}

/** The close code of the player's connection, once it got Notice{SessionRevoked} and closed. */
async function closedAsRevoked(p) {
    const notice = p.client.waitFor('Notice', (n) => n.code === enums.NoticeCode.SessionRevoked, 10000);
    const closed = p.client.waitFor('close', null, 10000);
    await notice;
    return (await closed).code;
}

/** Registers `name`, confirms the address with the link of the mail, logs in and connects. */
async function verifiedPlayer(name) {
    const api = new ApiClient({ host: '127.0.0.1', port: srv.apiPort, ca: srv.ca, servername: 'localhost' });
    const email = `${name}@example.org`;
    const r = await api.register({ username: name, email, password: PASSWORD });
    assert.deepEqual([r.status, r.body], [202, { status: 'verification_sent' }]);
    // No account before the link is used: the sign-in fails as for an unknown name, and the
    // database has no row for it.
    const early = await api.login(name, PASSWORD);
    assert.deepEqual([early.status, early.body.error], [401, 'invalid_credentials']);
    const db = new DatabaseSync(path.join(srv.dir, 'scacelith.db'), { readOnly: true });
    assert.equal(db.prepare('SELECT COUNT(*) AS n FROM users WHERE username = ?').get(name).n, 0);
    db.close();
    const mail = await mailTo(email, /Confirm your e-mail address for/);
    const token = new URL(`https://x${linkPath(mail.text)}`).searchParams.get('token');
    assert.equal((await page('GET', linkPath(mail.text))).status, 200);
    const done = await page('POST', '/verify-email', { token });
    assert.equal(done.status, 200);
    const l = await api.login(name, PASSWORD, { clientLabel: 'integration test' });
    assert.equal(l.status, 200, JSON.stringify(l.body));
    const client = await connect(srv, api.token);
    return { api, token: api.token, name, email, client, userId: l.body.user.id };
}

test('register, play, history, game record, PGN, e-mail change, export, deletion', { skip }, async () => {
    const alice = await verifiedPlayer('alice_api'), bob = await verifiedPlayer('bob_api');
    try {
        // A rated 3+2 game that Bob (Black) resigns.
        const g = await challengeGame(alice, bob, { baseSec: 180, incSec: 2, rated: true });
        const table = new Table(g);
        await table.playAll(['e2e4', 'e7e5', 'g1f3', 'b8c6', 'f1c4']);
        const m = alice.client.mark();
        bob.client.resign(g.id);
        await alice.client.waitFor('GameEnd', (x) => x.game === g.id, 10000, { since: m });

        // The history, once the game is committed.
        let hist;
        const deadline = Date.now() + 10000;
        do {
            hist = await alice.api.get('/account/games');
            if (hist.status === 200 && hist.body.total === 1) break;
            await new Promise((r) => setTimeout(r, 50));
        } while (Date.now() < deadline);
        assert.equal(hist.status, 200, JSON.stringify(hist.body));
        assert.equal(hist.body.total, 1);
        assert.equal(hist.body.next, null);
        const sum = hist.body.games[0];
        assert.equal(String(sum.id), String(g.id));
        assert.deepEqual([sum.outcome, sum.color, sum.category, sum.rated, sum.baseMs, sum.incMs, sum.plies], ['win', 'white', '3+2', true, 180000, 2000, 5]);
        assert.equal(sum.white.name, 'alice_api');
        assert.equal(sum.black.name, 'bob_api');
        assert.equal((await bob.api.get('/account/games?result=loss')).body.total, 1);
        assert.equal((await bob.api.get('/account/games?result=win')).body.total, 0);
        assert.equal((await alice.api.get('/account/games?limit=0')).body.error, 'invalid_limit');
        assert.equal((await alice.api.get('/account/games', { token: null })).status, 401);

        // The game record: caller-aware for its players, public otherwise.
        const rec = await alice.api.get(`/games/${g.id}`);
        assert.equal(rec.status, 200);
        assert.equal(rec.body.you, 'white');
        assert.equal(rec.body.reportable, true);
        const asBob = await bob.api.get(`/games/${g.id}`);
        assert.equal(asBob.body.you, 'black');
        const pub = await alice.api.get(`/games/${g.id}`, { token: null });
        assert.equal(pub.status, 200);
        assert.ok(!('you' in pub.body) && !('reportable' in pub.body));

        // The PGN.
        const pgn = await alice.api.get(`/games/${g.id}/pgn`);
        assert.equal(pgn.status, 200);
        assert.equal(pgn.headers['content-type'], 'application/x-chess-pgn; charset=utf-8');
        assert.equal(pgn.headers['content-disposition'], `attachment; filename="scacelith-${g.id}.pgn"`);
        const text = pgn.body;
        assert.equal(typeof text, 'string');
        assert.ok(text.includes(`[ScacelithGameId "${g.id}"]`));
        assert.ok(text.includes('[White "alice_api"]') && text.includes('[Black "bob_api"]') && text.includes('[Result "1-0"]'));
        assert.ok(text.includes('[TimeControl "180+2"]') && text.includes('[Termination "normal"]') && text.includes('[PlyCount "5"]'));
        assert.match(text, /^1\. e4 \{\[%clk \d:\d\d:\d\d\.\d\]/m);
        assert.ok(text.trimEnd().endsWith('1-0'));

        // An e-mail change through its link.
        const seenOld = mailsTo(alice.email).length;
        let r = await alice.api.post('/account/email', { newEmail: 'alice.new@example.org', password: PASSWORD });
        assert.deepEqual([r.status, r.body], [202, { status: 'verification_sent' }]);
        assert.equal((await alice.api.me()).body.user.pendingEmail, 'alice.new@example.org');
        const notice = await mailTo(alice.email, /was requested/, { after: 0 });
        assert.ok(notice.text.includes('a***@example.org') && !notice.text.includes('alice.new@example.org'));
        const link = await mailTo('alice.new@example.org', /Confirm your new e-mail address/);
        const lp = linkPath(link.text);
        assert.ok(lp.startsWith('/confirm-email-change?token='));
        const form = await page('GET', lp);
        assert.equal(form.status, 200);
        assert.ok(form.text.includes('alice.new@example.org'));
        const ok = await page('POST', '/confirm-email-change', { token: new URL(`https://x${lp}`).searchParams.get('token') });
        assert.equal(ok.status, 200);
        assert.match(ok.text, /E-mail address changed/);
        const me = await alice.api.me();
        assert.equal(me.status, 200, 'still signed in');
        assert.equal(me.body.user.email, 'alice.new@example.org');
        assert.equal(me.body.user.pendingEmail, null);
        const changed = await mailTo(alice.email, /e-mail address was changed/);
        assert.ok(changed.text.includes('a***@example.org'));
        assert.ok(mailsTo(alice.email).length >= seenOld + 2);

        // The export.
        r = await alice.api.post('/account/export', { password: PASSWORD });
        assert.equal(r.status, 200, JSON.stringify(r.body));
        assert.equal(r.headers['content-disposition'], 'attachment; filename="scacelith-account-alice_api.json"');
        const doc = r.body;
        assert.equal(doc.format, 'scacelith-account-export');
        assert.equal(doc.account.email, 'alice.new@example.org');
        assert.equal(doc.games.total, 1);
        assert.equal(String(doc.games.list[0].id), String(g.id));
        assert.ok(doc.sessions.some((s) => s.clientLabel === 'integration test'));
        assert.ok(doc.ratings.some((x) => x.category === '3+2' && x.games === 1));
        const db = new DatabaseSync(path.join(srv.dir, 'scacelith.db'), { readOnly: true });
        const row = db.prepare('SELECT password_hash FROM users WHERE id = ?').get(alice.userId);
        const hashes = db.prepare('SELECT token_hash FROM sessions WHERE user_id = ?').all(alice.userId).map((x) => Buffer.from(x.token_hash).toString('hex'));
        db.close();
        const json = JSON.stringify(doc);
        assert.ok(row.password_hash && !json.includes(row.password_hash), 'no password hash');
        for (const h of hashes) assert.ok(!json.includes(h), 'no session token hash');
        assert.ok(!json.includes(alice.token));

        // The deletion; it closes the live connection too.
        let kicked = closedAsRevoked(alice);
        r = await alice.api.post('/account/delete', { password: PASSWORD });
        assert.deepEqual([r.status, r.body], [200, { status: 'deleted' }]);
        assert.equal(await kicked, CloseCode.Unauthorized);
        assert.equal((await alice.api.me()).status, 401);
        assert.equal((await alice.api.post('/account/export', { password: PASSWORD })).status, 401);
        assert.equal((await alice.api.login('alice_api', PASSWORD)).status, 401);
        const after = await bob.api.get(`/games/${g.id}`);
        assert.equal(after.body.white.name, `deleted#${alice.userId}`);
        assert.equal((await bob.api.get('/account/games')).body.games[0].white.name, `deleted#${alice.userId}`);

        // A password reset revokes every session and closes the live connection.
        assert.equal((await bob.api.post('/auth/password/forgot', { email: bob.email }, { token: null })).status, 202);
        const reset = new URL(`https://x${linkPath((await mailTo(bob.email, /Reset your/)).text)}`).searchParams.get('token');
        kicked = closedAsRevoked(bob);
        r = await bob.api.post('/auth/password/reset', { token: reset, newPassword: 'another passphrase 42' }, { token: null });
        assert.deepEqual([r.status, r.body], [200, { status: 'password_reset' }]);
        assert.equal(await kicked, CloseCode.Unauthorized);
        assert.equal((await bob.api.me()).status, 401);
    } finally {
        await closeAll(alice, bob);
    }
});

// ---- the default port without the right to bind it --------------------------------------------------

const canDrop = (() => {
    try {
        if (process.getuid?.() !== 0) return false;
        if (Number(fs.readFileSync('/proc/sys/net/ipv4/ip_unprivileged_port_start', 'utf8')) <= 443) return false;
        execFileSync('setpriv', ['--version'], { stdio: 'ignore' });
        return true;
    } catch { return false; }
})();

test('without CAP_NET_BIND_SERVICE the default port 443 fails with the fixes in the log, and the worker exits non-zero', {
    skip: !canDrop && 'needs root (to run the server as nobody), setpriv and ip_unprivileged_port_start > 443',
}, async () => {
    const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'scacelith-it-port-'));
    fs.chmodSync(dir, 0o777);
    const bin = fileURLToPath(new URL('../../bin/scacelith-server.js', import.meta.url));
    const env = {
        PATH: process.env.PATH, HOME: dir, SERVER_SECRET: Buffer.alloc(48, 9).toString('base64'), DATA_DIR: dir, WORKERS: '1',
        TLS_MODE: 'off', ALLOW_INSECURE_DEV: '1', BIND_ADDRESS: '127.0.0.1', METRICS_PORT: '0', LOG_FORMAT: 'json', LOG_LEVEL: 'info',
        MAIL_TRANSPORT: 'none', REQUIRE_EMAIL_VERIFICATION: 'false', SCACELITH_ENV_FILE: '',
    };
    const child = spawn('setpriv', ['--reuid=65534', '--regid=65534', '--clear-groups', process.execPath, bin, 'start'], {
        cwd: path.dirname(path.dirname(bin)), env, stdio: ['ignore', 'pipe', 'pipe'],
    });
    const lines = [];
    let buf = '';
    const feed = (c) => {
        buf += c;
        let i;
        while ((i = buf.indexOf('\n')) >= 0) {
            const line = buf.slice(0, i);
            buf = buf.slice(i + 1);
            try { lines.push(JSON.parse(line)); } catch { lines.push({ raw: line }); }
        }
    };
    child.stdout.setEncoding('utf8').on('data', feed);
    child.stderr.setEncoding('utf8').on('data', feed);
    try {
        const deadline = Date.now() + 20000;
        let hint = null, exited = null;
        while (Date.now() < deadline && !(hint && exited)) {
            hint = lines.find((r) => typeof r.msg === 'string' && r.msg.startsWith('Cannot listen on port 443 (EACCES)'));
            exited = lines.find((r) => r.msg === 'shard exited');
            await new Promise((r) => setTimeout(r, 50));
        }
        assert.ok(hint, `the hint is logged:\n${lines.slice(-10).map((l) => JSON.stringify(l)).join('\n')}`);
        assert.equal(hint.level, 'error');
        assert.equal(hint.errorCode, 'EACCES');
        assert.equal(hint.port, 443);
        for (const fix of ['AmbientCapabilities=CAP_NET_BIND_SERVICE', 'setcap cap_net_bind_service=+ep', 'net.ipv4.ip_unprivileged_port_start=443']) {
            assert.ok(hint.msg.includes(fix), fix);
        }
        assert.ok(exited, 'the supervisor saw the worker exit');
        assert.equal(exited.exitCode, 1);
        assert.ok(!lines.some((r) => r.msg === 'shard ready'));
    } finally {
        // A graceful stop: the primary stops its supervisor (no restart) and its worker.
        const gone = new Promise((r) => (child.exitCode !== null || child.signalCode ? r() : child.once('exit', r)));
        child.kill('SIGTERM');
        const t = setTimeout(() => child.kill('SIGKILL'), 10000);
        await gone;
        clearTimeout(t);
        fs.rmSync(dir, { recursive: true, force: true });
    }
});

