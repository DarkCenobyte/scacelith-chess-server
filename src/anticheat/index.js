// Anti-cheat: anomaly classification and recording, automatic sanctions of certain protocol
// cheats, and the supervisor of the engine-analysis process.
//
//   const ac = createAnticheat({ config, store, primary, log, writer });
//   ac.recordAnomaly({ userId, gameId, kind, detail, posMatched }) -> { severity, certain }
//   ac.sanctionCertain({ userId, gameId, kind }) -> { banUntil, applied, refunds } (a Promise of it with a writer)
//   ac.classify(kind, ctx) -> { severity, certain }
//   ac.flush() -> rows written (handed to the writer) ; ac.pendingCount / ac.pendingSignalCount (buffered rows / of them not info)
//   startAnalysisProcess(config) -> handle { enabled, pid, restarts, stop() }   (primary only)
//
// Writes of a shard: with `writer` (the shard's store writer thread, src/store/writer.js) the
// anomaly rows and the automatic sanctions (sanction.js, with the rating refunds of refunds.js)
// are written by that thread, never on the event loop. The thread handles its messages in order,
// so the anomalies flushed right before a commit of finished games (GameHost) are written before
// it (DESIGN 6.5: the commit's analysis queue policy sees them), and a certain anomaly before the
// ban it causes. Without a writer (tools, tests) they are written on the caller's connection.
//
// Deviations from / precisions on docs/DESIGN.md (the contracts left these open):
//   * store.integrity.populationStats(prefix) / updatePopulation(observations, now): statistics are
//     kept per '<profile>|<category>|<ratingBucket>|<metric>' (DESIGN only says "ratingBucket";
//     they must also be per time control, and per analysis profile: games analysed by another
//     engine or at other depths are not comparable). populationStats('<profile>|<category>|
//     <ratingBucket>') returns { <metric>: { n, mean, m2 } }; the analysis process (the only
//     writer) sends one { key, value } observation per metric and game, which the store merges
//     (Welford).
//   * store.analysis.forUser(userId, limit) must return that player's completed analyses, newest
//     first, each row carrying the `features` object given to complete() (or being it).
//   * store.reports.forReporter(reporterId) (not in DESIGN) is used when present to weigh a
//     reporter by the outcomes of their past reports; without it every reporter has a neutral
//     track record. store.reports.forReported(userId) must return rows with { weight, at }.
//   * store.analysis.request(gameId, 'report' | 'signal', now) is used when present to queue a
//     reported game for analysis ahead of the ordinary ones ('signal' for a low-credibility report;
//     reports.js; the queue policy is the store's). store.analysis.next() returns each job with its
//     priority: only jobs of the ordinary random sample (0) feed the population statistics.
//   * Free-form values (anomaly detail, integrity evidence, analysis features, population stats)
//     are passed as objects; if the store refuses to bind an object (TypeError), the write is
//     retried with JSON text, and every reader accepts both (util.js).
//   * Review priority is not stored (no column in DESIGN): bin/admin.js computes it when listing,
//     from the integrity level/score and the capped report weights (reports.js).
//   * The report route is registered at the full path '/api/v1/reports' and validates its body
//     itself (the router's schema format is not specified).
//   * A 'repeated_desync' anomaly is reported by the caller (the room counts desyncs); a plain
//     'desync' stays info whatever its number.
//   * startAnalysisProcess forks bin/analysis-worker.js, which loads its configuration itself
//     (same environment, same .env) and opens its own store.
//   * store.refunds (rating refunds, refunds.js) is used when present: a partial store gives none.
//   * 'sanction.applied' carries `refunds` (the number of victims refunded): the primary then
//     looks for the refunds to notify at once (refund-notices.js).

import { fork } from 'node:child_process';
import os from 'node:os';
import { fileURLToPath } from 'node:url';
import { logger as rootLogger } from '../log.js';
import { metrics } from '../metrics.js';
import { applyCertainSanction } from './sanction.js';
import { writeStructured, HOUR_MS } from './util.js';

/** Severity of every anomaly kind (DESIGN 6.5). */
export const ANOMALY_KINDS = Object.freeze({
    malformed: 'suspicious',
    forged_type: 'certain',
    bad_seq: 'suspicious',
    flood: 'suspicious',
    foreign_game: 'certain',
    out_of_turn: 'certain',
    illegal_move: 'certain',
    repeated_desync: 'suspicious',
    clock_implausible: 'suspicious',
    stale_ply: 'info',
    desync: 'info',
    nothing_to_claim: 'info',
});

