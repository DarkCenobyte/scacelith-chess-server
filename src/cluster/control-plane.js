// Control plane (primary): implements the IPC catalog of DESIGN 5.7 on top of presence, the
// matchmaker, challenges, conduct, the global rate limiter and the single-use keys. Nothing per
// move goes through here.
//
// Dependencies are injected (unit tests use fakes):
//   shards    { notify(shard, type, payload), request(shard, type, payload, opts) -> Promise,
//               broadcast(type, payload), list() -> [shard] }       (the live shard workers)
//   matchmaker / challenges / conduct   DESIGN 5.4 objects (conduct optional)
//   activeBan(userId, now) -> { until } | until | null              (store.sanctions.activeBan)
//   ratingOf(userId, category) -> { rating, games } | null           (store.ratings.get)
//   acceptsChallenges(userId) -> bool                                 (store.users.byId().acceptChallenges)
//
// Game placement: a paired game is hosted by the shard of the player who waited longer (its
// frames then never cross the bus for that player); when that shard reports overload
// ('shard.load', event-loop p99 above SHARD_OVERLOAD_LAG_MS, or no report for 10 s) the least
// loaded shard (fewest games, then connections) hosts it. Challenges: the creator's shard.
// Rematches: the finished game's shard.
//
// Clock press: createGame fixes each game's autoPress (GameSnapshot.autoPress) from
// AUTO_PRESS_CLOCK, for the queue, the challenges and the private games alike; a rematch keeps the
// value of the game it follows, so a change of the setting applies to the games created after it.
//
// Notifications (QueueStatus, ChallengeReceived, ChallengeStatus, Notice) are encoded here and
// written by the shard of the user's live connection ('conn.send'); waiting players get a fresh
// QueueStatus every 3 s.
//
// Extensions to the catalog (documented for the integrators):
//   shard -> primary  'shard.load' { conns, games, lagP99, overloaded }   (every 2 s)
//                     'shard.ready' { shard }   (after host.recover() and listen: re-attaches the
//                     live connections of players whose game that shard hosts)
//                     'game.recovered' { gameId, whiteId, blackId, shard }   (sent by the GameHost
//                     for each game replayed from the journal: presence.claim returns it as
//                     activeGame after a full restart; 'game.active' is an alias)
//   primary -> shard  'shard.down' { shard }   (a shard died: forget its remote endpoints)
//                     'game.forfeit' { userId, gameId }   (sanction.applied: the host shard of the
//                     user's running game ends it Forfeit with host.forfeitUser)
//   game.rematch: white/black are already swapped by the room (the new game's colours); the
//   players' ratings are read again (the finished game changed them); autoPress is the finished
//   game's.
//   presence.release of the live connection also leaves the queue and withdraws the user's
//   pending challenges (so the router does not need a separate mm.leave that could race with a
//   newer connection's QueueJoin).

import { metrics as defaultRegistry } from '../metrics.js';
import { encode, enums, CloseCode } from '../protocol/index.js';
import { isGameId, shardOfGameId } from '../util/ids.js';
import { toErrorCode } from './router.js';

const E = enums.ErrorCode;
const N = enums.NoticeCode;
const QS = enums.QueueState;
const CS = enums.ChallengeState;
const CP = enums.ColorPref;
const QUEUE_REFRESH_MS = 3000;
const LOAD_STALE_MS = 10000;

const u16 = (v) => Math.max(0, Math.min(65535, Math.round(+v || 0)));
const u32 = (v) => Math.max(0, Math.min(0xffffffff, Math.round(+v || 0)));

function errorFrame(code, fatal = false) {
    return encode.Error({ ref: 0, code, fatal, game: 0 });
}

