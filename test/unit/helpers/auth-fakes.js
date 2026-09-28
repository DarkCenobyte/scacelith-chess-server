// Test doubles for the auth module: an in-memory Store implementing the subset of
// docs/DESIGN.md 5.5 the auth module uses, a fake primary (IPC) with the ratelimit.take /
// once.consume semantics, a capturing mailer, and a harness that runs the real API handler on
// a local HTTP server with a controllable clock.

import http from 'node:http';
import { createApiHandler } from '../../../src/http/server.js';
import { createAuth } from '../../../src/auth/index.js';
import { createMailer } from '../../../src/mail/index.js';
import { testConfig } from '../../../src/config.js';
import { configureLogging, logger } from '../../../src/log.js';
import { createPasswordHasher } from '../../../src/security/password.js';
import { createLocalControl } from '../../../src/security/ratelimit.js';

/** Every log line written by the process since the helpers were loaded (debug level). */
export const capturedLogs = [];
configureLogging({
    level: 'debug', format: 'json',
    out: { write(line) { capturedLogs.push(line); if (capturedLogs.length > 50000) capturedLogs.splice(0, 25000); } },
});

export class StoreError extends Error {
    constructor(code) { super(code); this.code = code; }
}

const copy = (o) => (o ? structuredClone(o) : null);

/** In-memory Store (users, mfa, sessions, tokens, sso, security, sanctions, ratings, meta). */
export function createFakeStore({ now = Date.now } = {}) {
    const users = new Map();
    const codes = new Map();
    const sessions = new Map();
    const tokens = new Map();
    const links = [];
    const securityEvents = [];
    const sanctions = [];
    const ratings = new Map();
    const calls = { touch: 0 };
    let userSeq = 0, sessionSeq = 0, sanctionSeq = 0;

    const findUser = (pred) => { for (const u of users.values()) if (pred(u)) return u; return null; };
    const lc = (s) => String(s ?? '').toLowerCase();

    const store = {
        meta: { get: (k) => (k === 'server_id' ? 'test-server-id' : null), set() {} },
        users: {
            create({ username, email, passwordHash, emailVerified }) {
                if (findUser((u) => lc(u.username) === lc(username))) throw new StoreError('username_taken');
                if (email && findUser((u) => u.email && lc(u.email) === lc(email))) throw new StoreError('email_taken');
                const id = ++userSeq;
                users.set(id, {
                    id, username, email, emailVerified: !!emailVerified, passwordHash: passwordHash ?? null,
                    mfaEnabled: false, mfaSecretEnc: null, mfaLastStep: 0, createdAt: now(), lastLoginAt: null,
                    status: 'active', acceptChallenges: 'all', pendingMfaSecretEnc: null,
                });
                return id;
            },
            byId: (id) => copy(users.get(id)),
            byUsername: (n) => copy(findUser((u) => lc(u.username) === lc(n))),
            byEmail: (e) => copy(findUser((u) => u.email && lc(u.email) === lc(e))),
            byLogin: (x) => store.users.byUsername(x) || store.users.byEmail(x),
            update(id, fields) { const u = users.get(id); if (u) Object.assign(u, structuredClone(fields)); },
            anonymize(id) {
                const u = users.get(id);
                if (!u) return;
                Object.assign(u, { username: `deleted-${id}`, email: null, passwordHash: null, mfaEnabled: false, mfaSecretEnc: null, pendingMfaSecretEnc: null, status: 'deleted' });
                codes.delete(id);
                for (let i = links.length - 1; i >= 0; i--) if (links[i].userId === id) links.splice(i, 1);
            },
            advanceMfaStep(id, step) {
                const u = users.get(id);
                if (!u || !(step > (u.mfaLastStep ?? 0))) return false;
                u.mfaLastStep = step;
                return true;
            },
        },
        mfa: {
            replaceRecoveryCodes(userId, hashes) { codes.set(userId, new Set(hashes)); },
            consumeRecoveryCode(userId, hash) { const s = codes.get(userId); return !!(s && s.delete(hash)); },
            countRecoveryCodes(userId) { return codes.get(userId)?.size ?? 0; },
        },
        sessions: {
            create(row) { const id = ++sessionSeq; sessions.set(id, { id, lastSeenAt: row.createdAt, revokedAt: null, ...row }); return id; },
            byTokenHash(h) { for (const s of sessions.values()) if (s.tokenHash === h) return copy(s); return null; },
            touch(id, t, idle) { calls.touch++; const s = sessions.get(id); if (s) { s.lastSeenAt = t; s.idleExpiresAt = idle; } },
            revoke(id) { const s = sessions.get(id); if (s && !s.revokedAt) s.revokedAt = now(); },
            revokeAllForUser(userId, exceptId) {
                const out = [];
                for (const s of sessions.values()) {
                    if (s.userId === userId && !s.revokedAt && s.id !== exceptId) { s.revokedAt = now(); out.push(s.tokenHash); }
                }
                return out;
            },
            listForUser(userId) { return [...sessions.values()].filter((s) => s.userId === userId).map(copy); },
            enforceLimit(userId, max) {
                const t = now();
                const active = [...sessions.values()].filter((s) => s.userId === userId && !s.revokedAt && s.expiresAt > t)
                    .sort((a, b) => b.createdAt - a.createdAt || b.id - a.id);
                const out = [];
                for (const s of active.slice(max)) { s.revokedAt = t; out.push(s.tokenHash); }
                return out;
            },
        },
        tokens: {
            create({ kind, tokenHash, userId, data, expiresAt }) {
                tokens.set(`${kind}:${tokenHash}`, { kind, tokenHash, userId: userId ?? null, data: JSON.stringify(data ?? null), expiresAt, createdAt: now(), usedAt: null });
            },
            consume(kind, tokenHash, t) {
                const r = tokens.get(`${kind}:${tokenHash}`);
                if (!r || r.usedAt || r.expiresAt <= t) return null;
                r.usedAt = t;
                return copy(r);
            },
            get: (kind, tokenHash) => copy(tokens.get(`${kind}:${tokenHash}`)),
            update(kind, tokenHash, data) { const r = tokens.get(`${kind}:${tokenHash}`); if (r) r.data = JSON.stringify(data); },
        },
        sso: {
            find: (provider, subject) => { const l = links.find((x) => x.provider === provider && x.subject === subject); return l ? { userId: l.userId } : null; },
            link(userId, provider, subject, email) { links.push({ userId, provider, subject, email }); },
            forUser: (userId) => links.filter((l) => l.userId === userId).map(copy),
        },
        security: { insertBatch(batch) { securityEvents.push(...batch); } },
        sanctions: {
            create(s) { const id = ++sanctionSeq; sanctions.push({ id, liftedAt: null, ...s }); return id; },
            activeBan: (userId, t) => copy(sanctions.find((s) => s.userId === userId && s.kind === 'ban' && !s.liftedAt && s.startsAt <= t && (s.endsAt == null || s.endsAt > t)) || null),
            list: (userId) => sanctions.filter((s) => s.userId === userId).map(copy),
        },
        ratings: {
            forUser: (userId) => (ratings.get(userId) || []).map(copy),
            _set(userId, list) { ratings.set(userId, list); },
        },
        _raw: { users, sessions, tokens, links, securityEvents, sanctions, codes, calls },
    };
    return store;
}

