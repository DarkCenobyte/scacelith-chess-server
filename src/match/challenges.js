// Direct challenges and private games joined with a code (primary process).
// Pure: no timer, no I/O; time comes from the injected clock (or the `now` argument), random
// draws from the injected `randomInt` (crypto.randomInt by default: private codes must not be
// guessable).
//
// A challenge:
//   { id (u32), kind: 'direct'|'private', from: player, target: username ('' for a private game),
//     targetUserId (0 for a private game), code ('' for a direct challenge), baseSec, incSec,
//     baseMs, incMs, category ('3+2' | 'custom'), rated, color (ColorPref asked by the creator),
//     receiverColor (ColorPref offered to the receiver: ChallengeReceived.yourColor),
//     createdAt, expiresAt, state (ChallengeState) }
// A player: { userId, username, rating, provisional, shard, connId }; `rating` is the player's
// rating in the challenge's category (the caller computes the category with elo.categoryOf
// before reading the rating).
//
// Rules:
//   * rated only with an official category (RatedRequiresOfficialTc); custom time controls only
//     with ALLOW_CUSTOM_TIME_CONTROLS (InvalidTimeControl otherwise); baseSec 15..10800 and
//     incSec 0..180 as in the protocol (InvalidTimeControl);
//   * at most MAX_PENDING_OUTGOING (3) pending challenges and private games per creator
//     (ChallengeLimit), and one pending direct challenge per (creator, target) pair
//     (ChallengeLimit too);
//   * no self-challenge (CannotChallengeSelf), neither by name nor by accepting one's own code;
//   * a direct challenge lives CHALLENGE_TTL_MS, a private code PRIVATE_GAME_TTL_MS; expire(now)
//     returns (and forgets) the expired ones, and accept/decline/cancel/joinCode already refuse
//     them in between;
//   * only the target accepts or declines a direct challenge; a private game is joined only with
//     its code (accepting it by id is refused); only the creator cancels. Every refusal of a
//     challenge that exists but is not the caller's is ChallengeNotFound (nothing leaks).
//
// Contract additions (DESIGN 5.4):
//   * create() takes `targetUser`: { userId, username, acceptChallenges, online } resolved by the
//     primary from the target username (presence + store.users), or null when no such account.
//     Unknown, offline or not accepting challenges -> UserUnavailable.
//   * accept() and joinCode() return { ok, challenge, game } where `game` is the game spec
//     { white, black, baseMs, incMs, category, rated, challengeId, rematchOf: 0 }.
//   * dropUser(userId) cancels the user's outgoing challenges and marks the incoming ones
//     Unavailable (for disconnections).

import crypto from 'node:crypto';
import { enums } from '../protocol/schema.js';
import { metrics } from '../metrics.js';
import { categoryOf, CUSTOM_CATEGORY } from './elo.js';

const { ErrorCode, ChallengeState, ColorPref } = enums;

/** Pending outgoing challenges (direct + private) per creator. */
export const MAX_PENDING_OUTGOING = 3;
/** Private game codes: 6 characters without 0/O, 1/I/L. */
export const CODE_ALPHABET = '23456789ABCDEFGHJKMNPQRSTUVWXYZ';
export const CODE_LENGTH = 6;
/** Time control limits of ChallengeCreate. */
export const MIN_BASE_SEC = 15;
export const MAX_BASE_SEC = 10800;
export const MAX_INC_SEC = 180;

const CODE_RE = new RegExp(`^[${CODE_ALPHABET}]{${CODE_LENGTH}}$`);

const mCreated = metrics.counter('scacelith_challenges_created_total', 'Challenges and private games created', ['kind']);
const mCreatedDirect = mCreated.labels('direct');
const mCreatedPrivate = mCreated.labels('private');
const mAccepted = metrics.counter('scacelith_challenges_accepted_total', 'Challenges and private games accepted');
const mExpired = metrics.counter('scacelith_challenges_expired_total', 'Challenges and private codes that expired');

/**
 * Normalises a code typed by a player (case, spaces, dashes); null when it cannot be a code.
 * @param {string} code
 * @returns {string|null}
 */
