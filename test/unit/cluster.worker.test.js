import assert from 'node:assert/strict';
import { execFileSync, fork } from 'node:child_process';
import fs from 'node:fs';
import net from 'node:net';
import os from 'node:os';
import path from 'node:path';
import { describe, it } from 'node:test';
import { Ipc } from '../../src/cluster/ipc.js';
import { testConfig } from '../../src/config.js';
import { TicketKeys } from '../../src/net/ticket-keys.js';
import { migrate, openStore } from '../../src/store/index.js';

const WORKER_MAIN = new URL('../../src/cluster/worker-main.js', import.meta.url).href;

async function freePort() {
    return new Promise((resolve, reject) => {
        const s = net.createServer();
        s.listen(0, '127.0.0.1', () => { const { port } = s.address(); s.close(() => resolve(port)); });
        s.on('error', reject);
    });
}

let hasOpenssl = true;
try { execFileSync('openssl', ['version'], { stdio: 'ignore' }); } catch { hasOpenssl = false; }

/** TLS_MODE=native with a self-signed certificate made in `dir`. */
function nativeTls(dir) {
    const key = path.join(dir, 'tls.key'), cert = path.join(dir, 'tls.crt');
    execFileSync('openssl', ['req', '-x509', '-newkey', 'ec', '-pkeyopt', 'ec_paramgen_curve:P-256', '-nodes', '-keyout', key, '-out', cert,
        '-days', '1', '-subj', '/CN=localhost'], { stdio: 'ignore' });
    return { TLS_MODE: 'native', TLS_CERT_FILE: cert, TLS_KEY_FILE: key };
}

/**
 * Starts the real worker main() as the cluster module would (it is not a cluster worker here),
 * with a fake primary whose request handlers are `handlers(config, primary)`, and resolves with
 * what the worker did first: 'exit <code>', 'listening' or 'still running'.
 * @param {(dir: string) => object} env configuration keys beyond the test configuration
 */
async function startWorker(t, env, handlers) {
    const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'scacelith-worker-'));
    t.after(() => fs.rmSync(dir, { recursive: true, force: true }));
    const config = testConfig({ DATA_DIR: dir, BIND_ADDRESS: '127.0.0.1', API_PORT: String(await freePort()), ...env(dir) });
    fs.mkdirSync(config.runDir, { recursive: true, mode: 0o700 });
    // The primary migrates the database before it forks the shards (the worker reads the last game id).
    const store = openStore(config);
    await migrate(store);
    store.close();
    const script = path.join(dir, 'worker.mjs');
    fs.writeFileSync(script, `import { main } from ${JSON.stringify(WORKER_MAIN)};\nmain().catch((e) => { console.error(e); process.exit(1); });\n`);
    const child = fork(script, [], {
        serialization: 'advanced', stdio: ['ignore', 'ignore', 'inherit', 'ipc'],
        env: { ...process.env, SHARD: '0', SCACELITH_SERVER_ID: 'test' },
    });
    t.after(() => { if (child.exitCode === null && child.signalCode === null) child.kill('SIGKILL'); });
    const primary = new Ipc(child);
    t.after(() => primary.close());
    for (const [type, handler] of Object.entries(handlers(config, primary))) primary.on(type, handler);
    return new Promise((resolve) => {
        child.once('exit', (code) => resolve(`exit ${code}`));
        primary.on('shard.ready', () => resolve('listening'));
        setTimeout(() => resolve('still running'), 15000).unref();
    });
}

describe('shard worker start-up', () => {
    it('a shutdown that comes while the worker starts stops it before it listens', async (t) => {
        // The primary is stopped while the worker opens its store and replays its journal.
        const outcome = await startWorker(t, () => ({}), (config, primary) => ({
            'config.snapshot': () => {
                setImmediate(() => primary.notify('shutdown', { graceMs: 100 }));
                return config.rawValues;
            },
        }));
        assert.equal(outcome, 'exit 0');
    });

    it('a shutdown that comes with the TLS session-ticket keys stops it before it listens', { skip: !hasOpenssl && 'openssl not available' }, async (t) => {
        // The primary is stopped once the journal is replayed, while the worker waits for the keys
        // every shard shares: the 'shutdown' reaches it before the reply.
        const keys = TicketKeys.random();
        const outcome = await startWorker(t, nativeTls, (config, primary) => ({
            'config.snapshot': () => config.rawValues,
            'tls.ticketKeys': () => {
                primary.notify('shutdown', { graceMs: 100 });
                return keys.state();
            },
        }));
        assert.equal(outcome, 'exit 0');
    });

    it('a native TLS worker asks the primary for the session-ticket keys once, then listens', { skip: !hasOpenssl && 'openssl not available' }, async (t) => {
        const keys = TicketKeys.random();
        let asked = 0;
        const outcome = await startWorker(t, nativeTls, (config) => ({
            'config.snapshot': () => config.rawValues,
            'tls.ticketKeys': () => { asked++; return keys.state(); },
        }));
        assert.equal(outcome, 'listening');
        assert.equal(asked, 1);
    });
});
