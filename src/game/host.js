// GameHost: the games of one shard (DESIGN.md section 5.3).
//
// * Rooms in a Map (game id -> entry). Each entry has at most one timer in a 10 ms timer wheel
//   driven by a single setInterval (no Node timer per game); the timer is (re)scheduled at the
//   room's nextDeadline() after every outcome.
// * Endpoints: { send(buf), connId, shard, close?(code, reason), rttMs? } - a local socket or a
//   relay over the bus; frames are sent as the room encoded them (one Buffer for both players).
// * Every outcome is journaled (journal.append), its anomaly goes to anticheat.recordAnomaly
//   (certain cheat + AUTO_SANCTION_CERTAIN_CHEATS: room.forfeit, Error{CheatDetected, fatal},
//   endpoint.close(4302) when the endpoint can, anticheat.sanctionCertain), its conduct events go
//   to the primary (conduct.record), a rematch agreement to the primary (game.rematch).
// * Persistence queue: finished games are committed with ONE store.games.finishBatch(records)
//   call at most DB_COMMIT_MS after the first of them ended (retry with exponential backoff on
//   error; the journal keeps them meanwhile). After the commit: RatingUpdate to both endpoints
//   still attached, journal.committed(id), primary 'game.ended'. A finished room stays in memory
//   until it is committed and its rematch window is closed.
// * recover() replays the journal at start-up: unfinished games are restored (both players
//   disconnected with a fresh grace, clocks restarted), ended-but-uncommitted games are queued for
//   the commit, games that cannot be rebuilt end ServerAborted and are committed as such.
//
// Deviations and additions to the DESIGN 5.3 contract:
//   * options: `createChessGame` (rules factory, required), `now` (clock function, default
//     clock.js now()), `metrics` (registry, default the process registry), `autoStart` (false:
//     no interval; tests drive runTimers(now) / pollCommits(now)), `commitBatchMax` (500).
//   * `bus` is accepted and unused: endpoints already know how to reach remote connections.
//   * recover() returns the count, or a Promise of it when journal.recover() is asynchronous.
//   * shutdown() is async (it awaits the pending commits and journal.flush()).
//   * When finishBatch throws for one record (err.gameId set), the batch is committed one game at a
//     time so that only the bad game stays pending (and in the journal).
//   * extra methods: onRtt(gameId, userId, rttMs), forfeitUser(userId) (the active game of a
//     sanctioned user on this shard ends Forfeit), activeGameOf(userId), room(gameId),
//     runTimers(now), pollCommits(now).
//   * onClientMessage for a game whose sender has no endpoint attached binds that endpoint
//     (a player coming back after a restart may only send Resync); a message for an unknown
//     game gets Error{NotInGame} without an anomaly (it may be an old, already dropped game);
//     a message for an existing game the user does not play is `foreign_game`.
//   * after recover(), each restored game is announced to the primary with
//     'game.recovered' { gameId, whiteId, blackId, shard } (not in the DESIGN 5.7 catalog: the
//     primary needs it to give the players their active game back after a full restart).
//   * the endpoint's `rttMs` (when a number) feeds room.onRtt before each message.
//
// Payloads this module produces or expects:
//   createGame(spec): { white, black: { userId, name, rating, provisional }, baseMs, incMs (ms),
//     rated, category? (derived from config.categories when absent; 'custom' is never rated),
//     rematchOf?, id? (pre-allocated id of this shard) }
//   primary 'game.ended': { gameId, whiteId, blackId, status, reason, rated (false for aborted
//     results), category, rematchOffer (colour of a pending rematch offer, 2 = none) }
//   primary 'game.rematch': { gameId, white, black (colours already swapped), category, baseMs,
//     incMs, rated }; a reply { error } or ok:false sends Error{RematchUnavailable} to both.
//   primary 'conduct.record': { userId, kind: 'abandon' | 'abort' | 'noshow' }
//   anticheat.recordAnomaly: { userId, gameId, kind, detail (short text), posMatched }; its
//     `certain` decides the sanction (the 6.5 table is the fallback when it answers nothing).
// Complexity: a message costs O(1) besides the rules; a timer (re)schedule is O(1); one 10 ms
// interval visits one wheel slot per call.

