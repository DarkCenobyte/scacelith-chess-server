#!/usr/bin/env node
// Live interoperability check: the game's C++ OnlineClient against this server, over real TLS.
//
//   node dedicated-server/tools/live-cpp-check.js [--only=game|account|sso] [path/to/scacelith_tests]
//                                                      (default build/scacelith_tests)
//   WINEDEBUG=-all node dedicated-server/tools/live-cpp-check.js [--only=...] wine path/to/scacelith_tests.exe
//                                                      (the Windows build: WinHTTP)
//
// Run it from the root of the source tree the test binary was built from, where the C++ tests
// expect to run (the live tests write their temporary credential files there). Three parts, one
// server each (--only takes a comma-separated list):
//
// game     A server (2 shards, self-signed certificate, HTTPS API and WSS on one port as in
//          production, proof of work on registration), a Node bot that queues in rated 3+2 and
//          plays random legal moves, then the C++ test `net_live_server_game`, which registers
//          (solving the proof of work), logs in, queues, plays 12 plies against the bot and resigns.
//          That server has no e-mail confirmation and its GIFs are turned off: then
//          `net_live_account_server_settings` changes the player's address (refused for the bot's
//          address: email_taken, then changed at once: email_changed; the notice to the former
//          address is looked up in the server's log) and asks for GIFs (gif_disabled).
//
// account  The account API (dedicated-server/docs/API.md) end to end: a server with one shard (so
//          that the GIF cache of the worker is the one every request reaches) and e-mail
//          confirmation on, the mails read from its log transport. The harness registers and
//          confirms three accounts, then plays games through the realtime protocol as the C++
//          player's account against a Node rival (rated win and loss, casual draw by agreement, a
//          custom time control won by checkmate, a promotion, an aborted game) and one game
//          between two other players. Then it runs the C++ test `net_live_account_api`, which
//          signs in with the C++ client and calls fetchMyGames (filters, paging), fetchGame,
//          downloadPgn, downloadGameGif / renderPgnGif (decoded; the per-account render quota and
//          a cache hit past it), setAcceptChallenges, changeEmail, exportAccount, fetchSessions /
//          revokeSession, the expired and revoked sessions, two-factor re-authentication (a
//          recovery code, an authenticator code) and deleteAccount.
//          The C++ test drives the harness through a small control server (plain HTTP on
//          127.0.0.1; SCACELITH_NET_LIVE_ACCOUNT gives its port): the games played (GET /state),
//          the mails (GET /mail), the e-mail change link opened as a browser would
//          (POST /confirm-email-change), a session made to expire in the database
//          (POST /expire-session), a session revoked from another device (POST /revoke-session),
//          a direct challenge from the rival (POST /challenge), TOTP codes (GET /totp) and the
//          server's metrics (GET /metric).
//
// sso      Google sign-in (docs/API.md "Google sign-in"): the account API in this process (real
//          auth module, SQLite store and API handler, no WebSocket) over native TLS, with Google
//          replaced by the fake provider of test/unit/helpers/auth-oidc.js and e-mail confirmation
//          on; SERVER_PUBLIC_HOST is LIVE_HOST and the API port the one it listens on, so that the
//          origin tag is the game's. Then the C++ test `net_live_sso` (SCACELITH_NET_LIVE_SSO=
//          host:port:control port, the certificate's SHA-256 in SCACELITH_NET_LIVE_SSO_PIN and in
//          GET /state) signs in through its 127.0.0.1 listener; its browser opener asks the
//          control server for Google's answer (GET /fake-authorize?url=<authUrl>&sub=&email=, a
//          302 whose Location it opens on the listener). The control server also seeds accounts
//          (POST /seed-password-account { username, email, password, mfa }), gives TOTP codes
//          (GET /totp?username=) and lists the Google links stored (GET /links).
//
// The C++ client trusts the server by pinning the SHA-256 of its certificate. The host name it
// connects to is LIVE_HOST (default localhost: the test certificate names both localhost and
// 127.0.0.1, but Wine's WinHTTP only matches DNS names).
// Exit code: 0 when every part passed, else the first failing C++ test's (or 1).
import { spawn } from 'node:child_process';
import crypto from 'node:crypto';
import fs from 'node:fs';
import http from 'node:http';
import https from 'node:https';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { DatabaseSync } from 'node:sqlite';
import { makeCertificate, startServer } from '../test/integration/helpers/harness.js';
import { createCaptureMailer, TEST_DEFAULTS } from '../test/unit/helpers/auth-fakes.js';
import { startFakeOidc } from '../test/unit/helpers/auth-oidc.js';
import { applyGame } from '../test/unit/helpers/real-auth.js';
import { createAuth } from '../src/auth/index.js';
import { testConfig } from '../src/config.js';
import { createApiHandler } from '../src/http/server.js';
import { logger } from '../src/log.js';
import { createPasswordHasher } from '../src/security/password.js';
import { migrate, openStore } from '../src/store/index.js';
import { account, connect, challengeGame, Table, closeAll, PASSWORD } from '../test/integration/helpers/players.js';
import { ApiClient, totpCode } from '../src/client/index.js';
import { ChessGame } from '../src/chess/index.js';
import { enums } from '../src/protocol/index.js';

