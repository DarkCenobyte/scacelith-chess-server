// Store: the SQLite persistence of the dedicated server (DESIGN.md 5.5 and 7).
//
// One synchronous connection per process (`node:sqlite` DatabaseSync). The primary and every
// shard open the same file: WAL lets readers run beside the single writer, `busy_timeout` makes a
// writer wait for the lock instead of failing, and every read-then-write goes through
// BEGIN IMMEDIATE (the write lock is taken first, so a transaction never has to upgrade a stale
// read snapshot, which SQLite would refuse with SQLITE_BUSY_SNAPSHOT). Transactions stay short:
// the retention job deletes in chunks of at most CHUNK rows. Every statement is prepared once, on
// first use (so a store can be opened before migrate() created the tables), and cached. Writable
// connections run with secure_delete, so deleted and erased personal data (IP addresses,
// anonymized accounts) does not stay readable in the file's free space.
//
// Deviations from / additions to the DESIGN 5.5 contract (the closest sensible reading where the
// contract is silent; all are supersets, no documented call changes meaning):
//   - openStore(config, { readonly, applyGame, log }): a DB_PATH whose last component is
//     ':memory:' opens a private in-memory database (config paths are resolved to absolute ones).
//   - games.finishBatch: an id already in the database is not inserted again and its ratings are
//     not applied again (crash recovery re-commits); its entry is { gameId, duplicate: true,
//     ratings } with the stored before/after and the player's current game count. Ratings are
//     applied only to rated games that were played out (status WhiteWins / BlackWins / Draw, not
//     Aborted) in an official category (not 'custom'); only those are queued for analysis (when
//     they have >= ANALYSIS_MIN_PLIES plies), under the queue policy below. A rated game in which
//     a player lost points (K formula) to an opponent whose integrity level is 'confirmed' and who
//     is under an active ban for cheating that refunds (an automatic ban of a certain cheat, or an
//     integrity confirm without --no-refund; refundAtCommit) is refunded in the same transaction,
//     with its 'rating_refund' security event, unless RATING_REFUND_DAYS is 0 (refunds below): a
//     game in progress when the opponent was banned, or still on its way to the database, which
//     the ban's own refunds could not see; a 'rating.refund' security log line follows the
//     commit. An invalid record throws StoreError 'invalid_record' (with .gameId) and the whole
//     batch is rolled back. The entry of a game left out of the queue carries analysisSkipped:
//     'sample' | 'backlog' | 'player'; the entry of a game that took over waiting jobs carries
//     analysisDisplaced: [ids of the games whose job it removed].
//   - users.anonymize(id, now?) -> { tokenHashes } (the revoked sessions, for the auth caches):
//     username becomes 'deleted#<id>' (also in the game records), e-mail, password, MFA,
//     sessions, tokens, SSO links, recovery codes, integrity record and stored IPs are erased;
//     ratings and games are kept.
//   - sessions.revoke(id, userId?) -> tokenHash | null (userId, when given, must own the session);
//     sessions.enforceLimit(userId, max, now?) -> [tokenHash] revoked (oldest first go);
//     sessions.listForUser returns the non-revoked sessions (with clientLabel and ip).
//   - tokens.consume(kind, hash, now) refuses expired tokens as well as consumed ones.
//   - sso.link throws StoreError 'sso_taken' when the identity belongs to another account;
//     sso.forUser(userId) lists an account's identities.
//   - ratings.leaderboard(category, limit, minGames) ranks the records with at least minGames
//     counted games (below), and leaves out deleted accounts, players whose integrity level is
//     'confirmed' and records still in their unrated phase.
//   - ratings: a record carries the FIDE unrated phase of match/elo.js (rated, unratedGames,
//     unratedOpponents, unratedHalfPoints; migration 004) and its counted games (countedGames,
//     the games that entered the rating; a record stored without them counts all its games when
//     rated, those of its unrated phase otherwise); a missing record is unrated at INITIAL_RATING.
//     A rating function that returns records without `rated` (tests) rates and counts every game.
//     `provisional` (forUser, the RatingChange objects) is: unrated, or fewer than
//     PROVISIONAL_GAMES counted games. finishBatch also stores the K factor of each side's change
//     (games.white_k / black_k, 0 when the K formula did not apply), which the refunds read.
//   - refunds (anticheat/refunds.js): applyForCheater({ cheaterId, since, now, sanctionId, source,
//     by }) gives back, in one transaction, to each opponent of the cheater the rating points they
//     lost (a K-formula change: k > 0, or NULL for the games finished before migration 004) in a
//     rated game against them that ended at `since` or later, on the victim's current record of that
//     category (the peak rises with it); one refund per (game, victim) at most (a second call
//     skips those already given, at the commit of the game included). Returns the refunds given.
//     list({ cheaterId, victimId, limit }), pendingSince(afterId, limit) and pendingFor(victimId)
//     (not notified yet), markNotified(ids, now).
//   - games.recentForUser returns summaries (no move arrays); games.byId the full record.
//     games.countBetween(a, b, since, { rated }) counts both colour orders; extra
//     games.countForUser(userId) (public profile).
//   - tokens.consume, mfa.consumeRecoveryCode and users.advanceMfaStep are single conditional
//     statements (UPDATE/DELETE ... WHERE still-valid): atomic across processes without an
//     explicit transaction (an autocommit write retries on the lock through busy_timeout).
//   - analysis: a job is claimed at most 3 times (ANALYSIS_MAX_ATTEMPTS); a running job older than
//     10 minutes is re-queued (or failed at the cap) by the next claim; fail() re-queues until the
//     cap and returns the new status; extra enqueue(gameId, now) (manual re-analysis) and stats().
//   - analysis queue policy (DESIGN.md 6.5): every job has a priority (AnalysisPriority: ordinary,
//     signal, report, manual) and next() takes the highest first, then the oldest, except that
//     every ORDINARY_SHARE-th claim of a store takes the oldest ordinary job first (when one
//     waits): ordinary games keep a quarter of the engine time however many prioritized games
//     arrive. At the end of a game, a suspicion signal (either player's integrity level above
//     'none', an open cheating/other report of weight >= 0.5 against either player in the last
//     30 days, a non-info anomaly recorded in this game) queues it as 'signal', under a cap of
//     SIGNAL_JOBS_PER_PLAYER signal jobs waiting per player: past it, a game with a non-info
//     anomaly of its own takes over the oldest waiting job, without such an anomaly, of each capped
//     player, in the same transaction (the count stays at the cap), and any other flagged game is
//     skipped ('player', also when every waiting job has an anomaly of its own). An ordinary game
//     is drawn with ANALYSIS_SAMPLE_RATE and queued only while fewer than ANALYSIS_QUEUE_MAX
//     ordinary jobs wait; otherwise it is not inserted at all. analysis.request(gameId, reason,
//     now) queues an eligible game for a report (or raises the priority of its waiting job, or
//     re-queues a failed one; 'signal' under the same per-player cap); enqueue() is a moderator
//     request (priority 'manual'). analysis.backlog() counts the waiting jobs { ordinary,
//     priority }, each up to BACKLOG_COUNT_MAX. A job records the game's players (migration 003)
//     for the per-player count.
//   - integrity.populationStats(category, ratingBucket) or (prefix) -> { metric: { n, mean, m2,
//     variance, stdev, updatedAt } }; integrity.updatePopulation(updates, now) with updates
//     [{ key | category+ratingBucket+metric, value | values | {n, mean, m2} }] (or (key, value,
//     now)), merged with Chan's parallel Welford formula in one transaction.
//   - sanctions.active(userId, now) lists every active sanction (activeBan returns the longest ban).
//   - security.forUser(userId, limit); security.purge(now, cfg) deletes events older than
//     RETENTION_SECURITY_DAYS and erases their IPs after RETENTION_IP_DAYS.
//   - retention.run(now, cfg) also deletes revoked sessions after a day, info/suspicious anomalies
//     after RETENTION_SECURITY_DAYS (as the config key describes), conduct events after 30 days
//     and failed analysis jobs after 30 days (done jobs are kept: their features are the players'
//     analysed history, read by the scoring and by bin/admin.js without a time limit).
//     retention.runAsync(now, cfg, { sliceMs, signal, pause }) runs the same statements in
//     smaller chunks, adapted so that one statement takes about sliceMs / 2, and pauses for
//     sliceMs whenever a slice of sliceMs is used up (the primary runs it; other writers get the
//     lock in between), and stops early when the AbortSignal fires or the store is closed.
//   - store.transaction(fn): runs fn in one BEGIN IMMEDIATE transaction (nested calls use
//     savepoints), for callers that need several store calls to be atomic.
//   - JSON columns (tokens.data, anomaly/security detail, evidence, features) round-trip any JSON
//     value; hashes are stored and compared exactly as given (string or Buffer); BLOBs come back
//     as Buffers.
//   - Account API (GET /account/games, the e-mail change and the data export; docs/API.md):
//       games.listForUser(userId, { before, limit, category, rated, result }) -> summaries, newest
//         first (the objects of recentForUser), filtered by category id ('custom' included), rated
//         (boolean) and result from the player's side ('win' | 'loss' | 'draw': played-out games
//         only, an aborted game never matches a result); games.countForUser(userId, filter) counts
//         the games matching the same filter (without one: every game, as before). Both use the
//         UNION shape of recentForUser: one range scan of games_white / games_black per colour,
//         the filter applied to the player's own rows, never a scan of the table (the tests check
//         the query plans of GAMES_FOR_USER_SQL / GAMES_COUNT_FOR_USER_SQL). limit is not capped
//         here (the route caps it; the export pages with `before`). StoreError 'invalid' for a
//         result outside the three values.
//       users.update(id, { email }) also sets email_normalized and throws StoreError 'email_taken'
//         when another account uses the address (the UNIQUE index decides, atomically).
//       tokens.deleteForUser(userId, kind) -> rows deleted; tokens.liveForUser(userId, kind, now)
//         -> the newest unconsumed, unexpired token of that kind for the user (with its data), or
//         null (both through the tokens_user index).
//       sessions.allForUser(userId) -> every stored session of the account, revoked and expired
//         ones included (until the retention deletes them), newest first, with revokedAt,
//         clientLabel and the stored ip (never the token hash).
//       security.forUser(userId, limit) (above) is newest first; sanctions.list(userId) includes
//         the lifted sanctions (createdBy / liftedBy name moderators: a caller showing the list to
//         the player leaves them out); refunds.list({ victimId, limit }) lists the refunds a player
//         received.
//       conduct.forUser(userId, limit = 1000) -> [{ kind, at }], newest first (the retention keeps
//         30 days).
//       reports.forReporter(userId, limit = 500) -> the reports the player filed, newest first,
//         each with reportedName and `outcome` (null while open, else 'actioned' | 'dismissed').
//         anticheat/reports.js reads the outcomes for the reporter's track record when this call
//         exists: before it, every reporter of a real store had the neutral track record.
//       explainQueryPlan(store, sql, params) (module export): the EXPLAIN QUERY PLAN rows of a
//         statement on the store's own connection (tests and diagnostics).

import crypto from 'node:crypto';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { createRequire } from 'node:module';
import { setTimeout as sleep } from 'node:timers/promises';
import { fileURLToPath } from 'node:url';
import { logger } from '../log.js';
import { metrics } from '../metrics.js';
import { enums } from '../protocol/schema.js';

const { DatabaseSync } = loadSqlite();

// node:sqlite prints an ExperimentalWarning on Node 22 when it is first loaded. Only that notice
// is filtered, only while the module loads; every other warning goes through unchanged.
function loadSqlite() {
    const require = createRequire(import.meta.url);
    const original = process.emitWarning;
    process.emitWarning = function (warning, ...args) {
        const message = typeof warning === 'string' ? warning : warning?.message;
        const type = typeof args[0] === 'string' ? args[0] : (args[0]?.type ?? warning?.name);
        if (type === 'ExperimentalWarning' && /SQLite/i.test(String(message))) return undefined;
        return original.call(process, warning, ...args);
    };
    try {
        return require('node:sqlite');
    } finally {
        process.emitWarning = original;
    }
}