// These are only certain when the client provably knew the position (its posHash matched).
const NEEDS_SYNC = new Set(['out_of_turn', 'illegal_move']);

/**
 * Classifies an anomaly. Unknown kinds are 'info' (a caller bug must not sanction anyone).
 * out_of_turn / illegal_move reported with ctx.posMatched === false are downgraded to
 * 'suspicious': without a synchronised position they can be an honest race.
 * @param {string} kind
 * @param {{ posMatched?: boolean }} [ctx]
 * @returns {{ severity: 'info'|'suspicious'|'certain', certain: boolean, known: boolean }}
 */
export function classify(kind, ctx = {}) {
    const base = Object.prototype.hasOwnProperty.call(ANOMALY_KINDS, kind) ? ANOMALY_KINDS[kind] : null;
    if (!base) return { severity: 'info', certain: false, known: false };
    let severity = base;
    if (severity === 'certain' && NEEDS_SYNC.has(kind) && ctx && ctx.posMatched === false) severity = 'suspicious';
    return { severity, certain: severity === 'certain', known: true };
}

const anomalyCounter = metrics.counter('scacelith_anticheat_anomalies_total', 'Anomalies recorded, by kind and severity', ['kind', 'severity']);
const anomalyDropped = metrics.counter('scacelith_anticheat_anomalies_dropped_total', 'Anomalies not persisted (buffer full or database error)');
const sanctionCounter = metrics.counter('scacelith_anticheat_sanctions_total', 'Automatic sanctions applied', ['kind']);

const MAX_PENDING = 5000;           // distinct buffered anomaly rows

/**
 * Creates the anti-cheat service of a process (shard or primary).
 * @param {object} o
 * @param {object} o.config     loadConfig() result
 * @param {object} o.store      Store (DESIGN 5.5): anomalies, sanctions, integrity, security
 * @param {object} [o.primary]  IPC client with request(type, payload) (shards); null in tools
 * @param {object} [o.log]
 * @param {() => number} [o.now]
 * @param {number} [o.flushMs=1000]  batching period of non-certain anomalies
 * @param {object} [o.writer]  store writer thread (insertAnomalies, sanction): the writes go through it
 */
