// Shard worker bootstrap (cluster worker, env SHARD=<n>): store, journal, anti-cheat, auth, the
// GameHost (its journal is replayed with recover() before anything listens; its finished games
// are committed by a store writer thread, src/store/writer.js), the protection per address
// (net/ipguard.js, shared by the API handler and the listeners), the HTTPS API handler, then the
// shard itself (bus, router, WebSocket server, listeners). Stops gracefully on
// the primary's 'shutdown' message or SIGTERM, and at once if the primary disappears.

import cluster from 'node:cluster';
import { createAnticheat } from '../anticheat/index.js';
import { createAuth } from '../auth/index.js';
import { ChessGame } from '../chess/index.js';
import { loadConfig } from '../config.js';
import { GameHost } from '../game/host.js';
import { createApiHandler } from '../http/server.js';
import { configureLogging, logger } from '../log.js';
import { applyGame } from '../match/elo.js';
import { openStore } from '../store/index.js';
import { openJournal } from '../store/journal.js';
import { startStoreWriter } from '../store/writer.js';
import { Ipc } from './ipc.js';
import { createBus, createShardGuard, startShard } from './shard.js';

/** Starts this worker's shard. */
export async function main() {
    const config = loadConfig();
    const shard = Number(process.env.SHARD);
    const serverId = String(process.env.SCACELITH_SERVER_ID || '');
    if (!Number.isInteger(shard) || shard < 0 || shard > 63 || !serverId) throw new Error('worker started without SHARD / SCACELITH_SERVER_ID');
    configureLogging({ level: config.logLevel, format: config.logFormat, ipMode: config.logIp, secret: config.serverSecret, base: { inst: config.instanceId, shard } });
    const log = logger.child(`shard${shard}`);
    process.on('uncaughtException', (e) => { log.error('uncaught exception: the shard restarts', { err: e }); process.exit(1); });
    process.on('unhandledRejection', (e) => { log.error('unhandled rejection', { err: e }); });

    const primary = new Ipc(process, { log: log.child('ipc') });
    const store = openStore(config, { applyGame });
    const journal = await openJournal({ dir: config.journalDir, shard, flushMs: config.journalFlushMs, fsync: config.journalFsync,
        compactSegments: config.journalCompactSegments });
    // Finished-game commits and the anti-cheat's writes run on a writer thread: the event loop
    // never waits for SQLite.
    const writer = startStoreWriter({ config, shard, log: log.child('writer') });
    const anticheat = createAnticheat({ config, store, primary, log: logger.child('anticheat'), writer });
    const auth = createAuth({ config, store, primary, log: logger.child('auth') });
    const bus = createBus({ config, shard, serverId, log: log.child('bus') });
    const hostStore = { games: { finishBatch: (records) => writer.finishBatch(records) } };
    const host = new GameHost({
        shard, config, store: hostStore, journal, anticheat, bus, primary, log: logger.child('game'),
        createChessGame: () => new ChessGame(),
    });
    // Replays the journal; the host announces each restored game to the primary
    // ('game.recovered'), which gives it back to the players as their activeGame.
    const recovered = await host.recover();
    log.info('journal replayed', { games: recovered });
    const guard = createShardGuard({ config, primary, log: log.child('guard') });
    const apiHandler = createApiHandler({ config, store, auth, primary, anticheat, log: logger.child('http'), guard });

    const s = await startShard({
        config, shard, serverId, primary, host, auth, anticheat, store, apiHandler, bus, log, guard,
        onStopped: async () => {
            try { await journal.flush?.(); await journal.close?.(); } catch (e) { log.error('journal close failed', { err: e }); }
            anticheat.close();   // its buffered anomalies reach the writer before its close
            try { await writer.close(); } catch (e) { log.error('store writer close failed', { err: e }); }
            try { store.close(); } catch (e) { log.error('store close failed', { err: e }); }
            primary.flush();
            setTimeout(() => process.exit(0), 50);
        },
    });
    process.on('SIGTERM', () => { s.stop(config.shutdownGraceMs); });
    process.on('SIGINT', () => { /* the primary coordinates the shutdown (Ctrl-C reaches the whole group) */ });
    process.on('SIGHUP', () => { s.listeners.reloadCertificates(); });
    process.on('disconnect', () => { log.warn('primary gone: stopping'); s.stop(0); });
    return s;
}

if (cluster.isWorker) {
    main().catch((e) => {
        // A listen failure on a privileged port carries the fix in its message (net/listeners.js
        // listenHint): it is logged as the message itself, so that the operator reads it first
        // (errorCode: the log hides a field named `code`).
        if (e && e.name === 'ListenError') logger.child('worker').error(e.message, { errorCode: e.code, port: e.port });
        else logger.child('worker').error('shard failed to start', { err: e });
        process.exit(1);
    });
}