const HERE = path.dirname(fileURLToPath(import.meta.url));
const args = process.argv.slice(2);
const PARTS = ['game', 'account', 'sso'];
let parts = PARTS;
while (args.length && args[0].startsWith('--')) {
    const a = args.shift();
    if (a.startsWith('--only=')) parts = a.slice(7).split(',').filter(Boolean);
    else { console.error(`unknown option ${a}`); process.exit(2); }
}
for (const p of parts) if (!PARTS.includes(p)) { console.error(`unknown part ${p}`); process.exit(2); }
const command = args.length ? args : [path.join(HERE, '../../build/scacelith_tests')];
const HOST = process.env.LIVE_HOST || 'localhost';
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const pinOf = (srv) => new crypto.X509Certificate(srv.ca).fingerprint256.replace(/:/g, '');

/** Runs the C++ tests whose names contain `filter` with `env`; resolves with the exit code. */
function runCpp(filter, env) {
    return new Promise((resolve) => {
        const child = spawn(command[0], [...command.slice(1), filter], {
            env: { ...process.env, ...env },
            stdio: ['ignore', 'inherit', 'inherit'],
        });
        child.on('exit', (c, sig) => resolve(c ?? (sig ? 128 : 1)));
        child.on('error', (e) => { console.error(`cannot run ${command.join(' ')}: ${e.message}`); resolve(127); });
    });
}

// ---- part 1: a rated game ------------------------------------------------------------------------

const CPP_USER = 'cppplayer';
const CPP_PASSWORD = 'live check password 1234';

async function gamePart() {
    const srv = await startServer({ workers: 2, sharedPort: true, env: { POW_REGISTER_BITS: '12', GIF_ENABLED: 'false' } });
    const pin = pinOf(srv);
    console.log(`[game] server on ${HOST}:${srv.apiPort} (API + WSS), certificate SHA-256 ${pin}`);
    let bot = null, code = 1;
    try {
        const acc = await account(srv, 'nodebot');
        bot = { ...acc, client: await connect(srv, acc.token) };
        const rules = new ChessGame();
        let gameId = null, me = null, sent = -1, plies = 0;
        const playIfMyTurn = () => {
            if (gameId === null || rules.isOver || rules.moves.length % 2 !== me || sent >= rules.moves.length) return;
            const legal = rules.position.legalMoves();
            if (!legal.length) return;
            const mv = legal[Math.floor(Math.random() * legal.length)];
            sent = rules.moves.length;
            bot.client.move(gameId, sent, mv, rules.position.digest(), 200 + Math.floor(Math.random() * 300), false);
        };
        bot.client.on('GameSnapshot', (m) => {
            gameId = m.game; me = m.you;
            console.log(`bot: game ${gameId}, playing ${me === 0 ? 'White' : 'Black'}`);
            setTimeout(playIfMyTurn, 50);
        });
        bot.client.on('MoveMade', (m) => {
            if (m.game !== gameId) return;
            rules.play(m.move); plies++;
            setTimeout(playIfMyTurn, 20);
        });
        bot.client.on('GameEnd', (m) => { if (m.game === gameId) console.log(`bot: game over, status ${m.status} reason ${m.reason}`); });
        bot.client.on('RatingUpdate', (m) => console.log(`bot: rating update ${JSON.stringify(m)}`));
        bot.client.joinQueue('3+2', true);

        code = await runCpp('net_live_server_game', { SCACELITH_NET_LIVE: `${HOST}:${srv.apiPort}:${pin}:${CPP_USER}:${CPP_PASSWORD}` });
        console.log(`bot saw ${plies} plies; C++ test exit code ${code}`);
        if (code === 0) {
            // This server has no e-mail confirmation (the e-mail change applies at once) and no GIFs.
            code = await runCpp('net_live_account_server_settings', {
                SCACELITH_NET_LIVE_SETTINGS: `${HOST}:${srv.apiPort}:${pin}:${CPP_USER}:${CPP_PASSWORD}:nodebot@example.org:${gameId}`,
            });
            const notice = srv.lines.find((r) => r.msg === 'mail (log transport)' && r.to === `${CPP_USER}@example.org` && /was changed/.test(r.subject));
            console.log(`[game] notice to the former address: ${notice ? `"${notice.subject}"` : 'MISSING'}`);
            if (!notice && code === 0) code = 1;
            const taken = srv.lines.find((r) => r.msg === 'mail (log transport)' && r.to === 'nodebot@example.org');
            console.log(`[game] notice to the owner of the address in use: ${taken ? `"${taken.subject}"` : 'none'}`);
        }
    } catch (e) {
        console.error(e);
    } finally {
        await closeAll(bot);
        await srv.stop();
    }
    return code;
}

