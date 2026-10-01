// Integration test harness: starts a real Scacelith server (bin/scacelith-server.js) in a child
// process with a temporary data directory, a throw-away self-signed certificate (made with the
// openssl command line) and free ports, and gives the tests helpers to create accounts and
// connect clients with the Node SDK (src/client/).
//
//   const srv = await startServer({ workers: 2, env: { FIRST_MOVE_TIMEOUT_MS: '5000' } });
//   const alice = await srv.player('alice');      // registered, logged in, WebSocket connected
//   ...
//   await srv.stop();

import { spawn, execFileSync } from 'node:child_process';
import fs from 'node:fs';
import net from 'node:net';
import os from 'node:os';
import path from 'node:path';
import http from 'node:http';
import https from 'node:https';
import { fileURLToPath } from 'node:url';

const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '../../..');

export function haveOpenssl() {
    try { execFileSync('openssl', ['version'], { stdio: 'ignore' }); return true; } catch { return false; }
}

export function makeCertificate(dir) {
    const cert = path.join(dir, 'cert.pem'), key = path.join(dir, 'key.pem');
    execFileSync('openssl', ['req', '-x509', '-newkey', 'ec', '-pkeyopt', 'ec_paramgen_curve:prime256v1', '-nodes',
        '-days', '2', '-subj', '/CN=localhost', '-addext', 'subjectAltName=DNS:localhost,IP:127.0.0.1',
        '-keyout', key, '-out', cert], { stdio: 'ignore' });
    return { cert, key, ca: fs.readFileSync(cert) };
}

export async function freePort() {
    return new Promise((resolve, reject) => {
        const s = net.createServer();
        s.unref();
        s.on('error', reject);
        s.listen(0, '127.0.0.1', () => { const { port } = s.address(); s.close(() => resolve(port)); });
    });
}

