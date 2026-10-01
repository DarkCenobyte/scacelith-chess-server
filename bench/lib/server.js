// A throw-away Scacelith server for the benchmark (same approach as
// test/integration/helpers/harness.js): temporary data directory, self-signed certificate made
// with the openssl command line, free ports, proof of work off, high per-IP limits, logs to a file.
// Bench accounts are created with `bin/admin.js bench-accounts` before the server starts, in a
// scratch copy of the database on tmpfs (/dev/shm) when there is one: account creation is
// fsync-bound on a real disk (about 3.5 ms per account here), the copy is then moved to the data
// directory, which stays on the normal disk so the journal and the game commits pay real fsyncs.

import { spawn, execFileSync } from 'node:child_process';
import fs from 'node:fs';
import net from 'node:net';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { ApiClient } from '../../src/client/index.js';
import { descendants, procEnv, procCmdline } from './procfs.js';

const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '../..');
const SERVER = path.join(ROOT, 'bin/scacelith-server.js');
const ADMIN = path.join(ROOT, 'bin/admin.js');
const MAX_PER_BATCH = 100000;            // bench-accounts --count limit

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

export function freePort() {
    return new Promise((resolve, reject) => {
        const s = net.createServer();
        s.unref();
        s.on('error', reject);
        s.listen(0, '127.0.0.1', () => { const { port } = s.address(); s.close(() => resolve(port)); });
    });
}

function makeCertificate(dir) {
    const cert = path.join(dir, 'cert.pem'), key = path.join(dir, 'key.pem');
    execFileSync('openssl', ['req', '-x509', '-newkey', 'ec', '-pkeyopt', 'ec_paramgen_curve:prime256v1', '-nodes',
        '-days', '2', '-subj', '/CN=localhost', '-addext', 'subjectAltName=DNS:localhost,IP:127.0.0.1',
        '-keyout', key, '-out', cert], { stdio: 'ignore' });
    return { cert, key, ca: fs.readFileSync(cert, 'utf8') };
}

/**
 * Command that runs `argv` with the soft open-file limit raised to the hard limit (POSIX sh).
 * @param {string[]} argv
 */
export function withNofile(argv) {
    if (process.platform === 'win32') return { cmd: argv[0], args: argv.slice(1) };
    return { cmd: '/bin/sh', args: ['-c', 'ulimit -n "$(ulimit -Hn)" 2>/dev/null; exec "$0" "$@"', ...argv] };
}

function runNode(args, env, what) {
    try {
        return execFileSync(process.execPath, args, { cwd: ROOT, env, encoding: 'utf8', stdio: ['ignore', 'pipe', 'pipe'], maxBuffer: 64 << 20 });
    } catch (e) {
        throw new Error(`${what} failed: ${(e.stderr || e.stdout || e.message).toString().slice(-2000)}`);
    }
}

/**
 * Creates `count` bench accounts. Returns [{ u, t }] (username, session token).
 * @param {object} env server environment (DATA_DIR = where the database is)
 * @param {number} count
 * @param {string} scratch directory for the token files (deleted afterwards)
 */
function createAccounts(env, count, scratch) {
    const out = [];
    const letters = 'abcdefghijklmnopqrstuvwxyz';
    for (let batch = 0; out.length < count; batch++) {
        const n = Math.min(MAX_PER_BATCH, count - out.length);
        const prefix = `bn${letters[batch % 26]}${batch >= 26 ? letters[Math.floor(batch / 26) % 26] : ''}`;
        const file = path.join(scratch, `tokens-${batch}.tsv`);
        runNode([ADMIN, 'bench-accounts', '--count', String(n), '--prefix', prefix, '--out', file, '--format', 'tsv', '--i-know-this-is-a-test-server'], env, 'bench-accounts');
        for (const line of fs.readFileSync(file, 'utf8').split('\n')) {
            if (!line) continue;
            const [u, t] = line.split('\t');
            out.push({ u, t });
        }
        fs.rmSync(file, { force: true });
    }
    return out;
}