// ---- part 2: the account API ---------------------------------------------------------------------

const ACCOUNT_USER = 'cpp_account';
const HARNESS_LABEL = 'live harness';
const FORM = 'application/x-www-form-urlencoded';
const ER = enums.EndReason, GS = enums.GameStatus;

const mails = (srv) => srv.lines.filter((r) => r.msg === 'mail (log transport)');

/** The latest mail to `to` whose subject matches, once more than `after` of them were sent. */
async function mailTo(srv, to, subject, { after = 0, timeoutMs = 5000 } = {}) {
    const deadline = Date.now() + timeoutMs;
    for (;;) {
        const all = mails(srv).filter((r) => r.to === to && subject.test(r.subject));
        if (all.length > after) return { ...all[all.length - 1], count: all.length };
        if (Date.now() > deadline) return null;
        await sleep(50);
    }
}
const linkPath = (text) => { const m = /(https:\/\/\S+)/.exec(text); if (!m) return null; const u = new URL(m[1]); return u.pathname + u.search; };

/** An HTML page of the server (the e-mail links), GET or POST with a form body, as a browser. */
function page(srv, method, p, form) {
    return new Promise((resolve, reject) => {
        const body = form ? Buffer.from(new URLSearchParams(form).toString()) : null;
        const headers = body ? { 'Content-Type': FORM, 'Content-Length': body.length } : {};
        const req = https.request({ host: '127.0.0.1', port: srv.apiPort, method, path: p, headers, ca: srv.ca, servername: 'localhost', agent: false }, (res) => {
            let text = '';
            res.setEncoding('utf8');
            res.on('data', (c) => { text += c; });
            res.on('end', () => resolve({ status: res.statusCode, text }));
        });
        req.on('error', reject);
        req.end(body || undefined);
    });
}

/** Registers `name`, confirms its address with the mailed link, signs in and connects. */
async function verifiedPlayer(srv, name, clientLabel = HARNESS_LABEL) {
    const api = new ApiClient({ host: '127.0.0.1', port: srv.apiPort, ca: srv.ca, servername: 'localhost' });
    const email = `${name}@example.org`;
    const r = await api.register({ username: name, email, password: PASSWORD });
    if (r.status !== 202) throw new Error(`register ${name}: ${r.status} ${JSON.stringify(r.body)}`);
    const mail = await mailTo(srv, email, /Confirm your e-mail address for/);
    if (!mail) throw new Error(`no confirmation mail to ${email}`);
    const token = new URL(`https://x${linkPath(mail.text)}`).searchParams.get('token');
    const done = await page(srv, 'POST', '/verify-email', { token });
    if (done.status !== 200) throw new Error(`verify ${name}: ${done.status}`);
    const l = await api.login(name, PASSWORD, { clientLabel });
    if (l.status !== 200) throw new Error(`login ${name}: ${l.status} ${JSON.stringify(l.body)}`);
    const client = await connect(srv, api.token);
    return { api, token: api.token, name, email, client, userId: l.body.user.id };
}

/**
 * The end of game `id` as `p` sees it, then its commit: the shard tells the primary that the
 * players are free only once the game is in the database (src/game/host.js), so the next
 * challenge between them would be refused AlreadyInGame before that.
 */
