// GameRoom: one authoritative online game (DESIGN.md sections 5.3, 6.1 to 6.5).
//
// The room is deterministic: time is always passed in (`now`, epoch ms from clock.js now(); it
// is floored to an integer at every entry point), it owns no timer and does no I/O. Every entry
// point first processes the deadlines that are due at `now` (flag, first-move timeout, grace
// expiry, rematch window), so simultaneous events resolve by time: a resignation or a move that
// arrives after the flag deadline finds the game already lost on time.
//
// Each call returns an Outcome:
//   { broadcast: [Buffer], toWhite: [Buffer], toBlack: [Buffer], reply: [Buffer] (to the sender),
//     anomaly: null | { color, kind, detail, posMatched }, ended: bool, journal: [{kind, at, payload}],
//     // additions to the DESIGN 5.3 contract:
//     conduct: [{ userId, kind: 'abandon'|'abort'|'noshow' }],  // forwarded as conduct.record
//     rematch: null | { gameId, white, black, category, baseMs, incMs, rated },  // both accepted: game.rematch
//     rejected: 0 | ErrorCode,   // the request was refused with this code (metrics)
//     moved: bool,               // a move was accepted
//     duplicate: bool }          // an already played move was resent (idempotent reply)
// Buffers are encoded once with the codec (the MoveMade of a move is one Buffer for both players).
// The host sends `broadcast` to both players, then `toWhite` / `toBlack`, then `reply`.
// `gseq` increments on every broadcast event (MoveMade, GameEvent, GameEnd).
//
// Deviations from the DESIGN 5.3 signatures (compatible additions):
//   * onResign / onDrawOffer / onDrawAnswer / onDrawClaim / onAbort / onRematch take an optional
//     trailing `seq` (the client message's seq) so that a refusal can be an Error{ref: seq}.
//   * onResync(color, now) returns the snapshot as a reply (after processing due deadlines).
//   * GameRoom.fromJournal(records, { config, createChessGame, strict }) needs the injected rules
//     and configuration; room.recover(now) applies the restart semantics of DESIGN 6.4 separately
//     so that a plain replay rebuilds an identical room.
//   * The 'game_over' anomaly kind (info) is reported for a Move on a finished game (DESIGN 6.2
//     step 2 says "info" without naming it).
//   * A game reaching 1200 plies (the protocol's Move.ply limit) ends ServerAborted.
//   * A draw offer made while the opponent's offer is pending is an agreement.
//   * Presence and rematch changes after the end are not journaled (the journal may forget a
//     game once it is committed, so nothing is appended after the `ended` record, except a
//     compaction snapshot while the commit is pending).
//
// Journal payloads (little-endian; records are { kind, at, payload } as in DESIGN 5.6):
//   1 created  JSON { v, id, category, baseMs, incMs, rated, white, black, createdAt, rematchOf }
//   2 move     32 bytes: u16 ply | u16 move | u8 flags | u8 bits (1 = draw offer made with the move,
//              2 = the move declined the opponent's offer) | u16 0 | u32 spentMs | u32 clockAfter |
//              u32 quotaAfter | u32 gseq (of its MoveMade) | f64 recvTime
//   3 event    12 bytes: u8 kind | u8 color | u16 0 | u32 gseqAfter | u32 arg
//              kinds (JournalEvent): 1 draw offer, 2 draw declined (color = decliner),
//              3 disconnect (arg = grace), 4 reconnect, 5 desync, 6 recovered (server restart),
//              7 checkpoint (60 bytes, written only by journalState(), see _checkpointRecord)
//   4 ended    24 bytes: u8 status | u8 reason | u8 culprit colour | u8 0 | u32 whiteMs |
//              u32 blackMs | u32 gseq (of its GameEnd) | f64 endedAt
//   6 snapshot the records of journalState() in one record (journal compaction, journalSnapshot()):
//              u8 format (1) | u8 0 | u16 count | count x (u8 kind | u32 length | f64 at | payload);
//              a replay starts from the latest snapshot and ignores the records before it
// The round-trip averages are not journaled (they restart from the default after a restart).

import { encode, enums } from '../protocol/index.js';
import { GameClock, clockPolicy, graceFor } from './clock.js';

const { GameStatus: GS, EndReason: ER, GameEventKind: EV, ErrorCode: EC } = enums;
const WHITE = 0, BLACK = 1, NONE = 2;

/** Journal record kinds (DESIGN 5.6). */
export const JournalKind = Object.freeze({ Created: 1, Move: 2, Event: 3, Ended: 4, Committed: 5, Snapshot: 6 });
/** Kinds of the `event` journal records. */
export const JournalEvent = Object.freeze({ DrawOffer: 1, DrawDecline: 2, Disconnect: 3, Reconnect: 4, Desync: 5, Recovered: 6, Checkpoint: 7 });
/** Bits of `record().flags` (finished game record, DESIGN 5.5). */
export const RecordFlag = Object.freeze({ RatedRequested: 1, Recovered: 2, Forfeit: 4 });
/** Anomaly kinds that are certain cheats when the position was synchronised (DESIGN 6.5). */
export const CERTAIN_KINDS = new Set(['foreign_game', 'out_of_turn', 'illegal_move']);

/** A rematch can be agreed during this long after the end. */
export const REMATCH_WINDOW_MS = 60000;
/** A declined draw offer cannot be repeated before this many plies. */
export const DRAW_REOFFER_PLIES = 10;
/** Both players leaving within this interval is a network or server event (DESIGN 6.4). */
export const BOTH_DISCONNECT_WINDOW_MS = 5000;
/** Longest game: Move.ply is at most 1199 and GameSnapshot holds at most 1200 moves. */
export const MAX_PLIES = 1200;
/** gseq jump at a server restart (events of the last journal flush may have been lost). */
export const RECOVERY_GSEQ_JUMP = 256;

const MB_OFFER = 1, MB_DECLINED = 2;
const RM_NONE = 0, RM_OPEN = 1, RM_AGREED = 2, RM_CLOSED = 3;
const MOVE_REC_BYTES = 32, EVENT_REC_BYTES = 12, ENDED_REC_BYTES = 24, CHECKPOINT_BYTES = 60;

/** Thrown when a journal cannot be replayed. */
export class JournalError extends Error {
    constructor(message) { super(`journal: ${message}`); this.name = 'JournalError'; }
}

const SNAPSHOT_FORMAT = 1, SNAPSHOT_HEADER = 4, SNAPSHOT_REC_HEADER = 13;