export class ControlPlane {
    /**
     * @param {object} o
     * @param {object} o.config
     * @param {import('./presence.js').Presence} o.presence
     * @param {object} o.matchmaker
     * @param {object} o.challenges
     * @param {object} [o.conduct]
     * @param {import('./limits.js').SlidingWindowLimiter} o.limiter
     * @param {import('./limits.js').OnceStore} o.once
     * @param {object} o.shards
     * @param {Function} [o.activeBan]
     * @param {Function} [o.ratingOf]
     * @param {Function} [o.acceptsChallenges]
     * @param {object} [o.log]
     * @param {() => number} [o.now]
     * @param {() => number} [o.random]
     * @param {object} [o.registry]
     */
    constructor({ config, presence, matchmaker, challenges, conduct = null, limiter, once, shards, activeBan = null, ratingOf = null,
        acceptsChallenges = null, log = null, now = Date.now, random = Math.random, registry = defaultRegistry }) {
        this.config = config;
        this.presence = presence;
        this.mm = matchmaker;
        this.ch = challenges;
        this.conduct = conduct;
        this.limiter = limiter;
        this.once = once;
        this.shards = shards;
        this.activeBan = activeBan;
        this.ratingOf = ratingOf;
        this.acceptsChallenges = acceptsChallenges;
        this.log = log;
        this.now = now;
        this.random = random;
        this.categories = new Map(config.categories.map((c) => [c.id, c]));
        /** @type {Map<number, number>} userId -> gameId in progress */
        this.activeGames = new Map();
        /** @type {Set<number>} users whose game is being created */
        this.starting = new Set();
        /** @type {Map<number, {category:string, rated:boolean}>} users searching */
        this.queued = new Map();
        /** @type {Map<number, number>} userId -> ban end (sanction.applied) */
        this.bans = new Map();
        /** @type {Map<number, object>} shard -> last load report */
        this.loads = new Map();
        this.readyShards = new Set();
        this._timers = [];

        const r = registry;
        r.gaugeFn('scacelith_presence_online', 'Authenticated players online', () => this.presence.size);
        r.gaugeFn('scacelith_presence_connections', 'WebSocket connections counted by the primary', () => this.presence.connections);
        r.gaugeFn('scacelith_mm_searching', 'Players in the matchmaking queues', () => this.queued.size);
        r.gaugeFn('scacelith_challenges_open', 'Open challenges and private codes', () => this.ch.size ?? 0);
        r.gaugeFn('scacelith_ratelimit_keys', 'Keys held by the global rate limiter', () => this.limiter.size);
        this._created = r.counter('scacelith_games_created_total', 'Games created by the primary', ['source']);
        this._createFailed = r.counter('scacelith_games_create_failed_total', 'game.create failures');
        this._kicks = r.counter('scacelith_presence_kicks_total', 'Connections kicked by the primary', ['reason']);

        /** IPC handlers: type -> (payload, fromShard) => reply */
        this.handlers = {
            'presence.claim': (p, s) => this.presenceClaim(p, s),
            'presence.release': (p, s) => this.presenceRelease(p, s),
            'conn.ipAcquire': (p, s) => this.presence.ipAcquire(p.ip, s),
            'conn.ipRelease': (p, s) => ({ ok: this.presence.ipRelease(p.ip, s) }),
            'mm.join': (p, s) => this.mmJoin(p, s),
            'mm.leave': (p) => this.mmLeave(p),
            'challenge.create': (p) => this.challengeCreate(p),
            'challenge.accept': (p) => this.challengeAccept(p),
            'challenge.decline': (p) => this.challengeDecline(p),
            'challenge.cancel': (p) => this.challengeCancel(p),
            'challenge.joinCode': (p) => this.challengeJoinCode(p),
            'game.ended': (p) => this.gameEnded(p),
            'game.rematch': (p) => this.gameRematch(p),
            'game.active': (p) => this.gameActive(p),
            'game.recovered': (p) => this.gameActive(p),
            'conduct.record': (p) => this.conductRecord(p),
            'ratelimit.take': (p) => this.limiter.take(p),
            'ratelimit.refund': (p) => this.limiter.refund(p),
            'once.consume': (p) => this.once.consume(p),
            'sanction.applied': (p) => this.sanctionApplied(p),
            'session.revoked': (p) => this.sessionRevoked(p),
            'shard.load': (p, s) => { this.loads.set(s, { ...p, at: this.now() }); return null; },
            'shard.ready': (p, s) => this.shardReady(s),
        };
    }