export function normalizeCode(code) {
    if (typeof code !== 'string') return null;
    const c = code.toUpperCase().replace(/[\s-]/g, '');
    return CODE_RE.test(c) ? c : null;
}

function opposite(pref) {
    if (pref === ColorPref.White) return ColorPref.Black;
    if (pref === ColorPref.Black) return ColorPref.White;
    return ColorPref.Random;
}

function playerOf(p) {
    return {
        userId: p.userId, username: p.username ?? '', rating: p.rating ?? 0, provisional: !!p.provisional,
        shard: p.shard ?? 0, connId: p.connId ?? 0,
    };
}

/**
 * Pending direct challenges and private games of the primary process.
 */
export class Challenges {
    /**
     * @param {object} opts
     * @param {object} opts.config configuration (categories, allowCustomTimeControls, challengeTtlMs, privateGameTtlMs)
     * @param {() => number} [opts.now] clock (epoch ms)
     * @param {(max:number) => number} [opts.randomInt] integer in [0, max) (codes and colour draws)
     */
    constructor({ config, now = Date.now, randomInt = crypto.randomInt } = {}) {
        if (!config) throw new TypeError('Challenges: config required');
        this.config = config;
        this.now = typeof now === 'function' ? now : Date.now;
        this.randomInt = randomInt;
        this.byId = new Map();          // id -> challenge (pending only)
        this.byCode = new Map();        // code -> challenge
        this.outgoing = new Map();      // userId -> Set of ids
        this.incoming = new Map();      // userId -> Set of ids
        this.lastId = 0;
        // Expiry queues, each in expiry order because each kind has one TTL (lazy deletion).
        this.expDirect = [];
        this.expPrivate = [];
        this.expDirectHead = 0;
        this.expPrivateHead = 0;
    }

    /** Pending challenges. */
    get size() { return this.byId.size; }

    _nextId() {
        do {
            this.lastId = this.lastId >= 0xFFFFFFFF ? 1 : this.lastId + 1;
        } while (this.byId.has(this.lastId));
        return this.lastId;
    }

    _newCode() {
        for (;;) {
            let c = '';
            for (let i = 0; i < CODE_LENGTH; i++) c += CODE_ALPHABET[this.randomInt(CODE_ALPHABET.length)];
            if (!this.byCode.has(c)) return c;
        }
    }

    /**
     * Creates a direct challenge (target username) or a private game (empty target).
     * @param {object} req { from: player, target: string, targetUser?: {userId, username, acceptChallenges, online}|null,
     *   baseSec, incSec, rated, color (ColorPref) }
     * @param {number} [now]
     * @returns {{ok: true, challenge: object} | {error: number}}
     */
    create(req, now = this.now()) {
        const from = req.from;
        if (!from || !Number.isSafeInteger(from.userId) || from.userId <= 0) throw new TypeError('Challenges.create: from.userId required');
        const baseSec = req.baseSec, incSec = req.incSec;
        if (!Number.isInteger(baseSec) || baseSec < MIN_BASE_SEC || baseSec > MAX_BASE_SEC
            || !Number.isInteger(incSec) || incSec < 0 || incSec > MAX_INC_SEC) return { error: ErrorCode.InvalidTimeControl };
        const baseMs = baseSec * 1000, incMs = incSec * 1000;
        const category = categoryOf(baseMs, incMs, this.config);
        const rated = !!req.rated;
        if (category === CUSTOM_CATEGORY) {
            if (rated) return { error: ErrorCode.RatedRequiresOfficialTc };
            if (!this.config.allowCustomTimeControls) return { error: ErrorCode.InvalidTimeControl };
        }
        const color = req.color === ColorPref.White || req.color === ColorPref.Black ? req.color : ColorPref.Random;
        const target = typeof req.target === 'string' ? req.target.trim() : '';
        const isPrivate = target === '';
        let targetUserId = 0, targetName = '';
        if (!isPrivate) {
            if (from.username && target.toLowerCase() === String(from.username).toLowerCase()) return { error: ErrorCode.CannotChallengeSelf };
            const tu = req.targetUser;
            if (!tu || !Number.isSafeInteger(tu.userId) || tu.userId <= 0) return { error: ErrorCode.UserUnavailable };
            if (tu.userId === from.userId) return { error: ErrorCode.CannotChallengeSelf };
            if (!tu.online || tu.acceptChallenges === false) return { error: ErrorCode.UserUnavailable };
            targetUserId = tu.userId;
            targetName = tu.username || target;
        }
        const mine = this.outgoing.get(from.userId);
        if (mine) {
            this._expireUser(mine, now);
            if (mine.size >= MAX_PENDING_OUTGOING) return { error: ErrorCode.ChallengeLimit };
            if (!isPrivate) {
                for (const id of mine) if (this.byId.get(id).targetUserId === targetUserId) return { error: ErrorCode.ChallengeLimit };
            }
        }
        const ttl = isPrivate ? this.config.privateGameTtlMs : this.config.challengeTtlMs;
        const challenge = {
            id: this._nextId(), kind: isPrivate ? 'private' : 'direct', from: playerOf(from),
            target: targetName, targetUserId, code: isPrivate ? this._newCode() : '',
            baseSec, incSec, baseMs, incMs, category, rated, color, receiverColor: opposite(color),
            createdAt: now, expiresAt: now + ttl, state: ChallengeState.Pending,
        };
        this.byId.set(challenge.id, challenge);
        addTo(this.outgoing, from.userId, challenge.id);
        if (isPrivate) {
            this.byCode.set(challenge.code, challenge);
            this.expPrivate.push(challenge);
            mCreatedPrivate.inc();
        } else {
            addTo(this.incoming, targetUserId, challenge.id);
            this.expDirect.push(challenge);
            mCreatedDirect.inc();
        }
        return { ok: true, challenge };
    }