// Header of one record inside the payload of a `snapshot` record (see the header of this file);
// returns the offset of that record's payload.
function putSnapshotHeader(b, o, kind, at, length) {
    b[o] = kind;
    b.writeUInt32LE(length, o + 1);
    b.writeDoubleLE(at, o + 5);
    return o + SNAPSHOT_REC_HEADER;
}

// One whole record ({ kind, at, payload }) inside a snapshot; returns the next offset.
function putSnapshotRecord(b, o, rec) {
    o = putSnapshotHeader(b, o, rec.kind, rec.at, rec.payload.length);
    b.set(rec.payload, o);
    return o + rec.payload.length;
}

function decodeSnapshot(payload) {
    const b = asBuffer(payload);
    if (b.length < SNAPSHOT_HEADER || b[0] !== SNAPSHOT_FORMAT) throw new JournalError('bad snapshot record');
    const n = b.readUInt16LE(2);
    const records = new Array(n);
    let o = SNAPSHOT_HEADER;
    for (let i = 0; i < n; i++) {
        if (o + SNAPSHOT_REC_HEADER > b.length) throw new JournalError('short snapshot record');
        const len = b.readUInt32LE(o + 1);
        const end = o + SNAPSHOT_REC_HEADER + len;
        if (end > b.length) throw new JournalError('short snapshot record');
        records[i] = { kind: b[o], at: b.readDoubleLE(o + 5), payload: b.subarray(o + SNAPSHOT_REC_HEADER, end) };
        o = end;
    }
    if (o !== b.length) throw new JournalError('bad snapshot record length');
    return records;
}

/** Result of one GameRoom call (see the header of this file). */
export class Outcome {
    constructor() {
        this.broadcast = [];
        this.toWhite = [];
        this.toBlack = [];
        this.reply = [];
        this.anomaly = null;
        this.ended = false;
        this.journal = [];
        this.conduct = [];
        this.rematch = null;
        this.rejected = 0;
        this.moved = false;
        this.duplicate = false;
    }
}

/** GameStatus of a win by `color`. */
export function winFor(color) { return color === WHITE ? GS.WhiteWins : GS.BlackWins; }

function clampInt(v, lo, hi) {
    const n = Math.floor(+v || 0);
    return n < lo ? lo : n > hi ? hi : n;
}

function truncUtf8(s, maxBytes) {
    let t = String(s ?? '').replace(/\0/g, '');
    while (Buffer.byteLength(t, 'utf8') > maxBytes) t = t.slice(0, -1);
    return t;
}

function normPlayer(p) {
    if (!p || typeof p !== 'object') throw new TypeError('GameRoom: white and black players are required');
    return Object.freeze({
        userId: p.userId >>> 0,
        name: truncUtf8(p.name, 24) || '?',
        rating: clampInt(p.rating, 0, 0xffff),
        provisional: !!p.provisional,
    });
}

function asBuffer(p) {
    if (Buffer.isBuffer(p)) return p;
    if (ArrayBuffer.isView(p)) return Buffer.from(p.buffer, p.byteOffset, p.byteLength);
    throw new JournalError('payload is not a buffer');
}

/**
 * One online game: canonical position, clocks, offers, presence and result.
 */
export class GameRoom {
    /**
     * @param {object} opts
     * @param {number} opts.id game id (id53)
     * @param {string} opts.category '3+2' ... or 'custom'
     * @param {number} opts.baseMs
     * @param {number} opts.incMs
     * @param {boolean} opts.rated
     * @param {{userId:number,name:string,rating:number,provisional:boolean}} opts.white
     * @param {{userId:number,name:string,rating:number,provisional:boolean}} opts.black
     * @param {number} opts.createdAt start of the game (epoch ms); White's first-move timer starts here
     * @param {object} opts.config frozen configuration
     * @param {number} [opts.rematchOf] id of the game this one is a rematch of
     * @param {() => object} opts.createChessGame rules factory (() => new ChessGame())
     */
    constructor({ id, category, baseMs, incMs, rated, white, black, createdAt, config = {}, rematchOf = 0, createChessGame }) {
        if (typeof createChessGame !== 'function') throw new TypeError('GameRoom: createChessGame is required');
        if (!Number.isSafeInteger(id) || id <= 0) throw new TypeError('GameRoom: invalid game id');
        this.id = id;
        this.category = truncUtf8(category || 'custom', 7);
        this.baseMs = clampInt(baseMs, 0, 0xffffffff);
        this.incMs = clampInt(incMs, 0, 0xffffffff);
        this.rated = !!rated;
        this.white = normPlayer(white);
        this.black = normPlayer(black);
        this.createdAt = Math.floor(+createdAt || 0);
        this.config = config;
        this.rematchOf = Number.isSafeInteger(rematchOf) && rematchOf > 0 ? rematchOf : 0;
        this.createChessGame = createChessGame;
        this.policy = clockPolicy(config);
        this.graceMs = graceFor(this.baseMs, config);
        this.drawOfferLimit = Number.isFinite(config.drawOffersPerGame) ? config.drawOffersPerGame : 3;

        this.game = createChessGame();
        this.clock = new GameClock({ baseMs: this.baseMs, incMs: this.incMs, policy: this.policy, startAt: this.createdAt });
        this.ply = 0;
        this._cap = 0;
        this._grow(64);
        this.gseq = 0;
        this.drawOffer = NONE;
        this.drawOffersUsed = [0, 0];
        this.drawDeclinedAt = [-DRAW_REOFFER_PLIES, -DRAW_REOFFER_PLIES];
        this.desyncs = [0, 0];
        this.connected = [true, true];
        this.disconnectedAt = [0, 0];
        this._over = false;
        this.result = null;          // { status, reason, whiteMs, blackMs, endedAt }
        this.endGseq = 0;
        this.culprit = NONE;         // colour whose act or absence ended the game (NONE: nobody)
        this.rematchBy = NONE;
        this.rematchState = RM_NONE;
        this.flags = this.rated ? RecordFlag.RatedRequested : 0;
        this.replayError = null;     // set by a lenient fromJournal() that stopped early
    }

    // ---- state accessors -----------------------------------------------------------------------

    /** True once the result is decided. */
    get isOver() { return this._over; }
    /** Colour to move (by ply parity: games start from the initial position). */
    get sideToMove() { return this.ply & 1; }
    /** True while a rematch can still be offered or accepted. */
    get rematchOpen() { return this.rematchState === RM_OPEN; }
    /** Moves played so far (copy). */
    get moveList() { return this.moves.slice(0, this.ply); }

