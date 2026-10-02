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
//   error; the journal keeps them meanwhile). A batch is committed only once the journal has
//   written (and fsynced) every record appended before it was chosen, their `ended` records
//   included (journal.hasUnwritten(), then journal.flush()): a crash can then never find a game
//   in the database whose journal says it is still running. When a journal write failed since
//   the last commit or fails meanwhile (journal.failedWrites), every game waiting for its commit
//   is marked (the failed write may have held its `ended` record) and journaled again as a
//   snapshot right before its own commit (_journalAgain: the batch's games only, which bounds the
//   synchronous work), and in the second case the commit is retried later. At the
//   JOURNAL_GATE_TRIES-th failed flush in a row (about 0.3 s after the first one with the default
//   DB_COMMIT_MS), or at the first one during shutdown(), the journal is considered down: the
//   batch is committed without waiting for it (error logged once per episode,
//   scacelith_game_commit_unjournaled_total), its snapshots and `committed` records still
//   appended, and each such commit starts one flush that ends the episode when it writes without
//   a failure. The database is then the only durable copy of the result (finishBatch ignores a
//   game id it already has, so a replay cannot apply it twice); what is left is the risk that
//   waiting for the journal avoids: after a crash or a restart before the journal has written
//   the game's snapshot or `committed` record, a game whose `ended` record was lost comes back
//   running (and its new result is ignored by the database).
//   Right before finishBatch, the anti-cheat's buffered anomalies are written when one of them is
//   not info (anticheat.flush() when anticheat.pendingSignalCount > 0), so that the analysis
//   queue policy of the commit sees those of the game's last second. In a shard both go to the
//   store writer thread, which writes its messages in order (src/store/writer.js): the flush only
//   hands the rows over. After the commit:
//   RatingUpdate to both endpoints still attached, journal.committed(id), primary 'game.ended'.
//   A finished room stays in memory until it is committed and its rematch window is closed.
// * recover() replays the journal at start-up: unfinished games are restored (both players
//   disconnected with the recovery grace, RECOVERY_GRACE_MS or the normal grace when longer; the
//   clock of the side to move restarts when that player is back, RECOVERY_CLOCK_HOLD_MS later at
//   most), ended-but-uncommitted games are queued for the commit, games that cannot be rebuilt
//   end ServerAborted and are committed as such. The grace starts at the replay, before the
//   shard listens: the players' connections were closed by the server (shutdown or crash), and
//   every client comes back at once. When a held clock starts (Outcome.clockStarted), the other
//   player gets a new snapshot (theirs showed the clock stopped).
//
// Deviations and additions to the DESIGN 5.3 contract:
//   * options: `createChessGame` (rules factory, required), `now` (clock function, default
//     clock.js now()), `metrics` (registry, default the process registry), `autoStart` (false:
//     no interval; tests drive runTimers(now) / pollCommits(now)), `commitBatchMax` (500).
//   * `bus` is accepted and unused: endpoints already know how to reach remote connections.
//   * recover() returns the count, or a Promise of it when journal.recover() is asynchronous.
//   * shutdown() is async (it awaits the pending commits and journal.flush(); a batch whose
//     journal flush fails is committed without it, and a failed final flush is logged, not thrown).
//   * When finishBatch throws for one record (err.gameId set), the batch is committed one game at a
//     time so that only the bad game stays pending (and in the journal).
//   * extra methods: onRtt(gameId, userId, rttMs), forfeitUser(userId) (the active game of a
//     sanctioned user on this shard ends Forfeit), declineRematch(gameId, userId) (the player
//     joined a queue), activeGameOf(userId), room(gameId), runTimers(now), pollCommits(now).
//   * onClientMessage for a game whose sender has no endpoint attached binds that endpoint
//     (a player coming back after a restart may only send Resync); a message for an unknown
//     game gets Error{NotInGame} without an anomaly (it may be an old, already dropped game);
//     a message for an existing game the user does not play is `foreign_game`.
//   * after recover(), each restored game is announced to the primary with
//     'game.recovered' { gameId, whiteId, blackId, shard } (not in the DESIGN 5.7 catalog: the
//     primary needs it to give the players their active game back after a full restart).
//   * the endpoint's `rttMs` (when a number) feeds room.onRtt before each message.
//   * stall credit (GAME_STALL_MIN_MS, GAME_STALL_CREDIT_MAX_MS; DESIGN 6.1): each run of the 10 ms
//     interval is a beat (heartbeat()). A beat that comes more than SLOT_MS + GAME_STALL_MIN_MS
//     after the previous one means the worker's event loop stopped meanwhile (garbage collection,
//     blocking I/O, CPU steal): the beat then runs the timers, commits and compaction from
//     setImmediate, once the poll phase has read what the sockets received during the stall,
//     since Node runs its timers before it reads sockets. Those timers fire only the deadlines
//     due by the beat that detected the stall (at the time they run): the poll phase read what
//     the sockets held when it began, but when it lasts (the backlog of a large stall) what
//     reaches a socket meanwhile waits for the next poll phase, so a later deadline waits for the
//     next beat, which detects such a poll phase as a stall of its own. Until the timers ran (and
//     while a beat is that late, before the interval has noticed) a game message (a Rematch
//     included), an attach, a detach or a forfeit counts as arrived when the stall began,
//     GAME_STALL_CREDIT_MAX_MS earlier than it is handled at most (stallCredit(): the room's
//     recvAt), so that a flag or a first-move timeout that fell during the stall overtakes neither
//     a move nor a request queued behind the opponent's closed connection. The forfeit of a
//     certain cheat takes the arrival of the game message that revealed it (or, when the
//     anti-cheat answers later, is credited like forfeitUser when that answer comes), so that the
//     victim's deadline that fell during the stall does not end the game first. The start of the
//     stall (stallStart()) goes to the room with it, and the timers that run after the stall pass
//     it to room.tick(), so that a first-move timeout that fell during the stall records no no-show
//     whichever of them processes it (a request does after a stall longer than the credit).
//     Nothing of it is journaled: the journal holds the clock values it produced, and a replay
//     uses them. Without the interval (autoStart false) there is no beat and no credit unless a
//     test calls heartbeat().
//     stallDuring(t0) tells the router whether a stall overlaps a round trip it measured.
//   * gesture relay (GESTURE_RATE): relayGesture(gameId, userId, frame) copies a player's
//     C_Gesture frame, without its seq, into an S_Gesture for the opponent's endpoint (the two
//     messages have the same fields after seq: protocol.codec.test.js), through the endpoint's
//     sendDroppable (which skips it when that connection, or the bus link to it, already holds a
//     backlog) when it has one. Cosmetic: nothing is journaled, the room, its clocks, gseq and the
//     anti-cheat never see it; it is dropped when the sender plays no colour (no anomaly: it is not
//     authoritative) or the opponent is not attached, and relayed as long as the room exists,
//     the rematch window included.
//   * journal compaction (JOURNAL_COMPACT_SEGMENTS, src/store/journal.js): compactJournal(now),
//     called by the 10 ms interval after the timers and the commits, appends a snapshot record
//     (room.journalSnapshot) of each game the journal asks for (journal.compactionCandidates():
//     at most SNAPSHOTS_PER_TICK per call and 8 per flushed batch, about 0.3 ms each for the
//     longest game) that is hosted here and not committed yet. It runs between two outcomes, so
//     the snapshot includes every record already appended for the game. The journal asks first
//     for the games whose records a failed write lost, running or waiting for their commit: the
//     snapshot supersedes the lost records, so a restart does not replay them from a gap. A
//     journal without compactionCandidates (the in-memory test journal) is never compacted.
//
// Payloads this module produces or expects:
//   createGame(spec): { white, black: { userId, name, rating, provisional }, baseMs, incMs (ms),
//     rated, category? (derived from config.categories when absent; 'custom' is never rated),
//     rematchOf?, id? (pre-allocated id of this shard), autoPress? (AUTO_PRESS_CLOCK when absent) }
//   primary 'game.ended': { gameId, whiteId, blackId, status, reason, rated (false for aborted
//     results), category, rematchOffer (colour of a pending rematch offer, 2 = none) }
//   primary 'game.rematch': { gameId, white, black (colours already swapped), category, baseMs,
//     incMs, rated, autoPress (the finished game's) }; a reply { error } or ok:false sends
//     Error{RematchUnavailable} to both.
//   primary 'conduct.record': { userId, kind: 'abandon' | 'abort' | 'noshow' }
//   anticheat.recordAnomaly: { userId, gameId, kind, detail (short text), posMatched }; its
//     `certain` decides the sanction (the 6.5 table is the fallback when it answers nothing).
// Complexity: a message costs O(1) besides the rules; a timer (re)schedule is O(1); one 10 ms
// interval visits the wheel slots since the previous call (normally two: the previous one again
// and the current one); a relayed gesture is one lookup and one 25-byte copy.