/**
 * Fake primary: ratelimit.take / once.consume with the primary's semantics (in-process),
 * session.revoked recorded and forwarded to the registered auth services.
 */
export function createFakePrimary({ now = Date.now } = {}) {
    const local = createLocalControl({ now });
    const calls = [];
    const listeners = [];
    const primary = {
        calls,
        failing: false,
        refuse: null,               // (type, payload) => reply | undefined, to override answers
        onRevoked(fn) { listeners.push(fn); },
        async request(type, payload) {
            calls.push({ type, payload: structuredClone(payload) });
            if (primary.failing) throw new Error('primary timeout');
            const forced = primary.refuse && primary.refuse(type, payload);
            if (forced) return forced;
            if (type === 'session.revoked') { for (const f of listeners) f(payload); return {}; }
            return local.request(type, payload);
        },
    };
    return primary;
}

/** A controllable clock. */
export function createClock(start = Date.UTC(2026, 8, 28, 12, 0, 0)) {
    let t = start;
    const now = () => t;
    now.advance = (ms) => { t += ms; return t; };
    now.set = (v) => { t = v; };
    return now;
}

/** A mailer that renders the real templates and messages and keeps them. */
export function createCaptureMailer(config, log) {
    const sent = [];
    const mailer = createMailer({
        config, log,
        transport: { async send(msg) { sent.push(msg); } },
    });
    mailer.sent = sent;
    return mailer;
}