const MIGRATIONS_DIR = fileURLToPath(new URL('./migrations/', import.meta.url));
const DB = Symbol('scacelith.store.db');
const LE = os.endianness() === 'LE';
const DAY = 86400000;
// Rows per retention statement: run() and security.purge() use CHUNK; runAsync starts at
// CHUNK_START and adapts between CHUNK_MIN and CHUNK so that one statement takes about half of
// its time slice (a large chunk holds the write lock, and the event loop, for tens of ms). Every
// statement also pays a fixed cost (the commit's fsync), which fewer rows do not reduce: CHUNK_MIN
// keeps the purge moving on a disk whose fsync alone exceeds the target.
const CHUNK = 1000;
const CHUNK_START = 200;
const CHUNK_MIN = 50;
const ANALYSIS_MAX_ATTEMPTS = 3;
const ANALYSIS_STALE_MS = 10 * 60000;
const CONDUCT_EVENT_TTL_MS = 30 * DAY;
const REVOKED_SESSION_TTL_MS = DAY;
const ANALYSIS_FAILED_TTL_MS = 30 * DAY;
const REPORT_SIGNAL_MS = 30 * DAY;
// A report flags the reported player's next games only from a credible reporter: the stored weight
// reaches REPORT_RULES.lowCredibility of anticheat/reports.js (sock puppets never flag anyone).
const REPORT_SIGNAL_MIN_WEIGHT = 0.5;
// Every ORDINARY_SHARE-th claim of next() takes the oldest ordinary job first (when one waits), so
// the ordinary games (the random sample that feeds the population statistics) get at least that
// share of the engine time however many prioritized games arrive.
const ORDINARY_SHARE = 4;
// Waiting 'signal' jobs per player: a flagged player's further games are not queued while this
// many of their games wait (the scoring reads their 30 latest analysed games, so these renew most
// of that window); one prolific flagged player cannot grow the signal tier without bound. A game
// with an anomaly of its own replaces a waiting one without (queueAnalysis), so games flagged only
// through the player cannot keep one with evidence out.
const SIGNAL_JOBS_PER_PLAYER = 20;
// analysis.backlog() counts at most this many jobs per tier (one index range scan each).
const BACKLOG_COUNT_MAX = 100000;
const INTEGRITY_LEVELS = ['none', 'suspected', 'high_confidence', 'confirmed'];
const { GameStatus } = enums;

const GAME_SUMMARY_COLS = 'id, category, rated, base_ms, inc_ms, white_id, black_id, white_name, black_name, white_rating, '
    + 'black_rating, started_at, ended_at, status, reason, ply_count, white_before, white_after, black_before, black_after, '
    + 'rematch_of, flags';

// The filter of a player's games (games.listForUser / countForUser) on one colour's rows: ?1 the
// player, ?3 the category (NULL: any), ?4 rated 0/1 (NULL: any), and the status that a result
// filter asks for on that colour (?5 as White, ?6 as Black; NULL: any status, aborted included).
const userGamesFilter = (status) => `AND (?3 IS NULL OR category = ?3) AND (?4 IS NULL OR rated = ?4) AND (${status} IS NULL OR status = ${status})`;

/** games.listForUser: ?2 the exclusive id cursor, ?7 the limit (the other parameters: userGamesFilter). */
export const GAMES_FOR_USER_SQL = `SELECT ${GAME_SUMMARY_COLS} FROM games WHERE id IN (
    SELECT id FROM games WHERE white_id = ?1 AND id < ?2 ${userGamesFilter('?5')}
    UNION SELECT id FROM games WHERE black_id = ?1 AND id < ?2 ${userGamesFilter('?6')}
    ORDER BY id DESC LIMIT ?7) ORDER BY id DESC`;

/** games.countForUser with a filter (?2 unused, the other parameters: userGamesFilter). */
export const GAMES_COUNT_FOR_USER_SQL = `SELECT (SELECT count(*) FROM games WHERE white_id = ?1 ${userGamesFilter('?5')})
    + (SELECT count(*) FROM games WHERE black_id = ?1 ${userGamesFilter('?6')}) AS n`;

// The status a result filter needs on each colour (index 0: as White, 1: as Black).
const RESULT_STATUS = Object.freeze({
    win: [GameStatus.WhiteWins, GameStatus.BlackWins],
    loss: [GameStatus.BlackWins, GameStatus.WhiteWins],
    draw: [GameStatus.Draw, GameStatus.Draw],
});

const mBatchMs = metrics.histogram('scacelith_store_commit_batch_ms', 'Duration of one finished-games commit transaction',
    [1, 2, 5, 10, 25, 50, 100, 250, 1000]);
const mGames = metrics.counter('scacelith_store_games_committed_total', 'Finished games written to the database');
const mBusy = metrics.counter('scacelith_store_busy_total', 'Store operations that gave up waiting for the database lock');
// The same help text is registered by store/writer.js (the shard counts its writer thread's answers).
const mAnalysisSkipped = metrics.counter('scacelith_anticheat_analysis_skipped_total',
    'Finished rated games not queued for engine analysis (sample: ANALYSIS_SAMPLE_RATE, backlog: ANALYSIS_QUEUE_MAX reached, player: 20 flagged games of a player already waiting, displaced: a waiting flagged game without an anomaly of its own gave its place to a game with one)', ['reason']);

// Counts the games of finishBatch results left out of the analysis queue, or taken out of it.
function countSkipped(counter, results) {
    for (const x of results) {
        if (x.analysisSkipped) counter.labels(x.analysisSkipped).inc();
        if (x.analysisDisplaced) counter.labels('displaced').inc(x.analysisDisplaced.length);
    }
}

/** Priority of an analysis job: the highest waiting priority is analysed first (DESIGN.md 6.5). */
export const AnalysisPriority = Object.freeze({ ordinary: 0, signal: 1, report: 2, manual: 3 });

/** Error thrown by the store; `code` is a stable snake_case identifier. */
export class StoreError extends Error {
    /**
     * @param {string} code  'username_taken' | 'email_taken' | 'sso_taken' | 'duplicate' | 'invalid' |
     *   'invalid_record' | 'foreign_key' | 'not_found' | 'busy' | 'readonly' | 'no_rating_function' |
     *   'migration_checksum' | 'migration_missing' | 'migration_failed'
     * @param {string} [message]
     * @param {object} [extra] extra properties (cause, gameId...)
     */
    constructor(code, message, extra) {
        super(message || code);
        this.name = 'StoreError';
        this.code = code;
        if (extra) Object.assign(this, extra);
    }
}

// ---- small helpers ---------------------------------------------------------------------------

const b01 = (v) => (v ? 1 : 0);
const ms = (v) => (v === undefined || v === null ? null : Math.floor(Number(v)));
const orNull = (v) => (v === undefined ? null : v);

function asBuffer(v) {
    if (v === null || v === undefined) return null;
    if (typeof v === 'string' || Buffer.isBuffer(v)) return v;
    return Buffer.from(v.buffer, v.byteOffset, v.byteLength);
}

function toJson(v) { return v === undefined || v === null ? null : JSON.stringify(v); }
function fromJson(s) {
    if (s === null || s === undefined) return null;
    try { return JSON.parse(s); } catch { return s; }
}

/**
 * Normalized form of an e-mail address used for uniqueness and lookups: trimmed, lower-case.
 * Provider-specific rewrites (Gmail dots, +tags) are deliberately not applied.
 * @param {string|null} email
 * @returns {string|null}
 */
export function normalizeEmail(email) {
    if (email === null || email === undefined) return null;
    const s = String(email).trim().toLowerCase();
    return s || null;
}

function cleanEmail(email) {
    if (email === null || email === undefined) return null;
    return String(email).trim() || null;
}

// Typed array <-> little-endian BLOB (zero-copy on little-endian hosts).
function packArray(arr, Type) {
    if (arr === null || arr === undefined) return null;
    const t = arr instanceof Type ? arr : Type.from(arr);
    if (LE) return new Uint8Array(t.buffer, t.byteOffset, t.byteLength);
    const out = new Uint8Array(t.byteLength);
    const dv = new DataView(out.buffer);
    for (let i = 0; i < t.length; i++) {
        if (Type === Uint16Array) dv.setUint16(i * 2, t[i], true); else dv.setUint32(i * 4, t[i], true);
    }
    return out;
}

function unpackArray(u8, Type) {
    if (u8 === null || u8 === undefined) return new Type(0);
    const size = Type.BYTES_PER_ELEMENT;
    const n = Math.floor(u8.byteLength / size);
    if (LE) {
        if (u8.byteOffset % size === 0) return new Type(u8.buffer, u8.byteOffset, n);
        return new Type(u8.slice(0, n * size).buffer);
    }
    const out = new Type(n);
    const dv = new DataView(u8.buffer, u8.byteOffset, u8.byteLength);
    for (let i = 0; i < n; i++) out[i] = size === 2 ? dv.getUint16(i * 2, true) : dv.getUint32(i * 4, true);
    return out;
}

function sqliteCode(e) { return e && e.code === 'ERR_SQLITE_ERROR' ? e.errcode : -1; }

// Maps SQLite failures to StoreErrors (constraint kinds, lock timeouts); anything else unchanged.
function mapSqliteError(e) {
    if (e instanceof StoreError) return e;
    const c = sqliteCode(e);
    if (c < 0) return e;
    switch (c & 0xff) {
        case 5: case 6:     // SQLITE_BUSY, SQLITE_LOCKED
            mBusy.inc();
            return new StoreError('busy', 'database is locked', { cause: e });
        case 19:            // SQLITE_CONSTRAINT
            if (c === 2067 || c === 1555) return new StoreError('duplicate', e.message, { cause: e });
            if (c === 787) return new StoreError('foreign_key', e.message, { cause: e });
            return new StoreError('invalid', e.message, { cause: e });
        default:
            return e;
    }
}

function guard(fn) {
    try { return fn(); } catch (e) { throw mapSqliteError(e); }
}

function resolveDbFile(config, override) {
    const file = override ?? config.dbPath;
    if (!file || file === ':memory:' || path.basename(file) === ':memory:') return ':memory:';
    return file;
}

// ---- openStore -------------------------------------------------------------------------------

/**
 * Opens the database (it is not migrated: the primary calls migrate() at start-up).
 * @param {object} config  frozen configuration (src/config.js)
 * @param {object} [opts]
 * @param {boolean} [opts.readonly=false]
 * @param {Function} [opts.applyGame]  match/elo.js applyGame(white, black, score, cfg), required to
 *   commit rated games
 * @param {object} [opts.log]  logger (default: logger.child('store'))
 * @param {string} [opts.path]  database file overriding config.dbPath
 * @param {() => number} [opts.random]  uniform [0, 1) draw of ANALYSIS_SAMPLE_RATE (tests)
 * @returns {object} Store (DESIGN.md 5.5)
 */
export function openStore(config, opts = {}) {
    const { readonly = false, applyGame = null, log = logger.child('store'), random = Math.random } = opts;
    const file = resolveDbFile(config, opts.path);
    if (file !== ':memory:' && !readonly) fs.mkdirSync(path.dirname(file), { recursive: true });
    const db = new DatabaseSync(file, { readOnly: readonly });
    try {
        db.exec('PRAGMA busy_timeout = 5000');
        if (!readonly) {
            db.exec('PRAGMA journal_mode = WAL');
            db.exec('PRAGMA synchronous = FULL');
            db.exec('PRAGMA journal_size_limit = 67108864');
            // Deleted rows and erased columns (IP addresses, anonymized accounts) are overwritten
            // with zeros in the file, freed pages included, instead of staying readable in free
            // space until SQLite reuses it. FAST would leave the content of freed pages (a
            // purge of whole pages of expired sessions) in the file. No measurable cost on the
            // retention purge or on game commits (DESIGN.md 7).
            db.exec('PRAGMA secure_delete = ON');
        }
        db.exec('PRAGMA foreign_keys = ON');
        db.exec('PRAGMA temp_store = MEMORY');
        db.exec(`PRAGMA cache_size = ${-1024 * Math.max(2, Math.floor(config.dbCacheMb ?? 64))}`);
        if (file !== ':memory:') db.exec(`PRAGMA mmap_size = ${1048576 * Math.max(0, Math.floor(config.dbMmapMb ?? 256))}`);
    } catch (e) {
        db.close();
        throw mapSqliteError(e);
    }
    return createStore(db, config, { readonly, applyGame, log, file, random });
}