import { performance } from 'node:perf_hooks';
import { encode, enums, MSG, CloseCode } from '../protocol/index.js';
import { logger } from '../log.js';
import { metrics as processMetrics } from '../metrics.js';
import { GameIdAllocator } from '../util/ids.js';
import { now as clockNow } from './clock.js';
import { GameRoom, JournalKind, CERTAIN_KINDS } from './room.js';
import { TimerWheel } from './timer-wheel.js';

const { ErrorCode: EC, EndReason: ER, GameStatus: GS } = enums;
const NONE = 2;
const SLOT_MS = 10;
const MAX_BACKOFF_MS = 10000;

const ERROR_NAMES = Object.fromEntries(Object.entries(EC).map(([k, v]) => [v, k]));
const REASON_NAMES = Object.fromEntries(Object.entries(ER).map(([k, v]) => [v, k]));

class RoomEntry {
    constructor(room) {
        this.room = room;
        this.ep = [null, null];
        this.committed = false;
        this.removed = false;
        this.queued = false;
        // Timer wheel links.
        this._twPrev = null; this._twNext = null; this._twSlot = -1; this._twDeadline = Infinity;
    }
}

function sameEndpoint(a, b) {
    return a === b || (!!a && !!b && a.connId === b.connId && a.shard === b.shard);
}

function ratingChange(r) {
    const u16 = (v) => Math.max(0, Math.min(0xffff, Math.round(+v || 0)));
    return { before: u16(r.before), after: u16(r.after), games: Math.max(0, Math.min(0xffffffff, Math.floor(+r.games || 0))), provisional: !!r.provisional };
}

/**
 * The games hosted by one shard.
 */