    /** Colour of a user in this game: 0 White, 1 Black, -1 not a player. */
    colorOf(userId) {
        if (userId === this.white.userId) return WHITE;
        if (userId === this.black.userId) return BLACK;
        return -1;
    }

    /** PlayerInfo of a colour. */
    playerOf(color) { return color === WHITE ? this.white : this.black; }

    /** Whether a player is connected (as far as the room knows). */
    isConnected(color) { return this.connected[color]; }

    /** Earliest moment at which tick() has something to do (Infinity: nothing). */
    nextDeadline() {
        if (!this._over) return Math.min(this.clock.deadline(this.ply), this._graceDeadline());
        if (this.rematchState === RM_OPEN) return this.result.endedAt + REMATCH_WINDOW_MS;
        return Infinity;
    }

    // ---- inputs --------------------------------------------------------------------------------

    /**
     * Move intent of `color` (DESIGN 6.2 validation order; the host has checked participation).
     * @param {number} color
     * @param {{seq:number, ply:number, move:number, posHash:number, thinkMs:number, drawOffer:boolean}} msg
     * @param {number} now
     * @returns {Outcome}
     */
    onMove(color, msg, now) {
        checkColor(color);
        now = Math.floor(now);
        const out = new Outcome();
        const ply = msg.ply | 0, move = msg.move & 0xffff;
        const wasOver = this._over;
        this._advance(now, out);
        // 2. Game over (a flag that fell at this very moment for the sender is FlagFell).
        if (this._over) {
            const r = this.result.reason;
            const fell = !wasOver && this.culprit === color && ply === this.ply && (r === ER.Timeout || r === ER.TimeoutVsInsufficient);
            return fell
                ? this._rejectMove(out, color, ply, move, EC.FlagFell, now, null, true)
                : this._rejectMove(out, color, ply, move, EC.GameOver, now, 'game_over', false);
        }
        // 3. Duplicate of a move already played: idempotent when identical.
        if (ply < this.ply) {
            if ((ply & 1) === color && this.moves[ply] === move) {
                out.reply.push(this._encodeMoveMade(ply));
                out.duplicate = true;
                return out;
            }
            return this._rejectMove(out, color, ply, move, EC.StalePly, now, 'stale_ply', false);
        }
        // 4 and 5. Position mismatch, or a future ply: desynchronised client.
        if (ply > this.ply || (msg.posHash >>> 0) !== (this.game.position.digest() >>> 0)) {
            this._applyEvent(JournalEvent.Desync, color, 0, now);
            out.journal.push(this._eventRecord(JournalEvent.Desync, color, 0, now));
            return this._rejectMove(out, color, ply, move, EC.Desync, now, this.desyncs[color] >= 3 ? 'repeated_desync' : 'desync', false);
        }
        // 6. Not the sender's turn although it knew the position.
        if ((this.ply & 1) !== color) return this._rejectMove(out, color, ply, move, EC.NotYourTurn, now, 'out_of_turn', true);
        // 7. Illegal move in the synchronised position.
        if (!this.game.position.isLegal(move)) return this._rejectMove(out, color, ply, move, EC.IllegalMove, now, 'illegal_move', true);
        // 8. Clock, then play.
        const idx = this.ply;
        const c = this.clock.check(color, idx, now, msg.thinkMs);
        if (c.implausible) {
            out.anomaly = { color, kind: 'clock_implausible', detail: `ply ${idx} thinkMs ${msg.thinkMs >>> 0} elapsed ${c.elapsed}`, posMatched: true };
        }
        if (c.flagged) {
            this._flag(color, now, out);
            return this._rejectMove(out, color, ply, move, EC.FlagFell, now, null, true);
        }
        const spent = c.charged, clockAfter = c.clockAfter, quotaAfter = c.quotaAfter;
        const r = this.game.play(move);
        if (!r || !r.ok) {
            // The rules accepted isLegal() but refused play(): a rules inconsistency, not a cheat.
            return this._rejectMove(out, color, ply, move, EC.IllegalMove, now, null, true);
        }
        let bits = 0, offerRefused = false;
        if (this.drawOffer === (color ^ 1)) bits |= MB_DECLINED;
        if (msg.drawOffer && this.drawOffer !== color) {
            if (this._mayOffer(color, idx + 1)) bits |= MB_OFFER;
            else offerRefused = true;
        }
        this._applyMove(color, move, (r.flags | 0) & 0xff, spent, clockAfter, quotaAfter, now, bits, this.gseq + 1);
        this.gseq++;
        out.broadcast.push(this._encodeMoveMade(idx));
        if (bits & MB_DECLINED) {
            this.gseq++;
            out.broadcast.push(this._gameEvent(EV.DrawDeclined, color, 0));
        }
        out.journal.push(this._moveRecord(idx));
        out.moved = true;
        if (offerRefused) out.reply.push(this._error(EC.DrawOfferLimit, msg.seq));
        const st = r.status | 0;
        if (st !== GS.Ongoing) this._end(st, r.reason | 0, now, out, NONE, NONE);
        else if (this.ply >= MAX_PLIES) this._end(GS.Aborted, ER.ServerAborted, now, out, NONE, NONE);
        return out;
    }

    /** Resignation (any time while the game runs). */
    onResign(color, now, seq = 0) {
        checkColor(color);
        now = Math.floor(now);
        const out = new Outcome();
        this._advance(now, out);
        if (this._over) return this._refuse(out, EC.GameOver, seq);
        this._end(winFor(color ^ 1), ER.Resignation, now, out, color, NONE);
        return out;
    }

    /** Draw offer without a move (DESIGN 6.3). */
    onDrawOffer(color, now, seq = 0) {
        checkColor(color);
        now = Math.floor(now);
        const out = new Outcome();
        this._advance(now, out);
        if (this._over) return this._refuse(out, EC.GameOver, seq);
        if (this.drawOffer === (color ^ 1)) {           // both want a draw
            this._end(GS.Draw, ER.Agreement, now, out, NONE, NONE);
            return out;
        }
        if (this.drawOffer === color) return out;       // already standing
        if (!this._mayOffer(color, this.ply)) return this._refuse(out, EC.DrawOfferLimit, seq);
        this._applyEvent(JournalEvent.DrawOffer, color, 0, now);
        this.gseq++;
        out.broadcast.push(this._gameEvent(EV.DrawOffered, color, 0));
        out.journal.push(this._eventRecord(JournalEvent.DrawOffer, color, 0, now));
        return out;
    }