    /**
     * Registers every handler on a shard's IPC endpoint.
     * @param {number} shard
     * @param {import('./ipc.js').Ipc} ipc
     */
    bind(shard, ipc) {
        for (const [type, h] of Object.entries(this.handlers)) ipc.on(type, (p) => h(p || {}, shard));
    }

    /** Starts the periodic work (pairing, queue status, challenge expiry, sweeps). */
    start() {
        const every = (ms, fn) => { const t = setInterval(() => { try { fn(); } catch (e) { this.log?.error?.('control-plane task failed', { err: e }); } }, ms); t.unref(); this._timers.push(t); };
        every(this.config.matchTickMs, () => this.matchTick());
        every(QUEUE_REFRESH_MS, () => this.refreshQueues());
        every(1000, () => this.expireChallenges());
        every(10000, () => this.sweep());
    }

    stop() { for (const t of this._timers) clearInterval(t); this._timers = []; }

    // ---- helpers --------------------------------------------------------------------------------

    _sendUser(userId, frames) {
        const p = this.presence.get(userId);
        if (!p) return false;
        this.shards.notify(p.shard, 'conn.send', { connId: p.connId, frames });
        return true;
    }

    _kick(userId, reason, code, closeCode, frames) {
        const p = this.presence.get(userId);
        if (!p) return false;
        this._kicks.labels(reason).inc();
        this.shards.notify(p.shard, 'conn.kick', { connId: p.connId, code, closeCode, frames });
        return true;
    }

    _banUntil(userId, now) {
        const cached = this.bans.get(userId);
        if (cached && cached > now) return cached;
        if (!this.activeBan) return 0;
        try {
            const b = this.activeBan(userId, now);
            if (!b) return 0;
            if (typeof b === 'number') return b;
            return b.until ?? b.endsAt ?? now + 86400000;
        } catch (e) {
            this.log?.error?.('ban lookup failed', { err: e });
            return 0;
        }
    }

    _busy(userId) { return this.activeGames.has(userId) || this.starting.has(userId); }

    _player(p, category) {
        const out = { userId: p.userId, username: p.username || this.presence.get(p.userId)?.username || '', rating: p.rating, provisional: p.provisional, shard: p.shard, connId: p.connId };
        if (out.rating === undefined) {
            let rating = this.config.initialRating, provisional = true;
            if (category && category !== 'custom' && this.ratingOf) {
                try {
                    const r = this.ratingOf(p.userId, category);
                    if (r) { rating = r.rating; provisional = (r.games ?? 0) < this.config.provisionalGames; }
                } catch (e) { this.log?.error?.('rating read failed', { err: e }); }
            }
            out.rating = rating;
            out.provisional = provisional;
        }
        return out;
    }

    _info(p) { return { userId: p.userId, name: p.username || p.name || '', rating: u16(p.rating), provisional: !!p.provisional }; }

    categoryOf(baseMs, incMs) {
        for (const c of this.categories.values()) if (c.baseMs === baseMs && c.incMs === incMs) return c.id;
        return 'custom';
    }

    _chooseShard(preferred) {
        const live = this.shards.list();
        if (!live.length) return -1;
        const now = this.now();
        const overloaded = (s) => {
            const l = this.loads.get(s);
            return !!(l && (l.overloaded || now - l.at > LOAD_STALE_MS));
        };
        if (preferred !== undefined && preferred !== null && live.includes(preferred) && !overloaded(preferred)) return preferred;
        let best = -1, bestKey = null;
        for (const s of live) {
            const l = this.loads.get(s) || { games: 0, conns: 0 };
            const key = [overloaded(s) ? 1 : 0, l.games || 0, l.conns || 0];
            if (bestKey === null || key[0] < bestKey[0] || (key[0] === bestKey[0] && (key[1] < bestKey[1] || (key[1] === bestKey[1] && key[2] < bestKey[2])))) {
                best = s; bestKey = key;
            }
        }
        return best;
    }

