import test from 'node:test';
import assert from 'node:assert/strict';
import { createHashLimiter, limitHasher, PasswordBusyError } from '../../src/security/password.js';
import { metrics } from '../../src/metrics.js';

// A task that runs until release() is called (or fail(err)).
function held() {
    let release, fail;
    const done = new Promise((res, rej) => { release = res; fail = rej; });
    return { done, release, fail };
}

const tick = () => new Promise((r) => setImmediate(r));
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

const metric = (name) => metrics.metrics.get(name);
const inFlight = () => metric('scacelith_password_hash_in_flight').root.value;
const queued = () => metric('scacelith_password_hash_queued').root.value;
const rejected = (reason) => metric('scacelith_password_hash_rejected_total').labels(reason).value;
const waits = () => metric('scacelith_password_hash_wait_ms').root.count;

test('waiters run in arrival order (FIFO), one at a time with concurrency 1', async () => {
    const lim = createHashLimiter({ concurrency: 1, queueMax: 10, queueTimeoutMs: 5000 });
    const order = [];
    const first = held();
    const all = [lim.run(async () => { order.push('a'); await first.done; })];
    for (const name of ['b', 'c', 'd', 'e']) all.push(lim.run(async () => { order.push(name); await tick(); }));
    await tick();
    assert.deepEqual(order, ['a']);
    assert.deepEqual(lim.stats(), { active: 1, waiting: 4, concurrency: 1, queueMax: 10, queueTimeoutMs: 5000 });
    first.release();
    // A newcomer arriving while the queue drains does not overtake the waiters.
    await tick();
    all.push(lim.run(async () => { order.push('late'); }));
    await Promise.all(all);
    assert.deepEqual(order, ['a', 'b', 'c', 'd', 'e', 'late']);
    assert.deepEqual([lim.stats().active, lim.stats().waiting], [0, 0]);
});

test('never more than `concurrency` tasks at once; every task runs and returns its value', async () => {
    const lim = createHashLimiter({ concurrency: 3, queueMax: 100, queueTimeoutMs: 5000 });
    let running = 0, peak = 0;
    const results = await Promise.all(Array.from({ length: 20 }, (_, i) => lim.run(async () => {
        running++;
        peak = Math.max(peak, running);
        await sleep(1 + (i % 3));
        running--;
        return i * 2;
    })));
    assert.equal(peak, 3);
    assert.deepEqual(results, Array.from({ length: 20 }, (_, i) => i * 2));
    assert.equal(lim.stats().active, 0);
});

test('a full queue refuses at once, without calling the task', async () => {
    const lim = createHashLimiter({ concurrency: 1, queueMax: 2, queueTimeoutMs: 5000 });
    const h = held();
    const before = rejected('queue_full');
    const running = lim.run(() => h.done);
    const w1 = lim.run(async () => 1), w2 = lim.run(async () => 2);
    let called = false;
    const t0 = performance.now();
    await assert.rejects(lim.run(async () => { called = true; }), (err) => err instanceof PasswordBusyError && err.reason === 'queue_full');
    assert.ok(performance.now() - t0 < 1000, 'refused at once, not after the timeout');
    assert.equal(called, false);
    assert.equal(rejected('queue_full'), before + 1);
    assert.equal(lim.stats().waiting, 2, 'the waiters keep their place');
    h.release();
    assert.deepEqual(await Promise.all([running, w1, w2]), [undefined, 1, 2]);

    // queueMax 0: no waiting at all.
    const none = createHashLimiter({ concurrency: 1, queueMax: 0, queueTimeoutMs: 5000 });
    const h2 = held();
    const p = none.run(() => h2.done);
    await assert.rejects(none.run(async () => {}), { reason: 'queue_full' });
    h2.release();
    await p;
    assert.equal(await none.run(async () => 'free again'), 'free again');
});

test('an expired wait rejects and leaves the queue; the next waiters still run in order', async () => {
    const lim = createHashLimiter({ concurrency: 1, queueMax: 5, queueTimeoutMs: 40 });
    const h = held();
    const before = rejected('timeout');
    const running = lim.run(() => h.done);
    let calledB = false;
    const t0 = performance.now();
    const b = lim.run(async () => { calledB = true; });
    await assert.rejects(b, (err) => err instanceof PasswordBusyError && err.reason === 'timeout');
    assert.ok(performance.now() - t0 >= 35, 'not before the timeout');
    assert.equal(calledB, false);
    assert.equal(lim.stats().waiting, 0, 'the expired waiter left the queue');
    assert.equal(rejected('timeout'), before + 1);
    const c = lim.run(async () => 'c');
    h.release();
    assert.equal(await c, 'c');
    await running;
    assert.deepEqual([lim.stats().active, lim.stats().waiting], [0, 0]);
});

