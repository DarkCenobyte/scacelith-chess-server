// The real auth module and the real SQLite store (a file in a temporary directory, so that a test
// can open a second connection to read what is stored) behind the real API handler, for the tests
// that must see what the store really keeps (account.export, auth.account-view, auth.sso).

import fs from 'node:fs';
import http from 'node:http';
import os from 'node:os';
import path from 'node:path';
import { createApiHandler } from '../../../src/http/server.js';
import { createAuth } from '../../../src/auth/index.js';
import { openStore, migrate } from '../../../src/store/index.js';
import { testConfig } from '../../../src/config.js';
import { logger } from '../../../src/log.js';
import { createPasswordHasher } from '../../../src/security/password.js';
import { createCaptureMailer, createClock } from './auth-fakes.js';

/** A rating function for finishBatch: the winner takes 10 points from the loser (tests). */
export function applyGame(white, black, score) {
    const d = Math.round(20 * (score - 0.5));
    const side = (r, delta, s) => ({
        before: r.rating, after: r.rating + delta,
        record: { rating: r.rating + delta, games: r.games + 1, wins: r.wins + (s === 1 ? 1 : 0), draws: r.draws + (s === 0.5 ? 1 : 0),
            losses: r.losses + (s === 0 ? 1 : 0), peak: Math.max(r.peak, r.rating + delta), reachedSenior: false },
    });
    return { white: side(white, d, score), black: side(black, -d, 1 - score) };
}

/**
 * Starts the server pieces; everything is closed and deleted after the test `t`. `primary`: the
 * auth service's IPC client (e.g. auth-fakes.js createFakePrimary, which records the broadcasts).
 * `oidcEndpoints`: a fake Google (auth-oidc.js), reached over plain HTTP.
 * @returns {Promise<{ config, store, auth, mailer, hasher, now, request, file }>}
 */
export async function startReal(t, env = {}, { primary = null, oidcEndpoints } = {}) {
    const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'scacelith-realauth-'));
    const file = path.join(dir, 'scacelith.db');
    const config = testConfig({ DB_PATH: file, AUTH_RATE_PER_IP: '10000', HTTP_RATE_PER_IP: '100000', REQUIRE_EMAIL_VERIFICATION: '1',
        SERVER_PUBLIC_HOST: 'chess.example.org', API_PORT: '8443', AUTH_REGISTER_PER_HOUR: '10000', AUTH_MAIL_PER_HOUR: '10000',
        AUTH_FORGOT_PER_HOUR: '10000', AUTH_FORGOT_PER_DAY: '10000', AUTH_RESET_PER_HOUR: '10000', AUTH_MFA_PER_ACCOUNT: '10000',
        AUTH_REAUTH_PER_USER: '10000', USER_RATE_PER_MIN: '100000', ...env });
    const store = openStore(config, { applyGame });
    await migrate(store);
    const now = createClock(Date.now());
    const log = logger.child('test');
    const mailer = createCaptureMailer(config, log);
    const hasher = createPasswordHasher({ scrypt: { logN: 10 }, argon2: false });
    const auth = createAuth({ config, store, primary, log, now, mailer, passwordHasher: hasher,
        ...(oidcEndpoints ? { oidcEndpoints, oidcAllowHttp: true } : {}) });
    const handler = createApiHandler({ config, store, auth, log, now });
    const server = http.createServer(handler);
    await new Promise((r) => server.listen(0, '127.0.0.1', r));
    const { port } = server.address();
    t.after(async () => {
        auth.close();
        await new Promise((r) => { server.closeAllConnections?.(); server.close(r); });
        store.close();
        fs.rmSync(dir, { recursive: true, force: true });
    });
    // body: a JSON body; raw + contentType: any other body (an HTML form).
    const request = (method, p, { token, body, raw, contentType } = {}) => new Promise((resolve, reject) => {
        const headers = {};
        let payload = null;
        if (token) headers.Authorization = `Bearer ${token}`;
        if (raw !== undefined) { payload = Buffer.from(raw); headers['Content-Type'] = contentType; headers['Content-Length'] = payload.length; }
        if (body !== undefined) { payload = Buffer.from(JSON.stringify(body)); headers['Content-Type'] = 'application/json'; headers['Content-Length'] = payload.length; }
        const r = http.request({ host: '127.0.0.1', port, method, path: p, headers, agent: false }, (res) => {
            const c = [];
            res.on('data', (x) => c.push(x));
            res.on('end', () => {
                const text = Buffer.concat(c).toString('utf8');
                let json; try { json = JSON.parse(text); } catch { /* not JSON */ }
                resolve({ status: res.statusCode, headers: res.headers, text, json });
            });
        });
        r.on('error', reject);
        if (payload) r.write(payload);
        r.end();
    });
    return { config, store, auth, mailer, hasher, now, request, file };
}