async function endOf(p, id, since) {
    const end = await p.client.waitFor('GameEnd', (m) => m.game === id, 20000, { since });
    const deadline = Date.now() + 10000;
    while ((await p.api.get(`/games/${id}`)).status !== 200) {
        if (Date.now() > deadline) throw new Error(`game ${id} not committed`);
        await sleep(30);
    }
    await sleep(150);   // the primary's 'game.ended'
    return end;
}

/**
 * The games of the check. `me` is the C++ player's account (played here by the harness), `rival`
 * its opponent, `third` the other player of a game `me` is not in. Returns what the C++ test
 * expects of each game of `me` (newest last) and the id of the other game.
 */
async function playGames(me, rival, third) {
    const list = [];
    const note = (g, fields) => { list.push({ id: String(g.id), ...fields }); console.log(`[account] game ${g.id}: ${ACCOUNT_USER} ${fields.color}, ${fields.outcome}`); };

    // 1. Rated 3+2, me White: the rival resigns.
    let g = await challengeGame(me, rival, { rated: true });
    let t = new Table(g);
    await t.playAll(['e2e4', 'e7e5', 'g1f3', 'b8c6', 'f1c4']);
    let m = me.client.mark();
    rival.client.resign(g.id);
    await endOf(me, g.id, m);
    note(g, { color: 'white', outcome: 'win', rated: true, category: '3+2', plies: 5, result: '1-0', status: GS.WhiteWins, reason: ER.Resignation, termination: 'normal' });

    // 2. Rated 3+2, me Black: I resign.
    g = await challengeGame(rival, me, { rated: true });
    t = new Table(g);
    await t.playAll(['e2e4', 'c7c5', 'g1f3']);
    m = me.client.mark();
    me.client.resign(g.id);
    await endOf(me, g.id, m);
    note(g, { color: 'black', outcome: 'loss', rated: true, category: '3+2', plies: 3, result: '1-0', status: GS.WhiteWins, reason: ER.Resignation, termination: 'normal' });

    // 3. Casual 3+2, me White: a draw by agreement.
    g = await challengeGame(me, rival);
    t = new Table(g);
    await t.playAll(['d2d4', 'd7d5', 'c2c4', 'e7e6']);
    m = me.client.mark();
    const mr = rival.client.mark();
    me.client.offerDraw(g.id);
    await rival.client.waitFor('GameEvent', (x) => x.game === g.id && x.kind === enums.GameEventKind.DrawOffered, 5000, { since: mr });
    rival.client.answerDraw(g.id, true);
    await endOf(me, g.id, m);
    note(g, { color: 'white', outcome: 'draw', rated: false, category: '3+2', plies: 4, result: '1/2-1/2', status: GS.Draw, reason: ER.Agreement, termination: 'normal' });

    // 4. Casual 7+1 (not an official category: custom), me Black: fool's mate.
    g = await challengeGame(rival, me, { baseSec: 420, incSec: 1 });
    t = new Table(g);
    m = me.client.mark();
    await t.playAll(['f2f3', 'e7e5', 'g2g4', 'd8h4']);
    await endOf(me, g.id, m);
    note(g, { color: 'black', outcome: 'win', rated: false, category: 'custom', plies: 4, result: '0-1', status: GS.BlackWins, reason: ER.Checkmate, termination: 'normal' });

    // 5. Casual 3+2, me White: a pawn promotes to a queen (b7xa8=Q), then the rival resigns.
    g = await challengeGame(me, rival);
    t = new Table(g);
    await t.playAll(['e2e4', 'd7d5', 'e4d5', 'c7c6', 'd5c6', 'g8f6', 'c6b7', 'b8d7', 'b7a8q']);
    m = me.client.mark();
    rival.client.resign(g.id);
    await endOf(me, g.id, m);
    note(g, { color: 'white', outcome: 'win', rated: false, category: '3+2', plies: 9, result: '1-0', status: GS.WhiteWins, reason: ER.Resignation, termination: 'normal', promotion: { ply: 8, uci: 'b7a8q', san: 'bxa8=Q' } });

    // 6. Casual 3+2, me White: aborted before the first move.
    g = await challengeGame(me, rival);
    m = me.client.mark();
    me.client.abort(g.id);
    await endOf(me, g.id, m);
    note(g, { color: 'white', outcome: 'aborted', rated: false, category: '3+2', plies: 0, result: '*', status: GS.Aborted, reason: ER.Aborted, termination: 'unterminated' });

    // Another players' game: the rival (White) against the third player, who resigns.
    g = await challengeGame(rival, third);
    t = new Table(g);
    await t.playAll(['e2e4', 'e7e5']);
    m = rival.client.mark();
    third.client.resign(g.id);
    await endOf(rival, g.id, m);
    return { games: list, other: { id: String(g.id), white: rival.name, black: third.name, plies: 2, result: '1-0' } };
}

