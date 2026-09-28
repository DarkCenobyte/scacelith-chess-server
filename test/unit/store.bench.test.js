// Store and journal micro-benchmarks. Skipped unless STORE_BENCH=1:
//   STORE_BENCH=1 node --test test/unit/store.bench.test.js
// STORE_BENCH_DIR chooses the directory (default: the OS temp dir; use a real disk, not tmpfs,
// for meaningful fsync numbers). Results are printed as test diagnostics.

import { test } from 'node:test';
import assert from 'node:assert/strict';
import crypto from 'node:crypto';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { openStore, migrate } from '../../src/store/index.js';
import { openJournal } from '../../src/store/journal.js';
import { testConfig } from '../../src/config.js';

const skip = !process.env.STORE_BENCH;
const base = process.env.STORE_BENCH_DIR || os.tmpdir();

function applyGame(white, black, score) {
    const d = Math.round(20 * (score - 0.5));
    const side = (r, delta, s) => ({
        before: r.rating, after: r.rating + delta,
        record: { rating: r.rating + delta, games: r.games + 1, wins: r.wins + (s === 1 ? 1 : 0), draws: r.draws + (s === 0.5 ? 1 : 0),
            losses: r.losses + (s === 0 ? 1 : 0), peak: Math.max(r.peak, r.rating + delta), reachedSenior: false },
    });
    return { white: side(white, d, score), black: side(black, -d, 1 - score) };
}

function pct(sorted, q) { return sorted[Math.min(sorted.length - 1, Math.floor(q * sorted.length))]; }

test('bench: finishBatch throughput (file DB, WAL, synchronous=FULL)', { skip }, (t) => {
    const dir = fs.mkdtempSync(path.join(base, 'scacelith-bench-'));
    const store = openStore(testConfig({ DB_PATH: path.join(dir, 'bench.db') }), { applyGame });
    migrate(store);
    const users = [];
    store.transaction(() => { for (let i = 0; i < 2000; i++) users.push(store.users.create({ username: `u${i}`, email: `u${i}@e.org` })); });
    let id = 5_000_000_000_000;
    const plies = 80;
    const moves = new Uint16Array(plies).map((_, i) => i * 3);
    const spent = new Uint32Array(plies).fill(2500);
    const clocks = new Uint32Array(plies).fill(150000);
    const rec = () => {
        const w = users[(Math.random() * users.length) | 0];
        let b = users[(Math.random() * users.length) | 0];
        if (b === w) b = users[(users.indexOf(w) + 1) % users.length];
        return { id: ++id, category: '3+2', rated: true, baseMs: 180000, incMs: 2000, whiteId: w, blackId: b, whiteName: 'w', blackName: 'b',
            whiteRating: 1500, blackRating: 1500, startedAt: Date.now() - 600000, endedAt: Date.now(), status: 1 + (id % 3), reason: 1,
            moves, spentMs: spent, clockMs: clocks, rematchOf: 0, flags: 0 };
    };
    for (const [size, total] of [[1, 400], [16, 3200], [64, 6400]]) {
        const lat = [];
        const t0 = performance.now();
        for (let n = 0; n < total; n += size) {
            const batch = [];
            for (let k = 0; k < size; k++) batch.push(rec());
            const s = performance.now();
            const out = store.games.finishBatch(batch);
            lat.push(performance.now() - s);
            assert.equal(out.length, size);
        }
        const secs = (performance.now() - t0) / 1000;
        lat.sort((a, b) => a - b);
        t.diagnostic(`finishBatch batch=${size}: ${Math.round(total / secs)} games/s, commit p50 ${pct(lat, 0.5).toFixed(2)} ms, p99 ${pct(lat, 0.99).toFixed(2)} ms`);
    }
    store.close();
    fs.rmSync(dir, { recursive: true, force: true });
});

test('bench: session lookup latency', { skip }, (t) => {
    const dir = fs.mkdtempSync(path.join(base, 'scacelith-bench-'));
    const store = openStore(testConfig({ DB_PATH: path.join(dir, 'bench.db') }));
    migrate(store);
    const uid = store.users.create({ username: 'sess', email: 's@e.org' });
    const hashes = [];
    const now = Date.now();
    store.transaction(() => {
        for (let i = 0; i < 20000; i++) {
            const h = crypto.createHash('sha256').update(`t${i}`).digest();
            hashes.push(h);
            store.sessions.create({ userId: uid, tokenHash: h, expiresAt: now + 1e9, idleExpiresAt: now + 1e9, ip: '192.0.2.1' });
        }
    });
    const N = 200000;
    for (let i = 0; i < 1000; i++) store.sessions.byTokenHash(hashes[i]);
    const t0 = performance.now();
    for (let i = 0; i < N; i++) assert.ok(store.sessions.byTokenHash(hashes[(i * 7919) % hashes.length]));
    const us = ((performance.now() - t0) * 1000) / N;
    const tt = performance.now();
    for (let i = 0; i < 2000; i++) store.sessions.touch(1 + (i % 20000), now + i, now + 1e9);
    const touchUs = ((performance.now() - tt) * 1000) / 2000;
    t.diagnostic(`sessions.byTokenHash: ${us.toFixed(2)} us/lookup (${Math.round(1e6 / us)} lookups/s, 20k sessions); touch (autocommit, fsync): ${touchUs.toFixed(0)} us`);
    store.close();
    fs.rmSync(dir, { recursive: true, force: true });
});

test('bench: journal appends and flush latency', { skip }, async (t) => {
    for (const fsync of [false, true]) {
        const dir = fs.mkdtempSync(path.join(base, 'scacelith-bench-'));
        const j = await openJournal({ dir, shard: 0, flushMs: 50, fsync });
        const payload = Buffer.alloc(16, 7);          // a typical move record payload
        // Raw append cost (no flush in the timed loop).
        const N = 1_000_000;
        let t0 = performance.now();
        for (let i = 0; i < N; i++) j.append(2, 1000 + (i & 1023), payload, i);
        const appendNs = ((performance.now() - t0) * 1e6) / N;
        await j.flush();
        // Flush latency of realistic batches (1000 records between flushes).
        const lat = [];
        for (let round = 0; round < 200; round++) {
            for (let i = 0; i < 1000; i++) j.append(2, 1000 + (i & 1023), payload, i);
            t0 = performance.now();
            await j.flush();
            lat.push(performance.now() - t0);
        }
        lat.sort((a, b) => a - b);
        // Sustained throughput with group commit (flush after every 100 appends, awaited).
        const M = 200000;
        t0 = performance.now();
        const waits = [];
        for (let i = 0; i < M; i++) {
            j.append(2, 1000 + (i & 1023), payload, i);
            if (i % 100 === 99) waits.push(j.flush());
        }
        await Promise.all(waits);
        await j.flush();
        const sustained = M / ((performance.now() - t0) / 1000);
        t.diagnostic(`journal fsync=${fsync}: append ${appendNs.toFixed(0)} ns (${Math.round(1e9 / appendNs)} appends/s), `
            + `flush of 1000 records p50 ${pct(lat, 0.5).toFixed(2)} ms p99 ${pct(lat, 0.99).toFixed(2)} ms, `
            + `sustained ${Math.round(sustained)} appends/s acknowledged by flush()`);
        await j.close();
        fs.rmSync(dir, { recursive: true, force: true });
    }
});
