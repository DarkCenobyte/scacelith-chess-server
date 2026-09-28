// Node SDK, HTTPS API client: prefixing, JSON, bearer token, keep-alive, transparent proof of
// work, login with MFA, TOTP (RFC 6238 vectors), TLS with a private CA.

import { test, describe, after } from 'node:test';
import assert from 'node:assert/strict';
import http from 'node:http';
import https from 'node:https';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { execFileSync } from 'node:child_process';
import { ApiClient, totpCode, base32Decode } from '../../src/client/api.js';
import { checkPow } from '../../src/client/pow.js';

const SECRET = 'JBSWY3DPEHPK3PXP';

// A fake account API: enough of DESIGN.md 5.9 to exercise the client.
function handler(state) {
    return (req, res) => {
        let body = '';
        req.setEncoding('utf8');
        req.on('data', (c) => { body += c; });
        req.on('end', () => {
            state.requests.push({ method: req.method, url: req.url, headers: req.headers, body });
            const json = body ? JSON.parse(body) : null;
            const send = (status, obj, headers = {}) => {
                const text = obj === undefined ? '' : typeof obj === 'string' ? obj : JSON.stringify(obj);
                res.writeHead(status, { 'content-type': typeof obj === 'string' ? 'text/plain' : 'application/json', 'content-length': Buffer.byteLength(text), ...headers });
                res.end(text);
            };
            const auth = req.headers.authorization;
            switch (`${req.method} ${req.url}`) {
                case 'GET /api/v1/info': return send(200, { name: 'Fake', protocol: { min: 1, max: 1 } });
                case 'POST /api/v1/auth/register': {
                    if (!json.pow) {
                        state.challenge = `ch-${state.requests.length}-${Date.now()}`;
                        state.issued.push(state.challenge);
                        return send(428, { error: 'pow_required', message: 'solve it', pow: { challenge: state.challenge, bits: 12, expiresAt: Date.now() + 120000 } });
                    }
                    if (json.pow.challenge !== state.challenge || !checkPow(json.pow.challenge, json.pow.nonce, 12)) return send(400, { error: 'pow_invalid' });
                    if (json.username !== 'alice' || json.email !== 'alice@example.test' || json.password !== 'correct horse battery') return send(400, { error: 'body_lost' });
                    state.challenge = null;
                    return send(202, { status: 'verification_sent' });
                }
                case 'POST /api/v1/auth/login':
                    if (json.login === 'bob' && json.password === 'pw') return send(200, { token: 'sct_bob', expiresAt: 1, user: { username: 'bob' } });
                    if (json.login === 'mfa' && json.password === 'pw') return send(200, { mfaRequired: true, mfaToken: 'mfa_token_1' });
                    if (json.login === 'stubborn') return send(428, { error: 'pow_required', pow: { challenge: 'never', bits: 1 } });
                    return send(401, { error: 'invalid_credentials', message: 'Invalid credentials' });
                case 'POST /api/v1/auth/login/mfa':
                    if (json.mfaToken === 'mfa_token_1' && (json.code === totpCode(SECRET) || json.recoveryCode === 'abcd-efgh-jk')) return send(200, { token: 'sct_mfa', user: { username: 'mfa' } });
                    return send(401, { error: 'invalid_code' });
                case 'GET /api/v1/account/me':
                    if (!auth || !auth.startsWith('Bearer ')) return send(401, { error: 'unauthorized' });
                    return send(200, { user: { token: auth.slice(7) } });
                case 'POST /api/v1/auth/logout': return auth ? send(204) : send(401, { error: 'unauthorized' });
                case 'GET /healthz': return send(200, 'ok');
                case 'GET /api/v1/slow': return setTimeout(() => send(200, {}), 2000);
                default: return send(404, { error: 'not_found' });
            }
        });
    };
}