test('a task that throws or rejects releases its slot', async () => {
    const lim = createHashLimiter({ concurrency: 1, queueMax: 5, queueTimeoutMs: 5000 });
    const boom = lim.run(() => { throw new Error('sync boom'); });
    const reject = lim.run(async () => { await tick(); throw new Error('async boom'); });
    const after = lim.run(async () => 'still works');
    await assert.rejects(boom, /sync boom/);
    await assert.rejects(reject, /async boom/);
    assert.equal(await after, 'still works');
    assert.equal(lim.stats().active, 0);
});

test('metrics: in flight, queued, wait time and refusals', async () => {
    const lim = createHashLimiter({ concurrency: 1, queueMax: 1, queueTimeoutMs: 5000 });
    const f0 = inFlight(), q0 = queued(), w0 = waits(), r0 = rejected('queue_full');
    const h = held();
    const a = lim.run(() => h.done);
    const b = lim.run(async () => {});
    await assert.rejects(lim.run(async () => {}), PasswordBusyError);
    assert.equal(inFlight(), f0 + 1);
    assert.equal(queued(), q0 + 1);
    assert.equal(rejected('queue_full'), r0 + 1);
    h.release();
    await Promise.all([a, b]);
    assert.equal(inFlight(), f0);
    assert.equal(queued(), q0);
    assert.equal(waits(), w0 + 2, 'one wait observed per granted slot');
});

test('invalid limits are refused', () => {
    assert.throws(() => createHashLimiter({ concurrency: 0 }), RangeError);
    assert.throws(() => createHashLimiter({ concurrency: 1.5 }), RangeError);
    assert.throws(() => createHashLimiter({ queueMax: -1 }), RangeError);
    assert.throws(() => createHashLimiter({ queueTimeoutMs: Number.NaN }), RangeError);
});

test('limitHasher: hash, verify, the dummy verification and the warm-up share the cap; refusals are mapped', async () => {
    let running = 0, peak = 0;
    const calls = [];
    const slow = (name, value) => async (...args) => {
        calls.push(name);
        running++;
        peak = Math.max(peak, running);
        await sleep(2);
        running--;
        return typeof value === 'function' ? value(...args) : value;
    };
    const stub = {
        algorithm: 'scrypt', parse: () => null,
        hash: slow('hash', (pw) => `h:${pw}`),
        verify: slow('verify', (stored, pw) => ({ ok: stored === `h:${pw}`, needsRehash: false })),
        verifyDummy: slow('verifyDummy', false),
        warmUp: slow('warmUp', undefined),
    };
    const lim = createHashLimiter({ concurrency: 1, queueMax: 10, queueTimeoutMs: 5000 });
    const capped = limitHasher(stub, lim, { onBusy: (err) => Object.assign(new Error('busy'), { mapped: err.reason }) });
    assert.equal(capped.algorithm, 'scrypt');
    const out = await Promise.all([
        capped.warmUp(), capped.hash('pw'), capped.verify('h:pw', 'pw'), capped.verify('h:pw', 'no'), capped.verifyDummy('x'),
    ]);
    assert.deepEqual(out, [undefined, 'h:pw', { ok: true, needsRehash: false }, { ok: false, needsRehash: false }, false]);
    assert.deepEqual(calls, ['warmUp', 'hash', 'verify', 'verify', 'verifyDummy']);
    assert.equal(peak, 1);

    const full = limitHasher(stub, createHashLimiter({ concurrency: 1, queueMax: 0 }), { onBusy: (err) => Object.assign(new Error('busy'), { mapped: err.reason }) });
    const first = full.verifyDummy('a');
    await assert.rejects(full.verifyDummy('b'), (err) => err.message === 'busy' && err.mapped === 'queue_full');
    await first;
    // An error of the hasher itself is not mapped.
    const failing = limitHasher({ ...stub, hash: async () => { throw new Error('scrypt failed'); } }, createHashLimiter(), { onBusy: () => new Error('busy') });
    await assert.rejects(failing.hash('x'), /scrypt failed/);
    // A hasher without warmUp (test doubles) is accepted.
    await limitHasher({ ...stub, warmUp: undefined }, createHashLimiter()).warmUp();
});