    /**
     * The target accepts a direct challenge.
     * @param {number} id
     * @param {object} by the accepting player (rating in the challenge's category)
     * @param {number} [now]
     * @returns {{ok: true, challenge: object, game: object} | {error: number}}
     */
    accept(id, by, now = this.now()) {
        const c = this._live(id, now);
        if (!c || c.kind !== 'direct' || !by) return { error: ErrorCode.ChallengeNotFound };
        if (by.userId === c.from.userId) return { error: ErrorCode.CannotChallengeSelf };
        if (by.userId !== c.targetUserId) return { error: ErrorCode.ChallengeNotFound };
        return this._start(c, by);
    }

    /**
     * Joins a private game with its code.
     * @param {string} code as typed (case, spaces and dashes ignored)
     * @param {object} by the joining player (rating in the game's category)
     * @param {number} [now]
     * @returns {{ok: true, challenge: object, game: object} | {error: number}}
     */
    joinCode(code, by, now = this.now()) {
        const c = this.getCode(code, now);
        if (!c || !by) return { error: ErrorCode.CodeInvalid };
        if (by.userId === c.from.userId) return { error: ErrorCode.CannotChallengeSelf };
        return this._start(c, by);
    }

    /**
     * The target declines a direct challenge.
     * @param {number} id
     * @param {number} userId
     * @param {number} [now]
     * @returns {{ok: true, challenge: object} | {error: number}}
     */
    decline(id, userId, now = this.now()) {
        const c = this._live(id, now);
        if (!c || c.kind !== 'direct' || c.targetUserId !== userId) return { error: ErrorCode.ChallengeNotFound };
        this._forget(c, ChallengeState.Declined);
        return { ok: true, challenge: c };
    }

    /**
     * The creator withdraws a challenge or a private game.
     * @param {number} id
     * @param {number} userId
     * @param {number} [now]
     * @returns {{ok: true, challenge: object} | {error: number}}
     */
    cancel(id, userId, now = this.now()) {
        const c = this._live(id, now);
        if (!c || c.from.userId !== userId) return { error: ErrorCode.ChallengeNotFound };
        this._forget(c, ChallengeState.Cancelled);
        return { ok: true, challenge: c };
    }

    /**
     * Forgets every challenge whose time is over. O(expired) amortised.
     * @param {number} [now]
     * @returns {object[]} the expired challenges (state Expired)
     */
    expire(now = this.now()) {
        const out = [];
        this.expDirectHead = this._drain(this.expDirect, this.expDirectHead, now, out);
        this.expPrivateHead = this._drain(this.expPrivate, this.expPrivateHead, now, out);
        if (this.expDirectHead > 1024 && this.expDirectHead * 2 > this.expDirect.length) {
            this.expDirect.splice(0, this.expDirectHead); this.expDirectHead = 0;
        }
        if (this.expPrivateHead > 1024 && this.expPrivateHead * 2 > this.expPrivate.length) {
            this.expPrivate.splice(0, this.expPrivateHead); this.expPrivateHead = 0;
        }
        if (out.length) mExpired.inc(out.length);
        return out;
    }