/** Reads a JSON request body of the control server. */
function readBody(req) {
    return new Promise((resolve) => {
        let text = '';
        req.setEncoding('utf8');
        req.on('data', (c) => { text += c; });
        req.on('end', () => { try { resolve(text ? JSON.parse(text) : {}); } catch { resolve({}); } });
    });
}

const metricValue = (text, line) => {
    const m = text.split('\n').find((l) => l.startsWith(line + ' '));
    return m ? Number(m.slice(line.length + 1)) : 0;
};

/**
 * A TOTP code of `secret` (base32) the server has not seen used: the current step's, or the next
 * step's when the current one was handed out already (the server takes one step ahead, and
 * refuses a step at or before the last one used). `steps`: the last step handed out per secret.
 */
function freshTotp(steps, secret) {
    let step = Math.floor(Date.now() / 30000);
    const last = steps.get(secret) ?? -1;
    if (step <= last) step = last + 1;
    steps.set(secret, step);
    return { code: totpCode(secret, step * 30000 + 1), step };
}

/** A second connection to the database of a server (data directory `dir`). */
function openDb(dir) {
    const d = new DatabaseSync(path.join(dir, 'scacelith.db'));
    d.exec('PRAGMA busy_timeout = 5000');
    return d;
}

/**
 * A control server of a C++ test (plain HTTP on 127.0.0.1). `routes` maps 'METHOD /path' to
 * (query, json body) -> answer, or [status, answer, headers?].
 */
async function serveControl(routes) {
    const server = http.createServer(async (req, res) => {
        const url = new URL(req.url, 'http://127.0.0.1');
        const route = routes[`${req.method} ${url.pathname}`];
        let status = 200, body, headers = {};
        try {
            if (!route) { status = 404; body = { error: 'no_route' }; } else {
                const json = req.method === 'POST' ? await readBody(req) : {};
                const out = await route(url.searchParams, json);
                if (Array.isArray(out)) [status, body, headers = {}] = out; else body = out;
            }
        } catch (e) {
            status = 500;
            body = { error: 'harness', message: e.message };
        }
        console.log(`[control] ${req.method} ${url.pathname}${url.search} -> ${status} ${JSON.stringify(body).slice(0, 160)}`);
        const text = JSON.stringify(body);
        res.writeHead(status, { ...headers, 'Content-Type': 'application/json', 'Content-Length': Buffer.byteLength(text) });
        res.end(text);
    });
    await new Promise((resolve) => server.listen(0, '127.0.0.1', resolve));
    return { port: server.address().port, close: () => new Promise((r) => server.close(r)) };
}

