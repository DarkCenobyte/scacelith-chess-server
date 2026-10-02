// HTTPS JSON client of the account API (docs/DESIGN.md section 5.9), for tests, bots and the load
// generator. One keep-alive Agent per ApiClient; proof of work (HTTP 428 pow_required) is solved
// and the request retried once, transparently (up to maxPowBits; a harder one is returned as is).
//
//   const api = new ApiClient({ host: '127.0.0.1', port: srv.apiPort, ca: srv.ca });
//   await api.register({ username: 'alice', email: 'alice@example.test', password });
//   const r = await api.login('alice', password);        // r.body.token, also kept in api.token
//   const me = await api.me();
//
// Every call resolves with { status, body, headers } (body: parsed JSON, or the text when the
// answer is not JSON); only network errors and timeouts reject.

import http from 'node:http';
import https from 'node:https';
import crypto from 'node:crypto';
import { solvePow } from './pow.js';

const MAX_BODY = 4 * 1024 * 1024;

export class ApiClient {
    /**
     * @param {object} [o]
     * @param {string} [o.host] default '127.0.0.1'
     * @param {number} [o.port] default 443 (80 with insecure)
     * @param {string|Buffer} [o.ca] trusted CA (self-signed servers)
     * @param {boolean} [o.rejectUnauthorized] default true
     * @param {string} [o.servername] TLS name when host is an address
     * @param {boolean} [o.insecure] plain http (TLS_MODE=off development servers)
     * @param {string} [o.token] session token sent as Bearer
     * @param {string} [o.prefix] API prefix (default '/api/v1')
     * @param {number} [o.timeoutMs] per request (default 15000)
     * @param {number} [o.maxSockets] keep-alive pool size (default 16)
     * @param {object} [o.headers] headers added to every request
     * @param {http.Agent} [o.agent] shared agent (then ca/rejectUnauthorized are the agent's business)
     * @param {number} [o.maxPowBits] hardest proof of work solved (default 28, the server issues at
     *        most 26): the 428 of a harder one is returned as it is, as the solve blocks the thread
     */
    constructor(o = {}) {
        this.host = o.host ?? '127.0.0.1';
        this.insecure = !!o.insecure;
        this.port = o.port ?? (this.insecure ? 80 : 443);
        this.prefix = o.prefix ?? '/api/v1';
        this.token = o.token ?? null;
        this.timeoutMs = o.timeoutMs ?? 15000;
        this.headers = o.headers ?? {};
        this.servername = o.servername;
        this._ownAgent = !o.agent;
        this.agent = o.agent ?? (this.insecure
            ? new http.Agent({ keepAlive: true, maxSockets: o.maxSockets ?? 16 })
            : new https.Agent({ keepAlive: true, maxSockets: o.maxSockets ?? 16, ca: o.ca, rejectUnauthorized: o.rejectUnauthorized !== false, servername: o.servername }));
        this.maxPowBits = o.maxPowBits ?? 28;
        this.powSolved = 0;   // proofs of work solved by this client
    }

    /**
     * One JSON request. Paths without the API prefix get it ('/auth/login' -> '/api/v1/auth/login').
     * @param {string} method
     * @param {string} path
     * @param {object} [body] JSON body
     * @param {{ token?: string|null, headers?: object, pow?: boolean, prefix?: boolean }} [opts]
     *        token: overrides api.token (null: none); pow: false disables the automatic proof of work;
     *        prefix: false sends the path as it is.
     * @returns {Promise<{ status: number, body: any, headers: object }>}
     */
    async request(method, path, body, opts = {}) {
        const full = opts.prefix === false || path.startsWith(this.prefix + '/') || path === this.prefix ? path : this.prefix + path;
        let res = await this._once(method, full, body, opts);
        if (opts.pow !== false && res.status === 428 && res.body && res.body.error === 'pow_required' && res.body.pow) {
            const { challenge, bits } = res.body.pow;
            const b = Number(bits);
            if (!Number.isInteger(b) || b < 0 || b > this.maxPowBits) return res;
            const nonce = solvePow(String(challenge), b);
            this.powSolved++;
            res = await this._once(method, full, { ...(body || {}), pow: { challenge, nonce } }, opts);
        }
        return res;
    }

