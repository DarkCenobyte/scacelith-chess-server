// Analysis worker: claims finished rated games from the analysis queue, runs the engine over
// them, stores the features and updates both players' integrity level and the population
// statistics. One loop per engine (ANALYSIS_WORKERS engines, one core each).

import os from 'node:os';
import { metrics } from '../../metrics.js';
import { UciEngine, EngineError } from './engine.js';
import { analyseGame, normaliseGame } from './analyzer.js';
import { Population, updatePlayerIntegrity, updatePopulationFromGame } from '../scoring.js';
import { readIntegrity, writeStructured } from '../util.js';

const gamesCounter = metrics.counter('scacelith_anticheat_analysis_games_total', 'Games analysed by the engine', ['result']);
const okGames = gamesCounter.labels('ok');
const failedGames = gamesCounter.labels('failed');
const analysisSeconds = metrics.histogram('scacelith_anticheat_analysis_seconds', 'Engine analysis time per game', [5, 15, 30, 60, 120, 300, 600]);

/**
 * Creates the worker.
 * @param {object} o
 * @param {object} o.config
 * @param {object} o.store
 * @param {object} [o.log]
 * @param {(i: number) => object} [o.engineFactory]   default: UciEngine on ANALYSIS_ENGINE_PATH
 * @param {string} [o.workerId]
 * @param {number} [o.pollMs]
 * @param {() => number} [o.now]
 * @returns {{ run: () => Promise<void>, stop: () => Promise<void>, processJob: Function, stats: object }}
 */
export function createAnalysisWorker({ config, store, log = null, engineFactory = null, workerId = `${os.hostname()}:${process.pid}`, pollMs = config.analysisPollMs || 5000, now = Date.now }) {
    const engines = [];
    const count = Math.max(1, config.analysisWorkers || 1);
    const factory = engineFactory || (() => new UciEngine({
        path: config.analysisEnginePath, threads: 1, hashMb: config.analysisHashMb || 32,
        timeoutMs: config.analysisPositionTimeoutMs || 60000, log,
    }));
    const population = new Population(store, { now });
    const stats = { analysed: 0, failed: 0, running: 0 };
    let stopping = false;
    const sleepers = new Set();

    function sleep(ms) {
        return new Promise((resolve) => {
            const t = setTimeout(() => { sleepers.delete(s); resolve(); }, ms);
            const s = () => { clearTimeout(t); resolve(); };
            sleepers.add(s);
        });
    }

    function claim() {
        const jobs = store.analysis.next(1, workerId, now());
        return Array.isArray(jobs) && jobs.length ? jobs[0] : null;
    }

    function ratingGames(userId, category) {
        try { return store.ratings.get(userId, category)?.games ?? null; } catch { return null; }
    }

    /**
     * Analyses one job end to end (exported for tests and the admin CLI).
     * @returns {Promise<object|null>} the features, null when the job failed
     */
    async function processJob(engine, job) {
        const gameId = job?.gameId ?? job?.game_id ?? job?.id;
        const started = now();
        try {
            let record = job && job.moves ? job : null;
            if (!record) record = store.games.byId(gameId);
            if (!record) throw new Error('game not found');
            const g = normaliseGame(record);
            const extra = {
                white: { ratingGames: ratingGames(g.whiteId, g.category) },
                black: { ratingGames: ratingGames(g.blackId, g.category) },
            };
            const features = await analyseGame(engine, record, { depthFast: config.analysisDepthFast, depthDeep: config.analysisDepthDeep, extra });
            features.gameId = features.gameId ?? gameId;
            writeStructured((f) => store.analysis.complete(gameId, f), features);
            // Score the players first (their new game is judged against the population as it
            // was), then let the game join the population.
            for (const uid of [g.whiteId, g.blackId]) {
                if (!uid) continue;
                try { updatePlayerIntegrity({ store, userId: uid, population, now: now(), log }); } catch (e) { log?.error('integrity update failed', { err: e, userId: uid }); }
            }
            updatePopulationFromGame(population, features, (uid) => readIntegrity(store, uid).level);
            stats.analysed++;
            okGames.inc();
            analysisSeconds.observe((now() - started) / 1000);
            log?.info('game analysed', { gameId, plies: features.plies, white: features.white.n, black: features.black.n, ms: now() - started });
            return features;
        } catch (e) {
            if (stopping) {
                // Interrupted by the shutdown: leave the job claimed, it is analysed again later.
                log?.info('analysis interrupted by shutdown', { gameId });
                return null;
            }
            stats.failed++;
            failedGames.inc();
            const msg = e instanceof EngineError ? `engine ${e.code}: ${e.message}` : String(e?.message || e);
            log?.warn('analysis failed', { gameId, err: msg });
            try { store.analysis.fail(gameId, msg.slice(0, 500)); } catch (e2) { log?.error('cannot mark the job failed', { err: e2, gameId }); }
            return null;
        }
    }

    async function loop(i) {
        const engine = factory(i);
        engines.push(engine);
        while (!stopping) {
            let job = null;
            try { job = claim(); } catch (e) { log?.error('analysis queue unavailable', { err: e }); }
            if (!job) { await sleep(pollMs); continue; }
            stats.running++;
            try { await processJob(engine, job); } finally { stats.running--; }
        }
    }

    let running = null;
    return {
        stats,
        processJob,
        population,
        run() {
            if (!running) {
                log?.info('analysis worker started', { engines: count, depthFast: config.analysisDepthFast, depthDeep: config.analysisDepthDeep, workerId });
                running = Promise.all(Array.from({ length: count }, (_, i) => loop(i))).then(() => undefined);
            }
            return running;
        },
        async stop() {
            stopping = true;
            for (const s of sleepers) s();
            sleepers.clear();
            await Promise.all(engines.map((e) => e.close().catch(() => {})));
            if (running) await running.catch(() => {});
        },
    };
}