/** The control server of the account part's C++ test. */
function controlServer(srv, ctx) {
    const db = () => openDb(srv.dir);
    return serveControl({
        'GET /state': () => ctx.state,
        // ?to=&subject=<regex>&after=<count already seen>
        'GET /mail': async (q) => {
            const mail = await mailTo(srv, q.get('to') || '', new RegExp(q.get('subject') || ''), { after: Number(q.get('after') || 0) });
            return mail ? { to: mail.to, subject: mail.subject, text: mail.text, count: mail.count } : [404, { error: 'no_mail' }];
        },
        // The confirmation link mailed to ?to=, opened (GET) and its button pressed (POST).
        'POST /confirm-email-change': async (q) => {
            const mail = await mailTo(srv, q.get('to') || '', /Confirm your new e-mail address/);
            if (!mail) return [404, { error: 'no_mail' }];
            const lp = linkPath(mail.text);
            const shown = await page(srv, 'GET', lp);
            const token = new URL(`https://x${lp}`).searchParams.get('token');
            const done = await page(srv, 'POST', '/confirm-email-change', { token });
            return { link: lp, getStatus: shown.status, status: done.status, changed: /E-mail address changed/.test(done.text) };
        },
        // The newest active session of the C++ player that this harness did not open expires now.
        'POST /expire-session': () => {
            const d = db();
            try {
                const t = Date.now();
                const row = d.prepare(`SELECT id, client_label FROM sessions WHERE user_id = ? AND revoked_at IS NULL AND expires_at > ?
                    AND (client_label IS NULL OR client_label NOT LIKE 'live harness%') ORDER BY id DESC LIMIT 1`).get(ctx.userId, t);
                if (!row) return [404, { error: 'no_session' }];
                d.prepare('UPDATE sessions SET expires_at = ?, idle_expires_at = ? WHERE id = ?').run(t - 1000, t - 1000, row.id);
                return { sessionId: row.id, clientLabel: row.client_label };
            } finally { d.close(); }
        },
        // Session ?id= of the C++ player revoked from another device (a new harness sign-in).
        'POST /revoke-session': async (q) => {
            const api = new ApiClient({ host: '127.0.0.1', port: srv.apiPort, ca: srv.ca, servername: 'localhost' });
            try {
                const l = await api.login(ACCOUNT_USER, ctx.state.password, { clientLabel: `${HARNESS_LABEL} (revoker)` });
                if (l.status !== 200) return [500, { error: 'login', status: l.status, body: l.body }];
                const r = await api.request('DELETE', `/auth/sessions/${encodeURIComponent(q.get('id') || '')}`);
                await api.logout();
                return { status: r.status, body: r.body };
            } finally { api.close(); }
        },
        // The rival challenges the C++ player by name (the harness's own connection of that account
        // is online): 'delivered' (then declined) or the refusal's error code.
        'POST /challenge': async () => {
            const { me, rival } = ctx;
            const mm = me.client.mark();
            const seq = rival.client.challenge(ACCOUNT_USER, 180, 2, false, 1);
            const answer = await Promise.race([
                me.client.waitFor('ChallengeReceived', null, 3000, { since: mm }).then((rec) => ({ delivered: rec })),
                rival.client.expectAck(seq, 3000).then(() => ({ acked: true }), (e) => ({ error: e.errorCode ?? e.message })),
            ]);
            if (answer.delivered) {
                me.client.declineChallenge(answer.delivered.id);
                return { result: 'delivered' };
            }
            if (answer.error !== undefined) {
                const name = Object.keys(enums.ErrorCode).find((k) => enums.ErrorCode[k] === answer.error);
                return { result: name || String(answer.error) };
            }
            // Acknowledged but not received: wait for the delivery a little longer.
            try {
                const rec = await me.client.waitFor('ChallengeReceived', null, 3000, { since: mm });
                me.client.declineChallenge(rec.id);
                return { result: 'delivered' };
            } catch { return { result: 'acked_not_delivered' }; }
        },
        // ?secret=<base32>
        'GET /totp': (q) => freshTotp(ctx.totpSteps, q.get('secret') || ''),
        // ?name=<metric line name, labels included>
        'GET /metric': async (q) => ({ value: metricValue(await srv.metrics(), q.get('name') || '') }),
    });
}

async function accountPart() {
    const srv = await startServer({
        workers: 1, sharedPort: true,
        env: { REQUIRE_EMAIL_VERIFICATION: 'true', FIRST_MOVE_TIMEOUT_MS: '20000', DB_COMMIT_MS: '20' },
    });
    const pin = pinOf(srv);
    console.log(`[account] server on ${HOST}:${srv.apiPort} (API + WSS), certificate SHA-256 ${pin}`);
    let me = null, rival = null, third = null, ctl = null, code = 1;
    try {
        me = await verifiedPlayer(srv, ACCOUNT_USER);
        rival = await verifiedPlayer(srv, 'rival_live');
        third = await verifiedPlayer(srv, 'third_live');
        const played = await playGames(me, rival, third);
        // Every game committed before the C++ client reads the history.
        const deadline = Date.now() + 10000;
        for (;;) {
            const h = await me.api.get('/account/games?limit=50');
            if (h.status === 200 && h.body.total === played.games.length) break;
            if (Date.now() > deadline) throw new Error(`history not committed: ${JSON.stringify(h.body)}`);
            await sleep(50);
        }
        const state = {
            user: ACCOUNT_USER, password: PASSWORD, email: me.email, userId: me.userId,
            rival: rival.name, third: third.name, harnessLabel: HARNESS_LABEL,
            games: played.games, other: played.other,
            gifUserRendersPerMin: Number(srv.env.GIF_USER_RENDERS_PER_MIN || 4),
        };
        console.log(`[account] games of ${ACCOUNT_USER}: ${played.games.map((x) => `${x.id} ${x.outcome}`).join(', ')}; other game ${played.other.id}`);
        ctl = await controlServer(srv, { state, userId: me.userId, me, rival, totpSteps: new Map() });
        code = await runCpp('net_live_account_api', { SCACELITH_NET_LIVE_ACCOUNT: `${HOST}:${srv.apiPort}:${pin}:${ctl.port}` });
        console.log(`[account] C++ test exit code ${code}`);
        const errors = srv.lines.filter((l) => l.level === 'error');
        if (errors.length) console.log(`[account] server errors logged:\n${errors.map((l) => JSON.stringify(l)).join('\n')}`);
    } catch (e) {
        console.error(e);
    } finally {
        if (ctl) await ctl.close();
        await closeAll(me, rival, third);
        await srv.stop();
    }
    return code;
}

