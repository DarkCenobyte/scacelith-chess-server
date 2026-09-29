import test from 'node:test';
import assert from 'node:assert/strict';
import { createCheckFloor, createHashLimiter, limitHasher, PasswordBusyError } from '../../src/security/password.js';
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

test('maxWaitMs: a call waits at most its own budget, never more than queueTimeoutMs', async () => {
    const lim = createHashLimiter({ concurrency: 1, queueMax: 5, queueTimeoutMs: 5000 });
    const h = held();
    const before = rejected('timeout');
    const running = lim.run(() => h.done);
    let called = false;
    const t0 = performance.now();
    await assert.rejects(lim.run(async () => { called = true; }, { maxWaitMs: 60 }), (err) => err instanceof PasswordBusyError && err.reason === 'timeout');
    const waited = performance.now() - t0;
    assert.ok(waited >= 55 && waited < 1000, `waited ${waited.toFixed(0)} ms for a budget of 60 ms (queue timeout 5000 ms)`);
    assert.equal(called, false);
    assert.equal(rejected('timeout'), before + 1);
    assert.equal(lim.stats().waiting, 0);

    // A budget above the queue timeout is capped by it.
    const short = createHashLimiter({ concurrency: 1, queueMax: 5, queueTimeoutMs: 40 });
    const h2 = held();
    const r2 = short.run(() => h2.done);
    const t1 = performance.now();
    await assert.rejects(short.run(async () => {}, { maxWaitMs: 60000 }), { reason: 'timeout' });
    assert.ok(performance.now() - t1 < 1000);
    h.release(); h2.release();
    await Promise.all([running, r2]);
    // A granted call with a budget still runs normally.
    assert.equal(await lim.run(async () => 'ran', { maxWaitMs: 60 }), 'ran');
});

test('maxWaitMs 0: runs only when a slot is free at once, else refused as no_wait without counting a refusal', async () => {
    const lim = createHashLimiter({ concurrency: 1, queueMax: 5, queueTimeoutMs: 5000 });
    assert.equal(await lim.run(async () => 'free', { maxWaitMs: 0 }), 'free');
    const h = held();
    const running = lim.run(() => h.done);
    const counts = () => ['queue_full', 'timeout', 'source_limit'].map(rejected);
    const c0 = counts(), q0 = queued();
    let called = false;
    const t0 = performance.now();
    await assert.rejects(lim.run(async () => { called = true; }, { maxWaitMs: 0, source: 'x' }), (err) => err instanceof PasswordBusyError && err.reason === 'no_wait');
    await assert.rejects(lim.run(async () => { called = true; }, { maxWaitMs: -5 }), { reason: 'no_wait' });
    assert.ok(performance.now() - t0 < 1000, 'refused at once');
    assert.equal(called, false);
    assert.deepEqual(counts(), c0, 'an optional task skipped is not a refused request');
    assert.equal(queued(), q0);
    assert.deepEqual([lim.stats().active, lim.stats().waiting, lim.waitingFrom('x')], [1, 0, 0]);
    h.release();
    await running;
});

