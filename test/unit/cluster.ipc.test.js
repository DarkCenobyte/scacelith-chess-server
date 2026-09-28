import assert from 'node:assert/strict';
import { fork } from 'node:child_process';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { describe, it } from 'node:test';
import { Ipc, IpcClosedError, IpcRemoteError, IpcTimeoutError, channelPair } from '../../src/cluster/ipc.js';
import { OnceStore, SlidingWindowLimiter } from '../../src/cluster/limits.js';

describe('ipc', () => {
    it('answers requests (sync and async handlers) and keeps Buffers', async () => {
        const [a, b] = channelPair();
        const left = new Ipc(a), right = new Ipc(b);
        right.on('echo', (p) => ({ got: p }));
        right.on('later', async (p) => { await new Promise((r) => setTimeout(r, 5)); return p.n * 2; });
        const r = await left.request('echo', { buf: Buffer.from([1, 2]), n: 1 });
        assert.ok(Buffer.isBuffer(r.got.buf));
        assert.deepEqual([...r.got.buf], [1, 2]);
        assert.equal(await left.request('later', { n: 21 }), 42);
        assert.deepEqual(await left.request('echo'), { got: null });
    });

    it('rejects on timeout, remote exception and missing handler', async () => {
        const [a, b] = channelPair();
        const left = new Ipc(a, { timeoutMs: 30 }), right = new Ipc(b);
        right.on('slow', () => new Promise(() => {}));
        right.on('boom', () => { throw new Error('kaput'); });
        await assert.rejects(left.request('slow'), IpcTimeoutError);
        await assert.rejects(left.request('boom'), (e) => e instanceof IpcRemoteError && /kaput/.test(e.message));
        await assert.rejects(left.request('nobody'), (e) => e instanceof IpcRemoteError && e.code === 'IPC_NO_HANDLER');
        assert.equal(left.pending.size, 0);
    });

    it('delivers notifications and batches messages of one turn', async () => {
        const [a, b] = channelPair();
        let sends = 0;
        const orig = a.send.bind(a);
        a.send = (...args) => { sends++; return orig(...args); };
        const left = new Ipc(a), right = new Ipc(b);
        const got = [];
        right.on('n', (p) => { got.push(p); });
        right.on('q', (p) => p + 1);
        for (let i = 0; i < 100; i++) left.notify('n', i);
        const replies = await Promise.all([left.request('q', 1), left.request('q', 2)]);
        assert.deepEqual(replies, [2, 3]);
        assert.equal(got.length, 100);
        assert.deepEqual(got.slice(0, 3), [0, 1, 2]);
        assert.equal(sends, 1);                                  // 102 messages, one channel send
    });

    it('fails pending requests when closed or when the channel is gone', async () => {
        const [a, b] = channelPair();
        const left = new Ipc(a);
        new Ipc(b).on('never', () => new Promise(() => {}));
        const p = left.request('never');
        await new Promise((r) => setTimeout(r, 10));
        left.close('bye');
        await assert.rejects(p, IpcClosedError);
        await assert.rejects(left.request('x'), IpcClosedError);
        const [c, d] = channelPair();
        const l2 = new Ipc(c);
        new Ipc(d);
        c.disconnect();
        await assert.rejects(l2.request('x'), IpcClosedError);
    });

    it('works over a real process channel with advanced serialization', async () => {
        const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'scacelith-ipc-'));
        const child = path.join(dir, 'child.mjs');
        const ipcUrl = new URL('../../src/cluster/ipc.js', import.meta.url).href;
        fs.writeFileSync(child, `import { Ipc } from ${JSON.stringify(ipcUrl)};
const ipc = new Ipc(process);
ipc.on('sum', ({ a, b }) => a + b);
ipc.on('buf', ({ b }) => ({ isBuffer: Buffer.isBuffer(b), len: b.length }));
ipc.on('ask', async () => ipc.request('parent.value'));
ipc.on('exit', () => { setTimeout(() => process.exit(0), 10); return true; });
`);
        const proc = fork(child, [], { serialization: 'advanced', stdio: 'inherit' });
        const ipc = new Ipc(proc);
        ipc.on('parent.value', () => 'from parent');
        assert.equal(await ipc.request('sum', { a: 2, b: 3 }), 5);
        assert.deepEqual(await ipc.request('buf', { b: Buffer.alloc(300) }), { isBuffer: true, len: 300 });
        assert.equal(await ipc.request('ask'), 'from parent');
        await ipc.request('exit');
        await new Promise((r) => proc.once('exit', r));
        fs.rmSync(dir, { recursive: true, force: true });
    });
});

describe('global rate limiter', () => {
    it('allows up to the limit in a window, then refuses with a retry time', () => {
        let t = 1_000_000;
        const l = new SlidingWindowLimiter({ now: () => t });
        for (let i = 0; i < 5; i++) assert.equal(l.take({ key: 'k', limit: 5, windowMs: 1000 }).allowed, true);
        const r = l.take({ key: 'k', limit: 5, windowMs: 1000 });
        assert.equal(r.allowed, false);
        assert.ok(r.retryAfterMs > 0 && r.retryAfterMs <= 2000, `retry ${r.retryAfterMs}`);
        assert.equal(l.take({ key: 'other', limit: 5, windowMs: 1000 }).allowed, true);
        t += r.retryAfterMs;
        assert.equal(l.take({ key: 'k', limit: 5, windowMs: 1000 }).allowed, true);
    });

    it('slides: the previous window weighs in proportionally', () => {
        let t = 0;
        const l = new SlidingWindowLimiter({ now: () => t });
        for (let i = 0; i < 10; i++) l.take({ key: 'k', limit: 10, windowMs: 1000 });
        t = 1500;                                                 // half of the previous window still counts: 5
        let ok = 0;
        while (l.take({ key: 'k', limit: 10, windowMs: 1000 }).allowed) ok++;
        assert.equal(ok, 5);
        t = 3000;                                                 // two windows later: fresh
        assert.equal(l.peek('k'), 0);
    });

    it('honours cost and stays bounded', () => {
        let t = 0;
        const l = new SlidingWindowLimiter({ now: () => t, maxKeys: 100 });
        assert.equal(l.take({ key: 'c', limit: 10, windowMs: 1000, cost: 7 }).allowed, true);
        assert.equal(l.take({ key: 'c', limit: 10, windowMs: 1000, cost: 7 }).allowed, false);
        for (let i = 0; i < 1000; i++) l.take({ key: `ip${i}`, limit: 1, windowMs: 1000 });
        assert.ok(l.size <= 100, `size ${l.size}`);
        t = 10_000;
        l.sweep();
        assert.equal(l.size, 0);
    });
});

describe('single-use keys', () => {
    it('is fresh once per TTL, bounded, swept', () => {
        let t = 0;
        const o = new OnceStore({ now: () => t, maxKeys: 50 });
        assert.equal(o.consume({ key: 'a', ttlMs: 100 }).fresh, true);
        assert.equal(o.consume({ key: 'a', ttlMs: 100 }).fresh, false);
        t = 150;
        assert.equal(o.consume({ key: 'a', ttlMs: 100 }).fresh, true);
        for (let i = 0; i < 500; i++) o.consume({ key: `k${i}`, ttlMs: 1000 });
        assert.ok(o.size <= 50);
        t = 10_000;
        o.sweep();
        assert.equal(o.size, 0);
    });
});
