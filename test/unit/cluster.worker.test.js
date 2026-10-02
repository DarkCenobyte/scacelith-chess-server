import assert from 'node:assert/strict';
import { fork } from 'node:child_process';
import fs from 'node:fs';
import net from 'node:net';
import os from 'node:os';
import path from 'node:path';
import { describe, it } from 'node:test';
import { Ipc } from '../../src/cluster/ipc.js';
import { testConfig } from '../../src/config.js';

const WORKER_MAIN = new URL('../../src/cluster/worker-main.js', import.meta.url).href;

async function freePort() {
    return new Promise((resolve, reject) => {
        const s = net.createServer();
        s.listen(0, '127.0.0.1', () => { const { port } = s.address(); s.close(() => resolve(port)); });
        s.on('error', reject);
    });
}

describe('shard worker start-up', () => {
    it('a shutdown that comes while the worker starts stops it before it listens', async (t) => {
        const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'scacelith-worker-'));
        t.after(() => fs.rmSync(dir, { recursive: true, force: true }));
        const config = testConfig({ DATA_DIR: dir, BIND_ADDRESS: '127.0.0.1', API_PORT: String(await freePort()) });
        fs.mkdirSync(config.runDir, { recursive: true, mode: 0o700 });
        // The real worker main(), started as the cluster module would (it is not a cluster worker here).
        const script = path.join(dir, 'worker.mjs');
        fs.writeFileSync(script, `import { main } from ${JSON.stringify(WORKER_MAIN)};\nmain().catch((e) => { console.error(e); process.exit(1); });\n`);
        const child = fork(script, [], {
            serialization: 'advanced', stdio: ['ignore', 'ignore', 'inherit', 'ipc'],
            env: { ...process.env, SHARD: '0', SCACELITH_SERVER_ID: 'test' },
        });
        t.after(() => { if (child.exitCode === null && child.signalCode === null) child.kill('SIGKILL'); });
        const primary = new Ipc(child);
        t.after(() => primary.close());
        // The primary is stopped while the worker opens its store and replays its journal.
        primary.on('config.snapshot', () => {
            setImmediate(() => primary.notify('shutdown', { graceMs: 100 }));
            return config.rawValues;
        });
        const outcome = await new Promise((resolve) => {
            child.once('exit', (code) => resolve(`exit ${code}`));
            primary.on('shard.ready', () => resolve('listening'));
            setTimeout(() => resolve('still running'), 15000).unref();
        });
        assert.equal(outcome, 'exit 0');
    });
});