    /**
     * Creates a game on a host shard and attaches both players' connections.
     * @returns {Promise<{ok:true, gameId:number}|{error:number}>}
     */
    async createGame(spec, preferredShard, source) {
        if (typeof spec.autoPress !== 'boolean') spec = { ...spec, autoPress: this.config.autoPressClock !== false };
        const ids = [spec.white.userId, spec.black.userId];
        if (ids.some((u) => this._busy(u))) return { error: E.AlreadyInGame };
        for (const u of ids) this.starting.add(u);
        let r = null;
        const shard = this._chooseShard(preferredShard);
        // The shards report their load every 2 s: count the games placed since the last report, so
        // that a burst of placements does not pile onto the shard that looked the least loaded.
        const load = this.loads.get(shard);
        if (load) load.games = (load.games || 0) + 1;
        try {
            if (shard >= 0) r = await this.shards.request(shard, 'game.create', { spec }, { timeoutMs: 5000 });
        } catch (e) {
            this.log?.error?.('game.create failed', { shard, err: e });
        } finally {
            for (const u of ids) this.starting.delete(u);
        }
        if (!r || !r.ok || !isGameId(r.gameId)) {
            this._createFailed.inc();
            return { error: r && r.error ? toErrorCode(r.error) : E.Internal };
        }
        const gameId = r.gameId;
        this._created.labels(source).inc();
        for (const u of ids) {
            this.activeGames.set(u, gameId);
            this._leaveQueue(u, source !== 'queue');
            const p = this.presence.get(u);
            if (p) this.shards.notify(p.shard, 'game.attach', { gameId, userId: u, connId: p.connId });
        }
        return { ok: true, gameId };
    }

    // ---- presence -------------------------------------------------------------------------------

    presenceClaim({ userId, username = '', shard, connId, ip = '' }, from) {
        const now = this.now();
        const until = this._banUntil(userId, now);
        if (until > now) return { error: E.Banned, until };
        const existing = this.presence.get(userId);
        // The exact MAX_CONNECTIONS check (the upgrade allows a small reserve beyond it, presence.js).
        // A player with a game in progress is admitted anyway: refusing them would lose the game by
        // abandonment. Their number is bounded by the live games.
        const full = this.presence.size >= this.config.maxConnections;
        if (full && !existing && !this.activeGames.has(userId)) return { error: E.ServerFull };
        const { previous } = this.presence.claim({ userId, username, shard: shard ?? from, connId, ip });
        if (previous) {
            this._kicks.labels('replaced').inc();
            this.shards.notify(previous.shard, 'conn.kick', {
                connId: previous.connId, code: E.Replaced, closeCode: CloseCode.Replaced,
                frames: [errorFrame(E.Replaced, true), encode.Notice({ code: N.ReplacedByNewConnection, arg: 0 })],
            });
            this._leaveQueue(userId, false);
        }
        return { ok: true, activeGame: this.activeGames.get(userId) || 0, kicked: !!previous };
    }

    presenceRelease({ userId, connId }, from) {
        if (!this.presence.release(userId, connId, from)) return { ok: true, current: false };
        this._userGone(userId);
        return { ok: true, current: true };
    }

    _userGone(userId) {
        this._leaveQueue(userId, false);
        this._dropChallengesOf(userId);
    }

    // ---- matchmaking ----------------------------------------------------------------------------