export class GameHost {
    /**
     * @param {object} opts
     * @param {number} opts.shard
     * @param {object} opts.config
     * @param {object} [opts.store] Store (store.games.finishBatch)
     * @param {object} [opts.journal] Journal (append, committed, recover, flush)
     * @param {object} [opts.anticheat] { recordAnomaly, sanctionCertain }
     * @param {object} [opts.bus] unused
     * @param {object} [opts.primary] { request(type, payload) -> Promise }
     * @param {object} [opts.log]
     * @param {() => object} opts.createChessGame rules factory
     * @param {() => number} [opts.now]
     * @param {object} [opts.metrics] metrics registry
     * @param {boolean} [opts.autoStart=true] start the 10 ms interval
     * @param {number} [opts.commitBatchMax=500]
     */
    constructor({ shard = 0, config = {}, store = null, journal = null, anticheat = null, bus = null, primary = null, log = null,
        createChessGame, now = clockNow, metrics = processMetrics, autoStart = true, commitBatchMax = 500 } = {}) {
        if (typeof createChessGame !== 'function') throw new TypeError('GameHost: createChessGame is required');
        this.shard = shard;
        this.config = config;
        this.store = store;
        this.journal = journal;
        this.anticheat = anticheat;
        this.bus = bus;
        this.primary = primary;
        this.log = log || logger.child('game');
        this.createChessGame = createChessGame;
        this.now = now;
        this.commitMs = Number.isFinite(config.dbCommitMs) ? config.dbCommitMs : 50;
        this.commitBatchMax = commitBatchMax;
        this.autoSanction = config.autoSanctionCertainCheats !== false;

        this.rooms = new Map();          // gameId -> RoomEntry
        this.byUser = new Map();         // userId -> gameId of the running game on this shard
        this.pending = new Map();        // gameId -> RoomEntry (ended, not committed yet)
        this.ids = new GameIdAllocator(shard);
        this.wheel = new TimerWheel({ slotMs: SLOT_MS, slots: 4096, startAt: now() });
        this.activeCount = 0;
        this.nextCommitAt = Infinity;
        this.backoffMs = 0;
        this.commitInFlight = null;
        this.counts = { created: 0, moves: 0, ended: 0, committed: 0, recovered: 0, requeued: 0, aborted: 0 };
        this.closed = false;

        const m = metrics;
        this.m = {
            active: m.gauge('scacelith_games_active', 'Games in progress', [], { perShard: true }),
            moves: m.counter('scacelith_game_moves_total', 'Moves accepted'),
            moveUs: m.histogram('scacelith_game_move_processing_us', 'Time to process one move intent, delivery and journaling included (microseconds)',
                [5, 10, 25, 50, 100, 250, 500, 1000, 2500, 5000, 10000]),
            rejected: m.counter('scacelith_game_rejects_total', 'Game requests refused, by error code', ['code']),
            ended: m.counter('scacelith_games_ended_total', 'Games ended, by reason', ['reason']),
            batch: m.histogram('scacelith_game_commit_batch_size', 'Finished games per database commit', [1, 2, 5, 10, 25, 50, 100, 250, 500]),
            commitMs: m.histogram('scacelith_game_commit_latency_ms', 'Database commit latency of finished games', [1, 2, 5, 10, 25, 50, 100, 250, 500, 1000, 5000]),
            commitErrors: m.counter('scacelith_game_commit_errors_total', 'Failed commits of finished games (retried)'),
        };
        this._rejectChildren = new Map();
        this._endedChildren = new Map();

        this._fire = (entry, t) => {
            if (entry.removed) return;
            try {
                this._process(entry, entry.room.tick(t), null, -1, 0);
            } catch (err) {
                // A bug must not leave the game without a timer: log and try again in a second.
                this.log.error('game tick failed', { err, gameId: entry.room.id });
                this.wheel.schedule(entry, t + 1000);
            }
        };
        this.interval = null;
        if (autoStart) {
            this.interval = setInterval(() => this._onInterval(), SLOT_MS);
            if (typeof this.interval.unref === 'function') this.interval.unref();
        }
    }

    // ---- games ---------------------------------------------------------------------------------

    /**
     * Creates a game from the primary's spec.
     * @param {{white:object, black:object, baseMs:number, incMs:number, rated:boolean, category?:string, rematchOf?:number, id?:number}} spec
     * @returns {number} gameId
     */
    createGame(spec) {
        const t = this.now();
        const baseMs = Math.floor(spec.baseMs), incMs = Math.floor(spec.incMs);
        const category = spec.category || this._categoryOf(baseMs, incMs);
        const id = Number.isSafeInteger(spec.id) && spec.id > 0 ? spec.id : this.ids.next(Math.floor(t));
        if (this.rooms.has(id)) throw new Error(`GameHost: game ${id} exists`);
        const room = new GameRoom({
            id, category, baseMs, incMs, rated: !!spec.rated && category !== 'custom',
            white: spec.white, black: spec.black, createdAt: t, config: this.config,
            rematchOf: spec.rematchOf || 0, createChessGame: this.createChessGame,
        });
        const entry = new RoomEntry(room);
        this.rooms.set(id, entry);
        this.byUser.set(room.white.userId, id);
        this.byUser.set(room.black.userId, id);
        this.activeCount++;
        this.counts.created++;
        this.m.active.set(this.activeCount);
        const rec = room.createdRecord();
        this._append(id, rec);
        this._reschedule(entry);
        return id;
    }

    /**
     * Binds a player's connection to a game and sends it the snapshot.
     * @returns {boolean} false when the game is unknown or the user does not play it
     */
    attach(gameId, userId, endpoint) {
        const entry = this.rooms.get(gameId);
        const room = entry && entry.room;
        const color = room ? room.colorOf(userId) : -1;
        if (color < 0) {
            this._send(endpoint, encode.Error({ ref: 0, code: EC.NotInGame, fatal: false, game: gameId }));
            return false;
        }
        this._bind(entry, color, endpoint, true);
        return true;
    }

