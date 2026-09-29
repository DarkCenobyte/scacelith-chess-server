// Store: the SQLite persistence of the dedicated server (DESIGN.md 5.5 and 7).
//
// One synchronous connection per process (`node:sqlite` DatabaseSync). The primary and every
// shard open the same file: WAL lets readers run beside the single writer, `busy_timeout` makes a
// writer wait for the lock instead of failing, and every read-then-write goes through
// BEGIN IMMEDIATE (the write lock is taken first, so a transaction never has to upgrade a stale
// read snapshot, which SQLite would refuse with SQLITE_BUSY_SNAPSHOT). Transactions stay short:
// the retention job deletes in chunks of CHUNK rows. Every statement is prepared once, on first
// use (so a store can be opened before migrate() created the tables), and cached.
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
//     they have >= ANALYSIS_MIN_PLIES plies), under the queue policy below. An invalid record
//     throws StoreError 'invalid_record' (with .gameId) and the whole batch is rolled back. The
//     entry of a game left out of the queue carries analysisSkipped: 'sample' | 'backlog'.
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
//   - ratings.leaderboard(category, limit, minGames) leaves out deleted accounts and players whose
//     integrity level is 'confirmed'.
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
//     signal, report, manual) and next() takes the highest first, then the oldest. At the end of
//     a game, a suspicion signal (either player's integrity level above 'none', an open
//     cheating/other report of weight >= 0.5 against either player in the last 30 days, a
//     non-info anomaly recorded in this game) queues it as 'signal'. An ordinary game is drawn
//     with ANALYSIS_SAMPLE_RATE and queued only while fewer than ANALYSIS_QUEUE_MAX ordinary jobs
//     wait; otherwise it is not inserted at all. analysis.request(gameId, reason, now) queues an
//     eligible game for a report (or raises the priority of its waiting job, or re-queues a failed
//     one); enqueue() is a moderator request (priority 'manual'). analysis.backlog() counts the
//     waiting jobs { ordinary, priority }.
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
//     retention.runAsync(now, cfg, { sliceMs, signal, pause }) runs the same statements but
//     returns to the event loop whenever a slice of sliceMs is used up (the primary runs it), and
//     stops early when the AbortSignal fires or the store is closed.
//   - store.transaction(fn): runs fn in one BEGIN IMMEDIATE transaction (nested calls use
//     savepoints), for callers that need several store calls to be atomic.
//   - JSON columns (tokens.data, anomaly/security detail, evidence, features) round-trip any JSON
//     value; hashes are stored and compared exactly as given (string or Buffer); BLOBs come back
//     as Buffers.

import crypto from 'node:crypto';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { createRequire } from 'node:module';
import { setImmediate as nextTurn } from 'node:timers/promises';
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
const CHUNK = 1000;
const ANALYSIS_MAX_ATTEMPTS = 3;
const ANALYSIS_STALE_MS = 10 * 60000;
const CONDUCT_EVENT_TTL_MS = 30 * DAY;
const REVOKED_SESSION_TTL_MS = DAY;
const ANALYSIS_FAILED_TTL_MS = 30 * DAY;
const REPORT_SIGNAL_MS = 30 * DAY;
// A report flags the reported player's next games only from a credible reporter: the stored weight
// reaches REPORT_RULES.lowCredibility of anticheat/reports.js (sock puppets never flag anyone).
const REPORT_SIGNAL_MIN_WEIGHT = 0.5;
const INTEGRITY_LEVELS = ['none', 'suspected', 'high_confidence', 'confirmed'];
const { GameStatus } = enums;

const mBatchMs = metrics.histogram('scacelith_store_commit_batch_ms', 'Duration of one finished-games commit transaction',
    [1, 2, 5, 10, 25, 50, 100, 250, 1000]);
