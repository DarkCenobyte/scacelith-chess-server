// Store writer thread of a shard: runs store.games.finishBatch (the finished-games transaction,
// synchronous=FULL, which may also wait for another process's write lock) on a worker thread, so
// the shard's event loop, and every game it hosts, never waits for SQLite. The thread opens its own
// connection to the database file (WAL). GameHost takes the Promise it returns like any
// asynchronous store; the games stay in the journal until their commit is confirmed.
// The anti-cheat's writes go through it too (anticheat/index.js): the anomaly rows, and the
// automatic sanction of a certain cheat with its rating refunds (anticheat/sanction.js). The
// thread handles one message at a time, in the order they were sent: anomalies handed over
// before a commit are written before it.
//
//   const writer = startStoreWriter({ config, shard });   // the shard's configuration
//   const results = await writer.finishBatch(records);   // same results / StoreError codes
//   await writer.insertAnomalies(rows);                   // store.anomalies.insertBatch
//   const r = await writer.sanction({ userId, gameId, kind, at });   // applyCertainSanction
//   await writer.close();
//
// The store's commit metrics (scacelith_store_commit_batch_ms, scacelith_store_games_committed_total,
// scacelith_store_busy_total, scacelith_anticheat_analysis_skipped_total) are counted in the
// shard's own registry from the thread's answers.

import { Worker, isMainThread, parentPort, workerData } from 'node:worker_threads';
import { fileURLToPath } from 'node:url';
import { metrics } from '../metrics.js';

if (!isMainThread && parentPort && workerData && workerData.scacelithStoreWriter) {
    const { configureLogging, logger } = await import('../log.js');
    const { openStore } = await import('./index.js');
    const { applyGame } = await import('../match/elo.js');
    const { applyCertainSanction } = await import('../anticheat/sanction.js');
    const config = workerData.config;
    if (!workerData.logging) configureLogging({ level: 'error' });
    else configureLogging({ level: config.logLevel, format: config.logFormat, ipMode: config.logIp, secret: config.serverSecret,
        base: { inst: config.instanceId, shard: workerData.shard, thread: 'store-writer' } });
    const store = openStore(config, { applyGame, log: logger.child('store') });
    const anticheatLog = logger.child('anticheat');
    const ops = {
        finishBatch: (records) => store.games.finishBatch(records),
        anomalies: (rows) => store.anomalies.insertBatch(rows),
        sanction: (s) => applyCertainSanction(store, config, s, anticheatLog),
    };
    parentPort.on('message', (msg) => {
        if (msg && msg.close) {
            try { store.close(); } catch (e) { logger.child('store').error('store close failed', { err: e }); }
            parentPort.postMessage({ closed: true });
            return;
        }
        const { id, op, arg } = msg;
        const t0 = performance.now();
        try {
            const result = ops[op](arg);
            parentPort.postMessage({ id, result, ms: performance.now() - t0 });
        } catch (e) {
            parentPort.postMessage({ id, error: { message: e.message, code: e.code, gameId: e.gameId } });
        }
    });
}

/**
 * Starts the writer thread.
 * @param {{ config: object, shard?: number, logging?: boolean, log?: object }} o config: the shard's
 *        (loadConfig, structured-cloned to the thread); logging: false keeps the thread's logs off
 *        (tests); log: where thread failures are reported
 * @returns {{ finishBatch(records: object[]): Promise<object[]>, insertAnomalies(rows: object[]): Promise<number>,
 *            sanction(s: object): Promise<object>, close(): Promise<void> }}
 */
export function startStoreWriter({ config, shard = 0, logging = true, log = null }) {
    const mBatchMs = metrics.histogram('scacelith_store_commit_batch_ms', 'Duration of one finished-games commit transaction',
        [1, 2, 5, 10, 25, 50, 100, 250, 1000]);
    const mGames = metrics.counter('scacelith_store_games_committed_total', 'Finished games written to the database');
    const mBusy = metrics.counter('scacelith_store_busy_total', 'Store operations that gave up waiting for the database lock');
    const mSkipped = metrics.counter('scacelith_anticheat_analysis_skipped_total',
        'Finished rated games not queued for engine analysis (sample: ANALYSIS_SAMPLE_RATE, backlog: ANALYSIS_QUEUE_MAX reached, player: 20 flagged games of a player already waiting, displaced: a waiting flagged game without an anomaly of its own gave its place to a game with one)', ['reason']);
    const waiting = new Map();
    let next = 1, w = null, closing = null;

    // The thread is started on first use and again after it died (the failed batches are rejected:
    // GameHost retries them with backoff, and the games stay journaled meanwhile).
    const spawn = () => {
        const t = new Worker(fileURLToPath(import.meta.url), { workerData: { scacelithStoreWriter: true, config, shard, logging } });
        const fail = (e) => {
            if (w !== t) return;
            w = null;
            for (const p of waiting.values()) p.reject(e);
            waiting.clear();
            if (closing) closing();
            else log?.error?.('store writer thread failed; restarted on the next commit', { err: e });
        };
        t.on('message', (msg) => {
            if (msg && msg.closed) { if (closing) closing(); return; }
            const { id, result, error, ms } = msg;
            const p = waiting.get(id);
            if (!p) return;
            waiting.delete(id);
            if (error) {
                if (error.code === 'busy') mBusy.inc();
                p.reject(Object.assign(new Error(error.message), { code: error.code, gameId: error.gameId }));
                return;
            }
            if (!p.commit) { p.resolve(result); return; }
            mBatchMs.observe(ms);
            mGames.inc(Array.isArray(result) ? result.reduce((n, x) => n + (x && x.duplicate ? 0 : 1), 0) : 0);
            if (Array.isArray(result)) {
                for (const x of result) {
                    if (x && x.analysisSkipped) mSkipped.labels(x.analysisSkipped).inc();
                    if (x && x.analysisDisplaced) mSkipped.labels('displaced').inc(x.analysisDisplaced.length);
                }
            }
            p.resolve(result);
        });
        t.on('error', fail);
        t.on('exit', (code) => fail(new Error(`store writer thread exited (${code})`)));
        return t;
    };

    const send = (op, arg) => {
        if (closing) return Promise.reject(new Error('store writer closed'));
        if (!w) w = spawn();
        const id = next++;
        const t = w;
        return new Promise((resolve, reject) => {
            waiting.set(id, { resolve, reject, commit: op === 'finishBatch' });
            // A value that cannot be cloned to the thread (an anomaly detail holding a function)
            // fails this message only.
            try { t.postMessage({ id, op, arg }); } catch (e) { waiting.delete(id); reject(e); }
        });
    };

    return {
        finishBatch: (records) => send('finishBatch', records),
        insertAnomalies: (rows) => send('anomalies', rows),
        sanction: (s) => send('sanction', s),
        // Closes the thread's database connection after the batches already sent, then the thread.
        close() {
            const t = w;
            if (!t) { closing = () => {}; return Promise.resolve(); }
            return new Promise((resolve) => {
                const done = () => { clearTimeout(timer); closing = () => {}; w = null; t.terminate().finally(resolve); };
                const timer = setTimeout(done, 5000);
                closing = done;
                t.postMessage({ close: true });
            });
        },
    };
}
