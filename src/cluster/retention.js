// Retention purge of the primary (docs/DESIGN.md section 7): expired and revoked sessions,
// expired tokens, old security events, anomalies, conduct events and failed analysis jobs are
// deleted, and stored IP addresses erased, by store.retention.runAsync every
// RETENTION_INTERVAL_MS, the first time about a minute after the start.
//
// The primary is the control plane (presence, matchmaking), so the purge must never hold its
// event loop, nor the database's write lock, for long: runAsync adapts the rows per statement (one
// short transaction, at most 1000 rows) so that a statement takes about sliceMs / 2, and pauses
// for sliceMs whenever `sliceMs` of work has been done. Runs never overlap:
// the next one is scheduled when the previous one ends. stop() aborts a run between two
// statements and waits for it, so the store can be closed right after (nothing runs after
// store.close()). Each run is logged at info level with its counts only (no personal data) and
// counted in the metrics.
//
//   const retention = startRetention({ config, store, log });
//   await retention.runNow();     // tests, maintenance
//   await retention.stop();       // before store.close()

import { metrics as defaultRegistry } from '../metrics.js';

const FIRST_RUN_DELAY_MS = 60000;
// Longest delay of a Node.js timer: a longer one fires after 1 ms (TimeoutOverflowWarning), which
// would run the purge in a loop. RETENTION_INTERVAL_MS is bounded by the configuration as well.
const MAX_TIMER_MS = 2147483647;

// store.retention counts -> metric label of the rows deleted.
const PURGED_KINDS = Object.freeze({
    sessions: 'sessions', tokens: 'tokens', securityEvents: 'security_events', anomalies: 'anomalies',
    conductEvents: 'conduct_events', analysisJobs: 'analysis_jobs',
});

// Log fields of a run: its counts, with `tokens` renamed (the logger redacts any field whose name
// looks like a credential, and a count of deleted single-use tokens is not one).
function logFields(counts) {
    if (!counts) return {};
    const { tokens, ...rest } = counts;
    return { ...rest, singleUse: tokens };
}

/**
 * Starts the periodic purge.
 * @param {object} o
 * @param {object} o.config           RETENTION_* keys (retentionIntervalMs, retentionIpDays, retentionSecurityDays)
 * @param {object} o.store            Store with retention.runAsync(now, cfg, { sliceMs, signal })
 * @param {object} [o.log]
 * @param {object} [o.registry]       metrics registry
 * @param {() => number} [o.now]
 * @param {{ setTimeout: Function, clearTimeout: Function }} [o.timers]
 * @param {number} [o.firstDelayMs]   delay of the first run
 * @param {number} [o.sliceMs]        work done before a pause of the same length
 * @returns {{ runNow: () => Promise<object|null>, stop: () => Promise<void>, readonly running: boolean }}
 */
export function startRetention({ config, store, log = null, registry = defaultRegistry, now = Date.now,
    timers = { setTimeout, clearTimeout }, firstDelayMs = FIRST_RUN_DELAY_MS, sliceMs = 10 }) {
    const intervalMs = Math.min(MAX_TIMER_MS, config.retentionIntervalMs ?? 3600000);
    const purged = registry.counter('scacelith_retention_purged_total', 'Rows deleted by the retention purge', ['kind']);
    const ipErased = registry.counter('scacelith_retention_ip_erased_total', 'Stored IP addresses erased by the retention purge');
    const runs = registry.counter('scacelith_retention_runs_total', 'Retention purge runs, by result', ['result']);
    const seconds = registry.histogram('scacelith_retention_run_seconds', 'Duration of one retention purge (pauses included)',
        [0.1, 1, 10, 60, 600, 3600]);
    let timer = null;
    let running = null;         // Promise of the run in progress
    let controller = null;      // its AbortController
    let stopped = false;

    function count(counts) {
        if (!counts) return;
        for (const [key, kind] of Object.entries(PURGED_KINDS)) if (counts[key] > 0) purged.labels(kind).inc(counts[key]);
        if (counts.ipErased > 0) ipErased.inc(counts.ipErased);
    }

    // One run; never rejects (a failure is logged and the next run happens at the next interval).
    async function runOnce() {
        const ctl = controller = new AbortController();
        const t0 = performance.now();
        try {
            const counts = await store.retention.runAsync(now(), config, { sliceMs, signal: ctl.signal });
            count(counts);
            const ms = Math.round(performance.now() - t0);
            if (ctl.signal.aborted) {
                runs.labels('aborted').inc();
                log?.info?.('retention purge interrupted by the shutdown', { ...logFields(counts), ms });
            } else {
                runs.labels('ok').inc();
                log?.info?.('retention purge done', { ...logFields(counts), ms });
            }
            return counts;
        } catch (e) {
            count(e?.counts);
            runs.labels('failed').inc();
            // A busy database (another process held the write lock too long) is retried next time.
            const level = e?.code === 'busy' ? 'warn' : 'error';
            log?.[level]?.('retention purge failed; retried at the next interval', { err: e, ...logFields(e?.counts) });
            return null;
        } finally {
            seconds.observe((performance.now() - t0) / 1000);
            if (controller === ctl) controller = null;
        }
    }

    function start() {
        const p = runOnce().finally(() => { if (running === p) running = null; });
        running = p;
        return p;
    }

    function schedule(delayMs) {
        if (stopped) return;
        timer = timers.setTimeout(tick, delayMs);
        timer?.unref?.();
    }

    function tick() {
        timer = null;
        if (stopped) return;
        (running || start()).then(() => schedule(intervalMs));
    }

    schedule(firstDelayMs);

    return {
        get running() { return running !== null; },
        /** Runs a purge now (after the one in progress, if any); resolves to its counts, null after a failure or stop(). */
        async runNow() {
            while (running) await running;
            if (stopped) return null;
            return start();
        },
        /** Cancels the next run and aborts the current one between two statements; resolves once it has ended. */
        async stop() {
            stopped = true;
            if (timer) { timers.clearTimeout(timer); timer = null; }
            controller?.abort();
            while (running) await running;
        },
    };
}