    _once(method, path, body, opts) {
        const headers = { accept: 'application/json', ...this.headers, ...(opts.headers || {}) };
        const token = opts.token === undefined ? this.token : opts.token;
        if (token) headers.authorization = `Bearer ${token}`;
        let payload = null;
        if (body !== undefined && body !== null) {
            payload = Buffer.from(JSON.stringify(body), 'utf8');
            headers['content-type'] = 'application/json';
            headers['content-length'] = String(payload.length);
        } else if (method !== 'GET' && method !== 'HEAD') headers['content-length'] = '0';
        const mod = this.insecure ? http : https;
        const ro = { method, host: this.host, port: this.port, path, headers, agent: this.agent };
        if (!this.insecure && this.servername) ro.servername = this.servername;
        return new Promise((resolve, reject) => {
            const req = mod.request(ro, (res) => {
                const chunks = [];
                let size = 0;
                res.on('data', (c) => {
                    size += c.length;
                    if (size > MAX_BODY) { req.destroy(new Error('api: response too large')); return; }
                    chunks.push(c);
                });
                res.on('end', () => {
                    const text = Buffer.concat(chunks).toString('utf8');
                    let parsed = text;
                    const type = String(res.headers['content-type'] || '');
                    if (text && (type.includes('json') || /^\s*[[{]/.test(text))) {
                        try { parsed = JSON.parse(text); } catch { parsed = text; }
                    } else if (!text) parsed = null;
                    resolve({ status: res.statusCode, body: parsed, headers: res.headers });
                });
                res.on('error', reject);
            });
            req.setTimeout(this.timeoutMs, () => req.destroy(new Error(`api: ${method} ${path} timed out`)));
            req.on('error', reject);
            req.end(payload || undefined);
        });
    }

    get(path, opts) { return this.request('GET', path, undefined, opts); }
    post(path, body, opts) { return this.request('POST', path, body ?? {}, opts); }

    /** GET /info: server name, protocol range, ports, categories... */
    info() { return this.get('/info', { token: null }); }

    /** POST /auth/register { username, email, password } (proof of work solved when asked). */
    register({ username, email, password }) { return this.post('/auth/register', { username, email, password }, { token: null }); }

    /**
     * POST /auth/login. On success api.token is set. When the account has MFA and a code (or a
     * recovery code) is given, the second step is done too; otherwise the answer carries
     * { mfaRequired: true, mfaToken } for loginMfa().
     * @param {string} login username or e-mail
     * @param {string} password
     * @param {{ clientLabel?: string, code?: string, totpSecret?: string, recoveryCode?: string }} [o]
     *        totpSecret: base32 secret; the current TOTP code is computed from it
     */
    async login(login, password, o = {}) {
        const body = { login, password };
        if (o.clientLabel) body.clientLabel = o.clientLabel;
        const res = await this.post('/auth/login', body, { token: null });
        if (res.status === 200 && res.body && res.body.token) this.token = res.body.token;
        else if (res.status === 200 && res.body && res.body.mfaRequired && (o.code || o.totpSecret || o.recoveryCode)) {
            return this.loginMfa(res.body.mfaToken, { code: o.code ?? (o.totpSecret ? totpCode(o.totpSecret) : undefined), recoveryCode: o.recoveryCode });
        }
        return res;
    }

    /** POST /auth/login/mfa { mfaToken, code | recoveryCode }; sets api.token on success. */
    async loginMfa(mfaToken, { code, recoveryCode } = {}) {
        const body = { mfaToken };
        if (code !== undefined) body.code = code;
        if (recoveryCode !== undefined) body.recoveryCode = recoveryCode;
        const res = await this.post('/auth/login/mfa', body, { token: null });
        if (res.status === 200 && res.body && res.body.token) this.token = res.body.token;
        return res;
    }

    /** POST /auth/logout (revokes the session); clears api.token on success. */
    async logout() {
        const res = await this.post('/auth/logout', {});
        if (res.status >= 200 && res.status < 300) this.token = null;
        return res;
    }

    /** POST /auth/logout-all. */
    async logoutAll() {
        const res = await this.post('/auth/logout-all', {});
        if (res.status >= 200 && res.status < 300) this.token = null;
        return res;
    }

    /** GET /account/me. */
    me() { return this.get('/account/me'); }

    /** Closes the keep-alive sockets (when the agent is this client's own). */
    close() { if (this._ownAgent) this.agent.destroy(); }
}

// ---- TOTP (RFC 6238, SHA-1, 30 s) for MFA tests ------------------------------------------------------

const B32 = 'ABCDEFGHIJKLMNOPQRSTUVWXYZ234567';

/** Decodes RFC 4648 base32 (case-insensitive, padding and spaces ignored). */
export function base32Decode(text) {
    const s = String(text).toUpperCase().replace(/[\s=-]/g, '');
    const out = [];
    let acc = 0, n = 0;
    for (const ch of s) {
        const v = B32.indexOf(ch);
        if (v < 0) throw new Error('base32: invalid character');
        acc = (acc << 5) | v;
        n += 5;
        if (n >= 8) { out.push((acc >>> (n - 8)) & 0xff); n -= 8; }
    }
    return Buffer.from(out);
}

/**
 * TOTP code of a base32 secret at a time (RFC 6238 with SHA-1).
 * @param {string} base32Secret
 * @param {number} [timeMs] default now
 * @param {{ step?: number, digits?: number }} [o] default 30 s, 6 digits
 * @returns {string}
 */
export function totpCode(base32Secret, timeMs = Date.now(), { step = 30, digits = 6 } = {}) {
    const counter = Math.floor(timeMs / 1000 / step);
    const msg = Buffer.alloc(8);
    msg.writeUInt32BE(Math.floor(counter / 4294967296), 0);
    msg.writeUInt32BE(counter >>> 0, 4);
    const h = crypto.createHmac('sha1', base32Decode(base32Secret)).update(msg).digest();
    const o = h[h.length - 1] & 0x0f;
    const bin = ((h[o] & 0x7f) << 24) | (h[o + 1] << 16) | (h[o + 2] << 8) | h[o + 3];
    return String(bin % 10 ** digits).padStart(digits, '0');
}
