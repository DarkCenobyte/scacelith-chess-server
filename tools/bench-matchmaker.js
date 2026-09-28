#!/usr/bin/env node
// Matchmaker benchmark: time of one tick with very large queues.
//
//   node tools/bench-matchmaker.js [players=100000] [runs=5]
//
// Scenarios:
//   burst-all     `players` join at once, spread over every official category, rated and casual
//                 (22 queues with the default categories), normal-ish ratings, 20% provisional;
//                 one tick pairs almost everyone (e.g. every client re-queues after a restart).
//   burst-one     the same crowd in a single queue.
//   churn         `players` arrivals per second (players / 4 per 250 ms tick) over 20 ticks, all
//                 queues: steady-state cost (the queues stay short: newcomers pair at once).
//   idle          `players` waiting, none can be paired (every window 0, distinct ratings): the
//                 fixed per-player cost of a tick.
// Prints the median and worst time per tick over `runs` runs (fresh matchmaker for each run).

import { testConfig } from '../src/config.js';
import { Matchmaker } from '../src/match/matchmaker.js';

const N = Number(process.argv[2] || 100000);
const RUNS = Number(process.argv[3] || 5);

function lcg(seed) {
    let s = seed >>> 0;
    return () => { s = (Math.imul(s, 1664525) + 1013904223) >>> 0; return s / 4294967296; };
}

function normalRating(rnd) {
    return Math.max(100, Math.min(3200, Math.round(1500 + (rnd() + rnd() + rnd() + rnd() - 2) * 600)));
}

function median(a) {
    const s = [...a].sort((x, y) => x - y);
    return s[Math.floor(s.length / 2)];
}

function time(fn) {
    const t0 = performance.now();
    const r = fn();
    return [performance.now() - t0, r];
}

const cfg = testConfig();
const cats = cfg.categories.map((c) => c.id);

function burst(single, seed) {
    const rnd = lcg(seed);
    const m = new Matchmaker({ config: cfg, now: () => 0, random: rnd });
    const [joinMs] = time(() => {
        for (let i = 1; i <= N; i++) {
            m.join({ userId: i, username: 'u' + i, category: single ? '3+2' : cats[i % cats.length],
                rated: single ? true : (i >> 3) % 2 === 0, rating: normalRating(rnd), provisional: rnd() < 0.2,
                shard: i % 8, connId: i, joinedAt: Math.floor(i / 1000) });
        }
    });
    const [ms, pairs] = time(() => m.tick(1000));
    return { ms, pairs: pairs.length, left: m.size, joinUs: (joinMs * 1000) / N };
}

// Arrivals at `players` per second (a quarter of them per 250 ms tick) over 20 ticks, spread
// over every queue; each tick pairs the newcomers with each other and with those still waiting.
function churn(seed) {
    const rnd = lcg(seed);
    const m = new Matchmaker({ config: cfg, now: () => 0, random: rnd });
    const queues = [];
    for (const cat of cats) for (const rated of [true, false]) queues.push([cat, rated]);
    const perTick = Math.max(1, Math.floor(N / 4));
    const times = [];
    let id = 1, paired = 0;
    for (let t = 1; t <= 20; t++) {
        for (let k = 0; k < perTick; k++) {
            const [cat, rated] = queues[Math.floor(rnd() * queues.length)];
            m.join({ userId: id, username: 'u' + id, category: cat, rated, rating: normalRating(rnd), provisional: rnd() < 0.2,
                shard: 0, connId: id, joinedAt: t * 250 - Math.floor(rnd() * 250) });
            id++;
        }
        const [ms, pairs] = time(() => m.tick(t * 250));
        times.push(ms);
        paired += pairs.length * 2;
    }
    return { ms: median(times), worst: Math.max(...times), joinsPerTick: perTick, waitingAfter: m.size, pairedPerTick: Math.round(paired / 20) };
}

function idle(seed) {
    const rnd = lcg(seed);
    const c = testConfig({ MATCH_WINDOW_START: '0', MATCH_WINDOW_STEP: '0', MATCH_PROVISIONAL_BONUS: '0' });
    const m = new Matchmaker({ config: c, now: () => 0, random: rnd });
    const queues = [];
    for (const cat of cats) for (const rated of [true, false]) queues.push([cat, rated]);
    for (let i = 0; i < N; i++) {
        const [cat, rated] = queues[i % queues.length];
        m.join({ userId: i + 1, username: 'u' + i, category: cat, rated, rating: 100 + Math.floor(i / queues.length), provisional: false, shard: 0, connId: i + 1, joinedAt: 0 });
    }
    const times = [];
    for (let t = 1; t <= 10; t++) times.push(time(() => m.tick(t * 250))[0]);
    return { ms: median(times), worst: Math.max(...times), waiting: m.size };
}

function run(name, fn) {
    const results = [];
    for (let i = 0; i < RUNS; i++) results.push(fn(i + 1));
    const ms = results.map((r) => r.ms);
    const worst = Math.max(...results.map((r) => r.worst ?? r.ms));
    const last = results[results.length - 1];
    const extra = Object.entries(last).filter(([k]) => k !== 'ms' && k !== 'worst')
        .map(([k, v]) => `${k}=${typeof v === 'number' && !Number.isInteger(v) ? v.toFixed(2) : v}`).join(' ');
    console.log(`${name.padEnd(10)} median ${median(ms).toFixed(1).padStart(7)} ms/tick   worst ${worst.toFixed(1).padStart(7)} ms   ${extra}`);
}

console.log(`Matchmaker benchmark: ${N} players, ${RUNS} runs, node ${process.version}`);
run('burst-all', (s) => burst(false, s));
run('burst-one', (s) => burst(true, s));
run('churn', (s) => churn(s));
run('idle', (s) => idle(s));
