// Server-authoritative chess clock of one game (DESIGN.md section 6.1).
//
// Everything here is deterministic: the caller passes the time in. All stored times and clock
// values are integer milliseconds (GameRoom floors `now` at its entry points), so the values the
// journal records (u32 clocks, f64 times) are exact and a replay rebuilds identical clocks.
//
// Rules implemented:
//   * plies 0 and 1 (each side's first move) run no clock and add no increment; each has
//     FIRST_MOVE_TIMEOUT_MS from its turn start (NoShow otherwise);
//   * later moves: elapsed = recvTime - turnStart, lag = elapsed - clamp(thinkMs, 0, elapsed),
//     comp = min(lag, rttEma + 50, LAG_COMP_MAX_MS, quota), charged = elapsed - comp; the move
//     flags when remaining - charged <= 0, otherwise remaining -= charged then += incMs;
//     quota -= comp then quota = min(quota + LAG_QUOTA_GAIN_MS, LAG_QUOTA_MAX_MS);
//   * the flag deadline is turnStart + remaining + min(quota, rttEma + 50, LAG_COMP_MAX_MS): the
//     latest moment a move could still arrive in time. A move arriving at or after it flags.
//   * thinkMs > elapsed + 100 is physically impossible for an honest client (clock_implausible);
//     thinkMs never adds time: it is clamped to elapsed and only bounds the compensation.
//
// Complexity: every operation is O(1) and allocation-free (check() fills a reused object).

import { performance } from 'node:perf_hooks';

/** Server clock in epoch milliseconds (monotonic): performance.timeOrigin + performance.now(). */
export function now() { return performance.timeOrigin + performance.now(); }

/** Margin above the measured elapsed time before a client's thinkMs is called implausible. */
export const IMPLAUSIBLE_MARGIN_MS = 100;
/** Added to the round-trip average to bound the lag compensation of one move. */
export const RTT_EXTRA_MS = 50;
/** The round-trip average is capped here (a client delaying its Pongs gains nothing more). */
export const RTT_EMA_MAX_MS = 2000;
/** Weight of a new round-trip sample in the exponential moving average. */
export const RTT_EMA_ALPHA = 0.25;
/** Round-trip assumed before the first measurement of a player. */
export const INITIAL_RTT_MS = 100;

const WHITE = 0, BLACK = 1;

/**
 * Clock settings of a game, read once from the configuration (defaults = config.js defaults).
 * @param {object} config frozen configuration (camelCase keys of the "games" section)
 * @returns {{firstMoveMs:number, lagCompMaxMs:number, quotaInitialMs:number, quotaGainMs:number, quotaMaxMs:number}}
 */
export function clockPolicy(config = {}) {
    const n = (v, d) => (Number.isFinite(v) ? Math.max(0, Math.floor(v)) : d);
    return Object.freeze({
        firstMoveMs: n(config.firstMoveTimeoutMs, 30000),
        lagCompMaxMs: n(config.lagCompMaxMs, 1000),
        quotaInitialMs: n(config.lagQuotaInitialMs, 2000),
        quotaGainMs: n(config.lagQuotaGainMs, 100),
        quotaMaxMs: n(config.lagQuotaMaxMs, 3000),
    });
}

/**
 * Reconnection grace of a time control: clamp(baseMs / 10, RECONNECT_GRACE_MIN_MS, RECONNECT_GRACE_MAX_MS).
 * @param {number} baseMs
 * @param {object} config
 * @returns {number} integer milliseconds
 */
export function graceFor(baseMs, config = {}) {
    const lo = Number.isFinite(config.reconnectGraceMinMs) ? config.reconnectGraceMinMs : 15000;
    const hi = Math.max(lo, Number.isFinite(config.reconnectGraceMaxMs) ? config.reconnectGraceMaxMs : 60000);
    return Math.floor(Math.min(hi, Math.max(lo, baseMs / 10)));
}

/**
 * One exponential-moving-average step of a round-trip measurement, capped at RTT_EMA_MAX_MS.
 * @param {number} prev previous average (NaN or undefined: none yet)
 * @param {number} sample new measurement in ms
 * @returns {number}
 */
export function rttEmaStep(prev, sample) {
    const s = Math.min(RTT_EMA_MAX_MS, Math.max(0, +sample || 0));
    if (!Number.isFinite(prev)) return s;
    return Math.min(RTT_EMA_MAX_MS, prev + RTT_EMA_ALPHA * (s - prev));
}