function getJson(url, ca) {
    return new Promise((resolve, reject) => {
        const req = https.get(url, { ca, timeout: 2000 }, (res) => {
            let body = '';
            res.setEncoding('utf8');
            res.on('data', (c) => { body += c; });
            res.on('end', () => { try { resolve({ status: res.statusCode, body: JSON.parse(body || 'null') }); } catch (e) { reject(e); } });
        });
        req.on('error', reject);
        req.on('timeout', () => req.destroy(new Error('timeout')));
    });
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

export async function startServer({ workers = 1, env = {}, keep = !!process.env.KEEP_TEST_SERVER, dataDir, sharedPort = false } = {}) {
    const dir = dataDir || fs.mkdtempSync(path.join(os.tmpdir(), 'scacelith-it-'));
    const tls = makeCertificate(dir);
    const apiPort = await freePort();
    const wsPort = sharedPort ? apiPort : await freePort();   // sharedPort: API and WSS on one port (the default layout)
    const metricsPort = await freePort();
    const fullEnv = {
        PATH: process.env.PATH,
        HOME: process.env.HOME,
        SERVER_NAME: 'Scacelith Test Server',
        SERVER_PUBLIC_HOST: 'localhost',
        SERVER_SECRET: Buffer.alloc(48, 42).toString('base64'),
        BIND_ADDRESS: '127.0.0.1',
        API_PORT: String(apiPort),
        WS_PORT: String(wsPort),
        METRICS_PORT: String(metricsPort),
        METRICS_BIND: '127.0.0.1',
        TLS_MODE: 'native',
        TLS_CERT_FILE: tls.cert,
        TLS_KEY_FILE: tls.key,
        DATA_DIR: dir,
        WORKERS: String(workers),
        MAIL_TRANSPORT: 'log',
        REQUIRE_EMAIL_VERIFICATION: 'false',
        POW_REGISTER_BITS: '0',
        POW_LOGIN_BITS: '0',
        AUTH_RATE_PER_IP: '100000',
        HTTP_RATE_PER_IP: '100000',
        // Every test client comes from 127.0.0.1: the per-address and per-account limits of the
        // auth family and the account budget are raised like AUTH_RATE_PER_IP.
        AUTH_REGISTER_PER_HOUR: '100000',
        AUTH_MAIL_PER_HOUR: '100000',
        AUTH_FORGOT_PER_HOUR: '100000',
        AUTH_FORGOT_PER_DAY: '100000',
        AUTH_RESET_PER_HOUR: '100000',
        AUTH_MFA_PER_ACCOUNT: '100000',
        AUTH_REAUTH_PER_USER: '100000',
        USER_RATE_PER_MIN: '100000',
        MAX_CONNECTIONS_PER_IP: '100000',
        // Every test client is on the loopback: outside the protection per address (net/ipguard.js);
        // test/integration/abuse.test.js narrows it to 127.0.0.1 and floods from 127.0.0.2.
        ABUSE_EXEMPT: '127.0.0.0/8,::1',
        LOG_LEVEL: 'info',
        LOG_FORMAT: 'json',
        SCACELITH_ENV_FILE: '',
        ...env,
    };
    const child = spawn(process.execPath, [path.join(ROOT, 'bin/scacelith-server.js'), 'start'], {
        cwd: ROOT, env: fullEnv, stdio: ['ignore', 'pipe', 'pipe'],
    });
    const lines = [];
    const waiters = [];
    const onLine = (line) => {
        let rec = null;
        try { rec = JSON.parse(line); } catch { rec = { raw: line }; }
        lines.push(rec);
        for (const w of [...waiters]) if (w.pred(rec)) { waiters.splice(waiters.indexOf(w), 1); w.resolve(rec); }
    };
    let buf = '';
    const feed = (chunk) => {
        buf += chunk;
        let i;
        while ((i = buf.indexOf('\n')) >= 0) { onLine(buf.slice(0, i)); buf = buf.slice(i + 1); }
    };
    child.stdout.setEncoding('utf8').on('data', feed);
    child.stderr.setEncoding('utf8').on('data', feed);
    let exited = null;
    child.on('exit', (code, signal) => { exited = { code, signal }; });

    const base = `https://127.0.0.1:${apiPort}`;
    const deadline = Date.now() + 20000;
    for (;;) {
        if (exited) throw new Error(`server exited during start-up (${JSON.stringify(exited)}):\n${lines.slice(-30).map((l) => JSON.stringify(l)).join('\n')}`);
        try {
            const r = await getJson(`${base}/api/v1/info`, tls.ca);
            if (r.status === 200) break;
        } catch { /* not yet */ }
        if (Date.now() > deadline) { child.kill('SIGKILL'); throw new Error('server did not answer /api/v1/info within 20 s'); }
        await sleep(100);
    }

    const srv = {
        dir, apiPort, wsPort, metricsPort, ca: tls.ca, base, child, lines,
        host: '127.0.0.1',
        get exited() { return exited; },
        // Resolves with the first log record matching pred (already logged ones included).
        waitLog(pred, timeoutMs = 5000) {
            const found = lines.find(pred);
            if (found) return Promise.resolve(found);
            return new Promise((resolve, reject) => {
                const w = { pred, resolve };
                waiters.push(w);
                setTimeout(() => { const i = waiters.indexOf(w); if (i >= 0) { waiters.splice(i, 1); reject(new Error('log line not seen')); } }, timeoutMs);
            });
        },
        async metrics() {
            return new Promise((resolve, reject) => {
                const req = http.get(`http://127.0.0.1:${metricsPort}/metrics`, (res) => {
                    let body = '';
                    res.setEncoding('utf8');
                    res.on('data', (c) => { body += c; });
                    res.on('end', () => resolve(body));
                });
                req.on('error', reject);
            });
        },
        async stop({ signal = 'SIGTERM', timeoutMs = 10000 } = {}) {
            if (!exited) {
                child.kill(signal);
                const t0 = Date.now();
                while (!exited && Date.now() - t0 < timeoutMs) await sleep(50);
                if (!exited) child.kill('SIGKILL');
                while (!exited) await sleep(20);
            }
            if (!keep && !dataDir) fs.rmSync(dir, { recursive: true, force: true });
            return exited;
        },
        // Hard crash (SIGKILL) keeping the data directory, for recovery tests.
        async crash() {
            child.kill('SIGKILL');
            while (!exited) await sleep(20);
            return exited;
        },
        env: fullEnv,
    };
    return srv;
}