/**
 * Starts a server.
 * @param {object} o
 * @param {number} o.workers
 * @param {boolean} [o.reusePort]
 * @param {number} o.accounts number of bench accounts to create
 * @param {Record<string,string>} [o.env] extra environment (overrides)
 * @param {string} [o.dataDir] keep the data here (not deleted)
 * @param {boolean} [o.keep] do not delete the temporary directory
 * @param {string} [o.cpuProfDir] write V8 CPU profiles of the server processes there
 * @param {(s: string) => void} [o.log]
 */
export async function startServer({ workers, reusePort = false, accounts = 0, env = {}, dataDir = null, keep = false, cpuProfDir = null, log = () => {} }) {
    const dir = dataDir ? path.resolve(dataDir) : fs.mkdtempSync(path.join(os.tmpdir(), 'scacelith-bench-'));
    fs.mkdirSync(dir, { recursive: true });
    const tlsFiles = makeCertificate(dir);
    const [apiPort, wsPort, metricsPort] = [await freePort(), await freePort(), await freePort()];
    const baseEnv = {
        PATH: process.env.PATH,
        HOME: process.env.HOME,
        SCACELITH_ENV_FILE: '',
        SERVER_NAME: 'Scacelith Bench Server',
        SERVER_PUBLIC_HOST: 'localhost',
        SERVER_SECRET: Buffer.from(Array.from({ length: 48 }, (_, i) => (i * 37 + 11) & 0xff)).toString('base64'),
        BIND_ADDRESS: '127.0.0.1',
        API_PORT: String(apiPort),
        WS_PORT: String(wsPort),
        METRICS_PORT: String(metricsPort),
        METRICS_BIND: '127.0.0.1',
        TLS_MODE: 'native',
        TLS_CERT_FILE: tlsFiles.cert,
        TLS_KEY_FILE: tlsFiles.key,
        DATA_DIR: dir,
        WORKERS: String(workers),
        LISTEN_REUSE_PORT: reusePort ? 'true' : 'false',
        MAIL_TRANSPORT: 'log',
        REQUIRE_EMAIL_VERIFICATION: 'false',
        POW_REGISTER_BITS: '0',
        POW_LOGIN_BITS: '0',
        AUTH_RATE_PER_IP: '1000000',
        HTTP_RATE_PER_IP: '1000000',
        AUTH_REGISTER_PER_HOUR: '1000000',
        AUTH_MAIL_PER_HOUR: '1000000',
        AUTH_FORGOT_PER_HOUR: '1000000',
        AUTH_FORGOT_PER_DAY: '1000000',
        AUTH_RESET_PER_HOUR: '1000000',
        AUTH_MFA_PER_ACCOUNT: '1000000',
        AUTH_REAUTH_PER_USER: '1000000',
        USER_RATE_PER_MIN: '1000000',
        MAX_CONNECTIONS: '1000000',
        MAX_CONNECTIONS_PER_IP: '1000000',
        // Each load process is one source address: only the per-worker handshake cap applies.
        MAX_PENDING_HANDSHAKES_PER_IP: String(Math.max(1, (parseInt(env.MAX_PENDING_HANDSHAKES, 10) || 128) - 1)),
        SHUTDOWN_GRACE_MS: '200',
        LOG_LEVEL: 'warn',
        LOG_FORMAT: 'json',
        ...env,
    };

    // Accounts, in a tmpfs copy of the database when possible.
    let users = [];
    if (accounts > 0) {
        const t0 = Date.now();
        const shm = fs.existsSync('/dev/shm') && !dataDir ? fs.mkdtempSync('/dev/shm/scacelith-bench-seed-') : null;
        const seedDir = shm || dir;
        const seedEnv = { ...baseEnv, DATA_DIR: seedDir, DB_PATH: '' };
        try {
            runNode([SERVER, 'migrate'], seedEnv, 'migrate');
            users = createAccounts(seedEnv, accounts, seedDir);
            if (shm) {
                for (const suffix of ['', '-wal', '-shm']) {
                    const f = path.join(shm, `scacelith.db${suffix}`);
                    if (fs.existsSync(f)) fs.copyFileSync(f, path.join(dir, `scacelith.db${suffix}`));
                }
            }
        } finally {
            if (shm) fs.rmSync(shm, { recursive: true, force: true });
        }
        log(`${users.length} bench accounts created in ${((Date.now() - t0) / 1000).toFixed(1)} s${shm ? ' (tmpfs seed)' : ''}`);
    }

    const logFile = path.join(dir, 'server.log');
    const logFd = fs.openSync(logFile, 'a');
    const { cmd, args } = withNofile([process.execPath, SERVER, 'start']);
    // --server-cpu-prof: every server process (primary, shards, analysis) writes a V8 CPU profile
    // when it exits (graceful stop).
    const runEnv = cpuProfDir ? { ...baseEnv, NODE_OPTIONS: `--cpu-prof --cpu-prof-dir=${path.resolve(cpuProfDir)}` } : baseEnv;
    const child = spawn(cmd, args, { cwd: ROOT, env: runEnv, stdio: ['ignore', logFd, logFd] });
    fs.closeSync(logFd);
    let exited = null;
    child.on('exit', (code, signal) => { exited = { code, signal }; });

    const plain = baseEnv.TLS_MODE === 'off';
    const api = new ApiClient({ host: '127.0.0.1', port: apiPort, ca: tlsFiles.ca, insecure: plain, timeoutMs: 2000 });
    const deadline = Date.now() + 60000;
    let info = null;
    for (;;) {
        if (exited) throw new Error(`server exited during start-up (${JSON.stringify(exited)}):\n${tail(logFile)}`);
        try {
            const r = await api.info();
            if (r.status === 200) { info = r.body; break; }
        } catch { /* not yet */ }
        if (Date.now() > deadline) { child.kill('SIGKILL'); throw new Error(`server did not answer /api/v1/info within 60 s:\n${tail(logFile)}`); }
        await sleep(200);
    }
    // Every shard ready.
    for (;;) {
        try {
            const r = await fetch(`http://127.0.0.1:${metricsPort}/readyz`);
            if (r.status === 200) break;
        } catch { /* not yet */ }
        if (Date.now() > deadline) break;
        await sleep(200);
    }
    api.close?.();

    const srv = {
        dir, apiPort, wsPort, metricsPort, ca: tlsFiles.ca, pid: child.pid, info, users, logFile,
        metricsUrl: `http://127.0.0.1:${metricsPort}/metrics`,
        env: Object.fromEntries(Object.entries(baseEnv).filter(([k]) => !/SECRET|PATH$|^HOME$/.test(k) || k === 'DB_PATH')),
        get exited() { return exited; },
        /** name -> pid of the server processes: primary, shard-<n>, analysis, other-<pid>. */
        processes() {
            const out = { primary: child.pid };
            for (const pid of descendants(child.pid)) {
                const shard = procEnv(pid, 'SHARD');
                if (shard !== null) out[`shard-${shard}`] = pid;
                else if (procCmdline(pid).includes('analysis')) out.analysis = pid;
                else out[`other-${pid}`] = pid;
            }
            return out;
        },
        async stop() {
            const pids = [child.pid, ...descendants(child.pid)];
            if (!exited) {
                child.kill('SIGTERM');
                const t0 = Date.now();
                while (!exited && Date.now() - t0 < 15000) await sleep(100);
                if (!exited) child.kill('SIGKILL');
                while (!exited) await sleep(50);
            }
            // Leftovers (a worker that did not follow the primary).
            for (const pid of pids) { try { process.kill(pid, 'SIGKILL'); } catch { /* gone */ } }
            if (!keep && !dataDir) fs.rmSync(dir, { recursive: true, force: true });
            return exited;
        },
        tail: (n) => tail(logFile, n),
    };
    return srv;
}

function tail(file, n = 30) {
    try { return fs.readFileSync(file, 'utf8').trim().split('\n').slice(-n).join('\n'); } catch { return ''; }
}