async function startFake(tlsOpts) {
    const state = { requests: [], connections: 0, challenge: null, issued: [] };
    const server = tlsOpts ? https.createServer(tlsOpts, handler(state)) : http.createServer(handler(state));
    server.on(tlsOpts ? 'secureConnection' : 'connection', () => { state.connections++; });
    await new Promise((r) => server.listen(0, '127.0.0.1', r));
    return { state, port: server.address().port, close: () => new Promise((r) => { server.closeAllConnections(); server.close(r); }) };
}

describe('ApiClient over plain HTTP', () => {
    let fake, api;
    after(async () => { api?.close(); await fake?.close(); });

    test('info, prefixing, JSON bodies and keep-alive', async () => {
        fake = await startFake();
        api = new ApiClient({ port: fake.port, insecure: true });
        const r = await api.info();
        assert.equal(r.status, 200);
        assert.deepEqual(r.body, { name: 'Fake', protocol: { min: 1, max: 1 } });
        assert.equal(fake.state.requests[0].url, '/api/v1/info');
        assert.equal(fake.state.requests[0].headers.authorization, undefined);
        const h = await api.get('/healthz', { prefix: false });
        assert.equal(h.body, 'ok');
        const nf = await api.get('/api/v1/nothing');
        assert.equal(nf.status, 404);
        assert.equal(fake.state.requests.at(-1).url, '/api/v1/nothing');
        for (let i = 0; i < 5; i++) await api.info();
        assert.equal(fake.state.connections, 1, 'one keep-alive connection');
    });

    test('proof of work is solved and the same body resent once', async () => {
        const r = await api.register({ username: 'alice', email: 'alice@example.test', password: 'correct horse battery' });
        assert.equal(r.status, 202);
        assert.deepEqual(r.body, { status: 'verification_sent' });
        assert.equal(api.powSolved, 1);
        const [first, second] = fake.state.requests.filter((q) => q.url === '/api/v1/auth/register');
        assert.equal(JSON.parse(first.body).pow, undefined);
        const again = JSON.parse(second.body);
        assert.equal(again.username, 'alice');
        assert.equal(again.pow.challenge, fake.state.issued[0]);
        assert.equal(checkPow(again.pow.challenge, again.pow.nonce, 12), true);
        assert.match(again.pow.nonce, /^[0-9]+$/);
        // A server that keeps asking gets one retry only; pow: false disables the retry.
        const n = fake.state.requests.length;
        const s = await api.login('stubborn', 'x');
        assert.equal(s.status, 428);
        assert.equal(fake.state.requests.length, n + 2);
        const noPow = await api.post('/auth/login', { login: 'stubborn', password: 'x' }, { pow: false });
        assert.equal(noPow.status, 428);
        assert.equal(fake.state.requests.length, n + 3);
    });

    test('login, bearer token, logout', async () => {
        const bad = await api.login('bob', 'nope');
        assert.equal(bad.status, 401);
        assert.equal(bad.body.error, 'invalid_credentials');
        assert.equal(api.token, null);
        const ok = await api.login('bob', 'pw');
        assert.equal(ok.status, 200);
        assert.equal(api.token, 'sct_bob');
        const me = await api.me();
        assert.deepEqual(me.body, { user: { token: 'sct_bob' } });
        assert.equal(fake.state.requests.at(-1).headers.authorization, 'Bearer sct_bob');
        const other = await api.get('/account/me', { token: 'sct_other' });
        assert.equal(other.body.user.token, 'sct_other');
        const none = await api.get('/account/me', { token: null });
        assert.equal(none.status, 401);
        const out = await api.logout();
        assert.equal(out.status, 204);
        assert.equal(out.body, null);
        assert.equal(api.token, null);
    });

    test('login with MFA: two steps, or one with a TOTP secret or a recovery code', async () => {
        const step1 = await api.login('mfa', 'pw');
        assert.deepEqual(step1.body, { mfaRequired: true, mfaToken: 'mfa_token_1' });
        assert.equal(api.token, null);
        const wrong = await api.loginMfa('mfa_token_1', { code: '000000' === totpCode(SECRET) ? '111111' : '000000' });
        assert.equal(wrong.status, 401);
        const step2 = await api.loginMfa(step1.body.mfaToken, { code: totpCode(SECRET) });
        assert.equal(step2.status, 200);
        assert.equal(api.token, 'sct_mfa');
        api.token = null;
        const oneShot = await api.login('mfa', 'pw', { totpSecret: SECRET });
        assert.equal(oneShot.body.token, 'sct_mfa');
        api.token = null;
        const recovery = await api.login('mfa', 'pw', { recoveryCode: 'abcd-efgh-jk' });
        assert.equal(recovery.body.token, 'sct_mfa');
    });

    test('timeouts and refused connections reject', async () => {
        const slow = new ApiClient({ port: fake.port, insecure: true, timeoutMs: 100 });
        await assert.rejects(slow.get('/slow'), /timed out/);
        slow.close();
        const dead = new ApiClient({ port: 1, insecure: true });
        await assert.rejects(dead.info(), (e) => e.code === 'ECONNREFUSED');
        dead.close();
    });
});