    /** Answer to the opponent's pending draw offer. */
    onDrawAnswer(color, accept, now, seq = 0) {
        checkColor(color);
        now = Math.floor(now);
        const out = new Outcome();
        this._advance(now, out);
        if (this._over) return this._refuse(out, EC.GameOver, seq);
        if (this.drawOffer !== (color ^ 1)) return this._refuse(out, EC.NoPendingOffer, seq);
        if (accept) {
            this._end(GS.Draw, ER.Agreement, now, out, NONE, NONE);
            return out;
        }
        this._applyEvent(JournalEvent.DrawDecline, color, 0, now);
        this.gseq++;
        out.broadcast.push(this._gameEvent(EV.DrawDeclined, color, 0));
        out.journal.push(this._eventRecord(JournalEvent.DrawDecline, color, 0, now));
        return out;
    }

    /** Draw claim: threefold repetition or fifty-move rule in the current position. */
    onDrawClaim(color, now, seq = 0) {
        checkColor(color);
        now = Math.floor(now);
        const out = new Outcome();
        this._advance(now, out);
        if (this._over) return this._refuse(out, EC.GameOver, seq);
        const g = this.game;
        let reason = ER.None;
        if (g.canClaimThreefold()) reason = ER.ThreefoldClaim;
        else if (g.canClaimFiftyMove()) reason = ER.FiftyMoveClaim;
        if (reason === ER.None) {
            this._refuse(out, EC.NothingToClaim, seq);
            out.anomaly = { color, kind: 'nothing_to_claim', detail: `ply ${this.ply}`, posMatched: true };
            return out;
        }
        this._end(GS.Draw, reason, now, out, NONE, NONE);
        return out;
    }

    /** Abort: only before the sender's own first move (conduct counter 'abort'). */
    onAbort(color, now, seq = 0) {
        checkColor(color);
        now = Math.floor(now);
        const out = new Outcome();
        this._advance(now, out);
        if (this._over) return this._refuse(out, EC.GameOver, seq);
        if (this.ply > color) return this._refuse(out, EC.AbortNotAllowed, seq);
        this._end(GS.Aborted, ER.Aborted, now, out, color, NONE);
        out.conduct.push({ userId: this.playerOf(color).userId, kind: 'abort' });
        return out;
    }

    /**
     * Rematch after the end: accept=true offers or accepts, accept=false declines or withdraws.
     * When both accepted, Outcome.rematch holds the new game's players (colours swapped).
     */
    onRematch(color, accept, now, seq = 0) {
        checkColor(color);
        now = Math.floor(now);
        const out = new Outcome();
        this._advance(now, out);
        if (!this._over || this.rematchState !== RM_OPEN) return this._refuse(out, EC.RematchUnavailable, seq);
        if (!accept) {
            this._closeRematch(out, color);
            return out;
        }
        if (this.rematchBy === (color ^ 1)) {
            this.rematchState = RM_AGREED;
            this.rematchBy = NONE;
            out.rematch = {
                gameId: this.id, white: this.black, black: this.white, category: this.category,
                baseMs: this.baseMs, incMs: this.incMs, rated: this.rated,
            };
            return out;
        }
        if (this.rematchBy === color) return out;
        this.rematchBy = color;
        this.gseq++;
        out.broadcast.push(this._gameEvent(EV.RematchOffered, color, 0));
        return out;
    }

    /** The player's connection is gone (DESIGN 6.4). After the end it only closes the rematch window. */
    onDisconnect(color, now) {
        checkColor(color);
        now = Math.floor(now);
        const out = new Outcome();
        this._advance(now, out);
        if (this._over) {
            this.connected[color] = false;
            this._closeRematch(out, color);
            return out;
        }
        if (!this.connected[color]) return out;
        this._applyEvent(JournalEvent.Disconnect, color, this.graceMs, now);
        this.gseq++;
        out.broadcast.push(this._gameEvent(EV.PlayerDisconnected, color, this.graceMs));
        out.journal.push(this._eventRecord(JournalEvent.Disconnect, color, this.graceMs, now));
        return out;
    }

    /** The player is back (the host sends the snapshot). */
    onReconnect(color, now) {
        checkColor(color);
        now = Math.floor(now);
        const out = new Outcome();
        this._advance(now, out);
        if (this._over) {
            this.connected[color] = true;
            return out;
        }
        if (this.connected[color]) return out;
        this._applyEvent(JournalEvent.Reconnect, color, 0, now);
        this.gseq++;
        out.broadcast.push(this._gameEvent(EV.PlayerReconnected, color, 0));
        out.journal.push(this._eventRecord(JournalEvent.Reconnect, color, 0, now));
        return out;
    }

    /** A server round-trip measurement of the player (exponential average, capped at 2 s). */
    onRtt(color, rttMs) {
        checkColor(color);
        this.clock.setRtt(color, rttMs);
    }

    /** Full snapshot for the sender (after processing due deadlines). */
    onResync(color, now) {
        now = Math.floor(now);
        const out = new Outcome();
        this._advance(now, out);
        out.reply.push(this.snapshotBuffer(color, now));
        return out;
    }

    /** Anti-cheat: `color` loses (EndReason.Forfeit; a rated game is rated normally). */
    forfeit(color, now) {
        checkColor(color);
        now = Math.floor(now);
        const out = new Outcome();
        this._advance(now, out);
        if (this._over) return out;
        this._end(winFor(color ^ 1), ER.Forfeit, now, out, color, NONE);
        return out;
    }

    /** The server cannot continue this game: Aborted / ServerAborted (unrated). */
    serverAbort(now) {
        now = Math.floor(now);
        const out = new Outcome();
        if (this._over) return out;
        this._end(GS.Aborted, ER.ServerAborted, now, out, NONE, NONE);
        return out;
    }

    /** Processes every deadline due at `now` (flags, first-move timeouts, grace, rematch window). */
    tick(now) {
        now = Math.floor(now);
        const out = new Outcome();
        this._advance(now, out);
        return out;
    }