import { performance } from 'node:perf_hooks';
import { encode, enums, MSG, CloseCode } from '../protocol/index.js';
import { logger } from '../log.js';
import { metrics as processMetrics } from '../metrics.js';
import { GameIdAllocator } from '../util/ids.js';
import { now as clockNow } from './clock.js';
import { GameRoom, CERTAIN_KINDS } from './room.js';
import { TimerWheel } from './timer-wheel.js';

const { ErrorCode: EC, EndReason: ER, GameStatus: GS } = enums;
const NONE = 2;
const SLOT_MS = 10;
const MAX_BACKOFF_MS = 10000;
/** Journal flushes failed in a row before commits stop waiting for the journal (see _commit). */
export const JOURNAL_GATE_TRIES = 3;
/** Journal snapshots built per 10 ms interval at most (compaction stays off the move path). */
const SNAPSHOTS_PER_TICK = 2;
/** Sizes of the fixed-size gesture frames; S_Gesture is C_Gesture without its seq (bytes 1-4). */
const GESTURE_FIELDS = { game: 1, ply: 0, touch: 64, aim: 64, placed: 0, flags: 0, yaw: 0, pitch: 0, lean: 0 };
const C_GESTURE_BYTES = encode.C_Gesture({ seq: 0, ...GESTURE_FIELDS }).length;
const S_GESTURE_BYTES = encode.S_Gesture(GESTURE_FIELDS).length;