const mGames = metrics.counter('scacelith_store_games_committed_total', 'Finished games written to the database');
const mBusy = metrics.counter('scacelith_store_busy_total', 'Store operations that gave up waiting for the database lock');
const mAnalysisSkipped = metrics.counter('scacelith_anticheat_analysis_skipped_total',
    'Finished rated games not queued for engine analysis (sample: ANALYSIS_SAMPLE_RATE, backlog: ANALYSIS_QUEUE_MAX reached)', ['reason']);

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

    // Runs a chunked DELETE/UPDATE (its SQL ends with LIMIT CHUNK) until it has nothing left.
    function chunked(sql, ...args) {
        let total = 0;
        for (;;) {
            const n = Number(guard(() => st(sql).run(...args)).changes);
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

    const defaultRating = () => ({ rating: initialRating, games: 0, wins: 0, draws: 0, losses: 0, peak: initialRating, reachedSenior: false });
    const toRating = (r) => ({
        rating: r.rating, games: r.games, wins: r.wins, draws: r.draws, losses: r.losses, peak: r.peak, reachedSenior: !!r.reached_senior,
    });
    function readRating(userId, category) {
        const r = st('SELECT rating, games, wins, draws, losses, peak, reached_senior FROM ratings WHERE user_id = ? AND category = ?')
            .get(userId, category);
        return r ? toRating(r) : defaultRating();
    }
    function writeRating(userId, category, rec, now) {
        st(`INSERT INTO ratings (user_id, category, rating, games, wins, draws, losses, peak, reached_senior, updated_at)
            VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
            ON CONFLICT (user_id, category) DO UPDATE SET rating = excluded.rating, games = excluded.games, wins = excluded.wins,
            draws = excluded.draws, losses = excluded.losses, peak = excluded.peak, reached_senior = excluded.reached_senior,
            updated_at = excluded.updated_at`)
            .run(userId, category, rec.rating, rec.games, rec.wins, rec.draws, rec.losses, rec.peak, b01(rec.reachedSenior), now);
    }

    const ratings = {
        get(userId, category) { return readRating(userId, category); },
        forUser(userId) {
            return st(`SELECT category, rating, games, wins, draws, losses, peak, reached_senior, updated_at FROM ratings
                WHERE user_id = ? ORDER BY category`).all(userId)
                .map((r) => ({ category: r.category, ...toRating(r), provisional: r.games < provisionalGames, updatedAt: r.updated_at }));
        },
        leaderboard(category, limit = 100, minGames = provisionalGames) {
            return st(`SELECT r.user_id, u.username, r.rating, r.games, r.wins, r.draws, r.losses, r.peak
                FROM ratings r JOIN users u ON u.id = r.user_id LEFT JOIN player_integrity pi ON pi.user_id = r.user_id
                WHERE r.category = ? AND r.games >= ? AND u.status = 'active' AND (pi.level IS NULL OR pi.level <> 'confirmed')
                ORDER BY r.rating DESC, r.games DESC, r.user_id LIMIT ?`).all(category, minGames, limit)
                .map((r) => ({ userId: r.user_id, username: r.username, rating: r.rating, games: r.games, wins: r.wins, draws: r.draws,
                    losses: r.losses, peak: r.peak }));
        },
    };

    // ---- games ---------------------------------------------------------------------------------

    const GAME_SUMMARY_COLS = 'id, category, rated, base_ms, inc_ms, white_id, black_id, white_name, black_name, white_rating, '
        + 'black_rating, started_at, ended_at, status, reason, ply_count, white_before, white_after, black_before, black_after, '
        + 'rematch_of, flags';
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
    // for any field it does not return.
    function nextRecord(prev, side, score) {
        const rec = (side && side.record) || {};
        const after = Math.round(side.after ?? rec.rating);
        return {
            rating: Math.round(rec.rating ?? after),
            games: rec.games ?? prev.games + 1,
            wins: rec.wins ?? prev.wins + (score === 1 ? 1 : 0),
            draws: rec.draws ?? prev.draws + (score === 0.5 ? 1 : 0),
            losses: rec.losses ?? prev.losses + (score === 0 ? 1 : 0),
            peak: Math.round(rec.peak ?? Math.max(prev.peak, after)),
            reachedSenior: !!(rec.reachedSenior ?? prev.reachedSenior),
        };
    }

    function storedChanges(row) {
        if (row.white_before === null) return null;
        const w = readRating(row.white_id, row.category);
        const b = readRating(row.black_id, row.category);
        return {
            white: { before: row.white_before, after: row.white_after, games: w.games, provisional: w.games < provisionalGames },
            black: { before: row.black_before, after: row.black_after, games: b.games, provisional: b.games < provisionalGames },
        };
    }

    // Analysis queue policy of a finished game (header, DESIGN.md 6.5): a suspicion signal queues
    // it ahead of the ordinary games; an ordinary game is drawn with ANALYSIS_SAMPLE_RATE and
    // queued only while fewer than ANALYSIS_QUEUE_MAX ordinary jobs wait. `batch.ordinary` caches
    // that count for the rest of the transaction (the write lock is held, nobody else changes it).
    // Returns why the game was left out ('sample' | 'backlog'), or null when it was queued.
    function queueAnalysis(r, now, batch) {
        const flagged = st(`SELECT EXISTS (SELECT 1 FROM player_integrity WHERE user_id IN (?1, ?2) AND level <> 'none')
            OR EXISTS (SELECT 1 FROM reports WHERE reported_id IN (?1, ?2) AND created_at >= ?3 AND status = 'open'
                AND category <> 'abuse' AND weight >= ?6)
            OR EXISTS (SELECT 1 FROM anomalies WHERE user_id IN (?1, ?2) AND at >= ?4 AND game_id = ?5 AND severity <> 'info')
            AS flagged`).get(r.whiteId, r.blackId, now - REPORT_SIGNAL_MS, ms(r.startedAt ?? r.endedAt ?? now), r.id,
            REPORT_SIGNAL_MIN_WEIGHT).flagged;
        const insert = (priority) => st('INSERT OR IGNORE INTO analysis_jobs (game_id, queued_at, priority) VALUES (?, ?, ?)')
            .run(r.id, now, priority);
        if (flagged) {
            insert(AnalysisPriority.signal);
            return null;
        }
        if (analysisSampleRate < 1 && !(random() < analysisSampleRate)) return 'sample';
        if (batch.ordinary === null) {
            batch.ordinary = st(`SELECT count(*) AS n FROM (SELECT 1 FROM analysis_jobs WHERE status = 'queued' AND priority = ?
                LIMIT ?)`).get(AnalysisPriority.ordinary, analysisQueueMax).n;
        }
        if (batch.ordinary >= analysisQueueMax) return 'backlog';
        insert(AnalysisPriority.ordinary);
        batch.ordinary++;
        return null;
    }

    function commitGame(r, now, batch) {
        checkRecord(r);
        const existing = st('SELECT white_id, black_id, category, white_before, white_after, black_before, black_after FROM games WHERE id = ?')
            .get(r.id);
        if (existing) return { gameId: r.id, duplicate: true, ratings: storedChanges(existing) };
        const played = r.status !== GameStatus.Aborted;
        const rate = !!r.rated && played && r.category !== 'custom';
        let changes = null;
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
                    provisional: wRec.games < provisionalGames },
                black: { before: Math.round(res.black.before ?? b.rating), after: bRec.rating, games: bRec.games,
                    provisional: bRec.games < provisionalGames },
            };
        }
        const moves = packArray(r.moves ?? [], Uint16Array);
        const plies = r.moves ? r.moves.length : 0;
        st(`INSERT INTO games (id, category, rated, base_ms, inc_ms, white_id, black_id, white_name, black_name, white_rating,
            black_rating, started_at, ended_at, status, reason, ply_count, white_before, white_after, black_before, black_after,
            rematch_of, flags, moves, spent, clocks) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)`)
            .run(r.id, r.category, b01(r.rated), ms(r.baseMs ?? 0), ms(r.incMs ?? 0), r.whiteId, r.blackId,
                String(r.whiteName ?? ''), String(r.blackName ?? ''), ms(r.whiteRating), ms(r.blackRating),
                ms(r.startedAt ?? r.endedAt ?? now), ms(r.endedAt ?? now), r.status, r.reason ?? 0, plies,
                changes ? changes.white.before : null, changes ? changes.white.after : null,
                changes ? changes.black.before : null, changes ? changes.black.after : null,
                r.rematchOf ? r.rematchOf : null, r.flags ?? 0,
                moves, packArray(r.spentMs, Uint32Array), packArray(r.clockMs, Uint32Array));
        const skipped = rate && plies >= analysisMinPlies ? queueAnalysis(r, now, batch) : null;
        return skipped ? { gameId: r.id, ratings: changes, analysisSkipped: skipped } : { gameId: r.id, ratings: changes };
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
            const batch = { ordinary: null };
            const out = tx(() => records.map((r) => commitGame(r, now, batch)));
            mBatchMs.observe(performance.now() - t0);
            mGames.inc(out.reduce((n, x) => n + (x.duplicate ? 0 : 1), 0));
            for (const x of out) if (x.analysisSkipped) mAnalysisSkipped.labels(x.analysisSkipped).inc();
            return out;
        },
        byId(id) {
            const r = st('SELECT * FROM games WHERE id = ?').get(id);
            return r ? toGame(r, true) : null;
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
        countForUser(userId) {
            return st('SELECT (SELECT count(*) FROM games WHERE white_id = ?1) + (SELECT count(*) FROM games WHERE black_id = ?1) AS n')
                .get(userId).n;
        },
    };

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

    const SECURITY_DELETE_SQL = `DELETE FROM security_events WHERE id IN (SELECT id FROM security_events WHERE at < ? LIMIT ${CHUNK})`;
    const SECURITY_IP_SQL = `UPDATE security_events SET ip = NULL WHERE id IN (SELECT id FROM security_events
        WHERE ip IS NOT NULL AND at < ? LIMIT ${CHUNK})`;

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
            const deleted = chunked(SECURITY_DELETE_SQL, ms(now) - cfg.retentionSecurityDays * DAY);
            const ipErased = chunked(SECURITY_IP_SQL, ms(now) - cfg.retentionIpDays * DAY);
            return { deleted, ipErased };
        },
    };

    // ---- analysis queue ----------------------------------------------------------------------------

    const toJob = (r) => ({
        gameId: r.game_id, priority: r.priority, attempts: r.attempts, queuedAt: r.queued_at, startedAt: r.started_at, worker: r.worker,
    });
    const { WhiteWins, BlackWins, Draw } = GameStatus;

    const analysis = {
        /** Claims up to `limit` queued jobs atomically (highest priority, then oldest first) for `workerId`. */
        next(limit = 1, workerId = null, now = Date.now()) {
            const t = ms(now);
            return tx(() => {
                st(`UPDATE analysis_jobs SET status = CASE WHEN attempts >= ?1 THEN 'failed' ELSE 'queued' END, worker = NULL,
                    error = 'stale: worker vanished', finished_at = CASE WHEN attempts >= ?1 THEN ?2 ELSE NULL END
                    WHERE status = 'running' AND started_at < ?3`).run(ANALYSIS_MAX_ATTEMPTS, t, t - ANALYSIS_STALE_MS);
                return st(`UPDATE analysis_jobs SET status = 'running', worker = ?1, started_at = ?2, attempts = attempts + 1
                    WHERE game_id IN (SELECT game_id FROM analysis_jobs WHERE status = 'queued'
                        ORDER BY priority DESC, queued_at, game_id LIMIT ?3)
                    RETURNING game_id, priority, attempts, queued_at, started_at, worker`)
                    .all(workerId === null || workerId === undefined ? null : String(workerId), t, limit)
                    .map(toJob).sort((a, b) => b.priority - a.priority || a.queuedAt - b.queuedAt || a.gameId - b.gameId);
            });
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
            guard(() => st(`INSERT INTO analysis_jobs (game_id, queued_at, priority) VALUES (?, ?, ?) ON CONFLICT (game_id) DO UPDATE SET
                status = 'queued', attempts = 0, worker = NULL, started_at = NULL, finished_at = NULL, error = NULL,
                queued_at = excluded.queued_at, priority = excluded.priority`).run(gameId, ms(now), AnalysisPriority.manual));
        },
        /**
         * Makes sure a game the automatic policy analyses (rated, official category, played out,
         * >= ANALYSIS_MIN_PLIES plies) gets analysed with at least the priority of `reason`
         * ('signal' | 'report' | 'manual'): queued when it has no job (left out by the policy),
         * its priority raised while it waits, a failed job queued again. A running or done job is
         * left alone. Returns true when a job was queued or changed.
         */
        request(gameId, reason = 'report', now = Date.now()) {
            // 'ordinary' is refused: it would bypass ANALYSIS_QUEUE_MAX.
            const priority = Object.hasOwn(AnalysisPriority, reason) ? AnalysisPriority[reason] : 0;
            if (priority <= AnalysisPriority.ordinary) throw new StoreError('invalid', `analysis.request: unknown or ordinary priority ${reason}`);
            return Number(guard(() => st(`INSERT INTO analysis_jobs (game_id, queued_at, priority)
                SELECT id, ?2, ?3 FROM games WHERE id = ?1 AND rated = 1 AND category <> 'custom' AND status IN (?5, ?6, ?7)
                    AND ply_count >= ?4
                ON CONFLICT (game_id) DO UPDATE SET priority = max(priority, excluded.priority), status = 'queued',
                    attempts = CASE WHEN status = 'failed' THEN 0 ELSE attempts END,
                    error = CASE WHEN status = 'failed' THEN NULL ELSE error END, finished_at = NULL
                WHERE status = 'failed' OR (status = 'queued' AND priority < excluded.priority)`)
                .run(gameId, ms(now), priority, analysisMinPlies, WhiteWins, BlackWins, Draw)).changes) === 1;
        },
        /** Jobs waiting: { ordinary, priority } (priority: every job above 'ordinary'). */
        backlog() {
            const r = st(`SELECT (SELECT count(*) FROM analysis_jobs WHERE status = 'queued' AND priority = ?1) AS ordinary,
                (SELECT count(*) FROM analysis_jobs WHERE status = 'queued' AND priority > ?1) AS priority`).get(AnalysisPriority.ordinary);
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
        /** outcome: 'actioned' | 'dismissed'; only an open report changes (returns true then). */
        resolve(id, outcome, by = null, now = Date.now()) {
            if (outcome !== 'actioned' && outcome !== 'dismissed') throw new StoreError('invalid', "outcome must be 'actioned' or 'dismissed'");
            return Number(st(`UPDATE reports SET status = ?, resolved_at = ?, resolved_by = ? WHERE id = ? AND status = 'open'`)
                .run(outcome, ms(now), by === null || by === undefined ? null : String(by), id).changes) === 1;
        },
    };

    // ---- retention ------------------------------------------------------------------------------------

    // The purge as a list of chunked statements (LIMIT CHUNK rows each, one short autocommit
    // transaction per execution), each run again until it touches fewer than CHUNK rows; `count`
    // names the total it adds to. Rows are deleted before the IPs of the remaining ones are erased;
    // an IP column is set to NULL, its row stays as long as it is needed.
    function retentionSteps(t, cfg) {
        const ipBefore = t - cfg.retentionIpDays * DAY;
        const securityBefore = t - cfg.retentionSecurityDays * DAY;
        return [
            { count: 'sessions', arg: t, sql: `DELETE FROM sessions WHERE id IN (SELECT id FROM sessions
                WHERE min(expires_at, idle_expires_at) <= ? LIMIT ${CHUNK})` },
            { count: 'sessions', arg: t - REVOKED_SESSION_TTL_MS, sql: `DELETE FROM sessions WHERE id IN (SELECT id FROM sessions
                WHERE revoked_at IS NOT NULL AND revoked_at <= ? LIMIT ${CHUNK})` },
            { count: 'tokens', arg: t, sql: `DELETE FROM tokens WHERE id IN (SELECT id FROM tokens WHERE expires_at <= ? LIMIT ${CHUNK})` },
            { count: 'ipErased', arg: ipBefore, sql: `UPDATE sessions SET ip = NULL WHERE id IN (SELECT id FROM sessions
                WHERE ip IS NOT NULL AND created_at < ? LIMIT ${CHUNK})` },
            { count: 'securityEvents', arg: securityBefore, sql: SECURITY_DELETE_SQL },
            { count: 'ipErased', arg: ipBefore, sql: SECURITY_IP_SQL },
            { count: 'anomalies', arg: securityBefore, sql: `DELETE FROM anomalies WHERE id IN (SELECT id FROM anomalies
                WHERE severity <> 'certain' AND at < ? LIMIT ${CHUNK})` },
            { count: 'conductEvents', arg: t - CONDUCT_EVENT_TTL_MS, sql: `DELETE FROM conduct_events WHERE id IN (SELECT id
                FROM conduct_events WHERE at < ? LIMIT ${CHUNK})` },
            { count: 'analysisJobs', arg: t - ANALYSIS_FAILED_TTL_MS, sql: `DELETE FROM analysis_jobs WHERE game_id IN (SELECT game_id
                FROM analysis_jobs WHERE status = 'failed' AND finished_at < ? LIMIT ${CHUNK})` },
        ];
    }
    const retentionCounts = () => ({ sessions: 0, tokens: 0, securityEvents: 0, anomalies: 0, conductEvents: 0, analysisJobs: 0, ipErased: 0 });

    const retention = {
        /** Periodic clean-up in one go (chunked, short transactions). Returns what it removed. */
        run(now = Date.now(), cfg = config) {
            const counts = retentionCounts();
            for (const step of retentionSteps(ms(now), cfg)) counts[step.count] += chunked(step.sql, step.arg);
            log.debug('retention done', counts);
            return counts;
        },
        /**
         * The same clean-up, returning to the event loop (await pause(), default setImmediate)
         * whenever sliceMs of work has been done, so a large purge never stalls the process. Stops
         * before the next statement once `signal` is aborted or the store closed (what was already
         * deleted stays deleted). Resolves to the counts of run(); a failure rejects with the error,
         * which carries the counts done so far as `err.counts`.
         * @param {number} [now]
         * @param {object} [cfg]
         * @param {{ sliceMs?: number, signal?: AbortSignal, pause?: () => Promise<void> }} [opts]
         */
        async runAsync(now = Date.now(), cfg = config, { sliceMs = 10, signal = null, pause = nextTurn } = {}) {
            const counts = retentionCounts();
            let sliceStart = performance.now();
            try {
                for (const step of retentionSteps(ms(now), cfg)) {
                    for (;;) {
                        if (closed || signal?.aborted) return counts;
                        const n = Number(guard(() => st(step.sql).run(step.arg)).changes);
                        counts[step.count] += n;
                        if (performance.now() - sliceStart >= sliceMs) {
                            await pause();
                            sliceStart = performance.now();
                        }
                        if (n < CHUNK) break;
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
        reports, retention,
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