    /** The connection of a player closed. Ignored when another endpoint replaced it already. */
    detach(gameId, userId, endpoint) {
        const entry = this.rooms.get(gameId);
        if (!entry) return false;
        const color = entry.room.colorOf(userId);
        if (color < 0) return false;
        const cur = entry.ep[color];
        if (cur && endpoint && !sameEndpoint(cur, endpoint)) return false;
        entry.ep[color] = null;
        this._process(entry, entry.room.onDisconnect(color, this.now()), null, -1, 0);
        return true;
    }

    /**
     * A decoded client->server game message (Move .. Rematch) from `userId`.
     * O(1) apart from the rules' move validation; nothing is allocated per move beyond the
     * outcome, the MoveMade buffer and the 32-byte journal record.
     */
    onClientMessage(gameId, userId, msg, endpoint) {
        const t0 = performance.now();
        const seq = (msg && msg.seq) >>> 0;
        const entry = this.rooms.get(gameId);
        if (!entry) {
            this._reject(endpoint, EC.NotInGame, seq, gameId);
            return;
        }
        const room = entry.room;
        const color = room.colorOf(userId);
        if (color < 0) {
            this._reject(endpoint, EC.NotInGame, seq, gameId);
            this._handleAnomaly(entry, -1, userId, gameId, { kind: 'foreign_game', detail: `message type ${msg && msg.type}`, posMatched: false }, endpoint, seq);
            return;
        }
        if (endpoint && !entry.ep[color]) this._bind(entry, color, endpoint, false);
        else if (endpoint && typeof endpoint.rttMs === 'number' && endpoint.rttMs > 0) room.onRtt(color, endpoint.rttMs);
        const t = this.now();
        let out;
        try {
            switch (msg.type) {
                case MSG.Move: out = room.onMove(color, msg, t); break;
                case MSG.Resign: out = room.onResign(color, t, seq); break;
                case MSG.DrawOffer: out = room.onDrawOffer(color, t, seq); break;
                case MSG.DrawAnswer: out = room.onDrawAnswer(color, !!msg.accept, t, seq); break;
                case MSG.DrawClaim: out = room.onDrawClaim(color, t, seq); break;
                case MSG.Abort: out = room.onAbort(color, t, seq); break;
                case MSG.Resync: out = room.onResync(color, t); break;
                case MSG.Rematch: out = room.onRematch(color, !!msg.accept, t, seq); break;
                default:
                    this._reject(endpoint, EC.NotInGame, seq, gameId);
                    return;
            }
        } catch (err) {
            this.log.error('game message failed', { err, gameId, type: msg.type });
            this._reject(endpoint, EC.Internal, seq, gameId);
            this._reschedule(entry);
            return;
        }
        this._process(entry, out, endpoint, color, seq);
        if (msg.type === MSG.Move) {
            if (out.moved) { this.m.moves.inc(); this.counts.moves++; }
            this.m.moveUs.observe((performance.now() - t0) * 1000);
        }
    }

    /** A server round-trip measurement of a player (for connections relayed over the bus). */
    onRtt(gameId, userId, rttMs) {
        const entry = this.rooms.get(gameId);
        if (!entry) return;
        const color = entry.room.colorOf(userId);
        if (color < 0) return;
        entry.room.onRtt(color, rttMs);
        this._reschedule(entry);
    }

    /** Ends the running game of `userId` on this shard as a Forfeit (sanction from elsewhere). */
    forfeitUser(userId) {
        const id = this.byUser.get(userId);
        const entry = id !== undefined && this.rooms.get(id);
        if (!entry || entry.room.isOver) return false;
        const color = entry.room.colorOf(userId);
        this._process(entry, entry.room.forfeit(color, this.now()), null, -1, 0);
        return true;
    }