const ERROR_NAMES = Object.fromEntries(Object.entries(EC).map(([k, v]) => [v, k]));
const REASON_NAMES = Object.fromEntries(Object.entries(ER).map(([k, v]) => [v, k]));

class RoomEntry {
    constructor(room) {
        this.room = room;
        this.ep = [null, null];
        this.committed = false;
        this.removed = false;
        this.queued = false;
        this.rejournal = false;          // a failed journal write may have lost its `ended` record
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
     * @param {object} [opts.journal] Journal (append, committed, recover, flush; hasUnwritten()
     *   and failedWrites when its writes are asynchronous; releaseRecovered() if it has one)
     * @param {object} [opts.anticheat] { recordAnomaly, sanctionCertain, flush?, pendingSignalCount? }
     * @param {object} [opts.bus] unused
     * @param {object} [opts.primary] { request(type, payload) -> Promise }
     * @param {object} [opts.log]
     * @param {() => object} opts.createChessGame rules factory
     * @param {() => number} [opts.now]
     * @param {object} [opts.metrics] metrics registry
     * @param {boolean} [opts.autoStart=true] start the 10 ms interval
     * @param {number} [opts.commitBatchMax=500]
     * @param {number} [opts.lastGameId] the largest game id of the database: new ids come after it
     */
    constructor({ shard = 0, config = {}, store = null, journal = null, anticheat = null, bus = null, primary = null, log = null,
        createChessGame, now = clockNow, metrics = processMetrics, autoStart = true, commitBatchMax = 500, lastGameId = 0 } = {}) {
        if (typeof createChessGame !== 'function') throw new TypeError('GameHost: createChessGame is required');
        this.shard = shard;
        this.config = config;
        this.store = store;
        this.journal = journal;
        this.journalFailures = journal && typeof journal.failedWrites === 'number' ? journal.failedWrites : 0;   // see _journalAgain
        this.gateFailures = 0;           // journal flushes failed in a row before a commit (see _commit)
        this.unjournaled = false;        // an episode of commits made without waiting for the journal
        this.journalProbe = null;        // the flush that may end that episode
        this.anticheat = anticheat;
        this.bus = bus;
        this.primary = primary;
        this.log = log || logger.child('game');
        this.createChessGame = createChessGame;
        this.now = now;
        this.commitMs = Number.isFinite(config.dbCommitMs) ? config.dbCommitMs : 50;
        this.commitBatchMax = commitBatchMax;
        this.autoSanction = config.autoSanctionCertainCheats !== false;
        this.stallMinMs = Number.isFinite(config.gameStallMinMs) ? config.gameStallMinMs : 30;
        this.stallCreditMaxMs = Number.isFinite(config.gameStallCreditMaxMs) ? config.gameStallCreditMaxMs : 5000;
        this._beat = NaN;                // time of the last beat (NaN: none yet, no stall detection)
        this._stallFrom = NaN;           // start of the stall the last beat detected, until its timers ran
        this._firingAt = 0;              // time at which runTimers fires the rooms due
        this._firingStall = Infinity;    // stall start passed to room.tick() by the timers that run after it
        this.lastStallEnd = -Infinity;   // time of the beat that detected the last stall

        this.rooms = new Map();          // gameId -> RoomEntry
        this.byUser = new Map();         // userId -> gameId of the running game on this shard
        this.pending = new Map();        // gameId -> RoomEntry (ended, not committed yet)
        this.ids = new GameIdAllocator(shard);
        this.ids.seed(lastGameId);
        this.wheel = new TimerWheel({ slotMs: SLOT_MS, slots: 4096, startAt: now() });
        this.activeCount = 0;
        this.nextCommitAt = Infinity;
        this.backoffMs = 0;
        this.commitInFlight = null;
        this.counts = { created: 0, moves: 0, ended: 0, committed: 0, recovered: 0, requeued: 0, aborted: 0, snapshots: 0, stalls: 0, gestures: 0 };
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
            unjournaled: m.counter('scacelith_game_commit_unjournaled_total',
                'Finished games committed to the database without waiting for the journal, whose writes kept failing'),
            stallMs: m.histogram('scacelith_game_stall_ms', 'Stalls of the worker\'s event loop detected by the game timers (ms, longer than GAME_STALL_MIN_MS)',
                [25, 50, 100, 250, 500, 1000, 2500, 5000, 10000, 30000]),
            stallCredit: m.counter('scacelith_game_stall_credit_ms_total', 'Time given back to game requests handled after a stall of the worker (ms)'),
            timerLate: m.histogram('scacelith_game_timer_late_ms', 'Lateness of the game deadlines (flags, first-move timeouts, graces) when their timer fired (ms)',
                [1, 5, 10, 25, 50, 100, 250, 1000, 5000]),
            gestures: m.counter('scacelith_gestures_relayed_total', 'Gestures relayed to the opponent'),
        };
        const gestureDrops = m.counter('scacelith_gestures_dropped_total', 'Gestures not relayed, by reason', ['reason']);
        this._gNoGame = gestureDrops.labels('no_game');
        this._gNotPlayer = gestureDrops.labels('not_player');
        this._gNoOpponent = gestureDrops.labels('no_opponent');
        this._gBacklog = gestureDrops.labels('backlog');
        this._gMalformed = gestureDrops.labels('malformed');
        this._rejectChildren = new Map();
        this._endedChildren = new Map();

        this._fire = (entry) => {
            if (entry.removed) return;
            const t = this._firingAt;
            this.m.timerLate.observe(Math.max(0, t - entry._twDeadline));
            try {
                this._process(entry, entry.room.tick(t, this._firingStall), null, -1, 0);
            } catch (err) {
                // A bug must not leave the game without a timer: log and try again in a second.
                this.log.error('game tick failed', { err, gameId: entry.room.id });
                this.wheel.schedule(entry, t + 1000);
            }
        };
        // The timers after a detected stall: those due by the beat that detected it (see the header).
        this._afterStall = () => {
            const from = this._stallFrom;
            this._stallFrom = NaN;
            if (!this.closed) this._tick(this.now(), from, this._beat);
        };
        this.interval = null;
        if (autoStart) {
            this.interval = setInterval(() => this.heartbeat(), SLOT_MS);
            if (typeof this.interval.unref === 'function') this.interval.unref();
        }
    }