    /**
     * Server restart semantics (DESIGN 6.4) on a room rebuilt by fromJournal(): both players are
     * marked disconnected with a fresh grace, and the running clock (or first-move timer)
     * restarts at `now` from its journaled value. A finished game only loses its rematch window.
     * The Outcome carries the journal record of the recovery (to append).
     */
    recover(now) {
        now = Math.floor(now);
        const out = new Outcome();
        if (!this._over && this.game.status !== undefined && this.game.status !== GS.Ongoing) {
            // The journal kept the move that ended the game but not its `ended` record.
            const at = this.ply ? this.recvTime[this.ply - 1] : this.createdAt;
            this._end(this.game.status, this.game.reason | 0, at, out, NONE, NONE);
        }
        if (this._over) {
            this.rematchState = RM_CLOSED;
            this.rematchBy = NONE;
            return out;
        }
        this._applyEvent(JournalEvent.Recovered, NONE, 0, now);
        this.gseq += RECOVERY_GSEQ_JUMP;
        out.journal.push(this._eventRecord(JournalEvent.Recovered, NONE, 0, now));
        return out;
    }

    // ---- outputs -------------------------------------------------------------------------------

    /**
     * Authoritative state for encode.GameSnapshot, clocks at `now`.
     * @param {number} forColor 0 White, 1 Black, 2 spectator
     * @param {number} now
     */
    snapshot(forColor, now) {
        now = Math.floor(now);
        const n = this.ply, moves = new Array(n);
        for (let i = 0; i < n; i++) moves[i] = { move: this.moves[i], spentMs: this.spent[i], clockMs: this.clockAfter[i] };
        const over = this._over, r = this.result;
        return {
            game: this.id,
            gseq: this.gseq >>> 0,
            category: this.category,
            baseMs: this.baseMs,
            incMs: this.incMs,
            rated: this.rated,
            white: this.white,
            black: this.black,
            you: forColor === WHITE || forColor === BLACK ? forColor : NONE,
            moves,
            running: !over && n >= 2 ? (n & 1) : NONE,
            whiteMs: over ? r.whiteMs : this.clock.remainingAt(WHITE, n, now),
            blackMs: over ? r.blackMs : this.clock.remainingAt(BLACK, n, now),
            serverTime: now,
            drawOffer: this.drawOffer,
            status: over ? r.status : GS.Ongoing,
            reason: over ? r.reason : ER.None,
            whiteConnected: this.connected[WHITE],
            blackConnected: this.connected[BLACK],
            graceMs: over ? 0 : this._graceLeft(forColor, now),
            firstMoveMs: over ? 0 : this.clock.firstMoveLeft(n, now),
            startedAt: this.createdAt,
            rematch: this.rematchState === RM_OPEN ? this.rematchBy : NONE,
        };
    }

    /** encode.GameSnapshot(snapshot(forColor, now)). */
    snapshotBuffer(forColor, now) { return encode.GameSnapshot(this.snapshot(forColor, now)); }

    /** Finished game record for store.games.finishBatch (DESIGN 5.5). */
    record() {
        if (!this._over) throw new Error('GameRoom.record: the game is not over');
        const r = this.result, n = this.ply;
        return {
            id: this.id,
            category: this.category,
            rated: this.rated && r.status !== GS.Aborted,
            baseMs: this.baseMs,
            incMs: this.incMs,
            whiteId: this.white.userId,
            blackId: this.black.userId,
            whiteName: this.white.name,
            blackName: this.black.name,
            whiteRating: this.white.rating,
            blackRating: this.black.rating,
            startedAt: this.createdAt,
            endedAt: r.endedAt,
            status: r.status,
            reason: r.reason,
            moves: this.moves.slice(0, n),
            spentMs: this.spent.slice(0, n),
            clockMs: this.clockAfter.slice(0, n),
            rematchOf: this.rematchOf,
            flags: this.flags,
        };
    }

    // ---- journal -------------------------------------------------------------------------------

    /** The `created` record (the host appends it when the game is created). */
    createdRecord() {
        const spec = {
            v: 1, id: this.id, category: this.category, baseMs: this.baseMs, incMs: this.incMs, rated: this.rated,
            white: this.white, black: this.black, createdAt: this.createdAt, rematchOf: this.rematchOf,
        };
        return { kind: JournalKind.Created, at: this.createdAt, payload: Buffer.from(JSON.stringify(spec), 'utf8') };
    }

    /**
     * The room as a compact list of journal records (created, moves, one checkpoint, ended):
     * GameRoom.fromJournal(room.journalState(), opts) rebuilds an identical room.
     */
    journalState() {
        const recs = [this.createdRecord()];
        for (let i = 0; i < this.ply; i++) recs.push(this._moveRecord(i));
        recs.push(this._checkpointRecord());
        if (this._over) recs.push(this._endedRecord());
        return recs;
    }

    /**
     * The room as ONE `snapshot` journal record (journal compaction): journalState() in a single
     * payload, which a replay uses in place of every earlier record of the game. Taken between two
     * outcomes, it includes every record the host appended for this game so far.
     * @param {number} now
     * @returns {{kind:number, at:number, payload:Buffer}}
     */
    journalSnapshot(now) {
        // journalState() written straight into one buffer, without a Buffer per move.
        const created = this.createdRecord(), check = this._checkpointRecord(), ended = this._over ? this._endedRecord() : null;
        const n = this.ply;
        const count = n + (ended ? 3 : 2);
        const size = SNAPSHOT_HEADER + count * SNAPSHOT_REC_HEADER + created.payload.length + n * MOVE_REC_BYTES
            + check.payload.length + (ended ? ended.payload.length : 0);
        const b = Buffer.alloc(size);            // zero-filled: the unused bytes of the move records
        b[0] = SNAPSHOT_FORMAT;
        b.writeUInt16LE(count, 2);
        let o = putSnapshotRecord(b, SNAPSHOT_HEADER, created);
        for (let i = 0; i < n; i++) {
            o = putSnapshotHeader(b, o, JournalKind.Move, this.recvTime[i], MOVE_REC_BYTES);
            this._writeMove(b, o, i);
            o += MOVE_REC_BYTES;
        }
        o = putSnapshotRecord(b, o, check);
        if (ended) putSnapshotRecord(b, o, ended);
        return { kind: JournalKind.Snapshot, at: Math.floor(now), payload: b };
    }