    /** Id of the running game of `userId` on this shard, or 0. */
    activeGameOf(userId) { return this.byUser.get(userId) || 0; }

    /** The GameRoom of a game (tests, admin), or null. */
    room(gameId) { const e = this.rooms.get(gameId); return e ? e.room : null; }

    /** Counters for the admin CLI and the metrics. */
    stats() {
        return {
            games: this.rooms.size,
            active: this.activeCount,
            finished: this.rooms.size - this.activeCount,
            pendingCommits: this.pending.size,
            commitInFlight: !!this.commitInFlight,
            timers: this.wheel.size,
            players: this.byUser.size,
            ...this.counts,
        };
    }

    // ---- time ----------------------------------------------------------------------------------

    /** Fires the rooms whose deadline is due at `t` (the interval calls it every 10 ms). */
    runTimers(t = this.now()) { return this.wheel.advance(t, this._fire); }

    /** Starts a commit when one is due at `t` (the interval calls it every 10 ms). */
    pollCommits(t = this.now()) {
        if (this.pending.size && !this.commitInFlight && t >= this.nextCommitAt) return this._commit(t);
        return null;
    }

    _onInterval() {
        const t = this.now();
        try { this.runTimers(t); } catch (err) { this.log.error('game timers failed', { err }); }
        try { this.pollCommits(t); } catch (err) { this.log.error('game commit poll failed', { err }); }
    }

    // ---- recovery and shutdown -------------------------------------------------------------------

    /**
     * Replays the journal (start-up). Returns the number of games taken back (restored + queued
     * for the commit), or a Promise of it when journal.recover() is asynchronous.
     */
    recover() {
        if (!this.journal || typeof this.journal.recover !== 'function') return 0;
        const r = this.journal.recover();
        if (r && typeof r.then === 'function') return r.then((map) => this._recoverFrom(map));
        return this._recoverFrom(r);
    }

    _recoverFrom(map) {
        if (!map) return 0;
        const t = this.now();
        let count = 0;
        for (const [gameId, records] of map) {
            if (this.rooms.has(gameId)) continue;
            let room = null, broken = null;
            try {
                room = GameRoom.fromJournal(records, { config: this.config, createChessGame: this.createChessGame });
            } catch (err) {
                broken = err;
                try {
                    room = GameRoom.fromJournal(records, { config: this.config, createChessGame: this.createChessGame, strict: false });
                } catch (err2) {
                    room = null;
                }
            }
            if (!room) {
                this.log.error('journal: game cannot be rebuilt, dropped', { gameId, err: broken });
                try { this.journal.committed(gameId); } catch { /* ignore */ }
                continue;
            }
            const entry = new RoomEntry(room);
            this.rooms.set(gameId, entry);
            count++;
            if (room.isOver) {
                room.recover(t);                       // closes the rematch window
                this.counts.requeued++;
                this._queueCommit(entry);
                this._endedCounter(room.result.reason).inc();
                continue;
            }
            this.activeCount++;
            if (broken) {
                this.log.warn('journal: game replayed partially, aborted', { gameId, err: broken });
                this.counts.aborted++;
                this._process(entry, room.serverAbort(t), null, -1, 0);
                continue;
            }
            const out = room.recover(t);
            this._process(entry, out, null, -1, 0);
            if (room.isOver) continue;
            this.byUser.set(room.white.userId, gameId);
            this.byUser.set(room.black.userId, gameId);
            this.counts.recovered++;
            this._request('game.recovered', { gameId, whiteId: room.white.userId, blackId: room.black.userId, shard: this.shard });
        }
        this.m.active.set(this.activeCount);
        this.log.info('games recovered from the journal', { restored: this.counts.recovered, requeued: this.counts.requeued, aborted: this.counts.aborted });
        return count;
    }