    // ---- games ---------------------------------------------------------------------------------

    /**
     * Creates a game from the primary's spec.
     * @param {{white:object, black:object, baseMs:number, incMs:number, rated:boolean, category?:string, rematchOf?:number, id?:number, autoPress?:boolean}} spec
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
            autoPress: typeof spec.autoPress === 'boolean' ? spec.autoPress : this.config.autoPressClock !== false,
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
        const t = this.now(), from = this.stallStart(t);
        this._process(entry, entry.room.onDisconnect(color, t, t - this.stallCredit(t, from), from), null, -1, 0);
        return true;
    }

    /**
     * A decoded client->server game message (Move .. Rematch) from `userId`.
     * O(1) apart from the rules' move validation; a move allocates only small fixed-size objects
     * (the outcome and its arrays, the rules' result, the MoveMade buffer, the 32-byte journal
     * record and its wrapper).
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
        const t = this.now();
        // Arrival of a request that waited in a socket during a stall of this worker (header).
        const from = this.stallStart(t);
        const credit = this.stallCredit(t, from);
        const te = t - credit;
        if (endpoint && !entry.ep[color]) this._bind(entry, color, endpoint, false, t, from, te);
        else if (endpoint && typeof endpoint.rttMs === 'number' && endpoint.rttMs > 0) room.onRtt(color, endpoint.rttMs);
        let out;
        try {
            switch (msg.type) {
                case MSG.Move: out = room.onMove(color, msg, t, te, from); break;
                case MSG.Resign: out = room.onResign(color, t, seq, te, from); break;
                case MSG.DrawOffer: out = room.onDrawOffer(color, t, seq, te, from); break;
                case MSG.DrawAnswer: out = room.onDrawAnswer(color, !!msg.accept, t, seq, te, from); break;
                case MSG.DrawClaim: out = room.onDrawClaim(color, t, seq, te, from); break;
                case MSG.Abort: out = room.onAbort(color, t, seq, te, from); break;
                case MSG.Resync: out = room.onResync(color, t, te, from); break;
                case MSG.Rematch: out = room.onRematch(color, !!msg.accept, t, seq, te, from); break;
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
        this._process(entry, out, endpoint, color, seq, te, from);
        if (credit > 0 && msg.type !== MSG.Resync && msg.type !== MSG.Rematch) this.m.stallCredit.inc(credit);
        if (msg.type === MSG.Move) {
            if (out.moved) { this.m.moves.inc(); this.counts.moves++; }
            this.m.moveUs.observe((performance.now() - t0) * 1000);
        }
    }

    /**
     * The player joined a queue: its finished game's rematch window closes, as with a
     * Rematch{accept: false} (DESIGN 6.3; the router, on every QueueJoin). This is no client
     * request: nothing is sent back, and a game already gone or a window already closed counts no
     * refusal.
     */
    declineRematch(gameId, userId) {
        const entry = this.rooms.get(gameId);
        const color = entry ? entry.room.colorOf(userId) : -1;
        if (color < 0) return;
        const t = this.now(), from = this.stallStart(t);
        const te = t - this.stallCredit(t, from);
        const out = entry.room.onRematch(color, false, t, 0, te, from);
        out.rejected = 0;
        this._process(entry, out, null, color, 0, te, from);
    }