test('perSourceMax: under contention one source has at most that many tasks waiting; others still queue; FIFO is kept', async () => {
    // queueMax 6: the cap applies from 3 waiting tasks on (half the queue).
    const lim = createHashLimiter({ concurrency: 1, queueMax: 6, queueTimeoutMs: 5000, perSourceMax: 2 });
    const order = [];
    const h = held();
    const s0 = rejected('source_limit'), f0 = rejected('queue_full');
    // The running task does not count: only waiting ones do.
    const running = lim.run(async () => { order.push('run'); await h.done; }, { source: 'A' });
    await tick();
    const a1 = lim.run(async () => { order.push('a1'); }, { source: 'A' });
    const b1 = lim.run(async () => { order.push('b1'); }, { source: 'B' });
    const a2 = lim.run(async () => { order.push('a2'); }, { source: 'A' });
    let called = false;
    await assert.rejects(lim.run(async () => { called = true; }, { source: 'A' }), (err) => err instanceof PasswordBusyError && err.reason === 'source_limit');
    assert.equal(called, false);
    assert.equal(rejected('source_limit'), s0 + 1);
    assert.equal(rejected('queue_full'), f0);
    const n1 = lim.run(async () => { order.push('n1'); });          // no source: never limited
    const n2 = lim.run(async () => { order.push('n2'); }, { source: null });
    const b2 = lim.run(async () => { order.push('b2'); }, { source: 'B' });
    assert.deepEqual([lim.waitingFrom('A'), lim.waitingFrom('B'), lim.stats().waiting], [2, 2, 6]);
    h.release();
    await Promise.all([running, a1, b1, a2, n1, n2, b2]);
    assert.deepEqual(order, ['run', 'a1', 'b1', 'a2', 'n1', 'n2', 'b2']);
    assert.deepEqual([lim.waitingFrom('A'), lim.waitingFrom('B')], [0, 0], 'granted waiters are no longer counted');

    // An expired waiter is no longer counted either.
    const h2 = held();
    const r2 = lim.run(() => h2.done);
    const e1 = lim.run(async () => {}, { source: 'C', maxWaitMs: 20 });
    const e2 = lim.run(async () => {}, { source: 'C', maxWaitMs: 20 });
    await Promise.all([assert.rejects(e1, { reason: 'timeout' }), assert.rejects(e2, { reason: 'timeout' })]);
    assert.equal(lim.waitingFrom('C'), 0);
    const c3 = lim.run(async () => 'c3', { source: 'C' });
    h2.release();
    assert.equal(await c3, 'c3');
    await r2;
    assert.throws(() => createHashLimiter({ perSourceMax: 0 }), RangeError);
    assert.throws(() => createHashLimiter({ perSourceMax: 1.5 }), RangeError);
});

test('perSourceMax applies only once the queue is half full: one source may use an idle queue, never more than half of it', async () => {
    const lim = createHashLimiter({ concurrency: 1, queueMax: 8, queueTimeoutMs: 5000, perSourceMax: 2 });
    assert.equal(lim.perSourceMax, 2);
    const s0 = rejected('source_limit');
    const h = held();
    const running = lim.run(() => h.done, { source: 'A' });
    await tick();
    // An idle queue: one source (a classroom behind one address) queues 4 tasks, beyond its cap of 2.
    const mine = [1, 2, 3, 4].map((i) => lim.run(async () => `a${i}`, { source: 'A' }));
    assert.deepEqual([lim.waitingFrom('A'), lim.stats().waiting], [4, 4]);
    assert.equal(rejected('source_limit'), s0);
    // Half full (4 of 8): the source is at its cap and refused, a new source is accepted.
    await assert.rejects(lim.run(async () => 'a5', { source: 'A' }), (err) => err instanceof PasswordBusyError && err.reason === 'source_limit');
    assert.equal(rejected('source_limit'), s0 + 1);
    const b1 = lim.run(async () => 'b1', { source: 'B' });
    const b2 = lim.run(async () => 'b2', { source: 'B' });
    await assert.rejects(lim.run(async () => 'b3', { source: 'B' }), { reason: 'source_limit' });
    assert.deepEqual([lim.waitingFrom('A'), lim.waitingFrom('B'), lim.stats().waiting], [4, 2, 6]);
    h.release();
    assert.deepEqual(await Promise.all([...mine, b1, b2]), ['a1', 'a2', 'a3', 'a4', 'b1', 'b2']);
    await running;

    // At exactly half full, a source with 2 waiting is refused while a new source is accepted.
    const h2 = held();
    const r2 = lim.run(() => h2.done);
    await tick();
    const fill = [lim.run(async () => {}, { source: 'X' }), lim.run(async () => {}, { source: 'Y' }),
        lim.run(async () => {}, { source: 'C' }), lim.run(async () => {}, { source: 'C' })];
    assert.equal(lim.stats().waiting, 4);
    await assert.rejects(lim.run(async () => {}, { source: 'C' }), { reason: 'source_limit' });
    const d = lim.run(async () => 'd', { source: 'D' });
    assert.equal(lim.stats().waiting, 5);
    // Just under half full (3 of 8), the same source would still have been accepted.
    const small = createHashLimiter({ concurrency: 1, queueMax: 8, queueTimeoutMs: 5000, perSourceMax: 2 });
    const h3 = held();
    const r3 = small.run(() => h3.done);
    await tick();
    const under = [small.run(async () => {}, { source: 'X' }), small.run(async () => {}, { source: 'C' }), small.run(async () => {}, { source: 'C' })];
    const third = small.run(async () => 'third', { source: 'C' });
    assert.equal(small.waitingFrom('C'), 3);
    h2.release(); h3.release();
    await Promise.all([r2, ...fill, r3, ...under]);
    assert.equal(await d, 'd');
    assert.equal(await third, 'third');
    // A queue of 0 or 1 is always contended: the cap never lets a task through that the queue refuses.
    const one = createHashLimiter({ concurrency: 1, queueMax: 1, queueTimeoutMs: 5000, perSourceMax: 1 });
    const h4 = held();
    const r4 = one.run(() => h4.done);
    const w4 = one.run(async () => {}, { source: 'E' });
    await assert.rejects(one.run(async () => {}, { source: 'E' }), { reason: 'source_limit' });
    await assert.rejects(one.run(async () => {}, { source: 'F' }), { reason: 'queue_full' });
    h4.release();
    await Promise.all([r4, w4]);
});