    /** Stops the timers, commits what is pending (a few attempts) and flushes the journal. */
    async shutdown() {
        if (this.interval) { clearInterval(this.interval); this.interval = null; }
        this.closed = true;
        for (let i = 0; i < 5 && (this.pending.size || this.commitInFlight); i++) {
            if (this.commitInFlight) { await this.commitInFlight; continue; }
            const r = this._commit(this.now());
            const ok = r && typeof r.then === 'function' ? await r : r;
            if (!ok) break;                             // the journal keeps them for the next start
        }
        if (this.journal && typeof this.journal.flush === 'function') await this.journal.flush();
    }

    // ---- internals -----------------------------------------------------------------------------

    _categoryOf(baseMs, incMs) {
        const cats = this.config.categories || [];
        for (const c of cats) if (c.baseMs === baseMs && c.incMs === incMs) return c.id;
        return 'custom';
    }

    _bind(entry, color, endpoint, sendSnapshot) {
        const room = entry.room;
        entry.ep[color] = endpoint;
        if (endpoint && typeof endpoint.rttMs === 'number' && endpoint.rttMs > 0) room.onRtt(color, endpoint.rttMs);
        const t = this.now();
        const out = room.isConnected(color) ? room.tick(t) : room.onReconnect(color, t);
        this._process(entry, out, endpoint, color, 0);
        if (sendSnapshot && !entry.removed) this._send(endpoint, room.snapshotBuffer(color, t));
    }

    _send(ep, buf) {
        if (!ep) return;
        try { ep.send(buf); } catch (err) { this.log.warn('game frame not delivered', { err, connId: ep.connId }); }
    }

    _reject(ep, code, seq, gameId) {
        this._rejectCounter(code).inc();
        this._send(ep, encode.Error({ ref: seq >>> 0, code, fatal: false, game: gameId }));
    }

    _rejectCounter(code) {
        let c = this._rejectChildren.get(code);
        if (!c) { c = this.m.rejected.labels(ERROR_NAMES[code] || String(code)); this._rejectChildren.set(code, c); }
        return c;
    }

    _endedCounter(reason) {
        let c = this._endedChildren.get(reason);
        if (!c) { c = this.m.ended.labels(REASON_NAMES[reason] || String(reason)); this._endedChildren.set(reason, c); }
        return c;
    }

    _append(gameId, rec) {
        if (!this.journal) return;
        try { this.journal.append(rec.kind, gameId, rec.payload, rec.at); } catch (err) {
            this.log.error('journal append failed', { err, gameId, kind: rec.kind });
        }
    }

    _request(type, payload) {
        if (!this.primary || typeof this.primary.request !== 'function') return;
        try {
            const p = this.primary.request(type, payload);
            if (p && typeof p.catch === 'function') p.catch((err) => this.log.warn('primary request failed', { type, err }));
        } catch (err) {
            this.log.warn('primary request failed', { type, err });
        }
    }

    // Delivers an outcome and applies its side effects. `ep` / `color` / `seq`: the sender.
    _process(entry, out, ep, color, seq) {
        const room = entry.room;
        const e0 = entry.ep[0], e1 = entry.ep[1];
        const b = out.broadcast;
        for (let i = 0; i < b.length; i++) { this._send(e0, b[i]); this._send(e1, b[i]); }
        for (let i = 0; i < out.toWhite.length; i++) this._send(e0, out.toWhite[i]);
        for (let i = 0; i < out.toBlack.length; i++) this._send(e1, out.toBlack[i]);
        if (ep) for (let i = 0; i < out.reply.length; i++) this._send(ep, out.reply[i]);
        const j = out.journal;
        for (let i = 0; i < j.length; i++) this._append(room.id, j[i]);
        for (let i = 0; i < out.conduct.length; i++) this._request('conduct.record', out.conduct[i]);
        if (out.rejected) this._rejectCounter(out.rejected).inc();
        if (out.ended) this._onEnded(entry);
        if (out.rematch) this._requestRematch(entry, out.rematch);
        this._reschedule(entry);
        if (out.anomaly) {
            const a = out.anomaly;
            const aep = a.color === color ? ep : entry.ep[a.color];
            this._handleAnomaly(entry, a.color, room.playerOf(a.color).userId, room.id, a, aep, a.color === color ? seq : 0);
        }
    }

