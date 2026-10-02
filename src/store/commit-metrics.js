// The store's commit metrics, defined once for the two places that count them: the store itself
// (store/index.js: games.finishBatch, the lock timeouts) and a shard's writer thread client
// (store/writer.js), which counts the thread's answers in the shard's registry. This module imports
// nothing of the store: writer.js imports it before its thread has configured the logging.

import { metrics } from '../metrics.js';

// Waiting 'signal' jobs per player: a flagged player's further games are not queued while this
// many of their games wait (the scoring reads their 30 latest analysed games, so these renew most
// of that window); one prolific flagged player cannot grow the signal tier without bound. A game
// with an anomaly of its own replaces a waiting one without (queueAnalysis), so games flagged only
// through the player cannot keep one with evidence out.
export const SIGNAL_JOBS_PER_PLAYER = 20;

const mBatchMs = metrics.histogram('scacelith_store_commit_batch_ms', 'Duration of one finished-games commit transaction',
    [1, 2, 5, 10, 25, 50, 100, 250, 1000]);
const mGames = metrics.counter('scacelith_store_games_committed_total', 'Finished games written to the database');
export const mBusy = metrics.counter('scacelith_store_busy_total', 'Store operations that gave up waiting for the database lock');
const mAnalysisSkipped = metrics.counter('scacelith_anticheat_analysis_skipped_total',
    `Finished rated games not queued for engine analysis (sample: ANALYSIS_SAMPLE_RATE, backlog: ANALYSIS_QUEUE_MAX reached, player: ${SIGNAL_JOBS_PER_PLAYER} flagged games of a player already waiting, displaced: a waiting flagged game without an anomaly of its own gave its place to a game with one)`, ['reason']);

/**
 * Counts one finishBatch: its duration, the games written (duplicates aside), and the games left
 * out of the analysis queue or taken out of it.
 * @param {object[]} results finishBatch's results
 * @param {number} ms
 */
export function countCommit(results, ms) {
    mBatchMs.observe(ms);
    const list = Array.isArray(results) ? results : [];
    mGames.inc(list.reduce((n, x) => n + (x && x.duplicate ? 0 : 1), 0));
    for (const x of list) {
        if (x && x.analysisSkipped) mAnalysisSkipped.labels(x.analysisSkipped).inc();
        if (x && x.analysisDisplaced) mAnalysisSkipped.labels('displaced').inc(x.analysisDisplaced.length);
    }
}
