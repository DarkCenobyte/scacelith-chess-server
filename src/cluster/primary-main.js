// Primary process bootstrap: configuration, database migrations, server id, the match module
// objects, the shard workers (cluster, 'advanced' IPC serialization, env SHARD=<n>), the
// anti-cheat analysis process, the control plane, the metrics endpoint, the retention purge
// (retention.js) and the analysis backlog gauges, signals (SIGTERM/SIGINT graceful stop, SIGHUP
// certificate reload).
//
// This file and worker-main.js are the only places that import the other modules of the server
// (store, match, anticheat...). Everything below them receives its dependencies as parameters.

import cluster from 'node:cluster';
import crypto from 'node:crypto';
import fs from 'node:fs';
import { fileURLToPath } from 'node:url';
import { startAnalysisProcess } from '../anticheat/index.js';
import { configureLogging, logger } from '../log.js';
import { loadConfig, describe, configWarnings } from '../config.js';
import { Conduct } from '../match/conduct.js';
import { Challenges } from '../match/challenges.js';
import { applyGame } from '../match/elo.js';
import { Matchmaker } from '../match/matchmaker.js';
import { metrics } from '../metrics.js';
import { migrate, openStore } from '../store/index.js';
import { makeBusFallbackDir } from './bus.js';
import { startPrimary } from './primary.js';
import { startRetention } from './retention.js';

const WORKER_MAIN = fileURLToPath(new URL('./worker-main.js', import.meta.url));

/**
 * Makes sure the database has a server id (uuid, created at the first start).
 * @param {object} store
 */
export function ensureServerId(store) {
    let id = store.meta.get('server_id');
    if (!id) {
        id = crypto.randomUUID();
        store.meta.set('server_id', id);
    }
    return String(id);
}

/**
 * Graceful stop of the primary: the pairing stops and the shards are told to drain at once
 * (primary.stop does both before it waits for them), while the retention purge and the analysis
 * process stop; the store is closed once all of them are done.
 */
export async function stopPrimary({ config, log, primary, retention, analysis, store }) {
    const quietly = async (what, stop) => { try { await stop(); } catch (e) { log.error(`${what} stop failed`, { err: e }); } };
    await Promise.all([
        primary.stop(config.shutdownGraceMs),
        quietly('retention', () => retention.stop()),
        quietly('analysis', () => analysis?.stop()),
    ]);
    try { store.close(); } catch (e) { log.error('store close failed', { err: e }); }
}

/** Starts the server (primary side). */
export async function main() {
    const config = loadConfig();
    configureLogging({ level: config.logLevel, format: config.logFormat, ipMode: config.logIp, secret: config.serverSecret, base: { inst: config.instanceId, proc: 'primary' } });
    const log = logger.child('primary');
    log.info('starting', { config: describe(config) });
    for (const warning of configWarnings(config)) log.warn('configuration works against the design', { warning });

    fs.mkdirSync(config.dataDir, { recursive: true, mode: 0o750 });
    fs.mkdirSync(config.runDir, { recursive: true, mode: 0o700 });
    try { fs.chmodSync(config.runDir, 0o700); } catch { /* not ours / Windows */ }

    const store = openStore(config, { applyGame });
    await migrate(store);
    const serverId = ensureServerId(store);

    const matchmaker = new Matchmaker({ config, now: Date.now });
    const challenges = new Challenges({ config, now: Date.now });
    const conduct = new Conduct({ config, store, now: Date.now, log: logger.child('conduct') });

    cluster.setupPrimary({ exec: WORKER_MAIN, args: [], serialization: 'advanced' });
    const busDir = makeBusFallbackDir(config.runDir);         // for the bus sockets too long for runDir
    const removeBusDir = () => {
        if (!busDir) return;
        try { fs.rmSync(busDir, { recursive: true, force: true }); } catch (e) { log.warn('bus socket directory not removed', { dir: busDir, err: e }); }
    };
    const fork = (shard) => cluster.fork({ SHARD: String(shard), SCACELITH_SERVER_ID: serverId, SCACELITH_BUS_DIR: busDir });

    let analysis = null;
    const primary = await startPrimary({
        config, log, fork, matchmaker, challenges, conduct,
        // The analysis process's metrics (it is started below).
        extraMetrics: async () => {
            const snapshot = await analysis?.metricsSnapshot?.();
            return snapshot ? [{ shard: 'analysis', snapshot }] : [];
        },
        activeBan: (userId, now) => {
            const b = store.sanctions.activeBan(userId, now);
            return b ? { until: b.endsAt ?? b.until ?? now + 86400000 } : null;
        },
        ratingOf: (userId, category) => store.ratings.get(userId, category),
        acceptsChallenges: (userId) => store.users.byId(userId)?.acceptChallenges !== false,
        refunds: store.refunds,
    }).catch((e) => { removeBusDir(); throw e; });
    log.info('primary ready', { serverId, workers: config.workers, shardBase: config.shardBase, metricsPort: primary.metricsPort });

    try {
        analysis = startAnalysisProcess(config);
    } catch (e) {
        log.error('analysis process failed to start', { err: e });
    }

    const retention = startRetention({ config, store, log: logger.child('retention') });
    // Engine analysis backlog, read once per scrape (both counts are ranges of the queue index).
    let backlog = null, backlogAt = 0;
    const readBacklog = () => {
        if (!backlog || Date.now() - backlogAt > 1000) { backlog = store.analysis.backlog(); backlogAt = Date.now(); }
        return backlog;
    };
    metrics.gaugeFn('scacelith_anticheat_analysis_queue_ordinary', 'Ordinary games waiting for engine analysis (at most ANALYSIS_QUEUE_MAX)',
        () => readBacklog().ordinary);
    metrics.gaugeFn('scacelith_anticheat_analysis_queue_priority', 'Reported, flagged or moderator-requested games waiting for engine analysis',
        () => readBacklog().priority);

    let stopping = false;
    const shutdown = async (signal) => {
        if (stopping) {
            log.warn('second signal: exiting now', { signal });
            process.exit(1);
        }
        stopping = true;
        log.info('shutting down', { signal, graceMs: config.shutdownGraceMs });
        await stopPrimary({ config, log, primary, retention, analysis, store });
        removeBusDir();
        log.info('stopped');
        process.exit(0);
    };
    process.on('SIGTERM', () => { shutdown('SIGTERM'); });
    process.on('SIGINT', () => { shutdown('SIGINT'); });
    process.on('SIGHUP', () => { log.info('SIGHUP: reloading certificates'); primary.reloadTls(); });
    process.on('uncaughtException', (e) => { log.error('uncaught exception in the primary', { err: e }); removeBusDir(); process.exit(1); });
    process.on('unhandledRejection', (e) => { log.error('unhandled rejection in the primary', { err: e }); });
    return primary;
}