    mmJoin(p, from) {
        const now = this.now();
        const cur = this.presence.get(p.userId);
        if (!cur || cur.connId !== p.connId) return { error: E.QueueNotAllowed };
        if (this._busy(p.userId)) return { error: E.AlreadyInGame };
        if (!this.categories.has(p.category)) return { error: E.InvalidCategory };
        if (p.rated && this.conduct) {
            let until = 0;
            try { until = this.conduct.cooldownUntil(p.userId, now) || 0; } catch (e) { this.log?.error?.('conduct lookup failed', { err: e }); }
            if (until > now) {
                this._sendUser(p.userId, [encode.Notice({ code: N.MatchmakingCooldown, arg: until })]);
                return { error: E.MatchmakingCooldown };
            }
        }
        if (this.queued.has(p.userId) || this.mm.has?.(p.userId)) this.mm.leave(p.userId);
        const entry = {
            userId: p.userId, username: p.username, category: p.category, rated: !!p.rated, rating: p.rating,
            provisional: !!p.provisional, shard: p.shard ?? from, connId: p.connId, colorBalance: p.colorBalance ?? 0, joinedAt: now,
        };
        const r = this.mm.join(entry);
        if (!r || r.error) return { error: r && r.error ? toErrorCode(r.error) : E.QueueNotAllowed };
        this.queued.set(p.userId, { category: p.category, rated: !!p.rated });
        setImmediate(() => this.sendQueueStatus(p.userId));
        return { ok: true };
    }

    mmLeave({ userId }) {
        this._leaveQueue(userId, true);
        return { ok: true };
    }

    // Removes a user from the queue; with `notify`, a searching user gets QueueStatus{Left}.
    _leaveQueue(userId, notify) {
        const q = this.queued.get(userId);
        this.queued.delete(userId);
        let was = false;
        try { was = this.mm.leave(userId); } catch (e) { this.log?.error?.('mm.leave failed', { err: e }); }
        if (notify && (q || was)) {
            this._sendUser(userId, [encode.QueueStatus({ category: q ? q.category : '', rated: q ? q.rated : false, state: QS.Left, waitMs: 0, window: 0, queued: 0 })]);
        }
    }

    /** Sends the current QueueStatus to a searching player. */
    sendQueueStatus(userId) {
        const q = this.queued.get(userId);
        if (!q) return;
        let st;
        try { st = this.mm.statusOf(userId, this.now()); } catch (e) { this.log?.error?.('mm.statusOf failed', { err: e }); return; }
        if (!st) return;
        let frame;
        try {
            frame = encode.QueueStatus({
                category: st.category ?? q.category, rated: st.rated ?? q.rated, state: st.state ?? QS.Searching,
                waitMs: u32(st.waitMs), window: u16(st.window), queued: u32(st.queued),
            });
        } catch (e) {
            this.log?.error?.('QueueStatus encoding failed', { err: e });
            return;
        }
        this._sendUser(userId, [frame]);
    }

    refreshQueues() {
        for (const userId of this.queued.keys()) this.sendQueueStatus(userId);
    }

    /** One pairing round. */
    matchTick() {
        const now = this.now();
        let pairs;
        try { pairs = this.mm.tick(now) || []; } catch (e) { this.log?.error?.('mm.tick failed', { err: e }); return; }
        for (const pair of pairs) this._startPairing(pair, now).catch((e) => this.log?.error?.('pairing failed', { err: e }));
    }

    async _startPairing({ category, rated, white, black }, now) {
        const cat = this.categories.get(category);
        for (const e of [white, black]) this.queued.delete(e.userId);
        if (!cat) return;
        for (const e of [white, black]) {
            this._sendUser(e.userId, [encode.QueueStatus({ category, rated: !!rated, state: QS.Matched, waitMs: u32(now - (e.joinedAt || now)), window: 0, queued: 0 })]);
        }
        const older = (white.joinedAt ?? now) <= (black.joinedAt ?? now) ? white : black;
        const preferred = this.presence.get(older.userId)?.shard ?? older.shard;
        const spec = {
            category, baseMs: cat.baseMs, incMs: cat.incMs, rated: !!rated,
            white: this._info(white), black: this._info(black), createdAt: now,
        };
        const r = await this.createGame(spec, preferred, 'queue');
        if (r.ok) return;
        // Back to the queue with their original waiting time.
        for (const e of [white, black]) {
            const p = this.presence.get(e.userId);
            if (!p || p.connId !== e.connId || this._busy(e.userId)) {
                this._sendUser(e.userId, [encode.QueueStatus({ category, rated: !!rated, state: QS.Left, waitMs: 0, window: 0, queued: 0 })]);
                continue;
            }
            const j = this.mm.join(e);
            if (j && j.ok) this.queued.set(e.userId, { category, rated: !!rated });
        }
    }