    _handleAnomaly(entry, color, userId, gameId, a, ep, seq) {
        let res = null;
        if (this.anticheat && typeof this.anticheat.recordAnomaly === 'function') {
            try {
                res = this.anticheat.recordAnomaly({ userId, gameId, kind: a.kind, detail: a.detail, posMatched: !!a.posMatched });
            } catch (err) {
                this.log.warn('anticheat.recordAnomaly failed', { err, kind: a.kind });
            }
        }
        const act = (r) => this._maybeSanction(r, entry, color, userId, gameId, a.kind, ep, seq);
        if (res && typeof res.then === 'function') res.then(act, (err) => { this.log.warn('anticheat.recordAnomaly failed', { err }); act(null); });
        else act(res);
    }

    _maybeSanction(res, entry, color, userId, gameId, kind, ep, seq) {
        const certain = res && typeof res.certain === 'boolean' ? res.certain : CERTAIN_KINDS.has(kind);
        if (!certain || !this.autoSanction) return;
        if (entry && color >= 0 && !entry.removed && !entry.room.isOver) {
            this._process(entry, entry.room.forfeit(color, this.now()), null, -1, 0);
        }
        this._send(ep, encode.Error({ ref: seq >>> 0, code: EC.CheatDetected, fatal: true, game: gameId }));
        if (this.anticheat && typeof this.anticheat.sanctionCertain === 'function') {
            try {
                const p = this.anticheat.sanctionCertain({ userId, gameId, kind });
                if (p && typeof p.catch === 'function') p.catch((err) => this.log.error('anticheat.sanctionCertain failed', { err }));
            } catch (err) {
                this.log.error('anticheat.sanctionCertain failed', { err });
            }
        }
        if (ep && typeof ep.close === 'function') {
            try { ep.close(CloseCode.CheatDetected, 'cheat detected'); } catch { /* already closed */ }
        }
    }

    _onEnded(entry) {
        const room = entry.room;
        this.activeCount--;
        this.counts.ended++;
        this.m.active.set(this.activeCount);
        for (const p of [room.white, room.black]) if (this.byUser.get(p.userId) === room.id) this.byUser.delete(p.userId);
        this._endedCounter(room.result.reason).inc();
        this._queueCommit(entry);
    }

    _queueCommit(entry) {
        if (entry.queued || entry.committed) return;
        if (this.pending.size === 0 && !this.commitInFlight && !this.backoffMs) this.nextCommitAt = this.now() + this.commitMs;
        entry.queued = true;
        this.pending.set(entry.room.id, entry);
    }

    _requestRematch(entry, r) {
        if (!this.primary || typeof this.primary.request !== 'function') return;
        const room = entry.room;
        const fail = (why) => {
            this.log.info('rematch refused', { gameId: room.id, why });
            const buf = encode.Error({ ref: 0, code: EC.RematchUnavailable, fatal: false, game: room.id });
            this._send(entry.ep[0], buf);
            this._send(entry.ep[1], buf);
        };
        let p;
        try { p = this.primary.request('game.rematch', r); } catch (err) { fail(err.message); return; }
        Promise.resolve(p).then((reply) => { if (!reply || reply.error || reply.ok === false) fail(reply && reply.error); }, (err) => fail(err.message));
    }

    _reschedule(entry) {
        if (entry.removed) return;
        const d = entry.room.nextDeadline();
        if (d === Infinity) {
            this.wheel.cancel(entry);
            if (entry.committed && entry.room.isOver) this._remove(entry);
        } else {
            this.wheel.schedule(entry, d);
        }
    }

    _remove(entry) {
        this.wheel.cancel(entry);
        entry.removed = true;
        entry.ep[0] = entry.ep[1] = null;
        this.rooms.delete(entry.room.id);
    }