export function createAnticheat({ config, store, primary = null, log = null, now = Date.now, flushMs = 1000, writer = null }) {
    const lg = log || rootLogger.child('anticheat');
    const pending = new Map();      // userId|gameId|kind -> row (repeats coalesced)
    let pendingSignal = 0;          // buffered rows that are not info (suspicious)
    let flushTimer = null;
    const sanctioned = new Map();   // userId|gameId -> ban end (idempotency within a game)

    function scheduleFlush() {
        if (flushTimer) return;
        flushTimer = setTimeout(() => { flushTimer = null; flush(); }, flushMs);
        flushTimer.unref?.();
    }

    // Throws when written here; through the writer, a failure is counted and logged when it answers.
    function insertRows(rows, what) {
        if (!writer) {
            writeStructured((r) => store.anomalies.insertBatch(r), rows, ['detail']);
            return;
        }
        writer.insertAnomalies(rows).catch((e) => {
            anomalyDropped.inc(rows.length);
            lg.error(what, { err: e, rows: rows.length });
        });
    }

    /**
     * Writes the buffered anomalies now (one insertBatch, handed to the writer thread when there
     * is one). Returns the number of rows written or handed over.
     */
    function flush() {
        if (flushTimer) { clearTimeout(flushTimer); flushTimer = null; }
        if (!pending.size) return 0;
        const rows = [];
        for (const r of pending.values()) {
            const detail = r.count > 1 ? { ...(r.detail || {}), count: r.count, lastAt: r.lastAt } : r.detail;
            rows.push({ userId: r.userId, gameId: r.gameId, kind: r.kind, severity: r.severity, detail, at: r.at });
        }
        pending.clear();
        pendingSignal = 0;
        try {
            insertRows(rows, 'anomaly batch lost');
            return rows.length;
        } catch (e) {
            anomalyDropped.inc(rows.length);
            lg.error('anomaly batch lost', { err: e, rows: rows.length });
            return 0;
        }
    }

    /**
     * Records an anomaly: metrics, security log, and the database (certain ones immediately, the
     * others batched every flushMs, repeats of the same kind in the same game coalesced).
     * @param {{ userId: number, gameId?: number, kind: string, detail?: *, posMatched?: boolean }} a
     * @returns {{ severity: string, certain: boolean }}
     */
    function recordAnomaly({ userId, gameId = 0, kind, detail = null, posMatched }) {
        const c = classify(kind, { posMatched });
        const label = c.known ? kind : 'unknown';
        anomalyCounter.labels(label, c.severity).inc();
        const at = now();
        let d = detail;
        if (d !== null && d !== undefined && typeof d !== 'object') d = { info: String(d) };
        if (posMatched !== undefined) d = { ...(d || {}), posMatched: !!posMatched };
        if (!c.known) d = { ...(d || {}), reportedKind: String(kind).slice(0, 40) };
        const row = { userId, gameId: gameId || 0, kind: label, severity: c.severity, detail: d, at };
        const key = `${userId}|${row.gameId}|${label}`;
        // Repeats within a batching period are counted, not logged again (no log flooding).
        if (c.severity === 'info') lg.debug('anomaly', { userId, gameId, kind: label, severity: c.severity });
        else if (c.certain || !pending.has(key)) lg.security('anomaly', { userId, gameId, kind: label, severity: c.severity, detail: d });

        if (c.certain) {
            try { insertRows([row], 'certain anomaly not persisted'); } catch (e) {
                anomalyDropped.inc();
                lg.error('certain anomaly not persisted', { err: e, userId, kind });
            }
            return { severity: c.severity, certain: true };
        }
        const prev = pending.get(key);
        if (prev) { prev.count++; prev.lastAt = at; }
        else if (pending.size >= MAX_PENDING && c.severity === 'info') anomalyDropped.inc();
        else if (pending.size >= 2 * MAX_PENDING) anomalyDropped.inc();
        else {
            pending.set(key, { ...row, count: 1, lastAt: at });
            if (c.severity !== 'info') pendingSignal++;
        }
        scheduleFlush();
        return { severity: c.severity, certain: false };
    }

    /**
     * Automatic sanction of a certain cheat (sanction.js): ban BAN_DURATION_HOURS (source 'auto'),
     * integrity level 'confirmed' with the evidence appended, security event, the rating refunds
     * of the player's victims, and 'sanction.applied' to the primary (which kicks the player
     * everywhere and notifies the refunded victims). Idempotent within a game: several certain
     * anomalies of one game make one ban. Does nothing when AUTO_SANCTION_CERTAIN_CHEATS is off.
     * With a writer the database work runs on its thread and this returns a Promise.
     * @param {{ userId: number, gameId?: number, kind: string }} s
     * @returns {{ banUntil: number, applied: boolean, refunds: number }|Promise<object>}
     */
    function sanctionCertain({ userId, gameId = 0, kind }) {
        if (!config.autoSanctionCertainCheats) return { banUntil: 0, applied: false, refunds: 0 };
        const t = now();
        const key = `${userId}|${gameId || 0}`;
        const seen = sanctioned.get(key);
        if (seen && seen > t) return { banUntil: seen, applied: false, refunds: 0 };
        if (sanctioned.size > 10000) for (const [k, v] of sanctioned) if (v <= t) sanctioned.delete(k);
        // Taken at once (the thread answers later): the next certain anomaly of this game is a repeat.
        sanctioned.set(key, t + config.banDurationHours * HOUR_MS);
        const s = { userId, gameId: gameId || 0, kind, at: t };

        const done = (r) => {
            sanctioned.set(key, r.until);
            if (r.created) {
                sanctionCounter.labels(kind in ANOMALY_KINDS ? kind : 'unknown').inc();
                lg.security('sanction.auto', { userId, gameId, kind, until: r.until, refunds: r.refunds.length });
                if (primary?.request) {
                    Promise.resolve()
                        .then(() => primary.request('sanction.applied', { userId, until: r.until, reason: `certain_cheat:${kind}`, refunds: r.refunds.length }))
                        .catch((e) => lg.warn('sanction.applied not delivered', { err: e, userId }));
                }
            }
            return { banUntil: r.until, applied: r.created, refunds: r.refunds.length };
        };
        if (!writer) return done(applyCertainSanction(store, config, s, lg));
        return writer.sanction(s).then(done, (e) => {
            // The thread died before answering: a later certain anomaly of this game tries again.
            sanctioned.delete(key);
            lg.error('automatic ban not stored', { err: e, userId, kind });
            return { banUntil: 0, applied: false, refunds: 0 };
        });
    }

    /** Flushes and stops the batching timer. */
    function close() {
        flush();
        if (flushTimer) { clearTimeout(flushTimer); flushTimer = null; }
    }

    return {
        classify,
        recordAnomaly,
        sanctionCertain,
        flush,
        close,
        get pendingCount() { return pending.size; },
        /** Buffered rows that are not info: the host writes them before a commit (analysis queue policy). */
        get pendingSignalCount() { return pendingSignal; },
        startAnalysisProcess: (cfg = config, opts = {}) => startAnalysisProcess(cfg, { log: lg, ...opts }),
    };
}