    // ---- challenges -----------------------------------------------------------------------------
    // The Challenges module (DESIGN 5.4) owns the challenge objects: { id, kind, from (player),
    // target, targetUserId, code, baseSec, incSec, baseMs, incMs, category, rated, color,
    // receiverColor, expiresAt, state }; accept/joinCode return the game spec with the colours.

    _statusFrame(c, state) {
        return encode.ChallengeStatus({ id: c.id >>> 0, state, target: c.target || '', code: c.code || '', baseSec: c.baseSec, incSec: c.incSec, rated: !!c.rated });
    }

    challengeCreate({ from, target = '', baseSec, incSec, rated, color }) {
        const now = this.now();
        let targetUser = null;
        if (target) {
            const tid = this.presence.userIdByName(target);
            if (tid) {
                let accepts = true;
                if (this.acceptsChallenges) {
                    try { accepts = this.acceptsChallenges(tid) !== false; } catch (e) { this.log?.error?.('preference read failed', { err: e }); }
                }
                targetUser = { userId: tid, username: this.presence.get(tid).username, online: true, acceptChallenges: accepts };
            }
        }
        let r;
        try { r = this.ch.create({ from, target, targetUser, baseSec, incSec, rated, color }, now); } catch (e) {
            this.log?.error?.('challenge.create failed', { err: e });
            return { error: E.Internal };
        }
        if (!r || r.error || !r.challenge) return { error: r && r.error ? toErrorCode(r.error) : E.Internal };
        const c = r.challenge;
        this._sendUser(from.userId, [this._statusFrame(c, CS.Pending)]);
        if (c.targetUserId) {
            this._sendUser(c.targetUserId, [encode.ChallengeReceived({
                id: c.id >>> 0, from: this._info(c.from), baseSec: c.baseSec, incSec: c.incSec, rated: !!c.rated,
                yourColor: c.receiverColor ?? CP.Random, expiresMs: u32(c.expiresAt - now),
            })]);
        }
        return { ok: true, id: c.id, code: c.code || '' };
    }

    async challengeAccept({ id, by }) {
        const pending = this.ch.get?.(id);
        if (this._busy(by.userId) || (pending && this._busy(pending.from.userId))) return { error: E.AlreadyInGame };
        let r;
        try { r = this.ch.accept(id, by, this.now()); } catch (e) { this.log?.error?.('challenge.accept failed', { err: e }); return { error: E.Internal }; }
        if (!r || r.error || !r.challenge) return { error: r && r.error ? toErrorCode(r.error) : E.ChallengeNotFound };
        return this._startChallengeGame(r.challenge, r.game, by);
    }

    async challengeJoinCode({ code, by }) {
        if (this._busy(by.userId)) return { error: E.AlreadyInGame };
        let r;
        try { r = this.ch.joinCode(code, by, this.now()); } catch (e) { this.log?.error?.('challenge.joinCode failed', { err: e }); return { error: E.Internal }; }
        if (!r || r.error || !r.challenge) return { error: r && r.error ? toErrorCode(r.error) : E.CodeInvalid };
        return this._startChallengeGame(r.challenge, r.game, by);
    }