    /**
     * Rebuilds a room from its journal records ([{ kind, at, payload }], in order). A replay starts
     * from the latest `snapshot` record when there is one (the records before it are ignored).
     * @param {Array<{kind:number, at:number, payload:Buffer}>} records
     * @param {{config?:object, createChessGame:Function, strict?:boolean}} opts strict=false stops at
     *   the first bad record (room.replayError is set) instead of throwing
     * @returns {GameRoom}
     */
    static fromJournal(records, { config = {}, createChessGame, strict = true } = {}) {
        if (!records || !records.length) throw new JournalError('no record');
        let base = -1;
        for (let i = records.length - 1; i >= 0; i--) if (records[i].kind === JournalKind.Snapshot) { base = i; break; }
        if (base >= 0) records = [...decodeSnapshot(records[base].payload), ...records.slice(base + 1)];
        if (!records.length) throw new JournalError('empty snapshot');
        const first = records[0];
        if (first.kind !== JournalKind.Created) throw new JournalError('the first record is not `created`');
        let spec;
        try { spec = JSON.parse(asBuffer(first.payload).toString('utf8')); } catch (err) {
            throw new JournalError(`bad created record (${err.message})`);
        }
        const room = new GameRoom({
            id: spec.id, category: spec.category, baseMs: spec.baseMs, incMs: spec.incMs, rated: spec.rated,
            white: spec.white, black: spec.black, createdAt: spec.createdAt, rematchOf: spec.rematchOf,
            config, createChessGame,
        });
        for (let i = 1; i < records.length; i++) {
            const rec = records[i];
            try {
                switch (rec.kind) {
                    case JournalKind.Move: room._replayMove(asBuffer(rec.payload)); break;
                    case JournalKind.Event: room._replayEvent(asBuffer(rec.payload), Math.floor(rec.at)); break;
                    case JournalKind.Ended: room._replayEnded(asBuffer(rec.payload)); break;
                    case JournalKind.Created: throw new JournalError('second created record');
                    case JournalKind.Snapshot: throw new JournalError('snapshot inside a snapshot');
                    default: break;   // committed and unknown kinds carry no room state
                }
            } catch (err) {
                const e = err instanceof JournalError ? err : new JournalError(err.message);
                if (strict) throw e;
                room.replayError = e;
                break;
            }
        }
        return room;
    }

    // ---- internals -----------------------------------------------------------------------------

    _grow(min) {
        const cap = Math.min(MAX_PLIES, Math.max(min, this._cap * 2));
        const copy = (Ctor, old) => { const a = new Ctor(cap); if (old) a.set(old); return a; };
        this.moves = copy(Uint16Array, this.moves);
        this.mflags = copy(Uint8Array, this.mflags);
        this.mbits = copy(Uint8Array, this.mbits);
        this.spent = copy(Uint32Array, this.spent);
        this.clockAfter = copy(Uint32Array, this.clockAfter);
        this.quotaAfter = copy(Uint32Array, this.quotaAfter);
        this.gseqMove = copy(Uint32Array, this.gseqMove);
        this.recvTime = copy(Float64Array, this.recvTime);
        this._cap = cap;
    }

    _applyMove(color, move, flags, spent, clockAfter, quotaAfter, at, bits, gseqMove) {
        const i = this.ply;
        if (i >= this._cap) this._grow(i + 1);
        this.moves[i] = move;
        this.mflags[i] = flags;
        this.mbits[i] = bits;
        this.spent[i] = spent;
        this.clockAfter[i] = clockAfter;
        this.quotaAfter[i] = quotaAfter;
        this.gseqMove[i] = gseqMove;
        this.recvTime[i] = at;
        this.clock.apply(color, clockAfter, quotaAfter, at);
        this.ply = i + 1;
        if (bits & MB_DECLINED) {
            this.drawOffer = NONE;
            this.drawDeclinedAt[color ^ 1] = this.ply;
        }
        if (bits & MB_OFFER) {
            this.drawOffer = color;
            this.drawOffersUsed[color]++;
        }
    }

    _applyEvent(kind, color, arg, at) {
        switch (kind) {
            case JournalEvent.DrawOffer:
                this.drawOffer = color;
                this.drawOffersUsed[color]++;
                break;
            case JournalEvent.DrawDecline:
                this.drawOffer = NONE;
                this.drawDeclinedAt[color ^ 1] = this.ply;
                break;
            case JournalEvent.Disconnect:
                this.connected[color] = false;
                this.disconnectedAt[color] = at;
                break;
            case JournalEvent.Reconnect:
                this.connected[color] = true;
                break;
            case JournalEvent.Desync:
                this.desyncs[color]++;
                break;
            case JournalEvent.Recovered:
                this.connected[WHITE] = this.connected[BLACK] = false;
                this.disconnectedAt[WHITE] = this.disconnectedAt[BLACK] = at;
                this.clock.restart(at);
                this.flags |= RecordFlag.Recovered;
                break;
            default:
                throw new JournalError(`unknown event kind ${kind}`);
        }
    }

    _applyEnd(status, reason, whiteMs, blackMs, at, culprit) {
        this._over = true;
        this.result = { status, reason, whiteMs, blackMs, endedAt: at };
        this.culprit = culprit;
        this.drawOffer = NONE;
        if (reason === ER.Forfeit) this.flags |= RecordFlag.Forfeit;
        this.rematchState = reason === ER.ServerAborted ? RM_CLOSED : RM_OPEN;
        this.rematchBy = NONE;
        const g = this.game;
        if ((g.status === undefined || g.status === GS.Ongoing) && typeof g.end === 'function') {
            try { g.end(status, reason); } catch { /* the rules object is only informative once over */ }
        }
    }

    // Ends the game now: result, GameEnd broadcast, `ended` journal record.
    _end(status, reason, now, out, culprit, flagged) {
        const wMs = flagged === WHITE ? 0 : this.clock.remainingAt(WHITE, this.ply, now);
        const bMs = flagged === BLACK ? 0 : this.clock.remainingAt(BLACK, this.ply, now);
        this._applyEnd(status, reason, wMs, bMs, now, culprit);
        this.gseq++;
        this.endGseq = this.gseq;
        out.broadcast.push(encode.GameEnd({ game: this.id, gseq: this.gseq, status, reason, whiteMs: wMs, blackMs: bMs, serverTime: now }));
        out.journal.push(this._endedRecord());
        out.ended = true;
    }

    _flag(side, now, out) {
        const opp = side ^ 1;
        const canMate = this._canMate(opp);
        this._end(canMate ? winFor(opp) : GS.Draw, canMate ? ER.Timeout : ER.TimeoutVsInsufficient, now, out, side, side);
    }

