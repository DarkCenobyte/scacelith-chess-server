import test from 'node:test';
import assert from 'node:assert/strict';
import { TimerWheel } from '../../src/game/timer-wheel.js';

const collect = (wheel, t) => { const got = []; wheel.advance(t, (e) => got.push(e.name)); return got; };

test('fires at the deadline, not before; cancel and reschedule are O(1) and exact', () => {
    const w = new TimerWheel({ slotMs: 10, slots: 64, startAt: 0 });
    const a = { name: 'a' }, b = { name: 'b' }, c = { name: 'c' };
    w.schedule(a, 105); w.schedule(b, 105); w.schedule(c, 300);
    assert.equal(w.size, 3);
    assert.deepEqual(collect(w, 104), []);
    assert.deepEqual(collect(w, 105).sort(), ['a', 'b']);
    assert.equal(w.size, 1);
    assert.equal(w.cancel(c), true);
    assert.equal(w.cancel(c), false);
    assert.equal(w.size, 0);
    w.schedule(a, 400); w.schedule(a, 350);          // reschedule replaces
    assert.equal(w.size, 1);
    assert.equal(w.deadlineOf(a), 350);
    assert.deepEqual(collect(w, 349), []);
    assert.deepEqual(collect(w, 351), ['a']);
    w.schedule(b, Infinity);
    assert.equal(w.isScheduled(b), false);
});

test('deadlines beyond one revolution and catch-up after a pause', () => {
    const w = new TimerWheel({ slotMs: 10, slots: 8, startAt: 0 });   // 80 ms per revolution
    const far = { name: 'far' }, near = { name: 'near' };
    w.schedule(far, 1003); w.schedule(near, 45);
    const fired = [];
    for (let t = 0; t <= 1100; t += 10) w.advance(t, (e) => fired.push([e.name, t]));
    assert.deepEqual(fired, [['near', 50], ['far', 1010]]);
    // A long pause: everything due fires on the next advance.
    for (let i = 0; i < 20; i++) w.schedule({ name: `x${i}` }, 2000 + i * 37);
    assert.equal(collect(w, 10_000).length, 20);
    assert.equal(w.size, 0);
});

test('a deadline in the past fires on the next advance; callbacks may reschedule without looping', () => {
    const w = new TimerWheel({ slotMs: 10, slots: 16, startAt: 1000 });
    const a = { name: 'a' }, b = { name: 'b' }, c = { name: 'c' };
    w.schedule(a, 5);                                 // long past
    assert.deepEqual(collect(w, 1000), ['a']);
    w.schedule(a, 1010); w.schedule(b, 1010); w.schedule(c, 1010);
    // The first one fired (order within a slot is unspecified) reschedules itself in the past and
    // cancels the two others while they are due.
    const fired = [];
    w.advance(1010, (e) => {
        fired.push(e.name);
        w.schedule(e, 1000);
        for (const x of [a, b, c]) if (x !== e) w.cancel(x);
    });
    assert.equal(fired.length, 1);
    assert.equal(w.size, 1);
    assert.deepEqual(collect(w, 1011), fired);
});

test('random operations agree with a naive model', () => {
    let seed = 12345;
    const rnd = (n) => { seed = (Math.imul(seed, 1103515245) + 12345) >>> 0; return seed % n; };
    const w = new TimerWheel({ slotMs: 10, slots: 256, startAt: 0 });
    const entries = Array.from({ length: 3000 }, (_, i) => ({ name: i }));
    const model = new Map();
    let t = 0;
    for (let step = 0; step < 4000; step++) {
        const op = rnd(10);
        const e = entries[rnd(entries.length)];
        if (op < 5) {
            const d = t + rnd(20000);
            w.schedule(e, d); model.set(e.name, d);
        } else if (op < 6) {
            w.cancel(e); model.delete(e.name);
        } else {
            t += rnd(50);
            const got = [];
            w.advance(t, (x) => got.push(x.name));
            const want = [...model].filter(([, d]) => d <= t).map(([n]) => n);
            for (const n of want) model.delete(n);
            assert.deepEqual(got.sort((x, y) => x - y), want.sort((x, y) => x - y));
        }
        assert.equal(w.size, model.size);
    }
});