function createStore(db, config, { readonly, applyGame, log, file, random }) {
    const provisionalGames = config.provisionalGames ?? 30;
    const initialRating = config.initialRating ?? 1500;
    const analysisMinPlies = config.analysisMinPlies ?? 30;
    const analysisQueueMax = config.analysisQueueMax ?? 5000;
    const analysisSampleRate = config.analysisSampleRate ?? 1;
    const refundWindowMs = Math.max(0, Math.floor(Number(config.ratingRefundDays ?? 60))) * DAY;

    // Statement cache: SQL text -> prepared statement.
    const cache = new Map();
    const st = (sql) => {
        let s = cache.get(sql);
        if (s === undefined) {
            s = db.prepare(sql);
            cache.set(sql, s);
        }
        return s;
    };

    let depth = 0;
    let savepoints = 0;
    let closed = false;
    function tx(fn) {
        if (depth > 0) {
            const name = `sp${++savepoints}`;
            db.exec(`SAVEPOINT ${name}`);
            depth++;
            try {
                const r = fn();
                db.exec(`RELEASE ${name}`);
                return r;
            } catch (e) {
                try { db.exec(`ROLLBACK TO ${name}`); db.exec(`RELEASE ${name}`); } catch { /* already rolled back */ }
                throw e;
            } finally {
                depth--;
            }
        }
        try { db.exec('BEGIN IMMEDIATE'); } catch (e) { throw mapSqliteError(e); }
        depth = 1;
        try {
            const r = fn();
            db.exec('COMMIT');
            return r;
        } catch (e) {
            try { db.exec('ROLLBACK'); } catch { /* SQLite may have rolled back already */ }
            throw mapSqliteError(e);
        } finally {
            depth = 0;
        }
    }

    // Runs a chunked DELETE/UPDATE (?1 its cutoff, ?2 its LIMIT) in chunks of CHUNK rows until it
    // has nothing left.
    function chunked(sql, arg) {
        let total = 0;
        for (;;) {
            const n = Number(guard(() => st(sql).run(arg, CHUNK)).changes);
            total += n;
            if (n < CHUNK) return total;
        }
    }

    // ---- users -------------------------------------------------------------------------------

    const USER_COLS = 'id, username, email, email_verified, password_hash, mfa_enabled, mfa_secret_enc, mfa_pending_secret_enc, '
        + 'mfa_last_step, status, accept_challenges, created_at, last_login_at, deleted_at';
    const toUser = (r) => (r ? {
        id: r.id, username: r.username, email: r.email, emailVerified: !!r.email_verified, passwordHash: r.password_hash,
        mfaEnabled: !!r.mfa_enabled, mfaSecretEnc: asBuffer(r.mfa_secret_enc), pendingMfaSecretEnc: asBuffer(r.mfa_pending_secret_enc),
        mfaLastStep: r.mfa_last_step, status: r.status, acceptChallenges: !!r.accept_challenges, createdAt: r.created_at,
        lastLoginAt: r.last_login_at, deletedAt: r.deleted_at,
    } : null);

    function mapUserError(e) {
        const err = mapSqliteError(e);
        if (err instanceof StoreError && err.code === 'duplicate') {
            if (/username_lower/.test(err.message)) return new StoreError('username_taken', 'username already taken');
            if (/email_normalized/.test(err.message)) return new StoreError('email_taken', 'e-mail address already used');
        }
        return err;
    }

    function requireName(v) {
        if (typeof v !== 'string' || v.length === 0 || v.length > 64) throw new StoreError('invalid', 'username must be a non-empty string');
    }

    const USER_FIELDS = {
        username: (v) => { requireName(v); return [['username', v], ['username_lower', v.toLowerCase()]]; },
        email: (v) => [['email', cleanEmail(v)], ['email_normalized', normalizeEmail(v)]],
        emailVerified: (v) => [['email_verified', b01(v)]],
        passwordHash: (v) => [['password_hash', orNull(v)]],
        mfaEnabled: (v) => [['mfa_enabled', b01(v)]],
        mfaSecretEnc: (v) => [['mfa_secret_enc', orNull(v)]],
        pendingMfaSecretEnc: (v) => [['mfa_pending_secret_enc', orNull(v)]],
        mfaPendingSecretEnc: (v) => [['mfa_pending_secret_enc', orNull(v)]],
        mfaLastStep: (v) => [['mfa_last_step', Math.floor(v)]],
        status: (v) => [['status', v]],
        acceptChallenges: (v) => [['accept_challenges', b01(v)]],
        lastLoginAt: (v) => [['last_login_at', ms(v)]],
        deletedAt: (v) => [['deleted_at', ms(v)]],
    };

    const users = {
        create({ username, email = null, passwordHash = null, emailVerified = false, acceptChallenges = true, createdAt = Date.now() }) {
            requireName(username);
            try {
                const r = st(`INSERT INTO users (username, username_lower, email, email_normalized, email_verified, password_hash,
                    accept_challenges, created_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?)`)
                    .run(username, username.toLowerCase(), cleanEmail(email), normalizeEmail(email), b01(emailVerified),
                        orNull(passwordHash), b01(acceptChallenges), ms(createdAt));
                return Number(r.lastInsertRowid);
            } catch (e) {
                throw mapUserError(e);
            }
        },
        byId(id) { return toUser(st(`SELECT ${USER_COLS} FROM users WHERE id = ?`).get(id)); },
        byUsername(name) {
            if (typeof name !== 'string') return null;
            return toUser(st(`SELECT ${USER_COLS} FROM users WHERE username_lower = ?`).get(name.toLowerCase()));
        },
        byEmail(email) {
            const n = normalizeEmail(email);
            return n ? toUser(st(`SELECT ${USER_COLS} FROM users WHERE email_normalized = ?`).get(n)) : null;
        },
        byLogin(login) {
            if (typeof login !== 'string') return null;
            return login.includes('@') ? users.byEmail(login) : users.byUsername(login.trim());
        },
        /** Updates the given camelCase fields; returns true when the user exists. */
        update(id, fields) {
            const cols = [];
            const vals = [];
            for (const [k, v] of Object.entries(fields || {})) {
                const f = USER_FIELDS[k];
                if (!f) throw new StoreError('invalid', `users.update: unknown field ${k}`);
                for (const [c, x] of f(v)) { cols.push(`${c} = ?`); vals.push(x); }
            }
            if (!cols.length) return !!users.byId(id);
            try {
                return Number(st(`UPDATE users SET ${cols.join(', ')} WHERE id = ?`).run(...vals, id).changes) > 0;
            } catch (e) {
                throw mapUserError(e);
            }
        },
        anonymize(id, now = Date.now()) {
            return tx(() => {
                if (!st('SELECT 1 FROM users WHERE id = ?').get(id)) throw new StoreError('not_found', 'no such user');
                const name = `deleted#${id}`;
                const tokenHashes = st('SELECT token_hash FROM sessions WHERE user_id = ? AND revoked_at IS NULL').all(id)
                    .map((r) => asBuffer(r.token_hash));
                st('DELETE FROM sessions WHERE user_id = ?').run(id);
                st('DELETE FROM tokens WHERE user_id = ?').run(id);
                st('DELETE FROM sso_identities WHERE user_id = ?').run(id);
                st('DELETE FROM mfa_recovery_codes WHERE user_id = ?').run(id);
                st('DELETE FROM player_integrity WHERE user_id = ?').run(id);
                st('UPDATE security_events SET ip = NULL WHERE user_id = ? AND ip IS NOT NULL').run(id);
                st('UPDATE games SET white_name = ? WHERE white_id = ?').run(name, id);
                st('UPDATE games SET black_name = ? WHERE black_id = ?').run(name, id);
                st(`UPDATE users SET username = ?, username_lower = ?, email = NULL, email_normalized = NULL, email_verified = 0,
                    password_hash = NULL, mfa_enabled = 0, mfa_secret_enc = NULL, mfa_pending_secret_enc = NULL,
                    status = 'deleted', accept_challenges = 0, deleted_at = ? WHERE id = ?`).run(name, name, ms(now), id);
                return { tokenHashes };
            });
        },
        /** TOTP replay protection: true only when `step` is newer than the last accepted one. */
        advanceMfaStep(id, step) {
            return Number(st('UPDATE users SET mfa_last_step = ? WHERE id = ? AND mfa_last_step < ?').run(step, id, step).changes) === 1;
        },
    };

    // ---- MFA recovery codes -------------------------------------------------------------------

    const mfa = {
        replaceRecoveryCodes(userId, hashes, now = Date.now()) {
            tx(() => {
                st('DELETE FROM mfa_recovery_codes WHERE user_id = ?').run(userId);
                const ins = st('INSERT OR IGNORE INTO mfa_recovery_codes (user_id, code_hash, created_at) VALUES (?, ?, ?)');
                for (const h of hashes || []) ins.run(userId, h, ms(now));
            });
        },
        consumeRecoveryCode(userId, hash) {
            return Number(st('DELETE FROM mfa_recovery_codes WHERE user_id = ? AND code_hash = ?').run(userId, hash).changes) === 1;
        },
        countRecoveryCodes(userId) {
            return st('SELECT count(*) AS n FROM mfa_recovery_codes WHERE user_id = ?').get(userId).n;
        },
    };

    // ---- sessions -----------------------------------------------------------------------------

    const sessions = {
        create({ userId, tokenHash, createdAt = Date.now(), expiresAt, idleExpiresAt, clientLabel = null, ip = null }) {
            return guard(() => Number(st(`INSERT INTO sessions (user_id, token_hash, created_at, last_seen_at, expires_at,
                idle_expires_at, client_label, ip) VALUES (?, ?, ?, ?, ?, ?, ?, ?)`)
                .run(userId, tokenHash, ms(createdAt), ms(createdAt), ms(expiresAt), ms(idleExpiresAt ?? expiresAt),
                    orNull(clientLabel), orNull(ip)).lastInsertRowid));
        },
        byTokenHash(hash) {
            const r = st(`SELECT id, user_id, created_at, last_seen_at, expires_at, idle_expires_at, revoked_at
                FROM sessions WHERE token_hash = ?`).get(hash);
            return r ? {
                id: r.id, userId: r.user_id, createdAt: r.created_at, lastSeenAt: r.last_seen_at, expiresAt: r.expires_at,
                idleExpiresAt: r.idle_expires_at, revokedAt: r.revoked_at,
            } : null;
        },
        touch(id, now, idleExpiresAt) {
            st('UPDATE sessions SET last_seen_at = ?, idle_expires_at = ? WHERE id = ?').run(ms(now), ms(idleExpiresAt), id);
        },
        revoke(id, userId, now = Date.now()) {
            const r = userId === undefined || userId === null
                ? st('UPDATE sessions SET revoked_at = ? WHERE id = ? AND revoked_at IS NULL RETURNING token_hash').get(ms(now), id)
                : st('UPDATE sessions SET revoked_at = ? WHERE id = ? AND user_id = ? AND revoked_at IS NULL RETURNING token_hash')
                    .get(ms(now), id, userId);
            return r ? asBuffer(r.token_hash) : null;
        },
        revokeAllForUser(userId, exceptId = null, now = Date.now()) {
            return st(`UPDATE sessions SET revoked_at = ? WHERE user_id = ? AND revoked_at IS NULL AND id IS NOT ?
                RETURNING token_hash`).all(ms(now), userId, exceptId ?? null).map((r) => asBuffer(r.token_hash));
        },
        listForUser(userId) {
            return st(`SELECT id, created_at, last_seen_at, expires_at, idle_expires_at, client_label, ip FROM sessions
                WHERE user_id = ? AND revoked_at IS NULL ORDER BY last_seen_at DESC, id DESC`).all(userId).map((r) => ({
                id: r.id, createdAt: r.created_at, lastSeenAt: r.last_seen_at, expiresAt: r.expires_at,
                idleExpiresAt: r.idle_expires_at, clientLabel: r.client_label, ip: r.ip,
            }));
        },
        /** Every stored session of the user, revoked and expired ones included, newest first. */
        allForUser(userId) {
            return st(`SELECT id, created_at, last_seen_at, expires_at, idle_expires_at, revoked_at, client_label, ip FROM sessions
                WHERE user_id = ? ORDER BY created_at DESC, id DESC`).all(userId).map((r) => ({
                id: r.id, createdAt: r.created_at, lastSeenAt: r.last_seen_at, expiresAt: r.expires_at,
                idleExpiresAt: r.idle_expires_at, revokedAt: r.revoked_at, clientLabel: r.client_label, ip: r.ip,
            }));
        },
        enforceLimit(userId, max, now = Date.now()) {
            return tx(() => {
                const live = st(`SELECT id, token_hash FROM sessions WHERE user_id = ? AND revoked_at IS NULL AND expires_at > ?
                    AND idle_expires_at > ? ORDER BY created_at DESC, id DESC`).all(userId, ms(now), ms(now));
                const out = [];
                for (const r of live.slice(Math.max(0, max))) {
                    st('UPDATE sessions SET revoked_at = ? WHERE id = ?').run(ms(now), r.id);
                    out.push(asBuffer(r.token_hash));
                }
                return out;
            });
        },
    };

    // ---- single-use tokens ----------------------------------------------------------------------

    const toToken = (r) => (r ? {
        id: r.id, kind: r.kind, userId: r.user_id, data: fromJson(r.data), createdAt: r.created_at,
        expiresAt: r.expires_at, consumedAt: r.consumed_at,
    } : null);

    const tokens = {
        create({ kind, tokenHash, userId = null, data = null, expiresAt, createdAt = Date.now() }) {
            return guard(() => Number(st(`INSERT INTO tokens (kind, token_hash, user_id, data, created_at, expires_at)
                VALUES (?, ?, ?, ?, ?, ?)`).run(kind, tokenHash, orNull(userId), toJson(data), ms(createdAt), ms(expiresAt)).lastInsertRowid));
        },
        /** Atomic single use (one UPDATE): the row the first time, null afterwards or once expired. */
        consume(kind, tokenHash, now = Date.now()) {
            return toToken(st(`UPDATE tokens SET consumed_at = ?3 WHERE kind = ?1 AND token_hash = ?2 AND consumed_at IS NULL
                AND expires_at > ?3 RETURNING id, kind, user_id, data, created_at, expires_at, consumed_at`).get(kind, tokenHash, ms(now)));
        },
        get(kind, tokenHash) {
            return toToken(st(`SELECT id, kind, user_id, data, created_at, expires_at, consumed_at FROM tokens
                WHERE kind = ? AND token_hash = ?`).get(kind, tokenHash));
        },
        update(kind, tokenHash, data) {
            return Number(st('UPDATE tokens SET data = ? WHERE kind = ? AND token_hash = ?').run(toJson(data), kind, tokenHash).changes) > 0;
        },
        /** Deletes every token of `kind` of the user (consumed or not); returns how many. */
        deleteForUser(userId, kind) {
            return guard(() => Number(st('DELETE FROM tokens WHERE user_id = ? AND kind = ?').run(userId, kind).changes));
        },
        /** The newest token of `kind` of the user that is neither consumed nor expired, or null. */
        liveForUser(userId, kind, now = Date.now()) {
            return toToken(guard(() => st(`SELECT id, kind, user_id, data, created_at, expires_at, consumed_at FROM tokens
                WHERE user_id = ? AND kind = ? AND consumed_at IS NULL AND expires_at > ? ORDER BY created_at DESC, id DESC LIMIT 1`)
                .get(userId, kind, ms(now))));
        },
    };

    // ---- single sign-on identities ----------------------------------------------------------------

    const sso = {
        find(provider, subject) {
            const r = st('SELECT user_id, email FROM sso_identities WHERE provider = ? AND subject = ?').get(provider, String(subject));
            return r ? { userId: r.user_id, email: r.email } : null;
        },
        link(userId, provider, subject, email = null, now = Date.now()) {
            const r = guard(() => st(`INSERT INTO sso_identities (provider, subject, user_id, email, created_at) VALUES (?, ?, ?, ?, ?)
                ON CONFLICT (provider, subject) DO UPDATE SET email = excluded.email WHERE user_id = excluded.user_id`)
                .run(provider, String(subject), userId, cleanEmail(email), ms(now)));
            if (Number(r.changes) === 0) throw new StoreError('sso_taken', 'identity linked to another account');
        },
        forUser(userId) {
            return st('SELECT provider, subject, email, created_at FROM sso_identities WHERE user_id = ? ORDER BY created_at').all(userId)
                .map((r) => ({ provider: r.provider, subject: r.subject, email: r.email, createdAt: r.created_at }));
        },
    };

    // ---- ratings -------------------------------------------------------------------------------

    const RATING_COLS = 'rating, games, wins, draws, losses, peak, reached_senior, rated, counted_games, unrated_games, unrated_opponents, '
        + 'unrated_half_points';
    const defaultRating = () => ({
        rating: initialRating, games: 0, wins: 0, draws: 0, losses: 0, peak: initialRating, reachedSenior: false,
        rated: false, countedGames: 0, unratedGames: 0, unratedOpponents: 0, unratedHalfPoints: 0,
    });
    // counted_games is NULL in a record stored before it existed (the header).
    const toRating = (r) => ({
        rating: r.rating, games: r.games, wins: r.wins, draws: r.draws, losses: r.losses, peak: r.peak, reachedSenior: !!r.reached_senior,
        rated: !!r.rated, countedGames: r.counted_games ?? (r.rated ? r.games : r.unrated_games), unratedGames: r.unrated_games,
        unratedOpponents: r.unrated_opponents, unratedHalfPoints: r.unrated_half_points,
    });
    // Shown as "1500?": unrated, or fewer than PROVISIONAL_GAMES counted games (K = 40).
    const provisionalOf = (rec) => !rec.rated || rec.countedGames < provisionalGames;
    function readRating(userId, category) {
        const r = st(`SELECT ${RATING_COLS} FROM ratings WHERE user_id = ? AND category = ?`).get(userId, category);
        return r ? toRating(r) : defaultRating();
    }
    function writeRating(userId, category, rec, now) {
        st(`INSERT INTO ratings (user_id, category, ${RATING_COLS}, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            ON CONFLICT (user_id, category) DO UPDATE SET rating = excluded.rating, games = excluded.games, wins = excluded.wins,
            draws = excluded.draws, losses = excluded.losses, peak = excluded.peak, reached_senior = excluded.reached_senior,
            rated = excluded.rated, counted_games = excluded.counted_games, unrated_games = excluded.unrated_games,
            unrated_opponents = excluded.unrated_opponents, unrated_half_points = excluded.unrated_half_points, updated_at = excluded.updated_at`)
            .run(userId, category, rec.rating, rec.games, rec.wins, rec.draws, rec.losses, rec.peak, b01(rec.reachedSenior), b01(rec.rated),
                rec.countedGames, rec.unratedGames, rec.unratedOpponents, rec.unratedHalfPoints, now);
    }

    const ratings = {
        get(userId, category) { return readRating(userId, category); },
        forUser(userId) {
            return st(`SELECT category, ${RATING_COLS}, updated_at FROM ratings WHERE user_id = ? ORDER BY category`).all(userId)
                .map((r) => {
                    const rec = toRating(r);
                    return { category: r.category, ...rec, provisional: provisionalOf(rec), updatedAt: r.updated_at };
                });
        },
        leaderboard(category, limit = 100, minGames = provisionalGames) {
            return st(`SELECT r.user_id, u.username, r.rating, r.games, r.wins, r.draws, r.losses, r.peak
                FROM ratings r JOIN users u ON u.id = r.user_id LEFT JOIN player_integrity pi ON pi.user_id = r.user_id
                WHERE r.category = ? AND r.rated = 1 AND coalesce(r.counted_games, r.games) >= ? AND u.status = 'active'
                AND (pi.level IS NULL OR pi.level <> 'confirmed')
                ORDER BY r.rating DESC, r.games DESC, r.user_id LIMIT ?`).all(category, minGames, limit)
                .map((r) => ({ userId: r.user_id, username: r.username, rating: r.rating, games: r.games, wins: r.wins, draws: r.draws,
                    losses: r.losses, peak: r.peak }));
        },
    };

    // ---- games ---------------------------------------------------------------------------------

    function toGame(r, full) {
        const g = {
            id: r.id, category: r.category, rated: !!r.rated, baseMs: r.base_ms, incMs: r.inc_ms, whiteId: r.white_id,
            blackId: r.black_id, whiteName: r.white_name, blackName: r.black_name, whiteRating: r.white_rating,
            blackRating: r.black_rating, startedAt: r.started_at, endedAt: r.ended_at, status: r.status, reason: r.reason,
            plyCount: r.ply_count, rematchOf: r.rematch_of, flags: r.flags,
            ratingChanges: r.white_before === null ? null : {
                white: { before: r.white_before, after: r.white_after },
                black: { before: r.black_before, after: r.black_after },
            },
        };
        if (full) {
            g.moves = unpackArray(r.moves, Uint16Array);
            g.spentMs = unpackArray(r.spent, Uint32Array);
            g.clockMs = unpackArray(r.clocks, Uint32Array);
        }
        return g;
    }

    function checkRecord(r) {
        const bad = (why) => new StoreError('invalid_record', `game ${r?.id}: invalid ${why}`, { gameId: r?.id });
        if (!r || !Number.isSafeInteger(r.id) || r.id <= 0) throw bad('id');
        if (!Number.isSafeInteger(r.whiteId) || !Number.isSafeInteger(r.blackId)) throw bad('players');
        if (r.status !== GameStatus.WhiteWins && r.status !== GameStatus.BlackWins && r.status !== GameStatus.Draw
            && r.status !== GameStatus.Aborted) throw bad('status');
        if (typeof r.category !== 'string' || !r.category) throw bad('category');
        if (!Number.isInteger(r.reason ?? 0)) throw bad('reason');
    }

    // The rating record written after a game: applyGame's record, completed from the previous one
    // for any field it does not return (a record without `rated` comes from a rating function
    // without the unrated phase, which rates and counts every game).
    function nextRecord(prev, side, score) {
        const rec = (side && side.record) || {};
        const after = Math.round(side.after ?? rec.rating);
        const rated = rec.rated ?? true;
        return {
            rating: Math.round(rec.rating ?? after),
            games: rec.games ?? prev.games + 1,
            wins: rec.wins ?? prev.wins + (score === 1 ? 1 : 0),
            draws: rec.draws ?? prev.draws + (score === 0.5 ? 1 : 0),
            losses: rec.losses ?? prev.losses + (score === 0 ? 1 : 0),
            peak: Math.round(rec.peak ?? Math.max(prev.peak, after)),
            reachedSenior: !!(rec.reachedSenior ?? prev.reachedSenior),
            rated: !!rated,
            countedGames: Math.floor(rec.countedGames ?? prev.countedGames + 1),
            unratedGames: rated ? 0 : Math.floor(rec.unratedGames ?? 0),
            unratedOpponents: rated ? 0 : Math.floor(rec.unratedOpponents ?? 0),
            unratedHalfPoints: rated ? 0 : Math.floor(rec.unratedHalfPoints ?? 0),
        };
    }

    function storedChanges(row) {
        if (row.white_before === null) return null;
        const w = readRating(row.white_id, row.category);
        const b = readRating(row.black_id, row.category);
        return {
            white: { before: row.white_before, after: row.white_after, games: w.games, provisional: provisionalOf(w) },
            black: { before: row.black_before, after: row.black_after, games: b.games, provisional: provisionalOf(b) },
        };
    }

    // The K factor applyGame reports for one side (0: no K-formula change), NULL when it reports none.
    const kOf = (side) => (Number.isInteger(side?.k) ? side.k : null);

    // Whether player `userId` already has SIGNAL_JOBS_PER_PLAYER signal jobs waiting (two range
    // scans of the partial indexes of migration 003, at most that many entries each). The literal
    // 'queued' and 1 match those indexes' WHERE clause (SQLite needs the constants to use them).
    function signalCapReached(userId) {
        return st(`SELECT count(*) AS n FROM (SELECT 1 FROM analysis_jobs WHERE white_id = ?1 AND status = 'queued' AND priority = 1
            UNION ALL SELECT 1 FROM analysis_jobs WHERE black_id = ?1 AND status = 'queued' AND priority = 1 LIMIT ?2)`)
            .get(userId, SIGNAL_JOBS_PER_PLAYER).n >= SIGNAL_JOBS_PER_PLAYER;
    }

    // The oldest waiting signal job of player `userId` whose game has no non-info anomaly of its
    // own (of that game's players since its start, as at its end), or null when every such job has
    // one: the job that a game with an anomaly of its own displaces when the player's cap is reached.
    function displaceableSignalJob(userId) {
        return st(`SELECT j.game_id AS gameId, j.white_id AS whiteId, j.black_id AS blackId FROM analysis_jobs j
            JOIN games g ON g.id = j.game_id
            WHERE j.game_id IN (SELECT game_id FROM analysis_jobs WHERE white_id = ?1 AND status = 'queued' AND priority = 1
                UNION ALL SELECT game_id FROM analysis_jobs WHERE black_id = ?1 AND status = 'queued' AND priority = 1)
            AND NOT EXISTS (SELECT 1 FROM anomalies a WHERE a.user_id IN (j.white_id, j.black_id) AND a.at >= g.started_at
                AND a.game_id = j.game_id AND a.severity <> 'info')
            ORDER BY j.queued_at, j.game_id LIMIT 1`).get(userId) ?? null;
    }

    // Analysis queue policy of a finished game (header, DESIGN.md 6.5): a suspicion signal queues
    // it ahead of the ordinary games, under the cap of SIGNAL_JOBS_PER_PLAYER waiting signal jobs
    // per player; an ordinary game is drawn with ANALYSIS_SAMPLE_RATE and queued only while fewer
    // than ANALYSIS_QUEUE_MAX ordinary jobs wait. `batch.ordinary` caches that count for the rest
    // of the transaction (the write lock is held, nobody else changes it). Sets
    // entry.analysisSkipped to why the game was left out ('sample' | 'backlog' | 'player'), and
    // entry.analysisDisplaced to the games whose waiting job it took over.
    function queueAnalysis(r, now, batch, entry) {
        const s = st(`SELECT EXISTS (SELECT 1 FROM anomalies WHERE user_id IN (?1, ?2) AND at >= ?4 AND game_id = ?5 AND severity <> 'info')
            AS own, EXISTS (SELECT 1 FROM player_integrity WHERE user_id IN (?1, ?2) AND level <> 'none')
            OR EXISTS (SELECT 1 FROM reports WHERE reported_id IN (?1, ?2) AND created_at >= ?3 AND status = 'open'
                AND category <> 'abuse' AND weight >= ?6) AS player`).get(r.whiteId, r.blackId, now - REPORT_SIGNAL_MS,
            ms(r.startedAt ?? r.endedAt ?? now), r.id, REPORT_SIGNAL_MIN_WEIGHT);
        const insert = (priority) => st(`INSERT OR IGNORE INTO analysis_jobs (game_id, queued_at, priority, white_id, black_id)
            VALUES (?, ?, ?, ?, ?)`).run(r.id, now, priority, r.whiteId, r.blackId);
        if (s.own || s.player) {
            // Never sampled out or capped by ANALYSIS_QUEUE_MAX, and never demoted to 'ordinary':
            // ordinary jobs are the random sample of the population statistics.
            const capped = [r.whiteId, r.blackId].filter(signalCapReached);
            if (capped.length) {
                // A game flagged only through its players is not queued past the cap. A game with
                // an anomaly of its own takes over the oldest waiting job, without an anomaly of its
                // own, of each capped player (the count stays at the cap): junk games can never
                // push a game with evidence out. Skipped only when every job waiting has some.
                if (!s.own) { entry.analysisSkipped = 'player'; return; }
                // Both players capped: the job taken from the first may be one of the second's too.
                // Nothing is removed before a job is found for each.
                const out = [];
                for (const p of capped) {
                    if (out.some((v) => v.whiteId === p || v.blackId === p)) continue;
                    const v = displaceableSignalJob(p);
                    if (!v) { entry.analysisSkipped = 'player'; return; }
                    out.push(v);
                }
                for (const v of out) st('DELETE FROM analysis_jobs WHERE game_id = ?').run(v.gameId);
                entry.analysisDisplaced = out.map((v) => v.gameId);
            }
            insert(AnalysisPriority.signal);
            return;
        }
        if (analysisSampleRate < 1 && !(random() < analysisSampleRate)) { entry.analysisSkipped = 'sample'; return; }
        if (batch.ordinary === null) {
            batch.ordinary = st(`SELECT count(*) AS n FROM (SELECT 1 FROM analysis_jobs WHERE status = 'queued' AND priority = ?
                LIMIT ?)`).get(AnalysisPriority.ordinary, analysisQueueMax).n;
        }
        if (batch.ordinary >= analysisQueueMax) { entry.analysisSkipped = 'backlog'; return; }
        insert(AnalysisPriority.ordinary);
        batch.ordinary++;
    }

    function commitGame(r, now, batch) {
        checkRecord(r);
        const existing = st('SELECT white_id, black_id, category, white_before, white_after, black_before, black_after FROM games WHERE id = ?')
            .get(r.id);
        if (existing) {
            if (existing.white_id !== r.whiteId || existing.black_id !== r.blackId) {
                log.warn('game id already stored for other players: this game is not stored', { gameId: r.id });
            }
            return { gameId: r.id, duplicate: true, ratings: storedChanges(existing) };
        }
        const played = r.status !== GameStatus.Aborted;
        const rate = !!r.rated && played && r.category !== 'custom';
        let changes = null;
        let k = [null, null];
        if (rate) {
            if (typeof applyGame !== 'function') {
                throw new StoreError('no_rating_function', 'openStore(config, { applyGame }) is required to commit rated games');
            }
            const score = r.status === GameStatus.WhiteWins ? 1 : r.status === GameStatus.BlackWins ? 0 : 0.5;
            const w = readRating(r.whiteId, r.category);
            const b = readRating(r.blackId, r.category);
            const res = applyGame({ ...w }, { ...b }, score, config);
            const wRec = nextRecord(w, res.white, score);
            const bRec = nextRecord(b, res.black, 1 - score);
            writeRating(r.whiteId, r.category, wRec, now);
            writeRating(r.blackId, r.category, bRec, now);
            changes = {
                white: { before: Math.round(res.white.before ?? w.rating), after: wRec.rating, games: wRec.games,
                    provisional: provisionalOf(wRec) },
                black: { before: Math.round(res.black.before ?? b.rating), after: bRec.rating, games: bRec.games,
                    provisional: provisionalOf(bRec) },
            };
            k = [kOf(res.white), kOf(res.black)];
        }
        const moves = packArray(r.moves ?? [], Uint16Array);
        const plies = r.moves ? r.moves.length : 0;
        st(`INSERT INTO games (id, category, rated, base_ms, inc_ms, white_id, black_id, white_name, black_name, white_rating,
            black_rating, started_at, ended_at, status, reason, ply_count, white_before, white_after, black_before, black_after,
            white_k, black_k, rematch_of, flags, moves, spent, clocks)
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)`)
            .run(r.id, r.category, b01(r.rated), ms(r.baseMs ?? 0), ms(r.incMs ?? 0), r.whiteId, r.blackId,
                String(r.whiteName ?? ''), String(r.blackName ?? ''), ms(r.whiteRating), ms(r.blackRating),
                ms(r.startedAt ?? r.endedAt ?? now), ms(r.endedAt ?? now), r.status, r.reason ?? 0, plies,
                changes ? changes.white.before : null, changes ? changes.white.after : null,
                changes ? changes.black.before : null, changes ? changes.black.after : null, k[0], k[1],
                r.rematchOf ? r.rematchOf : null, r.flags ?? 0,
                moves, packArray(r.spentMs, Uint32Array), packArray(r.clockMs, Uint32Array));
        if (rate) refundAtCommit(r, changes, k, now, batch);
        const entry = { gameId: r.id, ratings: changes };
        if (rate && plies >= analysisMinPlies) queueAnalysis(r, now, batch, entry);
        return entry;
    }

    // The refund of a rated game recorded while the opponent of the player who lost points is a
    // confirmed cheater under an active ban for cheating that refunds (header). A ban's own
    // refunds (applyForCheater, run when it is given) only see the games already in the table;
    // this gives those of the games in progress at the ban or still on their way to the database.
    // The ban taken is one whose refund window (RATING_REFUND_DAYS before its start) covers the
    // game's end. A ban that refunds is told by its source and reason, as anticheat/refunds.js
    // banRefunds does (not imported: the store does not depend on the anti-cheat): an automatic
    // ban of a certain cheat ('certain_cheat:<kind>'), or a moderator's integrity confirm without
    // --no-refund ('confirmed: <reason>'). Neither a `user ban` nor a confirm with --no-refund
    // ('confirmed, no refund: <reason>') refunds, whatever the player's integrity level. Adds the
    // refunds to batch.refunds, which finishBatch logs once committed.
    function refundAtCommit(r, changes, k, now, batch) {
        if (refundWindowMs <= 0) return;
        const endedAt = ms(r.endedAt ?? now);
        const sides = [[r.whiteId, r.blackId, changes.white, k[0]], [r.blackId, r.whiteId, changes.black, k[1]]];
        for (const [victim, cheaterId, change, kf] of sides) {
            const points = change.before - change.after;
            if (!(points > 0) || kf === 0) continue;
            const ban = st(`SELECT s.id FROM player_integrity pi JOIN sanctions s ON s.user_id = pi.user_id
                WHERE pi.user_id = ?1 AND pi.level = 'confirmed' AND s.kind = 'ban' AND s.lifted_at IS NULL AND s.starts_at <= ?2
                AND (s.ends_at IS NULL OR s.ends_at > ?2) AND s.starts_at <= ?3
                AND ((s.source = 'auto' AND instr(s.reason, 'certain_cheat:') = 1) OR (s.source = 'moderator' AND instr(s.reason, 'confirmed: ') = 1))
                ORDER BY s.starts_at, s.id LIMIT 1`)
                .get(cheaterId, now, endedAt + refundWindowMs);
            if (!ban) continue;
            const given = giveRefund({ id: r.id, victim, category: r.category, points, ended_at: endedAt }, cheaterId, now, ban.id, 'auto', null);
            if (!given) continue;
            st(`INSERT INTO security_events (kind, user_id, ip, at, detail) VALUES ('rating_refund', ?, NULL, ?, ?)`).run(victim, now,
                toJson({ refundId: given.id, gameId: r.id, cheaterId, category: r.category, points, source: 'auto', sanctionId: ban.id, by: null }));
            batch.refunds.push({ ...given, cheaterId, sanctionId: ban.id });
        }
    }

    const games = {
        /**
         * Commits finished games in ONE transaction: game rows, both ratings of each rated game (read
         * and written inside the transaction) and analysis jobs. Idempotent per game id.
         */
        finishBatch(records) {
            if (!Array.isArray(records) || records.length === 0) return [];
            const t0 = performance.now();
            const now = Date.now();
            const batch = { ordinary: null, refunds: [] };
            const out = tx(() => records.map((r) => commitGame(r, now, batch)));
            for (const f of batch.refunds) {
                log.security('rating.refund', { cheaterId: f.cheaterId, source: 'auto', sanctionId: f.sanctionId, gameId: f.gameId,
                    refunds: 1, victims: 1, points: f.points });
            }
            mBatchMs.observe(performance.now() - t0);
            mGames.inc(out.reduce((n, x) => n + (x.duplicate ? 0 : 1), 0));
            countSkipped(mAnalysisSkipped, out);
            return out;
        },
        byId(id) {
            const r = st('SELECT * FROM games WHERE id = ?').get(id);
            return r ? toGame(r, true) : null;
        },
        /** The largest game id, 0 without games (a shard's new ids come after it: util/ids.js). */
        lastId() {
            return st('SELECT max(id) AS id FROM games').get().id ?? 0;
        },
        /** Newest first; `before` is a game id (exclusive cursor). */
        recentForUser(userId, limit = 20, before = Number.MAX_SAFE_INTEGER) {
            return st(`SELECT ${GAME_SUMMARY_COLS} FROM games WHERE id IN (
                SELECT id FROM games WHERE white_id = ?1 AND id < ?2 UNION SELECT id FROM games WHERE black_id = ?1 AND id < ?2
                ORDER BY id DESC LIMIT ?3) ORDER BY id DESC`).all(userId, before ?? Number.MAX_SAFE_INTEGER, limit)
                .map((r) => toGame(r, false));
        },
        countBetween(a, b, since = 0, { rated = false } = {}) {
            const sql = `SELECT count(*) AS n FROM games WHERE ((white_id = ?1 AND black_id = ?2) OR (white_id = ?2 AND black_id = ?1))
                AND ended_at >= ?3${rated ? ' AND rated = 1' : ''}`;
            return st(sql).get(a, b, ms(since)).n;
        },
        /**
         * A player's games matching a filter, newest first (summaries, as recentForUser).
         * @param {number} userId
         * @param {{ before?: number|null, limit?: number, category?: string|null, rated?: boolean|null,
         *   result?: 'win'|'loss'|'draw'|null }} [opts]  before: exclusive game id cursor
         */
        listForUser(userId, { before = null, limit = 20, ...filter } = {}) {
            const f = userFilter(filter);
            const cursor = before ?? Number.MAX_SAFE_INTEGER;
            const n = Math.max(0, Math.floor(limit));
            if (!f) return games.recentForUser(userId, n, cursor);
            return st(GAMES_FOR_USER_SQL).all(userId, cursor, f.category, f.rated, f.white, f.black, n).map((r) => toGame(r, false));
        },
        /** A player's games (both colours); with a filter (listForUser's), those matching it. */
        countForUser(userId, filter = null) {
            const f = userFilter(filter);
            if (f) return st(GAMES_COUNT_FOR_USER_SQL).get(userId, null, f.category, f.rated, f.white, f.black).n;
            return st('SELECT (SELECT count(*) FROM games WHERE white_id = ?1) + (SELECT count(*) FROM games WHERE black_id = ?1) AS n')
                .get(userId).n;
        },
    };

    // The parameters of userGamesFilter, or null when the filter keeps every game.
    function userFilter(filter) {
        const { category = null, rated = null, result = null } = filter || {};
        if (result !== null && !Object.hasOwn(RESULT_STATUS, result)) throw new StoreError('invalid', `unknown result filter ${result}`);
        if (category === null && rated === null && result === null) return null;
        const status = result === null ? [null, null] : RESULT_STATUS[result];
        return { category: category === null ? null : String(category), rated: rated === null ? null : b01(rated), white: status[0], black: status[1] };
    }

    // ---- conduct -------------------------------------------------------------------------------

    const conduct = {
        record(userId, kind, at = Date.now()) {
            guard(() => st('INSERT INTO conduct_events (user_id, kind, at) VALUES (?, ?, ?)').run(userId, kind, ms(at)));
        },
        countSince(userId, since) {
            const out = { abandon: 0, abort: 0, noshow: 0 };
            for (const r of st('SELECT kind, count(*) AS n FROM conduct_events WHERE user_id = ? AND at >= ? GROUP BY kind').all(userId, ms(since))) {
                out[r.kind] = r.n;
            }
            return out;
        },
        /** The user's conduct events, newest first: [{ kind, at }]. */
        forUser(userId, limit = 1000) {
            return st('SELECT kind, at FROM conduct_events WHERE user_id = ? ORDER BY at DESC, id DESC LIMIT ?').all(userId, limit)
                .map((r) => ({ kind: r.kind, at: r.at }));
        },
        cooldown(userId) {
            const r = st('SELECT cooldown_until, level, updated_at FROM conduct_state WHERE user_id = ?').get(userId);
            return r ? { until: r.cooldown_until, level: r.level, updatedAt: r.updated_at } : { until: 0, level: 0, updatedAt: 0 };
        },
        setCooldown(userId, until, level, now = Date.now()) {
            guard(() => st(`INSERT INTO conduct_state (user_id, cooldown_until, level, updated_at) VALUES (?, ?, ?, ?)
                ON CONFLICT (user_id) DO UPDATE SET cooldown_until = excluded.cooldown_until, level = excluded.level,
                updated_at = excluded.updated_at`).run(userId, ms(until) ?? 0, level ?? 0, ms(now)));
        },
    };

    // ---- sanctions -----------------------------------------------------------------------------

    const SANCTION_COLS = 'id, user_id, kind, reason, source, game_id, starts_at, ends_at, created_at, created_by, lifted_at, lifted_by';
    const toSanction = (r) => (r ? {
        id: r.id, userId: r.user_id, kind: r.kind, reason: r.reason, source: r.source, gameId: r.game_id, startsAt: r.starts_at,
        endsAt: r.ends_at, createdAt: r.created_at, createdBy: r.created_by, liftedAt: r.lifted_at, liftedBy: r.lifted_by,
    } : null);

    const sanctions = {
        create({ userId, kind, reason = null, source = 'moderator', gameId = null, startsAt = Date.now(), endsAt = null,
            createdBy = null, createdAt = Date.now() }) {
            return guard(() => Number(st(`INSERT INTO sanctions (user_id, kind, reason, source, game_id, starts_at, ends_at, created_at,
                created_by) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)`)
                .run(userId, kind, orNull(reason), source, gameId || null, ms(startsAt), ms(endsAt), ms(createdAt),
                    createdBy === null || createdBy === undefined ? null : String(createdBy)).lastInsertRowid));
        },
        /** The active ban lasting longest (a permanent one first), or null. */
        activeBan(userId, now = Date.now()) {
            return toSanction(st(`SELECT ${SANCTION_COLS} FROM sanctions WHERE user_id = ?1 AND kind = 'ban' AND lifted_at IS NULL
                AND starts_at <= ?2 AND (ends_at IS NULL OR ends_at > ?2) ORDER BY ends_at IS NULL DESC, ends_at DESC LIMIT 1`)
                .get(userId, ms(now)));
        },
        active(userId, now = Date.now()) {
            return st(`SELECT ${SANCTION_COLS} FROM sanctions WHERE user_id = ?1 AND lifted_at IS NULL AND starts_at <= ?2
                AND (ends_at IS NULL OR ends_at > ?2) ORDER BY starts_at`).all(userId, ms(now)).map(toSanction);
        },
        list(userId) {
            return st(`SELECT ${SANCTION_COLS} FROM sanctions WHERE user_id = ? ORDER BY created_at DESC, id DESC`).all(userId).map(toSanction);
        },
        lift(id, by = null, now = Date.now()) {
            return Number(st('UPDATE sanctions SET lifted_at = ?, lifted_by = ? WHERE id = ? AND lifted_at IS NULL')
                .run(ms(now), by === null || by === undefined ? null : String(by), id).changes) === 1;
        },
    };

    // ---- anomalies and security events ------------------------------------------------------------

    const anomalies = {
        /** One transaction for the batch; returns the number of rows written. */
        insertBatch(list) {
            if (!list || !list.length) return 0;
            tx(() => {
                const ins = st('INSERT INTO anomalies (user_id, game_id, kind, severity, at, detail) VALUES (?, ?, ?, ?, ?, ?)');
                for (const a of list) {
                    ins.run(a.userId || null, a.gameId || null, String(a.kind), a.severity ?? 'info', ms(a.at ?? Date.now()), toJson(a.detail));
                }
            });
            return list.length;
        },
        forUser(userId, limit = 100) {
            return st(`SELECT id, user_id, game_id, kind, severity, at, detail FROM anomalies WHERE user_id = ?
                ORDER BY at DESC, id DESC LIMIT ?`).all(userId, limit).map((r) => ({
                id: r.id, userId: r.user_id, gameId: r.game_id, kind: r.kind, severity: r.severity, at: r.at, detail: fromJson(r.detail),
            }));
        },
    };

    const SECURITY_DELETE_SQL = 'DELETE FROM security_events WHERE id IN (SELECT id FROM security_events WHERE at < ?1 LIMIT ?2)';
    const SECURITY_IP_SQL = `UPDATE security_events SET ip = NULL WHERE id IN (SELECT id FROM security_events
        WHERE ip IS NOT NULL AND at < ?1 LIMIT ?2)`;

    const security = {
        insertBatch(list) {
            if (!list || !list.length) return 0;
            tx(() => {
                const ins = st('INSERT INTO security_events (kind, user_id, ip, at, detail) VALUES (?, ?, ?, ?, ?)');
                for (const e of list) ins.run(String(e.kind), e.userId || null, orNull(e.ip), ms(e.at ?? Date.now()), toJson(e.detail));
            });
            return list.length;
        },
        forUser(userId, limit = 100) {
            return st(`SELECT id, kind, user_id, ip, at, detail FROM security_events WHERE user_id = ? ORDER BY at DESC, id DESC LIMIT ?`)
                .all(userId, limit).map((r) => ({ id: r.id, kind: r.kind, userId: r.user_id, ip: r.ip, at: r.at, detail: fromJson(r.detail) }));
        },
        /** Deletes events older than RETENTION_SECURITY_DAYS, erases IPs older than RETENTION_IP_DAYS. */
        purge(now = Date.now(), cfg = config) {
            // IPs first, as in retention.run (see retentionSteps).
            const ipErased = chunked(SECURITY_IP_SQL, ms(now) - cfg.retentionIpDays * DAY);
            const deleted = chunked(SECURITY_DELETE_SQL, ms(now) - cfg.retentionSecurityDays * DAY);
            return { deleted, ipErased };
        },
    };

    // ---- analysis queue ----------------------------------------------------------------------------

    const toJob = (r) => ({
        gameId: r.game_id, priority: r.priority, attempts: r.attempts, queuedAt: r.queued_at, startedAt: r.started_at, worker: r.worker,
    });
    const { WhiteWins, BlackWins, Draw } = GameStatus;
    const CLAIM_SQL = (where, order) => `UPDATE analysis_jobs SET status = 'running', worker = ?1, started_at = ?2,
        attempts = attempts + 1 WHERE game_id IN (SELECT game_id FROM analysis_jobs WHERE ${where} ORDER BY ${order} LIMIT ?3)
        RETURNING game_id, priority, attempts, queued_at, started_at, worker`;
    const CLAIM_ANY_SQL = CLAIM_SQL("status = 'queued'", 'priority DESC, queued_at, game_id');
    const CLAIM_ORDINARY_SQL = CLAIM_SQL(`status = 'queued' AND priority = ${AnalysisPriority.ordinary}`, 'queued_at, game_id');
    let claims = 0;     // jobs claimed through this store (the reserved ordinary share of next())

    const analysis = {
        /**
         * Claims up to `limit` queued jobs atomically for `workerId`: the highest priority, then the
         * oldest first, except that every ORDINARY_SHARE-th claim of this store takes the oldest
         * ordinary job first when one waits (ordinary games are never starved by the others).
         */
        next(limit = 1, workerId = null, now = Date.now()) {
            const t = ms(now);
            const n = Math.max(0, Math.floor(limit));
            const w = workerId === null || workerId === undefined ? null : String(workerId);
            const rows = tx(() => {
                st(`UPDATE analysis_jobs SET status = CASE WHEN attempts >= ?1 THEN 'failed' ELSE 'queued' END, worker = NULL,
                    error = 'stale: worker vanished', finished_at = CASE WHEN attempts >= ?1 THEN ?2 ELSE NULL END
                    WHERE status = 'running' AND started_at < ?3`).run(ANALYSIS_MAX_ATTEMPTS, t, t - ANALYSIS_STALE_MS);
                // Claims number claims .. claims + n - 1; those numbered ORDINARY_SHARE - 1 modulo
                // ORDINARY_SHARE are the reserved ones.
                const reserved = Math.floor((claims + n) / ORDINARY_SHARE) - Math.floor(claims / ORDINARY_SHARE);
                const out = reserved > 0 ? st(CLAIM_ORDINARY_SQL).all(w, t, reserved) : [];
                if (out.length < n) out.push(...st(CLAIM_ANY_SQL).all(w, t, n - out.length));
                return out;
            });
            claims += rows.length;
            return rows.map(toJob).sort((a, b) => b.priority - a.priority || a.queuedAt - b.queuedAt || a.gameId - b.gameId);
        },
        complete(gameId, features, now = Date.now()) {
            return Number(st(`UPDATE analysis_jobs SET status = 'done', features = ?, finished_at = ?, error = NULL, worker = NULL
                WHERE game_id = ?`).run(toJson(features), ms(now), gameId).changes) === 1;
        },
        /** Re-queues the job, or marks it failed once it was tried ANALYSIS_MAX_ATTEMPTS times. */
        fail(gameId, error, now = Date.now()) {
            return tx(() => {
                const r = st('SELECT attempts FROM analysis_jobs WHERE game_id = ?').get(gameId);
                if (!r) return null;
                const status = r.attempts >= ANALYSIS_MAX_ATTEMPTS ? 'failed' : 'queued';
                st(`UPDATE analysis_jobs SET status = ?, error = ?, worker = NULL, finished_at = ? WHERE game_id = ?`)
                    .run(status, error === null || error === undefined ? null : String(error).slice(0, 2000),
                        status === 'failed' ? ms(now) : null, gameId);
                return status;
            });
        },
        /** Moderator request: (re-)analyses any stored game, before every other job (priority 'manual'). */
        enqueue(gameId, now = Date.now()) {
            guard(() => st(`INSERT INTO analysis_jobs (game_id, queued_at, priority, white_id, black_id)
                VALUES (?1, ?2, ?3, (SELECT white_id FROM games WHERE id = ?1), (SELECT black_id FROM games WHERE id = ?1))
                ON CONFLICT (game_id) DO UPDATE SET status = 'queued', attempts = 0, worker = NULL, started_at = NULL,
                finished_at = NULL, error = NULL, queued_at = excluded.queued_at, priority = excluded.priority,
                white_id = excluded.white_id, black_id = excluded.black_id`).run(gameId, ms(now), AnalysisPriority.manual));
        },
        /**
         * Makes sure a game the automatic policy analyses (rated, official category, played out,
         * >= ANALYSIS_MIN_PLIES plies) gets analysed with at least the priority of `reason`
         * ('signal' | 'report' | 'manual'): queued when it has no job (left out by the policy),
         * its priority raised while it waits, a failed job queued again. A running or done job is
         * left alone. 'signal' is refused, like at the end of a game, while either player has
         * SIGNAL_JOBS_PER_PLAYER signal jobs waiting. Returns true when a job was queued or changed.
         */
        request(gameId, reason = 'report', now = Date.now()) {
            // 'ordinary' is refused: it would bypass ANALYSIS_QUEUE_MAX.
            const priority = Object.hasOwn(AnalysisPriority, reason) ? AnalysisPriority[reason] : 0;
            if (priority <= AnalysisPriority.ordinary) throw new StoreError('invalid', `analysis.request: unknown or ordinary priority ${reason}`);
            return tx(() => {
                if (priority === AnalysisPriority.signal) {
                    const g = st('SELECT white_id, black_id FROM games WHERE id = ?').get(gameId);
                    if (!g || signalCapReached(g.white_id) || signalCapReached(g.black_id)) return false;
                }
                return Number(guard(() => st(`INSERT INTO analysis_jobs (game_id, queued_at, priority, white_id, black_id)
                    SELECT id, ?2, ?3, white_id, black_id FROM games WHERE id = ?1 AND rated = 1 AND category <> 'custom'
                        AND status IN (?5, ?6, ?7) AND ply_count >= ?4
                    ON CONFLICT (game_id) DO UPDATE SET priority = max(priority, excluded.priority), status = 'queued',
                        attempts = CASE WHEN status = 'failed' THEN 0 ELSE attempts END,
                        error = CASE WHEN status = 'failed' THEN NULL ELSE error END, finished_at = NULL,
                        white_id = excluded.white_id, black_id = excluded.black_id
                    WHERE status = 'failed' OR (status = 'queued' AND priority < excluded.priority)`)
                    .run(gameId, ms(now), priority, analysisMinPlies, WhiteWins, BlackWins, Draw)).changes) === 1;
            });
        },
        /**
         * Jobs waiting: { ordinary, priority } (priority: every job above 'ordinary'), each counted
         * up to BACKLOG_COUNT_MAX (the metrics gauges read it on the primary's event loop).
         */
        backlog() {
            const r = st(`SELECT (SELECT count(*) FROM (SELECT 1 FROM analysis_jobs WHERE status = 'queued' AND priority = ?1 LIMIT ?2))
                AS ordinary, (SELECT count(*) FROM (SELECT 1 FROM analysis_jobs WHERE status = 'queued' AND priority > ?1 LIMIT ?2))
                AS priority`).get(AnalysisPriority.ordinary, BACKLOG_COUNT_MAX);
            return { ordinary: r.ordinary, priority: r.priority };
        },
        forUser(userId, limit = 50) {
            return st(`SELECT a.game_id, a.status, a.attempts, a.finished_at, a.error, a.features, g.category, g.white_id, g.ended_at,
                g.ply_count FROM games g JOIN analysis_jobs a ON a.game_id = g.id WHERE g.id IN (
                SELECT id FROM games WHERE white_id = ?1 UNION SELECT id FROM games WHERE black_id = ?1)
                ORDER BY g.id DESC LIMIT ?2`).all(userId, limit).map((r) => ({
                gameId: r.game_id, status: r.status, attempts: r.attempts, finishedAt: r.finished_at, error: r.error,
                features: fromJson(r.features), category: r.category, color: r.white_id === userId ? 'white' : 'black',
                endedAt: r.ended_at, plyCount: r.ply_count,
            }));
        },
        stats() {
            const out = { queued: 0, running: 0, done: 0, failed: 0 };
            for (const r of st('SELECT status, count(*) AS n FROM analysis_jobs GROUP BY status').all()) out[r.status] = r.n;
            return out;
        },
    };

    // ---- integrity ---------------------------------------------------------------------------------

    const toIntegrity = (r) => ({
        level: r.level, score: r.score, evidence: fromJson(r.evidence), updatedAt: r.updated_at, reviewedBy: r.reviewed_by,
        reviewedAt: r.reviewed_at, note: r.note,
    });
    const LEVEL_RANK = "(CASE pi.level WHEN 'none' THEN 0 WHEN 'suspected' THEN 1 WHEN 'high_confidence' THEN 2 ELSE 3 END)";

    function statsOf(u) {
        if (u.n !== undefined && u.mean !== undefined) return { n: Math.floor(u.n), mean: +u.mean, m2: +(u.m2 ?? 0) };
        const values = u.values ?? (u.value !== undefined ? [u.value] : []);
        let n = 0, mean = 0, m2 = 0;
        for (const v of values) {
            const x = +v;
            if (!Number.isFinite(x)) continue;
            n++;
            const d = x - mean;
            mean += d / n;
            m2 += d * (x - mean);
        }
        return { n, mean, m2 };
    }

    const integrity = {
        get(userId) {
            const r = st(`SELECT level, score, evidence, updated_at, reviewed_by, reviewed_at, note FROM player_integrity
                WHERE user_id = ?`).get(userId);
            return r ? toIntegrity(r) : { level: 'none', score: 0, evidence: null, updatedAt: 0, reviewedBy: null, reviewedAt: null, note: null };
        },
        /** Upserts the given fields (level, score, evidence, reviewedBy, reviewedAt, note, updatedAt). */
        set(userId, fields = {}) {
            if (fields.level !== undefined && !INTEGRITY_LEVELS.includes(fields.level)) {
                throw new StoreError('invalid', `integrity level must be one of ${INTEGRITY_LEVELS.join(', ')}`);
            }
            tx(() => {
                const cur = integrity.get(userId);
                const pick = (k) => (fields[k] !== undefined ? fields[k] : cur[k]);
                const reviewedBy = pick('reviewedBy');
                guard(() => st(`INSERT INTO player_integrity (user_id, level, score, evidence, updated_at, reviewed_by, reviewed_at, note)
                    VALUES (?, ?, ?, ?, ?, ?, ?, ?) ON CONFLICT (user_id) DO UPDATE SET level = excluded.level, score = excluded.score,
                    evidence = excluded.evidence, updated_at = excluded.updated_at, reviewed_by = excluded.reviewed_by,
                    reviewed_at = excluded.reviewed_at, note = excluded.note`)
                    .run(userId, pick('level'), +pick('score') || 0, toJson(pick('evidence')), ms(fields.updatedAt ?? Date.now()),
                        reviewedBy === null || reviewedBy === undefined ? null : String(reviewedBy),
                        ms(pick('reviewedAt')), orNull(pick('note'))));
            });
        },
        listFlagged(minLevel = 'suspected', limit = 100) {
            const rank = INTEGRITY_LEVELS.indexOf(minLevel);
            if (rank < 0) throw new StoreError('invalid', 'unknown integrity level');
            return st(`SELECT pi.user_id, u.username, pi.level, pi.score, pi.evidence, pi.updated_at, pi.reviewed_by, pi.reviewed_at, pi.note
                FROM player_integrity pi JOIN users u ON u.id = pi.user_id WHERE pi.level <> 'none' AND ${LEVEL_RANK} >= ?
                ORDER BY ${LEVEL_RANK} DESC, pi.score DESC LIMIT ?`).all(Math.max(1, rank), limit)
                .map((r) => ({ userId: r.user_id, username: r.username, ...toIntegrity(r) }));
        },
        /** Population statistics of one rating bucket: (category, ratingBucket) or a key prefix. */
        populationStats(categoryOrPrefix, ratingBucket) {
            let prefix;
            if (categoryOrPrefix && typeof categoryOrPrefix === 'object') prefix = `${categoryOrPrefix.category}|${categoryOrPrefix.ratingBucket}`;
            else prefix = ratingBucket === undefined ? String(categoryOrPrefix) : `${categoryOrPrefix}|${ratingBucket}`;
            const out = {};
            // '|' is 0x7C and '}' 0x7D: the range holds exactly the keys starting with prefix + '|'.
            for (const r of st('SELECT key, n, mean, m2, updated_at FROM population_stats WHERE key > ? AND key < ?').all(prefix + '|', prefix + '}')) {
                const variance = r.n > 1 ? r.m2 / (r.n - 1) : 0;
                out[r.key.slice(prefix.length + 1)] = { n: r.n, mean: r.mean, m2: r.m2, variance, stdev: Math.sqrt(variance), updatedAt: r.updated_at };
            }
            return out;
        },
        /** Merges observations into the running statistics (one transaction). */
        updatePopulation(a, b, c) {
            let list, now;
            if (typeof a === 'string') {
                const sample = typeof b === 'number' ? { value: b } : Array.isArray(b) ? { values: b } : (b || {});
                list = [{ key: a, ...sample }];
                now = c ?? Date.now();
            } else {
                list = Array.isArray(a) ? a : [a];
                now = b ?? Date.now();
            }
            tx(() => {
                for (const u of list) {
                    const key = u.key ?? `${u.category}|${u.ratingBucket}|${u.metric}`;
                    const add = statsOf(u);
                    if (!add.n) continue;
                    const cur = st('SELECT n, mean, m2 FROM population_stats WHERE key = ?').get(key) || { n: 0, mean: 0, m2: 0 };
                    const n = cur.n + add.n;
                    const d = add.mean - cur.mean;
                    const mean = cur.mean + (d * add.n) / n;
                    const m2 = cur.m2 + add.m2 + (d * d * cur.n * add.n) / n;
                    st(`INSERT INTO population_stats (key, n, mean, m2, updated_at) VALUES (?, ?, ?, ?, ?) ON CONFLICT (key) DO UPDATE SET
                        n = excluded.n, mean = excluded.mean, m2 = excluded.m2, updated_at = excluded.updated_at`).run(key, n, mean, m2, ms(now));
                }
            });
        },
    };

    // ---- reports -------------------------------------------------------------------------------------

    const REPORT_COLS = 'r.id, r.reporter_id, r.reported_id, r.game_id, r.category, r.weight, r.status, r.created_at, r.resolved_at, '
        + 'r.resolved_by, r.comment';
    const toReport = (r) => ({
        id: r.id, reporterId: r.reporter_id, reportedId: r.reported_id, gameId: r.game_id || null, category: r.category,
        weight: r.weight, status: r.status, createdAt: r.created_at, resolvedAt: r.resolved_at, resolvedBy: r.resolved_by,
        comment: r.comment, reporterName: r.reporter_name, reportedName: r.reported_name,
    });

    const reports = {
        create({ reporterId, reportedId, gameId = 0, category, comment = null, weight = 1, at = Date.now() }) {
            return guard(() => Number(st(`INSERT INTO reports (reporter_id, reported_id, game_id, category, comment, weight, created_at)
                VALUES (?, ?, ?, ?, ?, ?, ?)`).run(reporterId, reportedId, gameId || 0, category, orNull(comment), +weight, ms(at)).lastInsertRowid));
        },
        countByReporterSince(reporterId, since) {
            return st('SELECT count(*) AS n FROM reports WHERE reporter_id = ? AND created_at >= ?').get(reporterId, ms(since)).n;
        },
        exists(reporterId, reportedId, gameId = 0) {
            return !!st('SELECT 1 FROM reports WHERE reporter_id = ? AND reported_id = ? AND game_id = ?').get(reporterId, reportedId, gameId || 0);
        },
        listOpen(limit = 50) {
            return st(`SELECT ${REPORT_COLS}, a.username AS reporter_name, b.username AS reported_name FROM reports r
                JOIN users a ON a.id = r.reporter_id JOIN users b ON b.id = r.reported_id
                WHERE r.status = 'open' ORDER BY r.created_at, r.id LIMIT ?`).all(limit).map(toReport);
        },
        forReported(userId, limit = 200) {
            return st(`SELECT ${REPORT_COLS}, a.username AS reporter_name, b.username AS reported_name FROM reports r
                JOIN users a ON a.id = r.reporter_id JOIN users b ON b.id = r.reported_id
                WHERE r.reported_id = ? ORDER BY r.created_at DESC, r.id DESC LIMIT ?`).all(userId, limit).map(toReport);
        },
        /** The reports the user filed, newest first, with reportedName and outcome (null while open). */
        forReporter(userId, limit = 500) {
            return st(`SELECT ${REPORT_COLS}, a.username AS reporter_name, b.username AS reported_name FROM reports r
                JOIN users a ON a.id = r.reporter_id JOIN users b ON b.id = r.reported_id
                WHERE r.reporter_id = ? ORDER BY r.created_at DESC, r.id DESC LIMIT ?`).all(userId, limit)
                .map((r) => ({ ...toReport(r), outcome: r.status === 'open' ? null : r.status }));
        },
        /** outcome: 'actioned' | 'dismissed'; only an open report changes (returns true then). */
        resolve(id, outcome, by = null, now = Date.now()) {
            if (outcome !== 'actioned' && outcome !== 'dismissed') throw new StoreError('invalid', "outcome must be 'actioned' or 'dismissed'");
            return Number(st(`UPDATE reports SET status = ?, resolved_at = ?, resolved_by = ? WHERE id = ? AND status = 'open'`)
                .run(outcome, ms(now), by === null || by === undefined ? null : String(by), id).changes) === 1;
        },
    };

    // ---- rating refunds ----------------------------------------------------------------------------

    const REFUND_COLS = 'f.id, f.game_id, f.victim_id, f.cheater_id, f.category, f.points, f.created_at, f.sanction_id, f.source, '
        + 'f.created_by, f.notified_at';
    const toRefund = (r) => ({
        id: r.id, gameId: r.game_id, victimId: r.victim_id, cheaterId: r.cheater_id, category: r.category, points: r.points,
        createdAt: r.created_at, sanctionId: r.sanction_id, source: r.source, createdBy: r.created_by, notifiedAt: r.notified_at,
        victimName: r.victim_name, cheaterName: r.cheater_name,
    });

    // Gives a victim back the points they lost in game g ({ id, victim, category, points,
    // ended_at }) against a cheater, on their current record of its category (the peak rises
    // with it). Nothing when the game already has its refund for that victim (UNIQUE (game_id,
    // victim_id)) or the victim has no record. Returns the refund given, or null.
    function giveRefund(g, cheaterId, now, sanctionId, source, by) {
        const rec = st('SELECT rating, peak FROM ratings WHERE user_id = ? AND category = ?').get(g.victim, g.category);
        if (!rec) return null;
        const ins = st(`INSERT INTO rating_refunds (game_id, victim_id, cheater_id, category, points, created_at, sanction_id,
            source, created_by) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?) ON CONFLICT (game_id, victim_id) DO NOTHING`)
            .run(g.id, g.victim, cheaterId, g.category, g.points, ms(now), sanctionId || null, source,
                by === null || by === undefined ? null : String(by));
        if (Number(ins.changes) !== 1) return null;
        const rating = rec.rating + g.points;
        st('UPDATE ratings SET rating = ?, peak = ?, updated_at = ? WHERE user_id = ? AND category = ?')
            .run(rating, Math.max(rec.peak, rating), ms(now), g.victim, g.category);
        return { id: Number(ins.lastInsertRowid), gameId: g.id, victimId: g.victim, category: g.category, points: g.points,
            endedAt: g.ended_at };
    }

    const refunds = {
        /**
         * Refunds the victims of a banned cheater (one transaction; header). The games are the
         * cheater's rated games that ended at `since` or later in which the opponent's rating fell
         * by the K formula. Returns [{ id, gameId, victimId, category, points, endedAt }].
         */
        applyForCheater({ cheaterId, since = 0, now = Date.now(), sanctionId = null, source = 'moderator', by = null }) {
            return tx(() => {
                const games = st(`SELECT id, category, ended_at, black_id AS victim, black_before - black_after AS points, black_k AS k
                    FROM games WHERE white_id = ?1 AND ended_at >= ?2 AND rated = 1 AND black_before IS NOT NULL
                    UNION ALL SELECT id, category, ended_at, white_id, white_before - white_after, white_k
                    FROM games WHERE black_id = ?1 AND ended_at >= ?2 AND rated = 1 AND white_before IS NOT NULL
                    ORDER BY 1`).all(cheaterId, ms(since));
                const out = [];
                for (const g of games) {
                    if (!(g.points > 0) || g.k === 0 || g.victim === cheaterId) continue;
                    const given = giveRefund(g, cheaterId, now, sanctionId, source, by);
                    if (given) out.push(given);
                }
                return out;
            });
        },
        /** Newest first: the refunds of a cheater's games, those of a victim, or all of them. */
        list({ cheaterId = null, victimId = null, limit = 100 } = {}) {
            const where = cheaterId !== null ? 'f.cheater_id = ?1' : victimId !== null ? 'f.victim_id = ?1' : '?1 IS NULL';
            return st(`SELECT ${REFUND_COLS}, v.username AS victim_name, c.username AS cheater_name FROM rating_refunds f
                JOIN users v ON v.id = f.victim_id JOIN users c ON c.id = f.cheater_id WHERE ${where} ORDER BY f.id DESC LIMIT ?2`)
                .all(cheaterId ?? victimId, limit).map(toRefund);
        },
        /** Refunds not notified yet, with an id above `afterId`, oldest first: [{ id, victimId, points }]. */
        pendingSince(afterId = 0, limit = 1000) {
            return st(`SELECT id, victim_id, points FROM rating_refunds WHERE notified_at IS NULL AND id > ? ORDER BY id LIMIT ?`)
                .all(afterId, limit).map((r) => ({ id: r.id, victimId: r.victim_id, points: r.points }));
        },
        /** A victim's refunds not notified yet: { ids, points } (the partial index of migration 004). */
        pendingFor(victimId) {
            const rows = st('SELECT id, points FROM rating_refunds WHERE victim_id = ? AND notified_at IS NULL').all(victimId);
            return { ids: rows.map((r) => r.id), points: rows.reduce((n, r) => n + r.points, 0) };
        },
        /** Marks refunds notified (those not marked yet); returns how many changed. */
        markNotified(ids, now = Date.now()) {
            if (!ids || !ids.length) return 0;
            return tx(() => {
                let n = 0;
                const up = st('UPDATE rating_refunds SET notified_at = ? WHERE id = ? AND notified_at IS NULL');
                for (const id of ids) n += Number(up.run(ms(now), id).changes);
                return n;
            });
        },
    };

    // ---- retention ------------------------------------------------------------------------------------

    // The purge as a list of chunked statements (?1 the cutoff, ?2 the LIMIT; one short autocommit
    // transaction per execution), each run again until it touches fewer rows than its LIMIT;
    // `count` names the total it adds to. An IP column is set to NULL, its row stays as long as it
    // is needed. IPs are erased before old rows are deleted: a deletion makes SQLite rebalance the
    // table's pages, which copies the neighbouring rows and can leave stale copies of them in the
    // unused part of a page, where secure_delete does not reach (DESIGN.md 7); erased first, the
    // old rows next to the deleted ones hold no IP any more. In the steady state (a run every
    // hour) the rows old enough to be deleted lost their IP at an earlier run.
    function retentionSteps(t, cfg) {
        const ipBefore = t - cfg.retentionIpDays * DAY;
        const securityBefore = t - cfg.retentionSecurityDays * DAY;
        return [
            { count: 'ipErased', arg: ipBefore, sql: `UPDATE sessions SET ip = NULL WHERE id IN (SELECT id FROM sessions
                WHERE ip IS NOT NULL AND created_at < ?1 LIMIT ?2)` },
            { count: 'ipErased', arg: ipBefore, sql: SECURITY_IP_SQL },
            { count: 'sessions', arg: t, sql: `DELETE FROM sessions WHERE id IN (SELECT id FROM sessions
                WHERE min(expires_at, idle_expires_at) <= ?1 LIMIT ?2)` },
            { count: 'sessions', arg: t - REVOKED_SESSION_TTL_MS, sql: `DELETE FROM sessions WHERE id IN (SELECT id FROM sessions
                WHERE revoked_at IS NOT NULL AND revoked_at <= ?1 LIMIT ?2)` },
            { count: 'tokens', arg: t, sql: 'DELETE FROM tokens WHERE id IN (SELECT id FROM tokens WHERE expires_at <= ?1 LIMIT ?2)' },
            { count: 'securityEvents', arg: securityBefore, sql: SECURITY_DELETE_SQL },
            { count: 'anomalies', arg: securityBefore, sql: `DELETE FROM anomalies WHERE id IN (SELECT id FROM anomalies
                WHERE severity <> 'certain' AND at < ?1 LIMIT ?2)` },
            { count: 'conductEvents', arg: t - CONDUCT_EVENT_TTL_MS, sql: `DELETE FROM conduct_events WHERE id IN (SELECT id
                FROM conduct_events WHERE at < ?1 LIMIT ?2)` },
            { count: 'analysisJobs', arg: t - ANALYSIS_FAILED_TTL_MS, sql: `DELETE FROM analysis_jobs WHERE game_id IN (SELECT game_id
                FROM analysis_jobs WHERE status = 'failed' AND finished_at < ?1 LIMIT ?2)` },
        ];
    }
    const retentionCounts = () => ({ sessions: 0, tokens: 0, securityEvents: 0, anomalies: 0, conductEvents: 0, analysisJobs: 0, ipErased: 0 });

    // LIMIT of the next statement of a runAsync step, from the one just run (`rows` touched in
    // `took` ms): shrunk in proportion when it took longer than targetMs, doubled (up to CHUNK)
    // when a full chunk took less than half of it.
    function nextChunk(limit, rows, took, targetMs) {
        if (took > targetMs) return Math.max(CHUNK_MIN, Math.min(limit, Math.floor(limit * targetMs / took)));
        if (rows >= limit && took < targetMs / 2) return Math.min(CHUNK, limit * 2);
        return limit;
    }

    const retention = {
        /** Periodic clean-up in one go (chunked, short transactions). Returns what it removed. */
        run(now = Date.now(), cfg = config) {
            const counts = retentionCounts();
            for (const step of retentionSteps(ms(now), cfg)) counts[step.count] += chunked(step.sql, step.arg);
            log.debug('retention done', counts);
            return counts;
        },
        /**
         * The same clean-up in slices, so that a large purge never stalls the process nor keeps
         * the database's write lock from the other writers (the shards' main threads write
         * sessions, security events and reports synchronously). Every step starts with a LIMIT of
         * CHUNK_START rows and adapts it after each statement so that one statement takes about
         * sliceMs / 2 (between CHUNK_MIN and CHUNK rows). Whenever sliceMs of work has been done it
         * awaits pause() (default: a timer of sliceMs), which keeps its share of the time, and of
         * the write lock, at about one half. Stops before the next statement once `signal` is
         * aborted or the store closed (what was already deleted stays deleted). Resolves to the
         * counts of run(); a failure rejects with the error, which carries the counts done so far
         * as `err.counts`.
         * @param {number} [now]
         * @param {object} [cfg]
         * @param {{ sliceMs?: number, signal?: AbortSignal, pause?: () => Promise<void>, chunk?: number,
         *   clock?: () => number }} [opts]  chunk: a fixed LIMIT instead of the adaptive one, and
         *   clock: the milliseconds clock (default performance.now) (tests)
         */
        async runAsync(now = Date.now(), cfg = config, { sliceMs = 10, signal = null, pause = null, chunk = null, clock = () => performance.now() } = {}) {
            const counts = retentionCounts();
            const rest = pause ?? (() => sleep(sliceMs));
            const fixed = chunk === null || chunk === undefined ? 0 : Math.max(1, Math.floor(chunk));
            let sliceStart = clock();
            try {
                for (const step of retentionSteps(ms(now), cfg)) {
                    let limit = fixed || CHUNK_START;
                    for (;;) {
                        if (closed || signal?.aborted) return counts;
                        const used = limit;
                        const t0 = clock();
                        const n = Number(guard(() => st(step.sql).run(step.arg, used)).changes);
                        const t1 = clock();
                        counts[step.count] += n;
                        if (!fixed) limit = nextChunk(used, n, t1 - t0, sliceMs / 2);
                        if (t1 - sliceStart >= sliceMs) {
                            await rest();
                            sliceStart = clock();
                        }
                        if (n < used) break;
                    }
                }
            } catch (e) {
                if (e && typeof e === 'object') e.counts = counts;
                throw e;
            }
            return counts;
        },
    };

    // ---- meta --------------------------------------------------------------------------------------------

    const meta = {
        get(key) {
            const r = st('SELECT value FROM meta WHERE key = ?').get(key);
            return r ? r.value : null;
        },
        set(key, value) {
            st('INSERT INTO meta (key, value) VALUES (?, ?) ON CONFLICT (key) DO UPDATE SET value = excluded.value').run(key, String(value));
        },
    };

    const store = {
        meta, users, mfa, sessions, tokens, sso, ratings, games, conduct, sanctions, anomalies, security, analysis, integrity,
        reports, refunds, retention,
        readonly,
        path: file,
        /** Runs fn() in one write transaction (BEGIN IMMEDIATE; nested calls use savepoints). */
        transaction(fn) { return tx(fn); },
        close() {
            if (closed) return;
            closed = true;
            if (!readonly) { try { db.exec('PRAGMA optimize'); } catch { /* best effort */ } }
            cache.clear();
            db.close();
        },
    };
    Object.defineProperty(store, DB, { value: db });
    return store;
}