test('TOTP: RFC 6238 SHA-1 vectors and base32', () => {
    const secret = 'GEZDGNBVGY3TQOJQGEZDGNBVGY3TQOJQ';     // "12345678901234567890"
    assert.equal(base32Decode(secret).toString(), '12345678901234567890');
    assert.equal(base32Decode('gezd gnbv gy3t qojq====').toString(), '1234567890');
    const vectors = [[59, '94287082'], [1111111109, '07081804'], [1111111111, '14050471'], [1234567890, '89005924'], [2000000000, '69279037'], [20000000000, '65353130']];
    for (const [t, code] of vectors) {
        assert.equal(totpCode(secret, t * 1000, { digits: 8 }), code, `T = ${t}`);
        assert.equal(totpCode(secret, t * 1000), code.slice(2));
    }
    assert.throws(() => base32Decode('abc1'), /invalid character/);
});

function haveOpenssl() {
    try { execFileSync('openssl', ['version'], { stdio: 'ignore' }); return true; } catch { return false; }
}

describe('ApiClient over HTTPS', { skip: !haveOpenssl() && 'openssl not found' }, () => {
    let dir;
    after(() => { if (dir) fs.rmSync(dir, { recursive: true, force: true }); });
    test('trusts the given CA, refuses an unknown certificate', async () => {
        dir = fs.mkdtempSync(path.join(os.tmpdir(), 'scacelith-api-tls-'));
        execFileSync('openssl', ['req', '-x509', '-newkey', 'ec', '-pkeyopt', 'ec_paramgen_curve:prime256v1', '-nodes', '-days', '2',
            '-subj', '/CN=localhost', '-addext', 'subjectAltName=DNS:localhost,IP:127.0.0.1',
            '-keyout', path.join(dir, 'key.pem'), '-out', path.join(dir, 'cert.pem')], { stdio: 'ignore' });
        const cert = fs.readFileSync(path.join(dir, 'cert.pem'));
        const fake = await startFake({ cert, key: fs.readFileSync(path.join(dir, 'key.pem')) });
        const api = new ApiClient({ port: fake.port, ca: cert });
        assert.equal((await api.info()).status, 200);
        const reg = await api.register({ username: 'alice', email: 'alice@example.test', password: 'correct horse battery' });
        assert.equal(reg.status, 202);
        const named = new ApiClient({ host: 'localhost', port: fake.port, ca: cert });
        assert.equal((await named.info()).status, 200);
        const untrusted = new ApiClient({ port: fake.port });
        await assert.rejects(untrusted.info(), (e) => /self[- ]signed|unable to verify|certificate/i.test(e.message));
        for (const a of [api, named, untrusted]) a.close();
        await fake.close();
    });
});