    _drain(queue, head, now, out) {
        while (head < queue.length) {
            const c = queue[head];
            if (c.state === ChallengeState.Pending) {
                if (c.expiresAt > now) break;
                this._forget(c, ChallengeState.Expired);
                out.push(c);
            }
            queue[head] = null;
            head++;
        }
        if (head === queue.length) { queue.length = 0; head = 0; }
        return head;
    }

    /**
     * Pending challenges of a user.
     * @param {number} userId
     * @param {number} [now]
     * @returns {{outgoing: object[], incoming: object[]}}
     */
    forUser(userId, now = this.now()) {
        const pick = (ids) => {
            const list = [];
            if (ids) for (const id of ids) { const c = this.byId.get(id); if (c && c.expiresAt > now) list.push(c); }
            return list;
        };
        return { outgoing: pick(this.outgoing.get(userId)), incoming: pick(this.incoming.get(userId)) };
    }

    /**
     * A pending, unexpired challenge by id, or null.
     * @param {number} id
     * @param {number} [now]
     */
    get(id, now = this.now()) { return this._live(id, now); }

    /**
     * The pending, unexpired private game of a code (as typed), or null; the code stays usable.
     * @param {string} code
     * @param {number} [now]
     */
    getCode(code, now = this.now()) {
        const norm = normalizeCode(code);
        const c = norm && this.byCode.get(norm);
        return c && c.expiresAt > now ? c : null;
    }

    /**
     * The user left (disconnected): outgoing challenges are cancelled, incoming ones become
     * Unavailable.
     * @param {number} userId
     * @returns {object[]} the affected challenges
     */
    dropUser(userId) {
        const out = [];
        for (const [map, state] of [[this.outgoing, ChallengeState.Cancelled], [this.incoming, ChallengeState.Unavailable]]) {
            const ids = map.get(userId);
            if (!ids) continue;
            for (const id of [...ids]) {
                const c = this.byId.get(id);
                if (c) { this._forget(c, state); out.push(c); }
            }
        }
        return out;
    }

    _live(id, now) {
        const c = this.byId.get(id);
        if (!c) return null;
        if (c.expiresAt <= now) return null;   // expire() reports it
        return c;
    }

    _start(c, by) {
        const creator = c.from, other = playerOf(by);
        let creatorWhite;
        if (c.color === ColorPref.White) creatorWhite = true;
        else if (c.color === ColorPref.Black) creatorWhite = false;
        else creatorWhite = this.randomInt(2) === 0;
        this._forget(c, ChallengeState.Accepted);
        mAccepted.inc();
        const game = {
            white: creatorWhite ? creator : other, black: creatorWhite ? other : creator,
            baseMs: c.baseMs, incMs: c.incMs, category: c.category, rated: c.rated,
            challengeId: c.id, rematchOf: 0,
        };
        return { ok: true, challenge: c, game };
    }

    _forget(c, state) {
        c.state = state;
        this.byId.delete(c.id);
        if (c.code) this.byCode.delete(c.code);
        removeFrom(this.outgoing, c.from.userId, c.id);
        if (c.targetUserId) removeFrom(this.incoming, c.targetUserId, c.id);
    }

    // Drops the expired entries of one user's outgoing set, so that the limit counts live ones
    // only; they are reported by the next expire().
    _expireUser(ids, now) {
        for (const id of ids) {
            const c = this.byId.get(id);
            if (c && c.expiresAt <= now) ids.delete(id);
        }
    }
}

function addTo(map, key, id) {
    let s = map.get(key);
    if (!s) { s = new Set(); map.set(key, s); }
    s.add(id);
}

function removeFrom(map, key, id) {
    const s = map.get(key);
    if (!s) return;
    s.delete(id);
    if (!s.size) map.delete(key);
}