/**
 * Both players' clocks, lag quotas and round-trip averages. The side to move and the ply come
 * from the owner (GameRoom): the clock does not know the chess rules.
 */
export class GameClock {
    /**
     * @param {{baseMs:number, incMs:number, policy:object, startAt:number}} opts
     */
    constructor({ baseMs, incMs, policy, startAt }) {
        this.baseMs = Math.floor(baseMs);
        this.incMs = Math.floor(incMs);
        this.policy = policy;
        this.ms = [this.baseMs, this.baseMs];          // remaining at turnStart (side to move) / now (other)
        this.quota = [policy.quotaInitialMs, policy.quotaInitialMs];
        this.rttF = [NaN, NaN];                         // raw moving averages (not journaled)
        this.rtt = [INITIAL_RTT_MS, INITIAL_RTT_MS];    // integer view used by the arithmetic
        this.turnStart = Math.floor(startAt);
        // Result of the last check(), reused to stay allocation-free.
        this.res = { elapsed: 0, think: 0, lag: 0, comp: 0, charged: 0, flagged: false, implausible: false, clockAfter: 0, quotaAfter: 0 };
    }

    /** Records a server round-trip measurement of `color`. */
    setRtt(color, sampleMs) {
        this.rttF[color] = rttEmaStep(this.rttF[color], sampleMs);
        this.rtt[color] = Math.round(this.rttF[color]);
    }

    /** Largest compensation `color` could still get on its next move. */
    compCap(color) {
        return Math.min(this.quota[color], this.rtt[color] + RTT_EXTRA_MS, this.policy.lagCompMaxMs);
    }

    /** Deadline of a first move (plies 0 and 1). */
    firstMoveDeadline() { return this.turnStart + this.policy.firstMoveMs; }

    /** Flag deadline of `color` when its clock runs. */
    flagDeadline(color) { return this.turnStart + this.ms[color] + this.compCap(color); }

    /** The deadline of the move expected at `ply` (first-move timer or flag). */
    deadline(ply) { return ply < 2 ? this.firstMoveDeadline() : this.flagDeadline(ply & 1); }

    /** First-move time left at `now` for the move of `ply` (0 when the clocks run). */
    firstMoveLeft(ply, nowMs) {
        return ply < 2 ? Math.max(0, this.firstMoveDeadline() - nowMs) : 0;
    }

    /**
     * Remaining time of `color` at `nowMs` (no compensation; clamped at 0).
     * @param {number} color
     * @param {number} ply plies played so far (the side to move is ply & 1)
     * @param {number} nowMs
     */
    remainingAt(color, ply, nowMs) {
        if (ply >= 2 && (ply & 1) === color) return Math.max(0, this.ms[color] - Math.max(0, nowMs - this.turnStart));
        return this.ms[color];
    }

    /**
     * Clock accounting of a move of `color` at `ply` received at `recvTime`, without changing any
     * state. Returns the shared result object (valid until the next call).
     */
    check(color, ply, recvTime, thinkMs) {
        const r = this.res;
        const elapsed = Math.max(0, recvTime - this.turnStart);
        const tm = Math.max(0, Math.floor(+thinkMs || 0));
        r.elapsed = elapsed;
        r.implausible = tm > elapsed + IMPLAUSIBLE_MARGIN_MS;
        r.think = Math.min(tm, elapsed);
        r.lag = elapsed - r.think;
        if (ply < 2) {
            // No clock: the first-move timeout is a deadline of its own (GameRoom checks it
            // before validating the move, like every other deadline).
            r.comp = 0; r.charged = 0;
            r.flagged = false;
            r.clockAfter = this.ms[color];
            r.quotaAfter = this.quota[color];
            return r;
        }
        r.comp = Math.min(r.lag, this.compCap(color));
        r.charged = elapsed - r.comp;
        const left = this.ms[color] - r.charged;
        r.flagged = left <= 0;
        r.clockAfter = r.flagged ? 0 : left + this.incMs;
        r.quotaAfter = Math.min(this.quota[color] - r.comp + this.policy.quotaGainMs, this.policy.quotaMaxMs);
        return r;
    }

    /** Applies an accepted move (live or replayed): the opponent's turn starts at `at`. */
    apply(color, clockAfter, quotaAfter, at) {
        this.ms[color] = clockAfter;
        this.quota[color] = quotaAfter;
        this.turnStart = at;
    }

    /** Server restart: the running clock restarts from its journaled value at `at`. */
    restart(at) { this.turnStart = at; }
}

export { WHITE, BLACK };