    async _startChallengeGame(c, game, by) {
        const now = this.now();
        if (this._busy(c.from.userId)) {
            this._sendUser(c.from.userId, [this._statusFrame(c, CS.Unavailable)]);
            return { error: E.UserUnavailable };
        }
        let white = game?.white, black = game?.black;
        if (!white || !black) {
            const creatorWhite = c.color === CP.White ? true : c.color === CP.Black ? false : this.random() < 0.5;
            white = creatorWhite ? c.from : by;
            black = creatorWhite ? by : c.from;
        }
        const category = game?.category || c.category || this.categoryOf(c.baseSec * 1000, c.incSec * 1000);
        const spec = {
            category, baseMs: game?.baseMs ?? c.baseSec * 1000, incMs: game?.incMs ?? c.incSec * 1000,
            rated: !!(game ? game.rated : c.rated) && category !== 'custom',
            white: this._info(this._player({ ...white, rating: undefined }, category)),
            black: this._info(this._player({ ...black, rating: undefined }, category)),
            createdAt: now,
        };
        const preferred = this.presence.get(c.from.userId)?.shard;
        const r = await this.createGame(spec, preferred, 'challenge');
        if (!r.ok) {
            this._sendUser(c.from.userId, [this._statusFrame(c, CS.Unavailable)]);
            return { error: r.error };
        }
        this._sendUser(c.from.userId, [this._statusFrame(c, CS.Accepted)]);
        return { ok: true, id: c.id, gameId: r.gameId };
    }

    challengeDecline({ id, userId }) {
        let r;
        try { r = this.ch.decline(id, userId, this.now()); } catch (e) { this.log?.error?.('challenge.decline failed', { err: e }); return { error: E.Internal }; }
        if (!r || r.error) return { error: r && r.error ? toErrorCode(r.error) : E.ChallengeNotFound };
        if (r.challenge) this._sendUser(r.challenge.from.userId, [this._statusFrame(r.challenge, CS.Declined)]);
        return { ok: true };
    }

    challengeCancel({ id, userId }) {
        let r;
        try { r = this.ch.cancel(id, userId, this.now()); } catch (e) { this.log?.error?.('challenge.cancel failed', { err: e }); return { error: E.Internal }; }
        if (!r || r.error) return { error: r && r.error ? toErrorCode(r.error) : E.ChallengeNotFound };
        const c = r.challenge;
        if (c && c.targetUserId) this._sendUser(c.targetUserId, [this._statusFrame(c, CS.Cancelled)]);
        return { ok: true };
    }

    /** Expires challenges and private codes, and tells both sides. */
    expireChallenges() {
        let list;
        try { list = this.ch.expire(this.now()) || []; } catch (e) { this.log?.error?.('challenge expiry failed', { err: e }); return; }
        for (const c of list) {
            const f = this._statusFrame(c, CS.Expired);
            this._sendUser(c.from.userId, [f]);
            if (c.targetUserId) this._sendUser(c.targetUserId, [f]);
        }
    }

    // The user went offline: outgoing challenges are cancelled (their targets are told), incoming
    // ones become Unavailable (their creators are told).
    _dropChallengesOf(userId) {
        let list;
        try { list = this.ch.dropUser?.(userId) || []; } catch (e) { this.log?.error?.('challenge cleanup failed', { err: e }); return; }
        for (const c of list) {
            if (c.from.userId === userId) {
                if (c.targetUserId) this._sendUser(c.targetUserId, [this._statusFrame(c, CS.Cancelled)]);
            } else {
                this._sendUser(c.from.userId, [this._statusFrame(c, CS.Unavailable)]);
            }
        }
    }

    // ---- games ----------------------------------------------------------------------------------

    gameEnded({ gameId, whiteId, blackId }) {
        for (const u of [whiteId, blackId]) if (u && this.activeGames.get(u) === gameId) this.activeGames.delete(u);
        return { ok: true };
    }

    gameActive({ gameId, whiteId, blackId }) {
        if (!isGameId(gameId)) return { ok: false };
        for (const u of [whiteId, blackId]) if (u && !this.activeGames.has(u)) this.activeGames.set(u, gameId);
        return { ok: true };
    }

