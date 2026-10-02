import assert from 'node:assert/strict';
import { fork } from 'node:child_process';
import { describe, it } from 'node:test';
import { fileURLToPath } from 'node:url';
import { ShardSupervisor } from '../../src/cluster/supervisor.js';

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
// Polls rather than sleeping a fixed time: the restart timers may run late on a busy machine.
async function waitFor(pred, ms = 5000) {
    const t0 = Date.now();
    while (!pred()) {
        if (Date.now() - t0 > ms) throw new Error('condition not reached');
        await sleep(5);
    }
}

function recorder() {
    const delays = [];
    const log = { info: (msg, f) => { if (msg === 'restarting shard') delays.push(f.delayMs); }, warn() {}, error() {} };
    return { delays, log };
}

describe('shard supervisor', () => {
    it('backs off when fork() throws, as after a crash: 1 s, then 5 s', async () => {
        const { delays, log } = recorder();
        let forks = 0;
        const sup = new ShardSupervisor({ shards: [0], log, fork: () => { forks++; throw Object.assign(new Error('spawn ENOMEM'), { code: 'ENOMEM' }); } });
        sup.start();
        await waitFor(() => forks === 2);
        await sup.stop(0);
        assert.equal(forks, 2);
        assert.deepEqual(delays, [1000, 5000]);
    });

    it('restarts a shard whose process could not be spawned (an error event, no exit)', async () => {
        const { delays, log } = recorder();
        const downs = [];
        const sup = new ShardSupervisor({
            shards: [0], log, onDown: (s) => downs.push(s),
            fork: () => fork(fileURLToPath(import.meta.url), [], { execPath: '/nonexistent/node', serialization: 'advanced', stdio: 'ignore' }),
        });
        sup.start();
        await waitFor(() => downs.length === 1);
        assert.deepEqual(sup.list(), []);
        assert.deepEqual(downs, [0]);
        assert.deepEqual(delays, [1000]);
        await sup.stop(0);
    });
});