    /**
     * Relays a player's gesture (the raw C_Gesture frame, validated by the router of its
     * connection) to the opponent as an S_Gesture (see the header). Returns whether it was sent.
     * @param {number} gameId
     * @param {number} userId the sender
     * @param {Buffer} frame
     * @returns {boolean}
     */
    relayGesture(gameId, userId, frame) {
        const entry = this.rooms.get(gameId);
        if (!entry || entry.removed) { this._gNoGame.inc(); return false; }
        const color = entry.room.colorOf(userId);
        if (color < 0) { this._gNotPlayer.inc(); return false; }
        const ep = entry.ep[color ^ 1];
        if (!ep) { this._gNoOpponent.inc(); return false; }
        if (frame.length !== C_GESTURE_BYTES) { this._gMalformed.inc(); return false; }
        const out = Buffer.allocUnsafe(S_GESTURE_BYTES);
        out[0] = MSG.S_Gesture;
        frame.copy(out, 1, 5);
        let sent;
        try {
            sent = typeof ep.sendDroppable === 'function' ? ep.sendDroppable(out) : ep.send(out) !== false;
        } catch (err) {
            this.log.warn('gesture not delivered', { err, connId: ep.connId });
            sent = false;
        }
        if (!sent) { this._gBacklog.inc(); return false; }
        this.m.gestures.inc();
        this.counts.gestures++;
        return true;
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
        // Like a game request, the sanction may have waited in a socket during a stall (header).
        const t = this.now(), from = this.stallStart(t);
        this._process(entry, entry.room.forfeit(color, t, t - this.stallCredit(t, from), from), null, -1, 0);
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

    /**
     * Fires, at `t`, the rooms whose deadline is due at `dueBy` (`t` at most; the interval calls
     * it every 10 ms with dueBy = t). `stalledSince`: start of the stall these timers ran late for
     * (room.tick()).
     */
    runTimers(t = this.now(), stalledSince = Infinity, dueBy = t) {
        this._firingAt = t;
        this._firingStall = stalledSince;
        try {
            return this.wheel.advance(Math.min(dueBy, t), this._fire);
        } finally {
            this._firingStall = Infinity;
        }
    }

    /**
     * One beat of the 10 ms interval (see the header): the timers, commits and compaction, at
     * once, or from setImmediate after a stall (the timers due by this beat). Returns whether
     * this beat detected a stall.
     * @param {number} [t]
     * @returns {boolean}
     */
    heartbeat(t = this.now()) {
        const prev = this._beat;
        this._beat = t;
        if (t - prev > SLOT_MS + this.stallMinMs) {
            this.m.stallMs.observe(t - prev - SLOT_MS);
            this.counts.stalls++;
            this.lastStallEnd = t;
            if (Number.isNaN(this._stallFrom)) {
                this._stallFrom = prev + SLOT_MS;
                setImmediate(this._afterStall);
            }
            return true;
        }
        if (Number.isNaN(this._stallFrom)) this._tick(t);
        return false;
    }

    /**
     * Time given back to a game request handled at `t` (ms, 0 without a stall): from the start of
     * the stall it waited through (stallStart); GAME_STALL_CREDIT_MAX_MS at most.
     * @param {number} t
     * @param {number} [from] stallStart(t)
     * @returns {number}
     */
    stallCredit(t, from = this.stallStart(t)) {
        return from < Infinity ? Math.max(0, Math.min(this.stallCreditMaxMs, t - from)) : 0;
    }

    /**
     * Start of the stall that a game request handled at `t` waited through (Infinity: none): the
     * stall the last beat detected, until the timers ran after it, or the one that began when the
     * next beat was due, when that beat is already GAME_STALL_MIN_MS late.
     * @param {number} t
     * @returns {number}
     */
    stallStart(t) {
        if (!Number.isNaN(this._stallFrom)) return this._stallFrom;
        return t - this._beat > SLOT_MS + this.stallMinMs ? this._beat + SLOT_MS : Infinity;
    }

    /**
     * Whether a stall of this worker overlaps the time since `t0` (a round trip measured over it
     * includes the stall: router.js leaves it out of the player's average).
     * @param {number} t0 clock.js time
     * @param {number} [t]
     * @returns {boolean}
     */
    stallDuring(t0, t = this.now()) {
        return this.lastStallEnd > t0 || this.stallCredit(t) > 0;
    }

    /** Starts a commit when one is due at `t` (the interval calls it every 10 ms). */
    pollCommits(t = this.now()) {
        if (this.pending.size && !this.commitInFlight && t >= this.nextCommitAt) return this._commit(t);
        return null;
    }

    /**
     * Journal compaction: appends a snapshot record of each game the journal asks for (see the
     * header). O(1) when the journal wants nothing. Returns the number of snapshots appended.
     */
    compactJournal(t = this.now()) {
        const j = this.journal;
        if (!j || typeof j.compactionCandidates !== 'function') return 0;
        const ids = j.compactionCandidates(SNAPSHOTS_PER_TICK);
        let n = 0;
        for (let i = 0; i < ids.length; i++) {
            const entry = this.rooms.get(ids[i]);
            // Not hosted here, or its 'committed' record is already appended: nothing to keep.
            if (!entry || entry.committed) continue;
            let rec;
            try { rec = entry.room.journalSnapshot(t); } catch (err) {
                this.log.error('journal snapshot failed', { err, gameId: ids[i] });
                continue;
            }
            this._append(ids[i], rec);
            n++;
        }
        this.counts.snapshots += n;
        return n;
    }

    _tick(t, stalledSince = Infinity, dueBy = t) {
        try { this.runTimers(t, stalledSince, dueBy); } catch (err) { this.log.error('game timers failed', { err }); }
        try { this.pollCommits(t); } catch (err) { this.log.error('game commit poll failed', { err }); }
        try { this.compactJournal(t); } catch (err) { this.log.error('journal compaction failed', { err }); }
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
            this.ids.seed(gameId);                    // never given again, even with the clock behind
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
        this.journal.releaseRecovered?.();     // the rooms keep no reference to the records
        this.log.info('games recovered from the journal', { restored: this.counts.recovered, requeued: this.counts.requeued, aborted: this.counts.aborted });
        return count;
    }

    /**
     * Stops the timers, commits what is pending (a few attempts) and flushes the journal. Once
     * closed, a batch whose journal flush fails is committed without waiting for the journal
     * (_commit): the database is then the only durable copy of those results. A failed final
     * flush is logged, not thrown, so the caller's stop goes on.
     */
    async shutdown() {
        if (this.interval) { clearInterval(this.interval); this.interval = null; }
        this.closed = true;
        this._beat = NaN;
        for (let i = 0; i < 5 && (this.pending.size || this.commitInFlight); i++) {
            let ok;
            try {
                if (this.commitInFlight) { await this.commitInFlight; continue; }
                const r = this._commit(this.now());
                ok = r && typeof r.then === 'function' ? await r : r;
            } catch (err) {
                this.log.error('commit of finished games failed at shutdown', { err, games: this.pending.size });
                ok = false;
            }
            if (!ok) break;                             // the database refused them: the journal keeps them for the next start
        }
        if (this.journal && typeof this.journal.flush === 'function') {
            try { await this.journal.flush(); } catch (err) {
                this.log.error('journal flush failed at shutdown', { err, pendingCommits: this.pending.size });
            }
        }
    }

    // ---- internals -----------------------------------------------------------------------------

    _categoryOf(baseMs, incMs) {
        const cats = this.config.categories || [];
        for (const c of cats) if (c.baseMs === baseMs && c.incMs === incMs) return c.id;
        return 'custom';
    }

    _bind(entry, color, endpoint, sendSnapshot, t = this.now(), from = this.stallStart(t), te = t - this.stallCredit(t, from)) {
        const room = entry.room;
        entry.ep[color] = endpoint;
        if (endpoint && typeof endpoint.rttMs === 'number' && endpoint.rttMs > 0) room.onRtt(color, endpoint.rttMs);
        const out = room.isConnected(color) ? room.tick(te, from) : room.onReconnect(color, t, te, from);
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

    // Delivers an outcome and applies its side effects. `ep` / `color` / `seq`: the sender;
    // `recvAt` / `stalledSince`: the arrival the room gave the sender's request (stall credit,
    // see the header), which the forfeit of a certain cheat it revealed keeps (_maybeSanction).
    _process(entry, out, ep, color, seq, recvAt, stalledSince = Infinity) {
        const room = entry.room;
        const e0 = entry.ep[0], e1 = entry.ep[1];
        const b = out.broadcast;
        for (let i = 0; i < b.length; i++) { this._send(e0, b[i]); this._send(e1, b[i]); }
        if (ep) for (let i = 0; i < out.reply.length; i++) this._send(ep, out.reply[i]);
        const j = out.journal;
        for (let i = 0; i < j.length; i++) this._append(room.id, j[i]);
        for (let i = 0; i < out.conduct.length; i++) this._request('conduct.record', out.conduct[i]);
        if (out.rejected) this._rejectCounter(out.rejected).inc();
        if (out.ended) this._onEnded(entry);
        if (out.rematch) this._requestRematch(entry, out.rematch);
        if ((out.clockStarted === 0 || out.clockStarted === 1) && !out.moved && !room.isOver) {
            // A clock held since a recovery started: the opponent's display shows it stopped.
            const opp = out.clockStarted ^ 1;
            if (entry.ep[opp]) this._send(entry.ep[opp], room.snapshotBuffer(opp, this.now()));
        }
        this._reschedule(entry);
        if (out.anomaly) {
            const a = out.anomaly;
            const aep = a.color === color ? ep : entry.ep[a.color];
            this._handleAnomaly(entry, a.color, room.playerOf(a.color).userId, room.id, a, aep, a.color === color ? seq : 0, recvAt, stalledSince);
        }
    }

    // `recvAt` / `stalledSince`: see _process.
    _handleAnomaly(entry, color, userId, gameId, a, ep, seq, recvAt, stalledSince = Infinity) {
        let res = null;
        if (this.anticheat && typeof this.anticheat.recordAnomaly === 'function') {
            try {
                res = this.anticheat.recordAnomaly({ userId, gameId, kind: a.kind, detail: a.detail, posMatched: !!a.posMatched });
            } catch (err) {
                this.log.warn('anticheat.recordAnomaly failed', { err, kind: a.kind });
            }
        }
        if (res && typeof res.then === 'function') {
            // An answer that comes later is a new event: like a sanction from elsewhere
            // (forfeitUser), its forfeit counts as arrived when a stall it waited through began.
            const later = (r) => {
                const t = this.now(), from = this.stallStart(t);
                this._maybeSanction(r, entry, color, userId, gameId, a.kind, ep, seq, t - this.stallCredit(t, from), from);
            };
            res.then(later, (err) => { this.log.warn('anticheat.recordAnomaly failed', { err }); later(null); });
        } else {
            this._maybeSanction(res, entry, color, userId, gameId, a.kind, ep, seq, recvAt, stalledSince);
        }
    }

    // `recvAt` / `stalledSince`: the arrival of the forfeit (see _process); without `recvAt`, now
    // (room.forfeit's default).
    _maybeSanction(res, entry, color, userId, gameId, kind, ep, seq, recvAt, stalledSince = Infinity) {
        const certain = res && typeof res.certain === 'boolean' ? res.certain : CERTAIN_KINDS.has(kind);
        if (!certain || !this.autoSanction) return;
        if (entry && color >= 0 && !entry.removed && !entry.room.isOver) {
            // The forfeit is part of the handling of the request that revealed the cheat: it takes
            // effect at that request's arrival, so a deadline that fell during a stall the request
            // waited through (the victim's flag or first-move timeout) cannot end the game first.
            this._process(entry, entry.room.forfeit(color, this.now(), recvAt, stalledSince), null, -1, 0);
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
    // of it when the store is asynchronous or the journal is flushed first.
    _commit(t = this.now()) {
        const batch = [];
        for (const entry of this.pending.values()) {
            batch.push(entry);
            if (batch.length >= this.commitBatchMax) break;
        }
        if (!batch.length) return true;
        // The database must not have a game before its `ended` record is in the journal (and
        // fsynced): a crash in between would bring the committed game back as a live one. When a
        // journal write failed since the last commit, it may have lost such records: the batch's
        // games are journaled again (one snapshot each; _journalAgain). When the journal still
        // holds records not written yet (or being written), the batch, chosen now, is committed
        // once they are; when a write fails meanwhile, the batch is journaled again and the commit
        // is retried after the backoff. After JOURNAL_GATE_TRIES such failures in a row, or once
        // shut down, the journal is considered down and the batch is committed without it
        // (_commitUnjournaled); that lasts until a journal flush writes without a failure.
        const j = this.journal;
        if (j && typeof j.hasUnwritten === 'function') {
            if (this.gateFailures >= JOURNAL_GATE_TRIES) {
                if (j.hasUnwritten() || j.failedWrites !== this.journalFailures) return this._commitUnjournaled(batch, t);
                this._journalWorks();              // everything appended so far is written, without a failure
            }
            this._journalAgain(t, batch, false);
            if (j.hasUnwritten()) {
                const failures = j.failedWrites;
                let flushed;
                try { flushed = Promise.resolve(j.flush()); } catch (err) { flushed = Promise.reject(err); }
                this.commitInFlight = flushed.then(
                    () => (j.failedWrites === failures ? null : new Error('journal write failed')),
                    (err) => err || new Error('journal write failed'),
                ).then((err) => {
                    this.commitInFlight = null;
                    const now = this.now();
                    try {
                        if (!err) {
                            this._journalWorks();
                            return this._commitBatch(batch, now);
                        }
                        this.gateFailures++;
                        if (this.closed || this.gateFailures >= JOURNAL_GATE_TRIES) return this._commitUnjournaled(batch, now, err);
                        this._journalAgain(now, batch, true);
                    } catch (e) { err = e; }
                    this._commitFailed(err, batch.length, now);
                    return false;
                });
                return this.commitInFlight;
            }
        }
        return this._commitBatch(batch, t);
    }

    // Journals again, as one snapshot record each, the games of `batch` whose `ended` record may
    // have been lost with a failed journal write (all of them when `all`). A failure seen for the
    // first time (journal.failedWrites changed since the last call) marks every game waiting for
    // its commit: those not in this batch are journaled again before their own commit, so the
    // synchronous work of one call is bounded by the batch.
    _journalAgain(t, batch, all) {
        const f = this.journal.failedWrites;
        if (typeof f === 'number' && f !== this.journalFailures) {
            this.journalFailures = f;
            for (const entry of this.pending.values()) entry.rejournal = true;
        }
        for (const entry of batch) {
            if (!entry.rejournal && !all) continue;
            entry.rejournal = false;
            let rec;
            try { rec = entry.room.journalSnapshot(t); } catch (err) {
                this.log.error('journal snapshot failed', { err, gameId: entry.room.id });
                continue;
            }
            this._append(entry.room.id, rec);
        }
    }

    // The journal's writes keep failing (or the host is shutting down): the batch is committed
    // without waiting for them, since the database is then the only durable copy of the results
    // left. Its snapshots are still appended (with the `committed` records that follow, they
    // reach the disk if the journal comes back), and one flush at a time tells when it does.
    _commitUnjournaled(batch, t, err = null) {
        this._journalAgain(t, batch, true);
        if (!this.unjournaled) {
            this.unjournaled = true;
            this.log.error('journal writes keep failing: finished games are committed without waiting for the journal', {
                err, failedWrites: this.journal.failedWrites, games: batch.length, pending: this.pending.size,
            });
        }
        const r = this._commitBatch(batch, t, true);
        this._probeJournal();
        return r;
    }

    // One journal flush at a time while commits do not wait for the journal: the first one that
    // writes without a failure ends that episode.
    _probeJournal() {
        const j = this.journal;
        if (this.journalProbe || this.closed || typeof j.flush !== 'function') return;
        const failures = j.failedWrites;
        let p;
        try { p = Promise.resolve(j.flush()); } catch (err) { p = Promise.reject(err); }
        this.journalProbe = p.then(() => j.failedWrites === failures, () => false).then((ok) => {
            this.journalProbe = null;
            if (ok && this.gateFailures >= JOURNAL_GATE_TRIES) this._journalWorks();
        });
    }

    // A journal flush wrote without a failure: commits wait for the journal again.
    _journalWorks() {
        this.gateFailures = 0;
        if (this.unjournaled) {
            this.unjournaled = false;
            this.log.info('journal writes succeed again: finished games wait for the journal before their commit', {
                failedWrites: this.journal.failedWrites,
            });
        }
    }

    _commitBatch(batch, t, unjournaled = false) {
        const records = batch.map((e) => e.room.record());
        const t0 = performance.now();
        if (!this.store || !this.store.games || typeof this.store.games.finishBatch !== 'function') {
            this._commitDone(batch, null, t0, t, unjournaled);
            return true;
        }
        // The anomalies still buffered by the anti-cheat (up to a second old) are written first
        // when one of them is not info: the commit's analysis queue policy looks for the game's
        // suspicious anomalies. In a shard they are handed to the store writer thread ahead of the
        // commit (written in that order); info anomalies wait for the anti-cheat's own timer.
        const ac = this.anticheat;
        if (ac && typeof ac.flush === 'function' && ac.pendingSignalCount > 0) {
            try { ac.flush(); } catch (err) { this.log.warn('anticheat flush failed', { err }); }
        }
        let res;
        try { res = this.store.games.finishBatch(records); } catch (err) {
            // The store rolls back the whole batch on one bad record (invalid_record, foreign_key):
            // commit the games one by one so that only the bad one stays pending.
            if (batch.length > 1 && err && err.gameId !== undefined) return this._commitEach(batch, records, t, unjournaled);
            this._commitFailed(err, batch.length, t);
            return false;
        }
        if (res && typeof res.then === 'function') {
            this.commitInFlight = res.then(
                (r) => { this.commitInFlight = null; this._commitDone(batch, r, t0, this.now(), unjournaled); return true; },
                (err) => {
                    this.commitInFlight = null;
                    // Same fallback as the synchronous path: one bad record must not hold the others.
                    if (batch.length > 1 && err && err.gameId !== undefined) return (this.commitInFlight = this._commitEachAsync(batch, records, unjournaled));
                    this._commitFailed(err, batch.length, this.now());
                    return false;
                });
            return this.commitInFlight;
        }
        this._commitDone(batch, res, t0, t, unjournaled);
        return true;
    }

    _commitDone(batch, results, t0, t, unjournaled = false) {
        this.m.commitMs.observe(performance.now() - t0);
        this.m.batch.observe(batch.length);
        if (unjournaled) this.m.unjournaled.inc(batch.length);
        const byId = new Map();
        if (Array.isArray(results)) for (const r of results) if (r) byId.set(r.gameId, r);
        for (const entry of batch) {
            const room = entry.room;
            this.pending.delete(room.id);
            entry.queued = false;
            entry.committed = true;
            entry.rejournal = false;
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

    _commitEach(batch, records, t, unjournaled = false) {
        let failed = null;
        for (let i = 0; i < batch.length; i++) {
            const t0 = performance.now();
            let res;
            try { res = this.store.games.finishBatch([records[i]]); } catch (err) { failed = err; continue; }
            if (res && typeof res.then === 'function') { failed = new Error('asynchronous store in _commitEach'); continue; }
            this._commitDone([batch[i]], res, t0, t, unjournaled);
        }
        if (failed) { this._commitFailed(failed, this.pending.size, t); return false; }
        return true;
    }

    async _commitEachAsync(batch, records, unjournaled = false) {
        let failed = null;
        for (let i = 0; i < batch.length; i++) {
            const t0 = performance.now();
            let res;
            try { res = await this.store.games.finishBatch([records[i]]); } catch (err) { failed = err; continue; }
            this._commitDone([batch[i]], res, t0, this.now(), unjournaled);
        }
        this.commitInFlight = null;
        if (failed) { this._commitFailed(failed, this.pending.size, this.now()); return false; }
        return true;
    }

    _commitFailed(err, n, t) {
        this.m.commitErrors.inc();
        this.backoffMs = this.backoffMs ? Math.min(this.backoffMs * 2, MAX_BACKOFF_MS) : Math.max(100, this.commitMs);
        this.nextCommitAt = t + this.backoffMs;
        this.log.error('commit of finished games failed; retrying', { err, games: n, retryInMs: this.backoffMs });
    }
}