test('createCheckFloor: the slowest check of the current or previous period, capped', () => {
    let t = 1000;
    const f = createCheckFloor({ capMs: 500, periodMs: 100, clock: () => t });
    assert.equal(f.floorMs(), 0);
    f.record(30); f.record(80); f.record(40);
    assert.equal(f.floorMs(), 80);
    t += 100;                   // next period: the previous one still counts
    f.record(20);
    assert.equal(f.floorMs(), 80);
    t += 100;                   // one period later again: only the last period's 20
    assert.equal(f.floorMs(), 20);
    t += 250;                   // a gap of more than one period forgets everything
    assert.equal(f.floorMs(), 0);
    f.record(9000);
    assert.equal(f.floorMs(), 500, 'capped');
});

test('createCheckFloor: the warm-up baseline does not decay', () => {
    let t = 1000;
    const f = createCheckFloor({ capMs: 500, periodMs: 100, clock: () => t });
    assert.equal(f.baselineMs(), 0);
    f.setBaseline(60);
    assert.equal(f.floorMs(), 60, 'the floor of a fresh worker, before any check');
    f.record(20);
    assert.equal(f.floorMs(), 60, 'a faster check does not lower it');
    f.record(90);
    assert.equal(f.floorMs(), 90, 'a slower recent check raises it');
    t += 100;                   // one period roll: the slower check still counts
    assert.equal(f.floorMs(), 90);
    t += 100;                   // two period rolls: the recent checks are forgotten, the baseline stays
    assert.equal(f.floorMs(), 60);
    t += 10 * 100;              // a long quiet time
    assert.equal(f.floorMs(), 60);
    f.setBaseline(40);          // a lower measure does not lower it
    f.setBaseline(Number.NaN);
    f.setBaseline(undefined);
    assert.deepEqual([f.baselineMs(), f.floorMs()], [60, 60]);
    f.setBaseline(9000);
    assert.equal(f.floorMs(), 500, 'capped');
});