/**
 * The query plan of a statement on a store's own connection (tests and diagnostics): the rows of
 * EXPLAIN QUERY PLAN, e.g. { id, parent, detail: 'SEARCH games USING INDEX games_white (...)' }.
 * @param {object} store  an open Store
 * @param {string} sql
 * @param {Array<*>} [params]
 * @returns {Array<{ id: number, parent: number, detail: string }>}
 */
export function explainQueryPlan(store, sql, params = []) {
    const db = store && store[DB];
    if (!db) throw new TypeError('explainQueryPlan: not a store');
    return guard(() => db.prepare(`EXPLAIN QUERY PLAN ${sql}`).all(...params)).map((r) => ({ id: r.id, parent: r.parent, detail: r.detail }));
}

// ---- migrations --------------------------------------------------------------------------------

function listMigrations(dir) {
    const out = [];
    for (const name of fs.readdirSync(dir)) {
        const m = /^(\d{3,})_([a-z0-9_]+)\.sql$/.exec(name);
        if (!m) continue;
        // Line endings are normalised so that a checkout with CRLF (Windows) keeps the checksum.
        const sql = fs.readFileSync(path.join(dir, name), 'utf8').replace(/\r\n/g, '\n');
        out.push({ version: parseInt(m[1], 10), name: `${m[1]}_${m[2]}`, sql, checksum: crypto.createHash('sha256').update(sql).digest('hex') });
    }
    out.sort((a, b) => a.version - b.version);
    for (let i = 1; i < out.length; i++) {
        if (out[i].version === out[i - 1].version) throw new StoreError('migration_failed', `two migrations with version ${out[i].version}`);
    }
    return out;
}