// ---- part 3: Google sign-in ----------------------------------------------------------------------

const SSO_CLIENT_ID = 'live-check.apps.googleusercontent.com';
const SSO_CLIENT_SECRET = 'GOCSPX-live-check';
// What Google adds to its redirect besides code, state and iss (the game forwards only those three).
const GOOGLE_EXTRA = { scope: 'email profile openid https://www.googleapis.com/auth/userinfo.email https://www.googleapis.com/auth/userinfo.profile', authuser: '0', prompt: 'consent' };

/**
 * The account API in this process, as test/unit/helpers/real-auth.js builds it (the real auth
 * module, SQLite store and API handler), with the fake Google of test/unit/helpers/auth-oidc.js
 * reached over plain HTTP, behind native TLS on 127.0.0.1. Its origin is LIVE_HOST and the port it
 * listens on, so its tag is the one the game computes for the server it connects to.
 */
async function startSsoServer() {
    const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'scacelith-live-sso-'));
    const tls = makeCertificate(dir);
    let handle = null;
    const server = https.createServer({ cert: fs.readFileSync(tls.cert), key: fs.readFileSync(tls.key) }, (req, res) => handle(req, res));
    await new Promise((r) => server.listen(0, '127.0.0.1', r));
    const apiPort = server.address().port;
    const idp = await startFakeOidc({ clientId: SSO_CLIENT_ID, clientSecret: SSO_CLIENT_SECRET, now: Date.now });
    const config = testConfig({
        ...TEST_DEFAULTS, DB_PATH: path.join(dir, 'scacelith.db'), SERVER_PUBLIC_HOST: HOST, API_PORT: String(apiPort),
        REQUIRE_EMAIL_VERIFICATION: '1', SSO_GOOGLE_ENABLED: '1', GOOGLE_CLIENT_ID: SSO_CLIENT_ID, GOOGLE_CLIENT_SECRET: SSO_CLIENT_SECRET,
    });
    const store = openStore(config, { applyGame });
    await migrate(store);
    const log = logger.child('live-sso');
    const mailer = createCaptureMailer(config, log);
    const auth = createAuth({ config, store, log, mailer, passwordHasher: createPasswordHasher({ scrypt: { logN: 10 }, argon2: false }),
        oidcEndpoints: idp.endpoints, oidcAllowHttp: true });
    handle = createApiHandler({ config, store, auth, log });
    return {
        dir, apiPort, ca: tls.ca, config, store, idp, mailer,
        async stop() {
            await handle.close();
            auth.close();
            await new Promise((r) => { server.closeAllConnections?.(); server.close(r); });
            await idp.close();
            store.close();
            fs.rmSync(dir, { recursive: true, force: true });
        },
    };
}

