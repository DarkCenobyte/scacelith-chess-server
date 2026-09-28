// Anti-cheat: anomaly classification and recording, automatic sanctions of certain protocol
// cheats, and the supervisor of the engine-analysis process.
//
//   const ac = createAnticheat({ config, store, primary, log });
//   ac.recordAnomaly({ userId, gameId, kind, detail, posMatched }) -> { severity, certain }
//   ac.sanctionCertain({ userId, gameId, kind }) -> { banUntil }
//   ac.classify(kind, ctx) -> { severity, certain }
//   startAnalysisProcess(config) -> handle { enabled, pid, restarts, stop() }   (primary only)
//
// Deviations from / precisions on docs/DESIGN.md (the contracts left these open):
//   * store.integrity.populationStats(key) / updatePopulation(key, stats): key is the string
//     '<category>|<ratingBucket>' (DESIGN only says "ratingBucket"; statistics must also be per
//     time control), stats is { v: 1, metrics: { <metric>: { n, mean, m2 } } } written whole by
//     the analysis process (its only writer); scoring.js also accepts rows [{ metric, n, mean, m2 }].
//   * store.analysis.forUser(userId, limit) must return that player's completed analyses, newest
//     first, each row carrying the `features` object given to complete() (or being it).
//   * store.reports.forReporter(reporterId) (not in DESIGN) is used when present to weigh a
//     reporter by the outcomes of their past reports; without it every reporter has a neutral
//     track record. store.reports.forReported(userId) must return rows with { weight, at }.
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

import { fork } from 'node:child_process';
import os from 'node:os';
import { fileURLToPath } from 'node:url';
import { logger as rootLogger } from '../log.js';
import { metrics } from '../metrics.js';
import { readIntegrity, writeIntegrity, writeStructured, HOUR_MS } from './util.js';

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
const MAX_EVIDENCE_ITEMS = 50;

/**
 * Creates the anti-cheat service of a process (shard or primary).
 * @param {object} o
 * @param {object} o.config     loadConfig() result
 * @param {object} o.store      Store (DESIGN 5.5): anomalies, sanctions, integrity, security
 * @param {object} [o.primary]  IPC client with request(type, payload) (shards); null in tools
 * @param {object} [o.log]
 * @param {() => number} [o.now]
 * @param {number} [o.flushMs=1000]  batching period of non-certain anomalies
 */
export function createAnticheat({ config, store, primary = null, log = null, now = Date.now, flushMs = 1000 }) {
    const lg = log || rootLogger.child('anticheat');
    const pending = new Map();      // userId|gameId|kind -> row (repeats coalesced)
    let flushTimer = null;
    const sanctioned = new Map();   // userId|gameId -> ban end (idempotency within a game)

    function scheduleFlush() {
        if (flushTimer) return;
        flushTimer = setTimeout(() => { flushTimer = null; flush(); }, flushMs);
        flushTimer.unref?.();
    }

    function insertRows(rows) {
        writeStructured((r) => store.anomalies.insertBatch(r), rows, ['detail']);
    }

    /**
     * Writes the buffered anomalies now (one insertBatch). Returns the number of rows written.
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
        try {
            insertRows(rows);
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
        if (c.severity === 'info') lg.debug('anomaly', { userId, gameId, kind: label, severity: c.severity });
        else lg.security('anomaly', { userId, gameId, kind: label, severity: c.severity, detail: d });

        const row = { userId, gameId: gameId || 0, kind: label, severity: c.severity, detail: d, at };
        if (c.certain) {
            try { insertRows([row]); } catch (e) {
                anomalyDropped.inc();
                lg.error('certain anomaly not persisted', { err: e, userId, kind });
            }
            return { severity: c.severity, certain: true };
        }
        const key = `${userId}|${row.gameId}|${label}`;
        const prev = pending.get(key);
        if (prev) { prev.count++; prev.lastAt = at; }
        else if (pending.size >= MAX_PENDING && c.severity === 'info') anomalyDropped.inc();
        else if (pending.size >= 2 * MAX_PENDING) anomalyDropped.inc();
        else pending.set(key, { ...row, count: 1, lastAt: at });
        scheduleFlush();
        return { severity: c.severity, certain: false };
    }

    /**
     * Automatic sanction of a certain cheat: ban BAN_DURATION_HOURS (source 'auto'), integrity
     * level 'confirmed' with the evidence appended, security event, and 'sanction.applied' to
     * the primary (which kicks the player everywhere). Idempotent within a game: several certain
     * anomalies of one game make one ban. Does nothing when AUTO_SANCTION_CERTAIN_CHEATS is off.
     * @param {{ userId: number, gameId?: number, kind: string }} s
     * @returns {{ banUntil: number, applied: boolean }}
     */
    function sanctionCertain({ userId, gameId = 0, kind }) {
        if (!config.autoSanctionCertainCheats) return { banUntil: 0, applied: false };
        const t = now();
        const key = `${userId}|${gameId || 0}`;
        const seen = sanctioned.get(key);
        if (seen && seen > t) return { banUntil: seen, applied: false };
        if (sanctioned.size > 10000) for (const [k, v] of sanctioned) if (v <= t) sanctioned.delete(k);

        const reason = `certain_cheat:${kind}`;
        let until = t + config.banDurationHours * HOUR_MS;
        let created = false;
        let active = null;
        try { active = store.sanctions.activeBan(userId, t); } catch { active = null; }
        // A ban without an end (permanent) counts as ending in 100 years.
        const activeEnd = active ? Number(active.endsAt ?? active.ends_at ?? 0) || t + 100 * 365 * 24 * HOUR_MS : 0;
        const sameGame = active && gameId && Number(active.gameId ?? active.game_id) === Number(gameId);
        if (active && (sameGame || activeEnd >= until)) {
            // Already banned (another shard, or an earlier anomaly of this game): no second ban.
            until = activeEnd;
        } else {
            try {
                store.sanctions.create({ userId, kind: 'ban', reason, source: 'auto', gameId: gameId || null, startsAt: t, endsAt: until, createdBy: null });
                created = true;
            } catch (e) {
                lg.error('automatic ban not stored', { err: e, userId, kind });
            }
        }
        sanctioned.set(key, until);

        try {
            const prev = readIntegrity(store, userId);
            const ev = { ...prev.evidence };
            ev.certain = [...(Array.isArray(ev.certain) ? ev.certain : []), { kind, gameId: gameId || 0, at: t, banUntil: until }].slice(-MAX_EVIDENCE_ITEMS);
            writeIntegrity(store, userId, { level: 'confirmed', score: prev.score, evidence: ev, updatedAt: t });
        } catch (e) {
            lg.error('integrity not updated after a certain cheat', { err: e, userId });
        }
        if (created) {
            sanctionCounter.labels(kind in ANOMALY_KINDS ? kind : 'unknown').inc();
            lg.security('sanction.auto', { userId, gameId, kind, until });
            try {
                writeStructured((r) => store.security.insertBatch(r), [{ kind: 'sanction_auto', userId, ip: null, detail: { kind, gameId: gameId || 0, until }, at: t }], ['detail']);
            } catch (e) { lg.warn('security event not stored', { err: e }); }
            if (primary?.request) {
                Promise.resolve()
                    .then(() => primary.request('sanction.applied', { userId, until, reason }))
                    .catch((e) => lg.warn('sanction.applied not delivered', { err: e, userId }));
            }
        }
        return { banUntil: until, applied: created };
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