    _canMate(color) {
        const p = this.game.position;
        return p && typeof p.canColorMate === 'function' ? !!p.canColorMate(color) : true;
    }

    _mayOffer(color, plyNow) {
        return this.drawOffersUsed[color] < this.drawOfferLimit && plyNow >= this.drawDeclinedAt[color] + DRAW_REOFFER_PLIES;
    }

    // Grace expiry moment (Infinity when both are connected). Both gone within 5 s of each
    // other: the longer grace; otherwise the first to leave is the one who abandons.
    _graceDeadline() {
        const w = !this.connected[WHITE], b = !this.connected[BLACK];
        if (!w && !b) return Infinity;
        const g = this.graceMs;
        if (w && b) {
            const dw = this.disconnectedAt[WHITE], db = this.disconnectedAt[BLACK];
            if (Math.abs(dw - db) <= BOTH_DISCONNECT_WINDOW_MS) return Math.max(dw, db) + g;
            return Math.min(dw, db) + g;
        }
        return (w ? this.disconnectedAt[WHITE] : this.disconnectedAt[BLACK]) + g;
    }

    _graceDeadlineOf(color) {
        if (this.connected[color]) return Infinity;
        const other = color ^ 1;
        if (!this.connected[other] && Math.abs(this.disconnectedAt[color] - this.disconnectedAt[other]) <= BOTH_DISCONNECT_WINDOW_MS) {
            return Math.max(this.disconnectedAt[color], this.disconnectedAt[other]) + this.graceMs;
        }
        return this.disconnectedAt[color] + this.graceMs;
    }

    // Grace left of the player the viewer waits for (the opponent; for a spectator the first to expire).
    _graceLeft(forColor, now) {
        let d;
        if (forColor === WHITE || forColor === BLACK) {
            d = this._graceDeadlineOf(forColor ^ 1);
            if (d === Infinity) d = this._graceDeadlineOf(forColor);
        } else {
            d = Math.min(this._graceDeadlineOf(WHITE), this._graceDeadlineOf(BLACK));
        }
        return d === Infinity ? 0 : clampInt(d - now, 0, 0xffffffff);
    }

    _advance(now, out) {
        if (!this._over) {
            const tm = this.clock.deadline(this.ply);
            const tg = this._graceDeadline();
            if (tm <= now && tm <= tg) this._onTimeDeadline(now, out);
            else if (tg <= now) this._onGraceExpired(now, out);
        }
        if (this._over && this.rematchState === RM_OPEN && now >= this.result.endedAt + REMATCH_WINDOW_MS) {
            this._closeRematch(out, NONE);
        }
    }

    _onTimeDeadline(now, out) {
        const side = this.ply & 1;
        if (this.ply < 2) {
            this._end(GS.Aborted, ER.NoShow, now, out, side, NONE);
            out.conduct.push({ userId: this.playerOf(side).userId, kind: 'noshow' });
        } else {
            this._flag(side, now, out);
        }
    }

    _onGraceExpired(now, out) {
        const w = !this.connected[WHITE], b = !this.connected[BLACK];
        const dw = this.disconnectedAt[WHITE], db = this.disconnectedAt[BLACK];
        if (w && b && Math.abs(dw - db) <= BOTH_DISCONNECT_WINDOW_MS) {
            this._end(GS.Aborted, ER.BothDisconnected, now, out, NONE, NONE);
            return;
        }
        const absent = w && b ? (dw <= db ? WHITE : BLACK) : (w ? WHITE : BLACK);
        if (this.ply < 2) {
            this._end(GS.Aborted, ER.NoShow, now, out, absent, NONE);
            out.conduct.push({ userId: this.playerOf(absent).userId, kind: 'noshow' });
            return;
        }
        const opp = absent ^ 1;
        const canMate = this._canMate(opp);
        this._end(canMate ? winFor(opp) : GS.Draw, canMate ? ER.Abandonment : ER.AbandonmentVsInsufficient, now, out, absent, NONE);
        out.conduct.push({ userId: this.playerOf(absent).userId, kind: 'abandon' });
    }

    _closeRematch(out, by) {
        if (this.rematchState !== RM_OPEN) return;
        this.rematchState = RM_CLOSED;
        if (this.rematchBy !== NONE || by !== NONE) {
            this.gseq++;
            out.broadcast.push(this._gameEvent(EV.RematchDeclined, by, 0));
        }
        this.rematchBy = NONE;
    }

    _rejectMove(out, color, ply, move, code, now, kind, posMatched) {
        out.rejected = code;
        out.reply.push(encode.MoveRejected({ game: this.id, ply: ply & 0xffff, move: move & 0xffff, code }));
        out.reply.push(this.snapshotBuffer(color, now));
        if (kind) out.anomaly = { color, kind, detail: `ply ${ply} move ${move} (game at ply ${this.ply})`, posMatched };
        return out;
    }

    _refuse(out, code, seq) {
        out.rejected = code;
        out.reply.push(this._error(code, seq));
        return out;
    }

    _error(code, seq) { return encode.Error({ ref: (seq >>> 0) || 0, code, fatal: false, game: this.id }); }

    _gameEvent(kind, color, arg) {
        return encode.GameEvent({ game: this.id, gseq: this.gseq >>> 0, kind, color, arg: clampInt(arg, 0, 0xffffffff) });
    }

    // The MoveMade of ply `i`, byte-identical to the one broadcast when it was played.
    _encodeMoveMade(i) {
        const mover = i & 1;
        const moverMs = this.clockAfter[i];
        const otherMs = i >= 1 ? this.clockAfter[i - 1] : this.baseMs;
        return encode.MoveMade({
            game: this.id,
            gseq: this.gseqMove[i],
            ply: i,
            move: this.moves[i],
            flags: this.mflags[i],
            spentMs: this.spent[i],
            whiteMs: mover === WHITE ? moverMs : otherMs,
            blackMs: mover === BLACK ? moverMs : otherMs,
            serverTime: this.recvTime[i],
            drawOffer: (this.mbits[i] & MB_OFFER) !== 0,
            firstMoveMs: i === 0 ? this.policy.firstMoveMs : 0,
        });
    }

    _moveRecord(i) {
        const b = Buffer.alloc(MOVE_REC_BYTES);
        this._writeMove(b, 0, i);
        return { kind: JournalKind.Move, at: this.recvTime[i], payload: b };
    }