    /**
     * Both players asked for a rematch of gameId. white/black are the new game's players, colours
     * already swapped by the room ({ userId, name|username, rating?, provisional? } or user ids);
     * autoPress is the finished game's (AUTO_PRESS_CLOCK when absent).
     */
    async gameRematch({ gameId, white, black, category, baseMs, incMs, rated, autoPress }) {
        const now = this.now();
        const norm = (x) => (typeof x === 'number' ? { userId: x } : { ...x, username: x.username ?? x.name });
        const nw = norm(white), nb = norm(black);
        for (const p of [nw, nb]) {
            if (this._banUntil(p.userId, now) > now) return { error: E.RematchUnavailable };
            if (!this.presence.get(p.userId)) return { error: E.RematchUnavailable };
            const g = this.activeGames.get(p.userId);
            if ((g && g !== gameId) || this.starting.has(p.userId)) return { error: E.RematchUnavailable };
            if (rated && this.conduct) {
                let until = 0;
                try { until = this.conduct.cooldownUntil(p.userId, now) || 0; } catch { /* ignore */ }
                if (until > now) return { error: E.RematchUnavailable };
            }
        }
        for (const p of [nw, nb]) if (this.activeGames.get(p.userId) === gameId) this.activeGames.delete(p.userId);
        const cat = category || this.categoryOf(baseMs, incMs);
        const spec = {
            category: cat, baseMs, incMs, rated: !!rated && cat !== 'custom',
            white: this._info(this._player({ ...nw, rating: undefined }, cat)),
            black: this._info(this._player({ ...nb, rating: undefined }, cat)),
            createdAt: now, rematchOf: gameId, autoPress,
        };
        const r = await this.createGame(spec, shardOfGameId(gameId), 'rematch');
        return r.ok ? { ok: true, gameId: r.gameId } : { error: r.error === E.AlreadyInGame ? E.RematchUnavailable : r.error };
    }

    conductRecord({ userId, kind }) {
        if (!this.conduct) return { ok: false };
        const now = this.now();
        try {
            this.conduct.record(userId, kind, now);
            const until = this.conduct.cooldownUntil(userId, now) || 0;
            if (until > now) this._sendUser(userId, [encode.Notice({ code: N.MatchmakingCooldown, arg: until })]);
        } catch (e) {
            this.log?.error?.('conduct.record failed', { err: e });
        }
        return { ok: true };
    }

    // ---- sanctions and sessions -----------------------------------------------------------------

    sanctionApplied({ userId, until, reason }) {
        const end = +until || this.now() + 86400000;
        this.bans.set(userId, end);
        this.log?.security?.('sanction applied', { userId, until: end, reason });
        this._kick(userId, 'banned', E.Banned, CloseCode.Banned, [errorFrame(E.Banned, true), encode.Notice({ code: N.Banned, arg: end })]);
        const gameId = this.activeGames.get(userId);
        if (gameId) this.shards.notify(shardOfGameId(gameId), 'game.forfeit', { userId, gameId });
        this._userGone(userId);
        return { ok: true };
    }

    sessionRevoked({ userId, tokenHashes }) {
        this.shards.broadcast('auth.invalidate', { userId, tokenHashes: tokenHashes || null });
        return { ok: true };
    }

    // ---- shards ---------------------------------------------------------------------------------

    /** A shard finished its start-up: re-attach the live players of the games it hosts. */
    shardReady(shard) {
        this.readyShards.add(shard);
        let n = 0;
        for (const [userId, gameId] of this.activeGames) {
            if (shardOfGameId(gameId) !== shard) continue;
            const p = this.presence.get(userId);
            if (!p) continue;
            this.shards.notify(p.shard, 'game.attach', { gameId, userId, connId: p.connId });
            n++;
        }
        return { ok: true, reattached: n };
    }

    /** A shard's process exited: forget its connections, tell the others. */
    shardDown(shard) {
        this.readyShards.delete(shard);
        this.loads.delete(shard);
        const gone = this.presence.dropShard(shard);
        for (const u of gone) this._userGone(u);
        for (const s of this.shards.list()) if (s !== shard) this.shards.notify(s, 'shard.down', { shard });
        return gone.length;
    }

    sweep() {
        const now = this.now();
        this.limiter.sweep(now);
        this.once.sweep(now);
        for (const [u, until] of this.bans) if (until <= now) this.bans.delete(u);
    }

    stats() {
        return {
            online: this.presence.size, connections: this.presence.connections, searching: this.queued.size,
            challenges: this.ch.size ?? 0, playing: this.activeGames.size, shardsReady: this.readyShards.size,
        };
    }
}
