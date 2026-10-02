// GIF rendering pool (src/gif/pool.js): real renders on a thread, and with a stand-in thread
// (helpers/gif-slow-worker.js): queue limit, wait timeout, render timeout, a dying thread,
// close(), stats.
import { test } from 'node:test';
import assert from 'node:assert/strict';

import { createGifPool } from '../../src/gif/pool.js';
import { decodeGif } from './helpers/gif-decode.js';

const SLOW = new URL('./helpers/gif-slow-worker.js', import.meta.url);
const E2E4 = 12 | (28 << 6), E7E5 = 52 | (36 << 6);

test('renders on a worker thread; render_failed for a bad job; threads restart after idling', async () => {
    const pool = createGifPool({ threads: 1, queueMax: 4, idleMs: 50 });
    try {
        const gif = await pool.render({ moves: [E2E4, E7E5], white: { name: 'a' }, black: { name: 'b' }, result: '*', options: { size: 'small' } });
        assert.ok(Buffer.isBuffer(gif));
        assert.equal(decodeGif(gif).frames.length, 3);
        await assert.rejects(pool.render({ moves: [E7E5] }), (e) => e.code === 'render_failed' && /illegal move at ply 1/.test(e.message));
        await assert.rejects(pool.render({ startFen: 'nonsense' }), (e) => e.code === 'render_failed' && /invalid start position/.test(e.message));
        let s = pool.stats();
        assert.equal(s.completed, 1);
        assert.equal(s.failed, 2);
        assert.equal(s.live, 1);
        assert.equal(s.threadsStarted, 1);
        assert.ok(s.bytesTotal === gif.length && s.renderMsMax > 0);
        // Idle: the thread stops, and the next job starts a new one.
        await new Promise((r) => setTimeout(r, 200));
        assert.equal(pool.stats().live, 0);
        await pool.render({ moves: [], options: { size: 'small' } });
        s = pool.stats();
        assert.equal(s.threadsStarted, 2);
        assert.equal(s.completed, 2);
    } finally {
        await pool.close();
    }
});

test('busy: the queue is full', async () => {
    const pool = createGifPool({ threads: 1, queueMax: 2, timeoutMs: 5000, workerUrl: SLOW });
    try {
        const running = pool.render({ sleepMs: 300 });
        const queued = [pool.render({ sleepMs: 0 }), pool.render({ sleepMs: 0 })];
        await assert.rejects(pool.render({}), (e) => e.code === 'busy' && /queue full/.test(e.message));
        const s = pool.stats();
        assert.equal(s.running, 1);
        assert.equal(s.queued, 2);
        assert.equal(s.rejectedFull, 1);
        for (const r of [running, ...queued]) assert.equal((await r).subarray(0, 6).toString('latin1'), 'GIF89a');
        assert.equal(pool.stats().completed, 3);
    } finally {
        await pool.close();
    }
});

test('busy: a job that waits longer than timeoutMs; jobs run in order', async () => {
    const pool = createGifPool({ threads: 1, queueMax: 4, timeoutMs: 100, workerUrl: SLOW });
    try {
        const order = [];
        const first = pool.render({ sleepMs: 400 }).then((g) => { order.push('first'); return g; });
        const late = pool.render({ sleepMs: 0 });
        await assert.rejects(late, (e) => e.code === 'busy' && /within 100 ms/.test(e.message));
        await first;
        await pool.render({ sleepMs: 0 }).then(() => order.push('second'));
        assert.deepEqual(order, ['first', 'second']);
        assert.equal(pool.stats().rejectedWait, 1);
    } finally {
        await pool.close();
    }
});

test('render_failed: a render running too long (the thread is replaced), a thread that dies, an error', async () => {
    const pool = createGifPool({ threads: 1, queueMax: 4, renderTimeoutMs: 150, workerUrl: SLOW });
    try {
        await assert.rejects(pool.render({ sleepMs: 2000 }), (e) => e.code === 'render_failed' && /longer than 150 ms/.test(e.message));
        assert.equal(pool.stats().renderTimeouts, 1);
        assert.ok((await pool.render({ bytes: 16 })).length === 16);
        await assert.rejects(pool.render({ crash: true }), (e) => e.code === 'render_failed' && /thread stopped/.test(e.message));
        await assert.rejects(pool.render({ fail: 'bad job' }), (e) => e.code === 'render_failed' && /bad job/.test(e.message));
        assert.ok((await pool.render({})).length === 8);
        const s = pool.stats();
        assert.equal(s.threadsStarted, 3);
        assert.equal(s.failed, 3);
        assert.equal(s.completed, 2);
    } finally {
        await pool.close();
    }
});

test('render_failed when no rendering thread can start: nothing stays queued', async () => {
    const pool = createGifPool({ threads: 1, queueMax: 4, workerUrl: 'not-a-path.js' });
    try {
        await assert.rejects(pool.render({}), (e) => e.code === 'render_failed' && /cannot start a rendering thread/.test(e.message));
        const s = pool.stats();
        assert.equal(s.queued, 0);
        assert.equal(s.live, 0);
        assert.equal(s.failed, 1);
    } finally {
        await pool.close();
    }
});

test('two threads render at the same time', async () => {
    const pool = createGifPool({ threads: 2, queueMax: 0, workerUrl: SLOW });
    try {
        const both = [pool.render({ sleepMs: 250 }), pool.render({ sleepMs: 250 })];
        assert.equal(pool.stats().running, 2);
        await assert.rejects(pool.render({}), (e) => e.code === 'busy');
        await Promise.all(both);
        assert.equal(pool.stats().live, 2);
        assert.equal(pool.stats().completed, 2);
    } finally {
        await pool.close();
    }
});

test('close() rejects running and waiting jobs as busy, and refuses new ones', async () => {
    const pool = createGifPool({ threads: 1, queueMax: 4, workerUrl: SLOW });
    const running = pool.render({ sleepMs: 500 });
    const waiting = pool.render({});
    const checks = [running, waiting].map((p) => assert.rejects(p, (e) => e.code === 'busy' && /closed/.test(e.message)));
    await new Promise((r) => setTimeout(r, 50));
    await pool.close();
    await Promise.all(checks);
    await assert.rejects(pool.render({}), (e) => e.code === 'busy');
    assert.equal(pool.stats().live, 0);
    await pool.close();     // twice is harmless
});
