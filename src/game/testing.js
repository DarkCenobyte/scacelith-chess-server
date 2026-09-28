// Test doubles for the game module and for modules that embed a GameHost (net, integration,
// bench): a scripted ChessGame (the rules do not matter), an in-memory journal, a store, an
// anti-cheat, a primary and endpoints. Not used by the server itself.

import { decode, enums, MoveFlag } from '../protocol/index.js';

const { GameStatus: GS } = enums;

/** A move that is legal for FakeChessGame (from != to, bit 15 clear). */
export function fakeMove(i, promo = 0) {
    const from = i & 63;
    const to = (from + 1 + ((i >> 6) % 63)) & 63;     // offset 1..63: never the origin square
    return from | (to << 6) | ((promo & 7) << 12);
}

/**
 * Scripted rules with the ChessGame API of DESIGN 5.2 (the parts GameRoom uses).
 * script: {
 *   illegal?: number[]            moves that are always illegal,
 *   flags?: { [ply]: number }     extra MoveFlag bits returned when the move of that ply is played,
 *   endAfter?: { [plies]: { status, reason } }  automatic end once that many plies are played,
 *   threefoldAt?: number[], fiftyAt?: number[]  plies at which a claim is valid,
 *   canMate?: [bool, bool]        whether White / Black can still mate (default both true)
 * }
 * A move with promotion bits returns MoveFlag.Promotion.
 */
export class FakeChessGame {
    constructor(script = {}) {
        this.script = script;
        this.moves = [];
        this.status = GS.Ongoing;
        this.reason = 0;
        this._hash = 0x811c9dc5;
        const self = this;
        this.position = {
            get side() { return self.moves.length & 1; },
            get halfmove() { return 0; },
            digest() { return self._hash >>> 0; },
            isLegal(m) { return self._legal(m); },
            canColorMate(c) { return !(self.script.canMate && self.script.canMate[c] === false); },
        };
    }

    _legal(m) {
        if (this.status !== GS.Ongoing) return false;
        if (!Number.isInteger(m) || m < 0 || m > 0x7fff) return false;
        if ((m & 63) === ((m >> 6) & 63)) return false;
        if (this.script.illegal && this.script.illegal.includes(m)) return false;
        return true;
    }

    play(m) {
        if (!this._legal(m)) return { ok: false };
        this.moves.push(m);
        let h = this._hash;
        h = Math.imul(h ^ (m & 0xff), 0x01000193) >>> 0;
        h = Math.imul(h ^ (m >> 8), 0x01000193) >>> 0;
        this._hash = h;
        const ply = this.moves.length - 1;
        let flags = (m >> 12) & 7 ? MoveFlag.Promotion : 0;
        if (this.script.flags && this.script.flags[ply] !== undefined) flags |= this.script.flags[ply];
        const end = this.script.endAfter && this.script.endAfter[this.moves.length];
        if (end) { this.status = end.status; this.reason = end.reason; }
        return { ok: true, flags, status: this.status, reason: this.reason };
    }

    repetitionCount() { return 1; }
    canClaimThreefold() { return !!(this.script.threefoldAt && this.script.threefoldAt.includes(this.moves.length)); }
    canClaimFiftyMove() { return !!(this.script.fiftyAt && this.script.fiftyAt.includes(this.moves.length)); }
    claimDraw() { return this.canClaimThreefold() || this.canClaimFiftyMove(); }
    resign(color) { this.end(color === 0 ? GS.BlackWins : GS.WhiteWins, enums.EndReason.Resignation); }
    agreeDraw() { this.end(GS.Draw, enums.EndReason.Agreement); }
    flagFall(color) { this.end(color === 0 ? GS.BlackWins : GS.WhiteWins, enums.EndReason.Timeout); }
    end(status, reason) { this.status = status; this.reason = reason; }
}