/**
 * Applies the pending migrations (src/store/migrations/NNN_name.sql) in order, each in its own
 * BEGIN IMMEDIATE transaction recorded in schema_migrations, after verifying that no applied
 * migration changed (StoreError 'migration_checksum') or disappeared ('migration_missing': the
 * database belongs to a newer server). Creates meta 'server_id' on the first run. Safe to run
 * from several processes at once.
 * @param {object} store  an open, writable Store
 * @param {object} [opts]
 * @param {string} [opts.dir]  migrations directory (tests)
 * @param {number} [opts.now]
 * @returns {{ applied: number[], version: number }}
 */
export function migrate(store, { dir = MIGRATIONS_DIR, now = Date.now() } = {}) {
    const db = store && store[DB];
    if (!db) throw new TypeError('migrate: not a store');
    if (store.readonly) throw new StoreError('readonly', 'cannot migrate a read-only store');
    const files = listMigrations(dir);
    const byVersion = new Map(files.map((m) => [m.version, m]));
    try {
        db.exec(`CREATE TABLE IF NOT EXISTS schema_migrations (version INTEGER PRIMARY KEY, name TEXT NOT NULL,
            applied_at INTEGER NOT NULL, checksum TEXT NOT NULL)`);
    } catch (e) {
        throw mapSqliteError(e);
    }
    const verify = (row) => {
        const m = byVersion.get(row.version);
        if (!m) throw new StoreError('migration_missing', `migration ${row.name} is applied but unknown to this server version (database from a newer server?)`);
        if (m.checksum !== row.checksum) {
            throw new StoreError('migration_checksum', `migration ${row.name} changed after it was applied (checksum ${row.checksum.slice(0, 12)} != ${m.checksum.slice(0, 12)}); refusing to start`);
        }
    };
    for (const row of db.prepare('SELECT version, name, checksum FROM schema_migrations ORDER BY version').all()) verify(row);
    const applied = [];
    for (const m of files) {
        store.transaction(() => {
            const row = db.prepare('SELECT version, name, checksum FROM schema_migrations WHERE version = ?').get(m.version);
            if (row) { verify(row); return; }     // applied meanwhile by another process
            try {
                db.exec(m.sql);
            } catch (e) {
                throw new StoreError('migration_failed', `migration ${m.name} failed: ${e.message}`, { cause: e });
            }
            db.prepare('INSERT INTO schema_migrations (version, name, applied_at, checksum) VALUES (?, ?, ?, ?)')
                .run(m.version, m.name, Math.floor(now), m.checksum);
            applied.push(m.version);
        });
    }
    if (db.prepare("SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'meta'").get()) {
        db.prepare("INSERT OR IGNORE INTO meta (key, value) VALUES ('server_id', ?)").run(crypto.randomUUID());
    }
    const version = files.length ? files[files.length - 1].version : 0;
    return { applied, version };
}