/** Extracts the first https/http link of an e-mail body. */
export function linkIn(text) {
    const m = /(https?:\/\/\S+)/.exec(text);
    return m ? m[1] : null;
}

export const TEST_DEFAULTS = Object.freeze({
    AUTH_RATE_PER_IP: '10000', HTTP_RATE_PER_IP: '100000', AUTH_FAILURES_PER_ACCOUNT: '5',
    REQUIRE_EMAIL_VERIFICATION: '1', SERVER_PUBLIC_HOST: 'chess.example.org', API_PORT: '8443',
});

/**
 * Starts the real API handler on 127.0.0.1 with fakes. The client address of a request is the
 * X-Test-Ip header when given (as the proxy layer would set req.clientIp).
 * @param {{ env?: object, scryptLogN?: number, hasher?: object, oidc?: object, oidcEndpoints?: object,
 *           handlerOptions?: object, primary?: object|null }} [opts]
 */
export async function startTestServer({ env = {}, scryptLogN = 10, hasher, oidc, oidcEndpoints, handlerOptions = {}, primary: givenPrimary } = {}) {
    const config = testConfig({ ...TEST_DEFAULTS, ...env });
    const now = createClock();
    const store = createFakeStore({ now });
    const primary = givenPrimary === undefined ? createFakePrimary({ now }) : givenPrimary;
    const log = logger.child('test');
    const mailer = createCaptureMailer(config, log);
    const passwordHasher = hasher || createPasswordHasher({ scrypt: { logN: scryptLogN }, argon2: false });
    const auth = createAuth({
        config, store, primary, log, now, mailer, passwordHasher, oidc,
        ...(oidcEndpoints ? { oidcEndpoints, oidcAllowHttp: true } : {}),
    });
    if (primary && primary.onRevoked) primary.onRevoked((p) => auth.invalidate(p));
    const handler = createApiHandler({ config, store, auth, primary, log, now, ...handlerOptions });
    const server = http.createServer((req, res) => {
        const ip = req.headers['x-test-ip'];
        if (ip) req.clientIp = String(ip);
        handler(req, res);
    });
    await new Promise((r) => server.listen(0, '127.0.0.1', r));
    const { port } = server.address();
    const base = `http://127.0.0.1:${port}`;

    /**
     * @returns {Promise<{ status: number, headers: object, text: string, json: any }>}
     */
    function request(method, path, { body, token, ip, headers = {}, raw, contentType } = {}) {
        return new Promise((resolve, reject) => {
            const h = { ...headers };
            let payload = null;
            if (raw !== undefined) payload = Buffer.from(raw);
            else if (body !== undefined) { payload = Buffer.from(JSON.stringify(body)); h['Content-Type'] = 'application/json'; }
            if (contentType) h['Content-Type'] = contentType;
            if (payload) h['Content-Length'] = payload.length;
            if (token) h.Authorization = `Bearer ${token}`;
            if (ip) h['X-Test-Ip'] = ip;
            const req = http.request(`${base}${path}`, { method, headers: h, agent: false }, (res) => {
                const chunks = [];
                res.on('data', (c) => chunks.push(c));
                res.on('end', () => {
                    const text = Buffer.concat(chunks).toString('utf8');
                    let json;
                    try { json = text ? JSON.parse(text) : undefined; } catch { json = undefined; }
                    resolve({ status: res.statusCode, headers: res.headers, text, json });
                });
            });
            req.on('error', reject);
            if (payload) req.write(payload);
            req.end();
        });
    }

    /** Inserts an account directly (fast path for tests). */
    async function createUser({ username = 'alice', email = `${username.toLowerCase()}@example.com`, password = 'correct horse battery', verified = true } = {}) {
        const id = store.users.create({ username, email, passwordHash: password === null ? null : await passwordHasher.hash(password), emailVerified: verified });
        return { id, username, email, password };
    }

    /** Logs in with a password; returns the answer's JSON (throws on failure). */
    async function login(loginName, password, extra = {}) {
        const r = await request('POST', '/api/v1/auth/login', { body: { login: loginName, password, ...extra } });
        if (r.status !== 200) throw new Error(`login failed: ${r.status} ${r.text}`);
        return r.json;
    }

    async function close() {
        auth.close();
        await new Promise((r) => server.close(r));
    }

    return { config, now, store, primary, mailer, auth, handler, server, base, request, createUser, login, close, hasher: passwordHasher };
}