/** In-memory journal with the API of DESIGN 5.6. */
export class MemoryJournal {
    constructor() {
        this.games = new Map();      // gameId -> [{ kind, at, payload }]
        this.done = new Set();       // committed game ids
        this.appends = 0;
        this.flushes = 0;
    }
    append(kind, gameId, payload, at) {
        let list = this.games.get(gameId);
        if (!list) { list = []; this.games.set(gameId, list); }
        list.push({ kind, at, payload: Buffer.from(payload) });
        this.appends++;
    }
    committed(gameId) { this.done.add(gameId); }
    flush() { this.flushes++; return Promise.resolve(); }
    recover() {
        const m = new Map();
        for (const [id, list] of this.games) if (!this.done.has(id)) m.set(id, list.slice());
        return m;
    }
    close() { return Promise.resolve(); }
}

/** Store double: store.games.finishBatch with failure injection. */
export class FakeStore {
    constructor() {
        this.batches = [];
        this.failures = 0;           // number of next calls that throw
        this.async = false;          // return a Promise instead of the result
        this.badIds = new Set();     // a batch holding one of these ids throws (like invalid_record)
        const self = this;
        this.games = {
            finishBatch(records) {
                if (self.failures > 0) {
                    self.failures--;
                    const err = new Error('database is locked');
                    if (self.async) return Promise.reject(err);
                    throw err;
                }
                const bad = records.find((r) => self.badIds.has(r.id));
                if (bad) {
                    const err = Object.assign(new Error('invalid finished game record'), { code: 'invalid_record', gameId: bad.id });
                    if (self.async) return Promise.reject(err);
                    throw err;
                }
                self.batches.push(records);
                const res = records.map((r) => ({
                    gameId: r.id,
                    ratings: r.rated ? {
                        white: { before: r.whiteRating, after: r.whiteRating + 8, games: 1, provisional: true },
                        black: { before: r.blackRating, after: r.blackRating - 8, games: 1, provisional: true },
                    } : null,
                }));
                return self.async ? Promise.resolve(res) : res;
            },
        };
    }
    get committedIds() { return this.batches.flat().map((r) => r.id); }
}

/** Anti-cheat double (DESIGN 5.8): certain kinds from the 6.5 table. */
export class FakeAnticheat {
    constructor() { this.anomalies = []; this.sanctions = []; }
    classify(kind) {
        const certain = ['foreign_game', 'out_of_turn', 'illegal_move', 'forged_type'].includes(kind);
        const info = ['stale_ply', 'desync', 'nothing_to_claim', 'game_over'].includes(kind);
        return { severity: certain ? 'certain' : info ? 'info' : 'suspicious', certain };
    }
    recordAnomaly(a) { this.anomalies.push(a); return this.classify(a.kind); }
    sanctionCertain(s) { this.sanctions.push(s); return { banUntil: 0 }; }
}

/** Primary IPC double: records requests; `handlers[type](payload)` gives the reply. */
export class FakePrimary {
    constructor(handlers = {}) { this.requests = []; this.handlers = handlers; }
    request(type, payload) {
        this.requests.push({ type, payload });
        const h = this.handlers[type];
        return Promise.resolve(h ? h(payload) : { ok: true });
    }
    of(type) { return this.requests.filter((r) => r.type === type).map((r) => r.payload); }
}

/** Endpoint double: collects frames; msgs() decodes them. */
export class FakeEndpoint {
    constructor(connId = 1, shard = 0) { this.connId = connId; this.shard = shard; this.sent = []; this.closed = null; }
    send(buf) { this.sent.push(buf); }
    close(code, reason) { this.closed = { code, reason }; }
    msgs() { return this.sent.map((b) => decode(b)); }
    clear() { this.sent.length = 0; }
}

/** Logger that discards everything. */
export const silentLog = Object.freeze({
    debug() {}, info() {}, warn() {}, error() {}, security() {}, child() { return silentLog; }, debugEnabled: false,
});
