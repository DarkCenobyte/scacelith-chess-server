// Primary process bootstrap: configuration, database migrations, server id, the match module
// objects, the shard workers (cluster, 'advanced' IPC serialization, env SHARD=<n>), the
// anti-cheat analysis process (when the anti-cheat module exports startAnalysisProcess), the
// control plane, the metrics endpoint, signals (SIGTERM/SIGINT graceful stop, SIGHUP certificate
// reload).
//
// This file and worker-main.js are the only places that import the other modules of the server
// (store, match, anticheat...). Everything below them receives its dependencies as parameters.

import cluster from 'node:cluster';
import crypto from 'node:crypto';
import fs from 'node:fs';
import { fileURLToPath } from 'node:url';
import * as anticheatModule from '../anticheat/index.js';
import { configureLogging, logger } from '../log.js';
import { loadConfig, describe } from '../config.js';
import * as conductModule from '../match/conduct.js';
import { Challenges } from '../match/challenges.js';
import { applyGame } from '../match/elo.js';
import { Matchmaker } from '../match/matchmaker.js';
import { migrate, openStore } from '../store/index.js';
import { startPrimary } from './primary.js';

const WORKER_MAIN = fileURLToPath(new URL('./worker-main.js', import.meta.url));

function makeConduct(config, store, log) {
    const now = Date.now;
    if (typeof conductModule.Conduct === 'function') return new conductModule.Conduct({ config, store, now, log });
    if (typeof conductModule.createConduct === 'function') return conductModule.createConduct({ config, store, now, log });
    return null;
}

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

/** Starts the server (primary side). */
export async function main() {
    const config = loadConfig();
    configureLogging({ level: config.logLevel, format: config.logFormat, ipMode: config.logIp, secret: config.serverSecret, base: { inst: config.instanceId, proc: 'primary' } });
    const log = logger.child('primary');
    log.info('starting', { config: describe(config) });

    fs.mkdirSync(config.dataDir, { recursive: true, mode: 0o750 });
    fs.mkdirSync(config.runDir, { recursive: true, mode: 0o700 });
    try { fs.chmodSync(config.runDir, 0o700); } catch { /* not ours / Windows */ }

    const store = openStore(config, { applyGame });
    await migrate(store);
    const serverId = ensureServerId(store);

    const matchmaker = new Matchmaker({ config, now: Date.now });
    const challenges = new Challenges({ config, now: Date.now });
    const conduct = makeConduct(config, store, logger.child('conduct'));
    if (!conduct) log.warn('match/conduct.js exports no Conduct: conduct cooldowns disabled');

    cluster.setupPrimary({ exec: WORKER_MAIN, args: [], serialization: 'advanced' });
    const fork = (shard) => cluster.fork({ SHARD: String(shard), SCACELITH_SERVER_ID: serverId });

    const primary = await startPrimary({
        config, log, fork, matchmaker, challenges, conduct,
        activeBan: (userId, now) => {
            const b = store.sanctions.activeBan(userId, now);
            return b ? { until: b.endsAt ?? b.until ?? now + 86400000 } : null;
        },
        ratingOf: (userId, category) => store.ratings.get(userId, category),
        acceptsChallenges: (userId) => store.users.byId(userId)?.acceptChallenges !== false,
    });
    log.info('primary ready', { serverId, workers: config.workers, shardBase: config.shardBase, metricsPort: primary.metricsPort });

    let analysis = null;
    if (typeof anticheatModule.startAnalysisProcess === 'function') {
        try {
            analysis = await anticheatModule.startAnalysisProcess(config);
        } catch (e) {
            log.error('analysis process failed to start', { err: e });
        }
    }

    let stopping = false;
    const shutdown = async (signal) => {
        if (stopping) {
            log.warn('second signal: exiting now', { signal });
            process.exit(1);
        }
        stopping = true;
        log.info('shutting down', { signal, graceMs: config.shutdownGraceMs });
        try {
            if (analysis) {
                if (typeof analysis.stop === 'function') await analysis.stop();
                else if (typeof analysis.kill === 'function') analysis.kill('SIGTERM');
            }
        } catch (e) { log.error('analysis stop failed', { err: e }); }
        await primary.stop(config.shutdownGraceMs);
        try { store.close(); } catch (e) { log.error('store close failed', { err: e }); }
        log.info('stopped');
        process.exit(0);
    };
    process.on('SIGTERM', () => { shutdown('SIGTERM'); });
    process.on('SIGINT', () => { shutdown('SIGINT'); });
    process.on('SIGHUP', () => { log.info('SIGHUP: reloading certificates'); primary.reloadTls(); });
    process.on('uncaughtException', (e) => { log.error('uncaught exception in the primary', { err: e }); process.exit(1); });
    process.on('unhandledRejection', (e) => { log.error('unhandled rejection in the primary', { err: e }); });
    return primary;
}
