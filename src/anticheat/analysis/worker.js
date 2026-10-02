// Analysis worker: claims finished rated games from the analysis queue, runs the engine over
// them, stores the features and updates both players' integrity level and, for the games of the
// ordinary random sample only, the population statistics. One loop per engine (ANALYSIS_WORKERS
// engines, one core each). Everything statistical is per analysis profile (analyzer.js): a game
// joins, and its players are judged against, the population of the profile it was analysed with.
//
// Every engine start is logged with where the engine keeps its network: Stockfish 19 and later
// share one copy between all the engines (engine.js, netMemory), and one that falls back to a
// copy of its own while several engines run is a warning (each engine then takes about 110 MB
// more). The gauges below count the running engines and those that share.

import os from 'node:os';
import { metrics } from '../../metrics.js';
import { UciEngine, EngineError } from './engine.js';
import { analyseGame, normaliseGame } from './analyzer.js';
import { Population, updatePlayerIntegrity, updatePopulationFromGame } from '../scoring.js';
import { readIntegrity, writeStructured } from '../util.js';

// AnalysisPriority.ordinary of src/store/index.js (not imported: this module only sees the Store
// API): jobs of the random sample, the only games that feed the population statistics.
const ORDINARY_PRIORITY = 0;

const gamesCounter = metrics.counter('scacelith_anticheat_analysis_games_total', 'Games analysed by the engine', ['result']);
const okGames = gamesCounter.labels('ok');
const failedGames = gamesCounter.labels('failed');
const analysisSeconds = metrics.histogram('scacelith_anticheat_analysis_seconds', 'Engine analysis time per game', [5, 15, 30, 60, 120, 300, 600]);
// Engines of the running workers of this process, read at each scrape.
const liveEngines = new Set();
function countEngines(pred) {
    let n = 0;
    for (const list of liveEngines) for (const e of list) if (e.alive && pred(e)) n++;
    return n;
}
metrics.gaugeFn('scacelith_anticheat_analysis_engines', 'Analysis engine processes running', () => countEngines(() => true));
metrics.gaugeFn('scacelith_anticheat_analysis_engines_shared', 'Analysis engines whose network is in memory shared with the other engines (Stockfish 19 and later)',
    () => countEngines((e) => e.netMemory === 'shared'));

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
    const populations = new Map();     // analysis profile -> Population
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

    function populationOf(profile) {
        let pop = populations.get(profile);
        if (!pop) {
            pop = new Population(store, { profile, now });
            populations.set(profile, pop);
            log?.info('analysis profile', { profile });
        }
        return pop;
    }

    // The evidence behind the player's rating in the category, which sets the width of the rating
    // band of the z-scores (scoring.js: provisional below MODEL.provisionalGames): 0 while unrated
    // (the rating is only a starting value), the counted games once rated. Games played are not
    // evidence: the lost games of the unrated phase never enter the rating. A record stored before
    // `rated` / `countedGames` existed is read as src/match/elo.js reads it: rated when it has
    // games, all of them counted.
    function ratingGames(userId, category) {
        try {
            const r = store.ratings.get(userId, category);
            if (!r) return null;
            const rated = typeof r.rated === 'boolean' ? r.rated : r.games > 0;
            if (!rated) return 0;
            return Number.isFinite(r.countedGames) ? r.countedGames : r.games ?? null;
        } catch { return null; }
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
            const population = populationOf(features.profile);
            // Score the players first (their new game is judged against the population as it
            // was), then let the game join the population, but only a game claimed at ordinary
            // priority: those are the random sample of the rated games (ANALYSIS_SAMPLE_RATE).
            // Flagged, reported and moderator-requested games are analysed first and in full, so
            // counting them would shift the baseline towards the suspects it is meant to judge.
            for (const uid of [g.whiteId, g.blackId]) {
                if (!uid) continue;
                try { updatePlayerIntegrity({ store, userId: uid, population, now: now(), log }); } catch (e) { log?.error('integrity update failed', { err: e, userId: uid }); }
            }
            if (job?.priority === ORDINARY_PRIORITY) {
                // The job is done: a failed write loses this sample, it does not send the game
                // back to the queue.
                try { updatePopulationFromGame(population, features, (uid) => readIntegrity(store, uid).level); } catch (e) { log?.error('population update failed', { err: e, gameId }); }
            }
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

    // One line per engine process: which engine, and whether its network is shared (a warning
    // when it is not while other engines run: each holds its own copy).
    function reportStart(i, engine) {
        const fields = { engine: i, name: engine.name, net: engine.net ?? null, pid: engine.proc?.pid ?? null,
            network: engine.netMemory === 'shared' ? 'shared memory' : engine.netMemory === 'local' ? 'local memory' : 'not reported' };
        if (engine.netMemory !== 'local') { log?.info('analysis engine started', fields); return; }
        fields.why = engine.netMemoryError;
        if (count > 1) {
            log?.warn('analysis engine started with its own copy of the network', { ...fields, engines: count,
                hint: 'the engines share it through /tmp/stockfish-<uid>: /tmp must be writable and the same for all of them' });
        } else log?.info('analysis engine started', fields);
    }

    async function loop(i) {
        const engine = factory(i);
        engines.push(engine);
        let backoff = 5000, reported = 0;
        while (!stopping) {
            // Never claim a job without a working engine: a wrong ANALYSIS_ENGINE_PATH must not
            // mark the whole queue failed.
            if (typeof engine.start === 'function') {
                try {
                    await engine.start();
                    backoff = 5000;
                    // A restart during a game's analysis is reported here, before the next job.
                    if (engine.starts !== undefined && engine.starts !== reported) {
                        reported = engine.starts;
                        reportStart(i, engine);
                    }
                } catch (e) {
                    if (stopping) break;
                    log?.error('analysis engine unavailable', { err: e, retryInMs: backoff });
                    await sleep(backoff);
                    backoff = Math.min(300000, backoff * 2);
                    continue;
                }
            }
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
        run() {
            if (!running) {
                log?.info('analysis worker started', { engines: count, depthFast: config.analysisDepthFast, depthDeep: config.analysisDepthDeep, workerId });
                liveEngines.add(engines);
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
            liveEngines.delete(engines);
        },
    };
}