/** The control server of `net_live_sso`. */
function ssoControl(srv, pin) {
    const totpSteps = new Map(), secrets = new Map();
    const api = () => new ApiClient({ host: '127.0.0.1', port: srv.apiPort, ca: srv.ca, servername: 'localhost' });
    return serveControl({
        'GET /state': () => ({ host: HOST, apiPort: srv.apiPort, pin, tag: srv.config.ssoRedirectTag, clientId: SSO_CLIENT_ID }),
        // { username, email, password?, mfa? }: an account whose address is confirmed, as a used
        // registration link leaves it; without a password, one like a Google-made account. With
        // mfa: two-step verification turned on through the API; the answer gives its secret.
        'POST /seed-password-account': async (q, b) => {
            const { username, email, password = null, mfa = false } = b;
            if (typeof username !== 'string' || typeof email !== 'string') return [400, { error: 'username_and_email' }];
            const passwordHash = password ? await createPasswordHasher({ scrypt: { logN: 10 }, argon2: false }).hash(password) : null;
            srv.store.users.create({ username, email, passwordHash, emailVerified: true });
            const userId = srv.store.users.byUsername(username).id;
            if (!mfa) return { userId };
            if (!password) return [400, { error: 'mfa_needs_password' }];
            const a = api();
            try {
                const l = await a.login(username, password, { clientLabel: 'live harness' });
                if (l.status !== 200) return [500, { error: 'login', status: l.status, body: l.body }];
                const setup = await a.post('/account/mfa/totp/setup', { password });
                if (setup.status !== 200) return [500, { error: 'mfa_setup', status: setup.status, body: setup.body }];
                const { secret } = setup.body;
                const on = await a.post('/account/mfa/totp/enable', { code: freshTotp(totpSteps, secret).code });
                if (on.status !== 200) return [500, { error: 'mfa_enable', status: on.status, body: on.body }];
                await a.logout();
                secrets.set(username.toLowerCase(), secret);
                return { userId, secret };
            } finally { a.close(); }
        },
        // ?username= (an account seeded with mfa) or ?secret=<base32>
        'GET /totp': (q) => {
            const secret = q.get('secret') || secrets.get((q.get('username') || '').toLowerCase());
            return secret ? freshTotp(totpSteps, secret) : [404, { error: 'no_secret' }];
        },
        // ?url=<authUrl of a start answer>, then the Google account picked (?sub=&email=&name=,
        // ?verified=false for an address Google has not confirmed) or ?error=access_denied: the
        // fake provider's GET /authorize with that URL's query, whose 302 to the redirect URI is
        // passed on (Location, also given as { location }), with the parameters Google adds.
        'GET /fake-authorize': async (q) => {
            const url = q.get('url') || '';
            Object.assign(srv.idp.state, {
                consentError: q.get('error') || null,
                consentClaims: {
                    sub: q.get('sub') || '1000001', email: q.get('email') || 'live.player@gmail.com',
                    email_verified: q.get('verified') !== 'false', name: q.get('name') || 'Live Player',
                },
            });
            const at = url.indexOf('?');
            const res = await new Promise((resolve, reject) => {
                http.get(`${srv.idp.base}/authorize?${at < 0 ? '' : url.slice(at + 1)}`, (r) => { r.resume(); resolve(r); }).on('error', reject);
            });
            if (res.statusCode !== 302) return [400, { error: 'bad_authorization_request', status: res.statusCode }];
            let location = res.headers.location;
            if (!q.get('error')) location += `&${new URLSearchParams(GOOGLE_EXTRA)}`;
            return [302, { location }, { Location: location }];
        },
        // The Google links stored (sso_identities), oldest first.
        'GET /links': () => {
            const d = openDb(srv.dir);
            try {
                return { links: d.prepare(`SELECT s.provider, s.subject, s.user_id AS userId, u.username, s.email, s.created_at AS createdAt
                    FROM sso_identities s JOIN users u ON u.id = s.user_id ORDER BY s.created_at, s.subject`).all().map((r) => ({ ...r })) };
            } finally { d.close(); }
        },
    });
}

async function ssoPart() {
    const srv = await startSsoServer();
    const pin = pinOf(srv);
    console.log(`[sso] server on ${HOST}:${srv.apiPort} (API only, no WebSocket), certificate SHA-256 ${pin}, origin tag ${srv.config.ssoRedirectTag}`);
    let ctl = null, code = 1;
    try {
        ctl = await ssoControl(srv, pin);
        code = await runCpp('net_live_sso', { SCACELITH_NET_LIVE_SSO: `${HOST}:${srv.apiPort}:${ctl.port}`, SCACELITH_NET_LIVE_SSO_PIN: pin });
        console.log(`[sso] C++ test exit code ${code}`);
        await srv.mailer.idle?.();
        console.log(`[sso] mails: ${srv.mailer.sent.map((m) => `${m.to} "${m.subject}"`).join(', ') || 'none'}`);
    } catch (e) {
        console.error(e);
    } finally {
        if (ctl) await ctl.close();
        await srv.stop();
    }
    return code;
}

let exitCode = 0;
for (const p of parts) {
    const c = await { game: gamePart, account: accountPart, sso: ssoPart }[p]();
    console.log(`== ${p}: ${c === 0 ? 'passed' : `FAILED (exit code ${c})`}`);
    if (c !== 0 && exitCode === 0) exitCode = c;
}
process.exit(exitCode);