test('limitHasher.warmUp sets the floor baseline from what the hasher warm-up measured, in a slot', async () => {
    let running = 0;
    const stub = {
        algorithm: 'stub', parse: () => null,
        hash: async (pw) => `h:${pw}`,
        verify: async () => ({ ok: false, needsRehash: false }),
        verifyDummy: async () => false,
        warmUp: async () => { running++; await sleep(5); running--; return 42; },
    };
    const lim = createHashLimiter({ concurrency: 1, queueMax: 10, queueTimeoutMs: 5000 });
    const capped = limitHasher(stub, lim, { floor: createCheckFloor({ capMs: 2000 }) });
    const h = held();
    const busy = lim.run(() => h.done);
    const warm = capped.warmUp();
    await tick();
    assert.deepEqual([running, lim.stats().waiting], [0, 1], 'the warm-up waits for a slot like any hash');
    h.release();
    assert.equal(await warm, undefined);
    await busy;
    assert.equal(capped.floor.baselineMs(), 42);
    assert.equal(capped.floor.floorMs(), 42);
    // The first failed check of the worker is padded to it.
    const t0 = performance.now();
    assert.deepEqual(await capped.checkPassword(null, 'x'), { ok: false, needsRehash: false });
    assert.ok(performance.now() - t0 >= 38, `first failure took ${(performance.now() - t0).toFixed(0)} ms, baseline 42 ms`);
});

test('limitHasher.checkPassword: a failure is padded to the floor after the slot is released', async () => {
    const stub = {
        algorithm: 'stub', parse: () => null,
        hash: async (pw) => `h:${pw}`,
        verify: async (stored, pw) => { await sleep(stored.startsWith('slow') ? 80 : 5); return { ok: stored.endsWith(`:${pw}`), needsRehash: false }; },
        verifyDummy: async () => { await sleep(5); return false; },
    };
    const lim = createHashLimiter({ concurrency: 1, queueMax: 10, queueTimeoutMs: 5000 });
    const capped = limitHasher(stub, lim, { floor: createCheckFloor({ capMs: 2000 }) });
    const time = async (f) => { const t0 = performance.now(); const r = await f(); return [r, performance.now() - t0]; };
    // A slow stored hash sets the floor; the fast dummy failure then takes as long.
    const [slowWrong, t1] = await time(() => capped.checkPassword('slow:pw', 'nope'));
    assert.deepEqual(slowWrong, { ok: false, needsRehash: false });
    assert.ok(capped.floor.floorMs() >= 75, `floor ${capped.floor.floorMs()}`);
    const [dummy, t2] = await time(() => capped.checkPassword(null, 'anything'));
    assert.deepEqual(dummy, { ok: false, needsRehash: false });
    assert.ok(t2 >= capped.floor.floorMs() - 5, `dummy failure took ${t2.toFixed(0)} ms, floor ${capped.floor.floorMs().toFixed(0)} ms (slow failure ${t1.toFixed(0)} ms)`);
    // A success is not padded.
    const [good, t3] = await time(() => capped.checkPassword('fast:pw', 'pw'));
    assert.deepEqual(good, { ok: true, needsRehash: false });
    assert.ok(t3 < 60, `success took ${t3.toFixed(0)} ms`);
    // The padding holds no slot: the next task runs while the failure still waits.
    let padded = false;
    const failing = capped.checkPassword(null, 'x').then(() => { padded = true; });
    await sleep(20);
    assert.equal(await lim.run(async () => (padded ? 'after' : 'during')), 'during');
    await failing;
    // The options reach the limiter.
    const h = held();
    const busy = lim.run(() => h.done);
    await assert.rejects(capped.checkPassword(null, 'x', { maxWaitMs: 0 }), { reason: 'no_wait' });
    await assert.rejects(capped.hash('x', { maxWaitMs: 0 }), { reason: 'no_wait' });
    await assert.rejects(capped.verify('fast:x', 'x', { maxWaitMs: 0 }), { reason: 'no_wait' });
    await assert.rejects(capped.verifyDummy('x', { maxWaitMs: 0 }), { reason: 'no_wait' });
    h.release();
    await busy;
});
