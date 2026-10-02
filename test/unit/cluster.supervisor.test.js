import assert from 'node:assert/strict';
import { fork } from 'node:child_process';
import { describe, it } from 'node:test';
import { fileURLToPath } from 'node:url';
import { ShardSupervisor } from '../../src/cluster/supervisor.js';

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

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
        await sleep(1200);
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
        await sleep(100);
        assert.deepEqual(sup.list(), []);
        assert.deepEqual(downs, [0]);
        assert.deepEqual(delays, [1000]);
        await sup.stop(0);
    });
});