    // The 32-byte move record of ply `i` at b[o..] (bytes 6-7 stay as they are: zero).
    _writeMove(b, o, i) {
        b.writeUInt16LE(i, o);
        b.writeUInt16LE(this.moves[i], o + 2);
        b[o + 4] = this.mflags[i];
        b[o + 5] = this.mbits[i];
        b.writeUInt32LE(this.spent[i], o + 8);
        b.writeUInt32LE(this.clockAfter[i], o + 12);
        b.writeUInt32LE(this.quotaAfter[i], o + 16);
        b.writeUInt32LE(this.gseqMove[i], o + 20);
        b.writeDoubleLE(this.recvTime[i], o + 24);
    }

    _eventRecord(kind, color, arg, at) {
        const b = Buffer.alloc(EVENT_REC_BYTES);
        b[0] = kind;
        b[1] = color;
        b.writeUInt32LE(this.gseq >>> 0, 4);
        b.writeUInt32LE(clampInt(arg, 0, 0xffffffff), 8);
        return { kind: JournalKind.Event, at, payload: b };
    }

    _endedRecord() {
        const r = this.result;
        const b = Buffer.alloc(ENDED_REC_BYTES);
        b[0] = r.status;
        b[1] = r.reason;
        b[2] = this.culprit;
        b.writeUInt32LE(r.whiteMs >>> 0, 4);
        b.writeUInt32LE(r.blackMs >>> 0, 8);
        b.writeUInt32LE(this.endGseq >>> 0, 12);
        b.writeDoubleLE(r.endedAt, 16);
        return { kind: JournalKind.Ended, at: r.endedAt, payload: b };
    }

    // Every counter that the move records do not carry (journalState() only).
    _checkpointRecord() {
        const b = Buffer.alloc(CHECKPOINT_BYTES);
        b[0] = JournalEvent.Checkpoint;
        b[1] = this.drawOffer;
        b[2] = (this.connected[WHITE] ? 1 : 0) | (this.connected[BLACK] ? 2 : 0);
        b.writeUInt32LE(this.gseq >>> 0, 4);
        b.writeUInt16LE(Math.min(0xffff, this.drawOffersUsed[WHITE]), 8);
        b.writeUInt16LE(Math.min(0xffff, this.drawOffersUsed[BLACK]), 10);
        b.writeInt32LE(this.drawDeclinedAt[WHITE], 12);
        b.writeInt32LE(this.drawDeclinedAt[BLACK], 16);
        b.writeUInt16LE(Math.min(0xffff, this.desyncs[WHITE]), 20);
        b.writeUInt16LE(Math.min(0xffff, this.desyncs[BLACK]), 22);
        b.writeUInt32LE(this.flags >>> 0, 24);
        b.writeDoubleLE(this.disconnectedAt[WHITE], 28);
        b.writeDoubleLE(this.disconnectedAt[BLACK], 36);
        b.writeDoubleLE(this.clock.turnStart, 44);
        b.writeUInt32LE(this.clock.quota[WHITE] >>> 0, 52);
        b.writeUInt32LE(this.clock.quota[BLACK] >>> 0, 56);
        return { kind: JournalKind.Event, at: this.clock.turnStart, payload: b };
    }

    _replayMove(b) {
        if (b.length < MOVE_REC_BYTES) throw new JournalError('short move record');
        if (this._over) throw new JournalError('move after the end');
        const ply = b.readUInt16LE(0);
        if (ply !== this.ply) throw new JournalError(`move for ply ${ply} at ply ${this.ply}`);
        const move = b.readUInt16LE(2);
        const r = this.game.play(move);
        if (!r || !r.ok) throw new JournalError(`move ${move} refused by the rules at ply ${ply}`);
        const bits = b[5];
        const gseqMove = b.readUInt32LE(20);
        this._applyMove(ply & 1, move, b[4], b.readUInt32LE(8), b.readUInt32LE(12), b.readUInt32LE(16), b.readDoubleLE(24), bits, gseqMove);
        this.gseq = gseqMove + ((bits & MB_DECLINED) ? 1 : 0);
    }

    _replayEvent(b, at) {
        if (b.length < EVENT_REC_BYTES) throw new JournalError('short event record');
        const kind = b[0];
        if (kind === JournalEvent.Checkpoint) { this._replayCheckpoint(b); return; }
        if (this._over) return;   // nothing is journaled after the end; ignore defensively
        const color = b[1];
        if (kind !== JournalEvent.Recovered && color !== WHITE && color !== BLACK) throw new JournalError(`bad colour ${color}`);
        this._applyEvent(kind, color, b.readUInt32LE(8), at);
        this.gseq = b.readUInt32LE(4);
    }

    _replayCheckpoint(b) {
        if (b.length < CHECKPOINT_BYTES) throw new JournalError('short checkpoint record');
        this.drawOffer = b[1] <= NONE ? b[1] : NONE;
        this.connected[WHITE] = (b[2] & 1) !== 0;
        this.connected[BLACK] = (b[2] & 2) !== 0;
        this.gseq = b.readUInt32LE(4);
        this.drawOffersUsed[WHITE] = b.readUInt16LE(8);
        this.drawOffersUsed[BLACK] = b.readUInt16LE(10);
        this.drawDeclinedAt[WHITE] = b.readInt32LE(12);
        this.drawDeclinedAt[BLACK] = b.readInt32LE(16);
        this.desyncs[WHITE] = b.readUInt16LE(20);
        this.desyncs[BLACK] = b.readUInt16LE(22);
        this.flags = b.readUInt32LE(24);
        this.disconnectedAt[WHITE] = b.readDoubleLE(28);
        this.disconnectedAt[BLACK] = b.readDoubleLE(36);
        this.clock.turnStart = b.readDoubleLE(44);
        this.clock.quota[WHITE] = b.readUInt32LE(52);
        this.clock.quota[BLACK] = b.readUInt32LE(56);
    }

    _replayEnded(b) {
        if (b.length < ENDED_REC_BYTES) throw new JournalError('short ended record');
        if (this._over) throw new JournalError('second ended record');
        const culprit = b[2] <= NONE ? b[2] : NONE;
        this._applyEnd(b[0], b[1], b.readUInt32LE(4), b.readUInt32LE(8), b.readDoubleLE(16), culprit);
        this.endGseq = b.readUInt32LE(12);
        this.gseq = this.endGseq;
    }
}

function checkColor(color) {
    if (color !== WHITE && color !== BLACK) throw new RangeError(`GameRoom: bad colour ${color}`);
}

export { WHITE, BLACK, NONE };