    // One store.games.finishBatch call for the pending games. Returns true / false, or a Promise
    // of it when the store is asynchronous.
    _commit(t = this.now()) {
        const batch = [];
        for (const entry of this.pending.values()) {
            batch.push(entry);
            if (batch.length >= this.commitBatchMax) break;
        }
        if (!batch.length) return true;
        const records = batch.map((e) => e.room.record());
        const t0 = performance.now();
        if (!this.store || !this.store.games || typeof this.store.games.finishBatch !== 'function') {
            this._commitDone(batch, null, t0, t);
            return true;
        }
        let res;
        try { res = this.store.games.finishBatch(records); } catch (err) {
            // The store rolls back the whole batch on one bad record (invalid_record, foreign_key):
            // commit the games one by one so that only the bad one stays pending.
            if (batch.length > 1 && err && err.gameId !== undefined) return this._commitEach(batch, records, t);
            this._commitFailed(err, batch.length, t);
            return false;
        }
        if (res && typeof res.then === 'function') {
            this.commitInFlight = res.then(
                (r) => { this.commitInFlight = null; this._commitDone(batch, r, t0, this.now()); return true; },
                (err) => { this.commitInFlight = null; this._commitFailed(err, batch.length, this.now()); return false; });
            return this.commitInFlight;
        }
        this._commitDone(batch, res, t0, t);
        return true;
    }

    _commitDone(batch, results, t0, t) {
        this.m.commitMs.observe(performance.now() - t0);
        this.m.batch.observe(batch.length);
        const byId = new Map();
        if (Array.isArray(results)) for (const r of results) if (r) byId.set(r.gameId, r);
        for (const entry of batch) {
            const room = entry.room;
            this.pending.delete(room.id);
            entry.queued = false;
            entry.committed = true;
            this.counts.committed++;
            const r = byId.get(room.id);
            if (r && r.ratings && r.ratings.white && r.ratings.black) {
                let buf = null;
                try {
                    buf = encode.RatingUpdate({ game: room.id, category: room.category, white: ratingChange(r.ratings.white), black: ratingChange(r.ratings.black) });
                } catch (err) {
                    this.log.error('RatingUpdate not encodable', { err, gameId: room.id });
                }
                if (buf) { this._send(entry.ep[0], buf); this._send(entry.ep[1], buf); }
            }
            if (this.journal) {
                try { this.journal.committed(room.id); } catch (err) { this.log.error('journal.committed failed', { err, gameId: room.id }); }
            }
            const res = room.result;
            this._request('game.ended', {
                gameId: room.id, whiteId: room.white.userId, blackId: room.black.userId,
                status: res.status, reason: res.reason, rated: room.rated && res.status !== GS.Aborted,
                category: room.category, rematchOffer: room.rematchOpen ? room.rematchBy : NONE,
            });
            this._reschedule(entry);
        }
        this.backoffMs = 0;
        this.nextCommitAt = this.pending.size ? t : Infinity;
    }

    _commitEach(batch, records, t) {
        let failed = null;
        for (let i = 0; i < batch.length; i++) {
            const t0 = performance.now();
            let res;
            try { res = this.store.games.finishBatch([records[i]]); } catch (err) { failed = err; continue; }
            if (res && typeof res.then === 'function') { failed = new Error('asynchronous store in _commitEach'); continue; }
            this._commitDone([batch[i]], res, t0, t);
        }
        if (failed) { this._commitFailed(failed, this.pending.size, t); return false; }
        return true;
    }

    _commitFailed(err, n, t) {
        this.m.commitErrors.inc();
        this.backoffMs = this.backoffMs ? Math.min(this.backoffMs * 2, MAX_BACKOFF_MS) : Math.max(100, this.commitMs);
        this.nextCommitAt = t + this.backoffMs;
        this.log.error('commit of finished games failed; retrying', { err, games: n, retryInMs: this.backoffMs });
    }
}

export { JournalKind };
