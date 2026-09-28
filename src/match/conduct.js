// Conduct: abandoned, aborted and no-show games pause rated matchmaking (DESIGN 6.4). Runs in
// the primary; the incidents and the cooldown are persisted through store.conduct (so they
// survive restarts) with an in-memory cache in front of the cooldown reads, which happen at every
// rated queue join. Pure apart from the store calls: time is passed in.
//
// Rules:
//   * every incident is recorded (store.conduct.record);
//   * when the incidents of the last 24 h (abandon + abort + noshow) reach CONDUCT_ABANDON_LIMIT,
//     rated matchmaking is paused: 15 min at level 0, then 1 h, then 6 h (and 6 h for every
//     further offence) - each new incident while the 24 h count is at or over the limit is a
//     repeated offence and starts the next cooldown from now (an active cooldown is never
//     shortened);
//   * the level (number of cooldowns in the current series, at most 3) decays by one for every
//     full 24 h without any incident; the decay is applied (and persisted) at the next incident,
//     which is the only moment the level is used;
//   * direct challenges and casual games stay possible (the caller only checks cooldownUntil()
//     before a rated queue join).
//
// store.conduct API used (DESIGN 5.5):
//   record(userId, kind, at) ; countSince(userId, since) -> { abandon, abort, noshow } ;
//   cooldown(userId) -> { until, level } | null ; setCooldown(userId, until, level)
// cooldown() may also return a bare number (the end) or an object with `untilMs`; both are
// accepted. countSince(since) is expected to count incidents with at >= since (or > since: the
// difference cannot change a decision here).

/** Cooldown lengths by level. */
export const CONDUCT_COOLDOWNS_MS = Object.freeze([15 * 60000, 60 * 60000, 6 * 3600000]);
/** Window in which incidents are counted, and decay period of the level. */
export const CONDUCT_WINDOW_MS = 24 * 3600000;
/** Incident kinds. */
export const CONDUCT_KINDS = Object.freeze(['abandon', 'abort', 'noshow']);

const MAX_LEVEL = CONDUCT_COOLDOWNS_MS.length;
const CACHE_MAX = 200000;

function total(counts) {
    if (!counts) return 0;
    if (typeof counts === 'number') return counts;
    return (counts.abandon || 0) + (counts.abort || 0) + (counts.noshow || 0);
}

function normalize(row) {
    if (row == null) return { until: 0, level: 0 };
    if (typeof row === 'number') return { until: row, level: 0 };
    const until = Number(row.until ?? row.untilMs ?? 0) || 0;
    const level = Math.max(0, Math.min(MAX_LEVEL, Math.trunc(Number(row.level) || 0)));
    return { until, level };
}

/**
 * Conduct cooldowns of the primary process.
 */
export class Conduct {
    /**
     * @param {object} opts
     * @param {object} opts.config configuration (conductAbandonLimit)
     * @param {object} opts.store object with the store.conduct API (the Store itself or store.conduct)
     * @param {() => number} [opts.now] clock (epoch ms)
     * @param {object} [opts.log] logger (security events on new cooldowns)
     */
    constructor({ config, store, now = Date.now, log = null } = {}) {
        if (!config) throw new TypeError('Conduct: config required');
        const api = store && store.conduct ? store.conduct : store;
        if (!api || typeof api.record !== 'function') throw new TypeError('Conduct: store.conduct required');
        this.config = config;
        this.db = api;
        this.now = typeof now === 'function' ? now : Date.now;
        this.log = log;
        this.limit = config.conductAbandonLimit;
        this.cache = new Map();   // userId -> { until, level }
    }

    _state(userId) {
        let s = this.cache.get(userId);
        if (!s) {
            s = normalize(this.db.cooldown(userId));
            if (this.cache.size >= CACHE_MAX) this._trim();
            this.cache.set(userId, s);
        }
        return s;
    }

    // Drops cache entries without an active cooldown or a level (cheap to read again).
    _trim() {
        const now = this.now();
        for (const [id, s] of this.cache) if (s.until <= now && s.level === 0) this.cache.delete(id);
        if (this.cache.size >= CACHE_MAX) this.cache.clear();
    }

    /**
     * Records an incident and starts a cooldown when the limit is reached.
     * @param {number} userId
     * @param {'abandon'|'abort'|'noshow'} kind
     * @param {number} [now]
     * @returns {{until:number, level:number, incidents:number, started:boolean}} the cooldown state
     *   after the incident (until = 0 when none), the 24 h incident count and whether a cooldown
     *   started now
     */
    record(userId, kind, now = this.now()) {
        if (!CONDUCT_KINDS.includes(kind)) throw new TypeError(`Conduct.record: unknown kind ${kind}`);
        const prev = this._state(userId);
        let level = prev.level;
        // Decay: one level per full 24 h without incident before this one.
        for (let days = 1; level > 0 && days <= MAX_LEVEL; days++) {
            if (total(this.db.countSince(userId, now - days * CONDUCT_WINDOW_MS)) > 0) break;
            level--;
        }
        this.db.record(userId, kind, now);
        const incidents = total(this.db.countSince(userId, now - CONDUCT_WINDOW_MS));
        let until = prev.until;
        let started = false;
        if (incidents >= this.limit) {
            const end = now + CONDUCT_COOLDOWNS_MS[Math.min(level, MAX_LEVEL - 1)];
            if (end > until) until = end;
            level = Math.min(level + 1, MAX_LEVEL);
            started = true;
        }
        if (started || level !== prev.level) {
            this.db.setCooldown(userId, until, level);
            if (started && this.log) this.log.security('conduct_cooldown', { userId, kind, incidents, level, until });
        }
        this.cache.set(userId, { until, level });
        return { until: until > now ? until : 0, level, incidents, started };
    }

    /**
     * End of the rated matchmaking pause of a user, or 0 when there is none.
     * @param {number} userId
     * @param {number} [now]
     * @returns {number}
     */
    cooldownUntil(userId, now = this.now()) {
        const s = this._state(userId);
        return s.until > now ? s.until : 0;
    }

    /**
     * Cooldown state as cached ({ until, level }), for the admin CLI and tests.
     * @param {number} userId
     */
    state(userId) {
        const s = this._state(userId);
        return { until: s.until, level: s.level };
    }

    /**
     * Forgets the cached state of a user (after an administrator changed it in the database).
     * @param {number} [userId] all users when omitted
     */
    invalidate(userId) {
        if (userId === undefined) this.cache.clear(); else this.cache.delete(userId);
    }
}