/**
 * Starts the engine-analysis process (bin/analysis-worker.js) as a low-priority child of the
 * primary, restarting it with exponential backoff when it dies. Disabled (no process) when
 * ANALYSIS_ENGINE_PATH is empty or ANALYSIS_WORKERS is 0.
 * @param {object} config
 * @param {{ log?: object, script?: string, env?: object, minBackoffMs?: number, maxBackoffMs?: number, stableMs?: number }} [o]
 * @returns {{ enabled: boolean, readonly pid: number|null, readonly restarts: number, stop: (graceMs?: number) => Promise<void> }}
 */
export function startAnalysisProcess(config, { log = null, script = null, env = {}, minBackoffMs = 1000, maxBackoffMs = 60000, stableMs = 60000 } = {}) {
    const lg = log || rootLogger.child('anticheat');
    if (!config.analysisEnginePath || !config.analysisWorkers) {
        lg.info('engine analysis disabled', { reason: !config.analysisEnginePath ? 'ANALYSIS_ENGINE_PATH empty' : 'ANALYSIS_WORKERS=0' });
        return { enabled: false, pid: null, restarts: 0, stop: async () => {} };
    }
    const file = script || fileURLToPath(new URL('../../bin/analysis-worker.js', import.meta.url));
    let child = null, timer = null, stopped = false, restarts = 0, delay = minBackoffMs;

    function launch() {
        timer = null;
        if (stopped) return;
        const startedAt = Date.now();
        try {
            child = fork(file, [], { stdio: ['ignore', 'inherit', 'inherit', 'ipc'], env: { ...process.env, ...env }, execArgv: [] });
        } catch (e) {
            lg.error('cannot start the analysis process', { err: e });
            return retry(startedAt);
        }
        try { os.setPriority(child.pid, os.constants.priority.PRIORITY_LOW); } catch { /* the worker lowers itself too */ }
        lg.info('analysis process started', { pid: child.pid });
        const me = child;
        me.on('error', (e) => lg.warn('analysis process error', { err: e }));
        me.on('exit', (code, signal) => {
            if (child === me) child = null;
            if (stopped) return;
            lg.warn('analysis process exited', { code, signal });
            retry(startedAt);
        });
    }

    function retry(startedAt) {
        if (stopped) return;
        if (Date.now() - startedAt >= stableMs) delay = minBackoffMs;
        restarts++;
        timer = setTimeout(launch, delay);
        timer.unref?.();
        delay = Math.min(maxBackoffMs, delay * 2);
    }

    launch();
    return {
        enabled: true,
        get pid() { return child?.pid ?? null; },
        get restarts() { return restarts; },
        async stop(graceMs = 5000) {
            stopped = true;
            if (timer) { clearTimeout(timer); timer = null; }
            const c = child;
            if (!c || c.exitCode !== null) return;
            await new Promise((resolve) => {
                const kill = setTimeout(() => { try { c.kill('SIGKILL'); } catch { /* gone */ } resolve(); }, graceMs);
                c.once('exit', () => { clearTimeout(kill); resolve(); });
                try { c.send({ type: 'shutdown' }); } catch { try { c.kill('SIGTERM'); } catch { /* gone */ } }
            });
        },
    };
}
