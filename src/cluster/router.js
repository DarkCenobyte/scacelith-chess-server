// Router (one per shard): the state machine of every WebSocket connection, and the glue between
// sockets, the local GameHost, the shard bus and the primary (DESIGN 3, 5.7, 5.8).
//
//   hello  The first message must be Hello with seq 1 (else Error{HelloRequired} + close 4010),
//          within WS_HELLO_TIMEOUT_MS (same). proto in [PROTOCOL_MIN, PROTOCOL_VERSION] and
//          schema == SCHEMA_HASH, else Error{UnsupportedProtocol} + 4002. The token is validated by
//          the auth service (Unauthorized + 4003), e-mail verification enforced (EmailUnverified +
//          4003), then the primary's presence.claim (Banned: Error + Notice{Banned, until} + 4004;
//          ServerFull: + 4006) -> Welcome, then the active game (if any) is attached. Up to 8
//          messages pipelined behind Hello wait for the Welcome.
//   ready  Per message: token bucket (WS_MSG_RATE / WS_MSG_BURST; over it the message is dropped
//          with Error{RateLimited} at most once a second, and more than max(10, burst) drops in
//          10 s is a flood: anomaly + close 4301), server->client type (anomaly forged_type,
//          certain: sanction + 4302), strict decoding (anomaly malformed + 4300), seq == last+1
//          (else anomaly bad_seq, reported once per connection, and the message is dropped; a gap
//          resynchronises on the new seq so one lost message does not poison the connection).
//          C_Ping -> S_Pong (1/s), C_Pong -> RTT (EMA, capped at 2 s) fed to the game hosts,
//          queue/challenge requests -> primary IPC -> Ack / Error, game messages -> the local
//          GameHost or the host shard over the bus (raw frame, no re-encoding).
//   close  presence.release (the primary also leaves the queue and drops the user's pending
//          challenges when the release matches the live connection), host.detach (local or bus).
//
// Heartbeat: one sweeper interval per shard (TICK_MS) walks the connections in slices, so every
// connection is visited once per HEARTBEAT_INTERVAL_MS or more often, without a timer per
// connection: S_Ping with the server time when the last one is half an interval old or more (so
// pings are between half an interval and an interval plus a tick apart), and close 1001 after
// HEARTBEAT_TIMEOUT_MS of silence. Hello deadlines are a FIFO checked every tick (connections are
// queued in arrival order, so the head is the oldest).
//
// Deviations from DESIGN 5.3/5.7, documented for the integrators:
//   - Endpoints given to the GameHost: { send(buf), close(code, reason), connId, shard, userId,
//     rttMs } (local: the socket; remote: relayed over the bus, close included). RTT: `rttMs` is
//     the live EMA (local) or the last value relayed (remote), and host.onRtt(gameId, userId,
//     rttMs) is called on every measurement.
//   - forged_type with AUTO_SANCTION_CERTAIN_CHEATS: host.forfeitUser(userId) on the host shard of
//     the connection's games (local call or bus Forfeit), then Error{CheatDetected} + 4302.
//   - QueueJoin closes the rematch window of the connection's last game: a Rematch{accept:false}
//     is given to its host without a reply endpoint (no error goes back when no window is open).
//   - primary -> shard 'game.forfeit' { userId } -> host.forfeitUser(userId) (bans decided on the
//     primary).
//   - Extra IPC: shard -> primary 'shard.load' { shard, conns, games, lagP99, overloaded }
//     every 2 s (host placement), primary -> shard 'shard.down' { shard } (forget the remote
//     endpoints of a dead shard: its players are disconnected from their games).
//   - 'conn.ipAcquire' is called before the 101 response (refused upgrades never become
//     connections: HTTP 429 per IP, 503 when the server is full). Its answers also feed
//     isFull(), the server-full signal with which the listeners shed new connections before the
//     TLS handshake (net/listeners.js); that is only an estimate. This check (MAX_CONNECTIONS plus
//     a small reserve, cluster/presence.js) and the MAX_CONNECTIONS check at Hello stay exact.
//     A ServerFull at Hello does not feed isFull (see there): a server at MAX_CONNECTIONS itself
//     does not shed.
//   - ServerFull closes with 4006 (4000 + ErrorCode.ServerFull; CloseCode has no entry for it).
//
// Complexity per message: O(1) (one decode, one Map lookup, one token bucket update).

import crypto from 'node:crypto';
import { performance } from 'node:perf_hooks';
import { metrics as defaultRegistry } from '../metrics.js';
import {
    MSG, encode, decode, ProtocolError, PROTOCOL_VERSION, PROTOCOL_MIN, SCHEMA_HASH, enums, CloseCode, isClientType,
} from '../protocol/index.js';
import { isGameId, shardOfGameId } from '../util/ids.js';
import { BusKind, BusOp } from './bus.js';

const E = enums.ErrorCode;
const N = enums.NoticeCode;
export const CLOSE_SERVER_FULL = 4006;
const DIR_C2S = Object.freeze({ dir: 'c2s' });
const MAX_PENDING_HELLO = 8;
/** How long a server-full refusal ('global') keeps this worker in the server-full state (isFull). */
export const FULL_HOLD_MS = 5000;
const MAX_GAMES_PER_CONN = 8;
const RTT_CAP_MS = 2000;
const VALID_ERRORS = new Set(Object.values(E));

/** Name of each message type, by type byte (metric labels). */
export const TYPE_NAMES = (() => {
    const a = new Array(256).fill(null);
    for (const [k, v] of Object.entries(MSG)) a[v] = k;
    return a;
})();
const S2C_TYPES = new Set(Object.values(MSG).filter((t) => !isClientType(t)));
const GAME_TYPES = new Set([MSG.Move, MSG.Resign, MSG.DrawOffer, MSG.DrawAnswer, MSG.DrawClaim, MSG.Abort, MSG.Resync, MSG.Rematch]);

function errorFrame(ref, code, fatal = false, game = 0) {
    return encode.Error({ ref: ref >>> 0, code: VALID_ERRORS.has(code) ? code : E.Internal, fatal, game: game || 0 });
}

/** Converts a reply's error (ErrorCode number or its name) to an ErrorCode. */
export function toErrorCode(e) {
    if (typeof e === 'number' && VALID_ERRORS.has(e)) return e;
    if (typeof e === 'string') {
        if (E[e] !== undefined) return E[e];
        const pascal = e.replace(/(^|_)([a-z])/g, (_, __, c) => c.toUpperCase());
        if (E[pascal] !== undefined) return E[pascal];
    }
    return E.Internal;
}

/** The endpoint through which a GameHost reaches a local connection. */
class LocalEndpoint {
    constructor(conn, shard) { this.conn = conn; this.connId = conn.id; this.shard = shard; this.local = true; }
    get userId() { return this.conn.userId; }
    get rttMs() { return this.conn.rttMs; }
    send(buf) { return this.conn.sendFrame(buf); }
    close(code, reason) { this.conn.close(code, reason || ''); }
}

/** The endpoint through which a GameHost reaches a connection of another shard (bus). */
class RemoteEndpoint {
    constructor(bus, shard, connId, userId) {
        this.bus = bus; this.shard = shard; this.connId = connId; this.userId = userId;
        this.rttMs = 0; this.local = false; this.games = new Set();
    }
    send(buf) { return this.bus.send(this.shard, BusKind.ToConn, this.connId, this.userId, 0, buf); }
    close(code) {
        const b = Buffer.allocUnsafe(2);
        b.writeUInt16LE(code & 0xffff, 0);
        this.bus.control(this.shard, BusOp.Close, this.connId, this.userId, 0, b);
    }
}

/** Router state of one connection (fixed shape). */
class ConnCtx {
    constructor(router, conn, now) {
        this.lastSeq = 0;
        this.tokens = router.burst;
        this.tokenAt = now;
        this.drops = 0;
        this.dropWindowAt = now;
        this.lastRateErrorAt = 0;
        this.helloStarted = false;
        this.pending = null;
        this.claimed = false;
        this.pingNonce = 0;
        this.pingSentAt = 0;
        this.pingAt = now;
        this.lastClientPingAt = 0;
        this.games = null;
        this.endpoint = new LocalEndpoint(conn, router.shard);
        this.tokenHash = null;
        this.sessionId = 0;
        this.badSeqReported = false;
    }
}

export class Router {
    /**
     * @param {object} o
     * @param {object} o.config
     * @param {number} o.shard
     * @param {object} o.host GameHost (DESIGN 5.3)
     * @param {{ validateToken(token: string): Promise<object|null>, invalidate?(p: object): void }} o.auth
     * @param {import('./ipc.js').Ipc} o.primary
     * @param {import('./bus.js').Bus|null} o.bus
     * @param {object} [o.anticheat] createAnticheat() result
     * @param {object} [o.store] for ratings (store.ratings.get)
     * @param {object} [o.log]
     * @param {object} [o.registry]
     * @param {() => number} [o.lagP99] event-loop delay p99 in ms (overload report)
     * @param {number} [o.tickMs] sweeper period
     * @param {(shard: number) => boolean} [o.isShard] valid host shards (default: this instance's range)
     */
    constructor({ config, shard, host, auth, primary, bus = null, anticheat = null, store = null, log = null,
        registry = defaultRegistry, lagP99 = () => 0, tickMs = 250, isShard = null }) {
        this.config = config;
        this.shard = shard;
        this.host = host;
        this.auth = auth;
        this.primary = primary;
        this.bus = bus;
        this.anticheat = anticheat;
        this.store = store;
        this.log = log;
        this.lagP99 = lagP99;
        this.tickMs = tickMs;
        this.rate = config.wsMsgRate;
        this.burst = config.wsMsgBurst;
        this.floodDrops = Math.max(10, config.wsMsgBurst);
        const lo = config.shardBase, hi = config.shardBase + config.workers;
        this.isShard = isShard || ((s) => s >= lo && s < hi);
        this.categories = new Map(config.categories.map((c) => [c.id, c]));
        /** @type {Map<number, import('../net/ws.js').WsConnection>} */
        this.conns = new Map();
        /** @type {Map<number, import('../net/ws.js').WsConnection>} ready connections by user */
        this.byUser = new Map();
        /** @type {Map<number, RemoteEndpoint>} key: shard * 2^32 + connId */
        this.remote = new Map();
        this.draining = false;
        this._helloQ = [];
        this._helloHead = 0;
        this._iter = null;
        this._timer = null;
        this._loadTimer = null;

        const r = registry;
        r.gaugeFn('scacelith_ws_connections', 'Open WebSocket connections', () => this.conns.size, { perShard: true });
        r.gaugeFn('scacelith_ws_players', 'Authenticated connections', () => this.byUser.size, { perShard: true });
        this._inByType = r.counter('scacelith_ws_messages_in_total', 'Messages received by type', ['type']);
        this._inChildren = new Array(256).fill(null);
        this._hello = r.counter('scacelith_ws_hello_total', 'Hello outcomes', ['result']);
        this._helloMs = r.histogram('scacelith_ws_hello_ms', 'Connection open to Welcome (token check and presence included)', [1, 2, 5, 10, 25, 50, 100, 250, 500, 1000]);
        this._rtt = r.histogram('scacelith_ws_rtt_ms', 'Round trip measured with the heartbeat', [5, 10, 25, 50, 75, 100, 150, 250, 500, 1000, 2000]);
        const dropped = r.counter('scacelith_ws_dropped_total', 'Client messages dropped', ['reason']);
        this._dropRate = dropped.labels('rate');
        this._dropSeq = dropped.labels('bad_seq');
        this._dropPing = dropped.labels('ping_limit');
        this._anomalies = r.counter('scacelith_ws_anomalies_total', 'Protocol anomalies seen by the router', ['kind']);
        this._relayed = r.counter('scacelith_ws_relayed_total', 'Game messages relayed to another shard');

        // Server-full signal for the admission before TLS (isFull): the last answers of the
        // primary's global check (MAX_CONNECTIONS plus its reserve, not MAX_CONNECTIONS alone), and
        // this worker's share of MAX_CONNECTIONS with 20 % of slack.
        this.localCap = Math.ceil((config.maxConnections || 200000) * 1.2 / Math.max(1, config.workers || 1));
        this._fullAt = -Infinity;
        this._admitAt = -Infinity;
        this.admission = {
            acquire: (ip) => this.primary.request('conn.ipAcquire', { ip, shard: this.shard }).then(
                (res) => {
                    if (res && res.ok) { this._admitAt = performance.now(); return true; }
                    const global = res && res.reason === 'global';
                    if (global) this._fullAt = performance.now();
                    return { ok: false, status: global ? 503 : 429, error: global ? 'server_full' : 'too_many_connections' };
                },
                (e) => { this.log?.warn?.('ipAcquire failed', { err: e }); return { ok: false, status: 503, error: 'unavailable' }; }),
            release: (ip) => { this.primary.notify('conn.ipRelease', { ip, shard: this.shard }); },
        };
        this.onConnection = (conn) => this._onConnection(conn);
        this.onBus = (kind, from, connId, userId, gameId, payload) => this._onBus(kind, from, connId, userId, gameId, payload);
    }

    // ---- lifecycle ------------------------------------------------------------------------------

    /** Starts the heartbeat sweeper and the load reports. */
    start() {
        if (this._timer) return;
        this._timer = setInterval(() => this.sweep(), this.tickMs);
        this._timer.unref();
        this._loadTimer = setInterval(() => this._reportLoad(), 2000);
        this._loadTimer.unref();
    }

    stop() {
        clearInterval(this._timer);
        clearInterval(this._loadTimer);
        this._timer = this._loadTimer = null;
    }

    /**
     * Whether the server is full as far as this worker knows, so that the listeners shed new
     * connections before the TLS handshake (net/listeners.js TlsGate). True while the primary's
     * last answer to 'conn.ipAcquire' was a server-full refusal less than FULL_HOLD_MS ago
     * (an admitted upgrade or the delay ends it: some connections still reach the primary's check,
     * so a freed slot is found), or while this worker holds 1.2 times its share of
     * MAX_CONNECTIONS. O(1). The times are monotonic (performance.now), so a step of the wall
     * clock neither prolongs nor shortens the state.
     *
     * A ServerFull answer at Hello (a newcomer on a server at MAX_CONNECTIONS) deliberately does
     * not start the state, and an upgrade admitted inside the reserve ends it like any other
     * admitted upgrade. Shedding there would make
     * a player coming back to a game in progress compete with the newcomers for the
     * MAX_PENDING_HANDSHAKES / 2 connections let through per second, and the official game already
     * waits 60 to 120 s after a ServerFull. So a server at MAX_CONNECTIONS does not shed: each
     * newcomer costs a handshake, an upgrade and a Hello (scacelith_ws_hello_total{result=
     * "server_full"} counts them), and shedding starts only once the connections counted at the
     * upgrade reach MAX_CONNECTIONS plus the reserve (max(16, 2 %)), or this worker holds 1.2
     * times its share (test/integration/admission.test.js).
     * @param {number} [now] performance.now()
     */
    isFull(now = performance.now()) {
        if (this.conns.size >= this.localCap) return true;
        const age = now - this._fullAt;
        return this._fullAt > this._admitAt && age >= 0 && age < FULL_HOLD_MS;
    }

    /**
     * Registers the primary -> shard IPC handlers this router serves.
     * @param {import('./ipc.js').Ipc} ipc
     */
    bindPrimary(ipc) {
        ipc.on('game.create', async ({ spec }) => {
            try {
                const gameId = await this.host.createGame(spec);
                return { ok: true, gameId };
            } catch (e) {
                this.log?.error?.('game.create failed', { err: e });
                return { error: E.Internal };
            }
        });
        ipc.on('game.attach', ({ gameId, userId, connId }) => this.attach(gameId, userId, connId));
        ipc.on('conn.send', ({ connId, frames }) => this.sendTo(connId, frames));
        ipc.on('conn.kick', ({ connId, closeCode, frames }) => this.kick(connId, closeCode, frames));
        ipc.on('auth.invalidate', (p) => this.invalidateSessions(p));
        ipc.on('shard.down', ({ shard }) => this.forgetShard(shard));
        ipc.on('game.forfeit', ({ userId }) => ({ ok: this._forfeitLocal(userId) }));
    }

    /**
     * Warns every player, stops accepting messages after graceMs and closes every connection.
     * @param {number} graceMs
     */
    async drain(graceMs) {
        this.draining = true;
        const notice = encode.Notice({ code: N.ServerShutdown, arg: graceMs });
        for (const conn of this.conns.values()) if (conn.state === 'ready') conn.sendFrame(notice);
        if (graceMs > 0) await new Promise((r) => setTimeout(r, graceMs));
        const bye = errorFrame(0, E.ShuttingDown, true);
        for (const conn of [...this.conns.values()]) {
            conn.sendFrame(bye);
            conn.close(CloseCode.ShuttingDown, 'server shutting down');
        }
    }

    // ---- connections ----------------------------------------------------------------------------

    _onConnection(conn) {
        const now = Date.now();
        conn.ctx = new ConnCtx(this, conn, now);
        conn.onMessage = (cn, buf) => this._onMessage(cn, buf);
        conn.onClose = (cn, code) => this._onClose(cn, code);
        this.conns.set(conn.id, conn);
        this._helloQ.push(conn);
        if (this.draining) {
            conn.sendFrame(errorFrame(0, E.ShuttingDown, true));
            conn.close(CloseCode.ShuttingDown, 'server shutting down');
        }
    }

    _onClose(conn) {
        this.conns.delete(conn.id);
        const c = conn.ctx;
        if (!c) return;
        if (conn.userId && this.byUser.get(conn.userId) === conn) this.byUser.delete(conn.userId);
        if (c.claimed) {
            c.claimed = false;
            this.primary.notify('presence.release', { userId: conn.userId, connId: conn.id });
        }
        if (c.games) {
            for (const g of c.games) this._detach(conn, g);
            c.games = null;
        }
    }

    _fatal(conn, ref, code, closeCode, extra = null) {
        conn.sendFrame(errorFrame(ref, code, true));
        if (extra) for (const f of extra) conn.sendFrame(f);
        conn.close(closeCode, '');
    }

    _countIn(type) {
        let ch = this._inChildren[type];
        if (ch === null) ch = this._inChildren[type] = this._inByType.labels(TYPE_NAMES[type] || `0x${type.toString(16)}`);
        ch.inc();
    }

    _onMessage(conn, buf) {
        const c = conn.ctx;
        if (conn.state !== 'ready') {
            if (conn.state === 'hello') this._onHelloMessage(conn, c, buf);
            return;
        }
        const now = Date.now();
        // Token bucket.
        let t = c.tokens + (now - c.tokenAt) * this.rate / 1000;
        if (t > this.burst) t = this.burst;
        c.tokenAt = now;
        if (t < 1) { c.tokens = t; this._rateLimited(conn, c, buf, now); return; }
        c.tokens = t - 1;

        const type = buf.length ? buf[0] : 0;
        if (S2C_TYPES.has(type)) { this._forged(conn, c, type); return; }
        let msg;
        try {
            msg = decode(buf, DIR_C2S);
        } catch (e) {
            if (!(e instanceof ProtocolError)) throw e;
            this._anomaly(conn, 'malformed', { reason: e.reason, type }, 0);
            this._fatal(conn, 0, E.Malformed, CloseCode.ProtocolViolation);
            return;
        }
        if (msg.seq !== c.lastSeq + 1) { this._badSeq(conn, c, msg.seq); return; }
        c.lastSeq = msg.seq;
        this._countIn(type);

        switch (type) {
            case MSG.C_Ping:
                if (now - c.lastClientPingAt >= 950) {
                    c.lastClientPingAt = now;
                    conn.sendFrame(encode.S_Pong({ nonce: msg.nonce, serverTime: now }));
                } else this._dropPing.inc();
                return;
            case MSG.C_Pong:
                this._onPong(conn, c, msg.nonce);
                return;
            case MSG.Hello:
                conn.sendFrame(errorFrame(msg.seq, E.ProtocolViolation));
                return;
            case MSG.QueueJoin: {
                if (!this.categories.has(msg.category)) { conn.sendFrame(errorFrame(msg.seq, E.InvalidCategory)); return; }
                const { rating, provisional } = this._rating(conn.userId, msg.category);
                this._closeRematch(conn, c);
                this._call(conn, msg.seq, 'mm.join', {
                    userId: conn.userId, username: conn.username, category: msg.category, rated: msg.rated,
                    rating, provisional, shard: this.shard, connId: conn.id,
                });
                return;
            }
            case MSG.QueueLeave:
                this._call(conn, msg.seq, 'mm.leave', { userId: conn.userId });
                return;
            case MSG.ChallengeCreate: {
                const category = this.categoryOf(msg.baseSec * 1000, msg.incSec * 1000);
                const { rating, provisional } = this._rating(conn.userId, category);
                this._call(conn, msg.seq, 'challenge.create', {
                    from: { userId: conn.userId, username: conn.username, rating, provisional, shard: this.shard, connId: conn.id },
                    target: msg.target, baseSec: msg.baseSec, incSec: msg.incSec, rated: msg.rated, color: msg.color,
                });
                return;
            }
            case MSG.ChallengeAccept:
                this._call(conn, msg.seq, 'challenge.accept', { id: msg.id, by: this._who(conn) });
                return;
            case MSG.ChallengeDecline:
                this._call(conn, msg.seq, 'challenge.decline', { id: msg.id, userId: conn.userId });
                return;
            case MSG.ChallengeCancel:
                this._call(conn, msg.seq, 'challenge.cancel', { id: msg.id, userId: conn.userId });
                return;
            case MSG.ChallengeJoinCode:
                this._call(conn, msg.seq, 'challenge.joinCode', { code: msg.code, by: this._who(conn) });
                return;
            default:
                if (GAME_TYPES.has(type)) this._game(conn, c, msg, buf);
        }
    }

    _who(conn) {
        return { userId: conn.userId, username: conn.username, shard: this.shard, connId: conn.id };
    }

    /** Official category id of a time control, or 'custom'. */
    categoryOf(baseMs, incMs) {
        for (const c of this.categories.values()) if (c.baseMs === baseMs && c.incMs === incMs) return c.id;
        return 'custom';
    }

    _rating(userId, category) {
        if (category !== 'custom' && this.store?.ratings?.get) {
            try {
                const r = this.store.ratings.get(userId, category);
                if (r) return { rating: r.rating, provisional: (r.games ?? 0) < this.config.provisionalGames };
            } catch (e) {
                this.log?.error?.('rating read failed', { err: e });
            }
        }
        return { rating: this.config.initialRating, provisional: true };
    }

    async _call(conn, seq, type, payload) {
        let r;
        try {
            r = await this.primary.request(type, payload);
        } catch (e) {
            this.log?.warn?.('primary request failed', { type, err: e });
            if (conn.state === 'ready') conn.sendFrame(errorFrame(seq, E.Internal));
            return;
        }
        if (conn.state !== 'ready') return;
        if (r && r.error) conn.sendFrame(errorFrame(seq, toErrorCode(r.error)));
        else conn.sendFrame(encode.Ack({ ref: seq }));
    }

    _game(conn, c, msg, buf) {
        const g = msg.game;
        if (!isGameId(g)) { conn.sendFrame(errorFrame(msg.seq, E.NotInGame)); return; }
        const hs = shardOfGameId(g);
        if (hs === this.shard) {
            try {
                this.host.onClientMessage(g, conn.userId, msg, c.endpoint);
            } catch (e) {
                this.log?.error?.('host.onClientMessage failed', { gameId: g, err: e });
                conn.sendFrame(errorFrame(msg.seq, E.Internal, false, g));
            }
            return;
        }
        if (!this.bus || !this.isShard(hs)) { conn.sendFrame(errorFrame(msg.seq, E.NotInGame, false, g)); return; }
        this._relayed.inc();
        this.bus.send(hs, BusKind.ToHost, conn.id, conn.userId, g, buf);
    }

    _onPong(conn, c, nonce) {
        if (!c.pingSentAt || nonce !== c.pingNonce) return;
        const rtt = performance.now() - c.pingSentAt;
        c.pingSentAt = 0;
        this._rtt.observe(rtt);
        const sample = Math.min(RTT_CAP_MS, rtt);
        conn.rttMs = conn.rttMs ? Math.min(RTT_CAP_MS, conn.rttMs * 0.8 + sample * 0.2) : sample;
        if (!c.games) return;
        const ms = Math.round(conn.rttMs);
        for (const g of c.games) {
            const hs = shardOfGameId(g);
            if (hs === this.shard) {
                if (typeof this.host.onRtt === 'function') {
                    try { this.host.onRtt(g, conn.userId, ms); } catch (e) { this.log?.error?.('host.onRtt failed', { err: e }); }
                }
            } else if (this.bus) {
                const b = Buffer.allocUnsafe(2);
                b.writeUInt16LE(Math.min(65535, ms), 0);
                this.bus.control(hs, BusOp.Rtt, conn.id, conn.userId, g, b);
            }
        }
    }

    _rateLimited(conn, c, buf, now) {
        this._dropRate.inc();
        const seq = buf.length >= 5 ? buf.readUInt32LE(1) : 0;
        if (seq === c.lastSeq + 1) c.lastSeq = seq;           // the client counted it: stay in step
        if (now - c.dropWindowAt > 10000) { c.dropWindowAt = now; c.drops = 0; }
        if (++c.drops > this.floodDrops) {
            this._anomaly(conn, 'flood', { drops: c.drops }, 0);
            this._fatal(conn, seq, E.Flood, CloseCode.Flood);
            return;
        }
        if (now - c.lastRateErrorAt >= 1000) {
            c.lastRateErrorAt = now;
            conn.sendFrame(errorFrame(seq, E.RateLimited));
        }
    }

    _badSeq(conn, c, seq) {
        this._dropSeq.inc();
        if (!c.badSeqReported) {
            c.badSeqReported = true;
            this._anomaly(conn, 'bad_seq', { expected: c.lastSeq + 1, got: seq }, 0);
        }
        if (seq > c.lastSeq) c.lastSeq = seq;
    }

    _forged(conn, c, type) {
        const gameId = this._currentGame(c);
        this._anomaly(conn, 'forged_type', { type }, gameId);
        if (!this.config.autoSanctionCertainCheats) {
            this._fatal(conn, 0, E.ProtocolViolation, CloseCode.ProtocolViolation);
            return;
        }
        if (this.anticheat?.sanctionCertain) {
            try {
                const r = this.anticheat.sanctionCertain({ userId: conn.userId, gameId, kind: 'forged_type' });
                if (r && typeof r.then === 'function') r.then(undefined, (e) => this.log?.error?.('sanctionCertain failed', { err: e }));
            } catch (e) {
                this.log?.error?.('sanctionCertain failed', { err: e });
            }
        }
        if (c.games) {
            const done = new Set();
            for (const g of c.games) {
                const hs = shardOfGameId(g);
                if (done.has(hs)) continue;
                done.add(hs);
                if (hs === this.shard) this._forfeitLocal(conn.userId);
                else if (this.bus && this.isShard(hs)) this.bus.control(hs, BusOp.Forfeit, conn.id, conn.userId, g);
            }
        }
        this._fatal(conn, 0, E.CheatDetected, CloseCode.CheatDetected);
    }

    _forfeitLocal(userId) {
        if (typeof this.host.forfeitUser !== 'function') return false;
        try { return !!this.host.forfeitUser(userId); } catch (e) { this.log?.error?.('host.forfeitUser failed', { err: e }); return false; }
    }

    // Joining a queue ends the rematch window of the last game (DESIGN 6.3).
    _closeRematch(conn, c) {
        const g = this._currentGame(c);
        if (!g) return;
        const hs = shardOfGameId(g);
        if (hs === this.shard) this._declineRematchLocal(g, conn.userId);
        else if (this.bus && this.isShard(hs)) this.bus.control(hs, BusOp.RematchDecline, conn.id, conn.userId, g);
    }

    _declineRematchLocal(gameId, userId) {
        try {
            this.host.onClientMessage(gameId, userId, { type: MSG.Rematch, seq: 0, game: gameId, accept: false }, null);
        } catch (e) {
            this.log?.error?.('rematch decline failed', { gameId, err: e });
        }
    }

    _currentGame(c) {
        let last = 0;
        if (c.games) for (const g of c.games) last = g;
        return last;
    }

    _anomaly(conn, kind, detail, gameId) {
        this._anomalies.labels(kind).inc();
        if (!this.anticheat?.recordAnomaly || !conn.userId) return null;
        try {
            return this.anticheat.recordAnomaly({ userId: conn.userId, gameId: gameId || 0, kind, detail, posMatched: false });
        } catch (e) {
            this.log?.error?.('recordAnomaly failed', { err: e });
            return null;
        }
    }

    // ---- hello ----------------------------------------------------------------------------------

    _onHelloMessage(conn, c, buf) {
        if (c.helloStarted) {
            if (!c.pending) c.pending = [];
            if (c.pending.length >= MAX_PENDING_HELLO) { this._fatal(conn, 0, E.Flood, CloseCode.Flood); return; }
            c.pending.push(Buffer.from(buf));
            return;
        }
        if (buf.length === 0 || buf[0] !== MSG.Hello) {
            this._hello.labels('hello_required').inc();
            this._fatal(conn, 0, E.HelloRequired, CloseCode.HelloTimeout);
            return;
        }
        let msg;
        try {
            msg = decode(buf, DIR_C2S);
        } catch (e) {
            if (!(e instanceof ProtocolError)) throw e;
            this._hello.labels('malformed').inc();
            this._fatal(conn, 0, E.Malformed, CloseCode.ProtocolViolation);
            return;
        }
        if (msg.seq !== 1) {
            this._hello.labels('malformed').inc();
            this._fatal(conn, msg.seq, E.ProtocolViolation, CloseCode.ProtocolViolation);
            return;
        }
        if (msg.proto < PROTOCOL_MIN || msg.proto > PROTOCOL_VERSION || msg.schema !== SCHEMA_HASH) {
            this._hello.labels('unsupported_protocol').inc();
            this._fatal(conn, 1, E.UnsupportedProtocol, CloseCode.UnsupportedProtocol);
            return;
        }
        c.helloStarted = true;
        c.lastSeq = 1;
        this._authenticate(conn, c, msg).catch((e) => {
            this.log?.error?.('hello failed', { err: e });
            if (conn.state === 'hello') { this._hello.labels('internal').inc(); this._fatal(conn, 1, E.Internal, CloseCode.Internal); }
        });
    }

    async _authenticate(conn, c, msg) {
        let session;
        try {
            session = await this.auth.validateToken(msg.token);
        } catch (e) {
            this.log?.error?.('token validation failed', { err: e });
            session = undefined;
        }
        if (conn.state !== 'hello') return;
        if (session === undefined) { this._hello.labels('internal').inc(); this._fatal(conn, 1, E.Internal, CloseCode.Internal); return; }
        if (!session) { this._hello.labels('unauthorized').inc(); this._fatal(conn, 1, E.Unauthorized, CloseCode.Unauthorized); return; }
        const now = Date.now();
        const banUntil = session.bannedUntil || session.banUntil || 0;
        if (banUntil > now) { this._banned(conn, banUntil); return; }
        if (this.config.requireEmailVerification && !session.emailVerified) {
            this._hello.labels('email_unverified').inc();
            this._fatal(conn, 1, E.EmailUnverified, CloseCode.Unauthorized);
            return;
        }
        conn.userId = session.userId;
        conn.username = session.username;
        c.sessionId = session.sessionId || 0;
        c.tokenHash = crypto.createHash('sha256').update(msg.token).digest();
        let r;
        try {
            r = await this.primary.request('presence.claim', { userId: conn.userId, username: conn.username, shard: this.shard, connId: conn.id, ip: conn.ip });
        } catch (e) {
            this.log?.warn?.('presence.claim failed', { err: e });
            r = null;
        }
        if (conn.state !== 'hello') {
            if (r && r.ok) this.primary.notify('presence.release', { userId: conn.userId, connId: conn.id });
            return;
        }
        if (!r) { this._hello.labels('internal').inc(); this._fatal(conn, 1, E.Internal, CloseCode.Internal); return; }
        if (r.error) {
            const code = toErrorCode(r.error);
            if (code === E.Banned) { this._banned(conn, r.until || 0); return; }
            if (code === E.ServerFull) { this._hello.labels('server_full').inc(); this._fatal(conn, 1, E.ServerFull, CLOSE_SERVER_FULL); return; }
            this._hello.labels('refused').inc();
            this._fatal(conn, 1, code, CloseCode.Policy);
            return;
        }
        c.claimed = true;
        conn.state = 'ready';
        const prev = this.byUser.get(conn.userId);
        this.byUser.set(conn.userId, conn);
        if (prev && prev !== conn && prev.state !== 'closed') {
            // Same shard, same user: the primary kicks it too, but be sure (no double routing).
            prev.sendFrame(errorFrame(0, E.Replaced, true));
            prev.sendFrame(encode.Notice({ code: N.ReplacedByNewConnection, arg: 0 }));
            prev.close(CloseCode.Replaced, '');
        }
        const activeGame = isGameId(r.activeGame) ? r.activeGame : 0;
        conn.sendFrame(encode.Welcome({
            proto: msg.proto, serverTime: now, userId: conn.userId, username: conn.username,
            serverName: this.config.serverName, heartbeatMs: this.config.heartbeatIntervalMs,
            clientPingMs: this.config.clientPingIntervalMs, maxMsgPerSec: Math.min(65535, this.rate), activeGame,
        }));
        this._hello.labels('ok').inc();
        this._helloMs.observe(now - conn.openedAt);
        if (activeGame) this.attach(activeGame, conn.userId, conn.id);
        const pending = c.pending;
        c.pending = null;
        if (pending) for (const b of pending) { if (conn.state !== 'ready') break; this._onMessage(conn, b); }
    }

    _banned(conn, until) {
        this._hello.labels('banned').inc();
        this._fatal(conn, 1, E.Banned, CloseCode.Banned, [encode.Notice({ code: N.Banned, arg: until || 0 })]);
    }

    // ---- games ----------------------------------------------------------------------------------

    /**
     * Binds a connection of this shard to a game (primary 'game.attach', or the active game at
     * Welcome). The host sends the snapshot.
     * @returns {{ ok: boolean }}
     */
    attach(gameId, userId, connId) {
        const conn = this.conns.get(connId);
        if (!conn || conn.userId !== userId || conn.state !== 'ready' || !isGameId(gameId)) return { ok: false };
        const c = conn.ctx;
        if (!c.games) c.games = new Set();
        c.games.delete(gameId);
        c.games.add(gameId);
        if (c.games.size > MAX_GAMES_PER_CONN) {
            const oldest = c.games.values().next().value;
            c.games.delete(oldest);
            this._detach(conn, oldest);
        }
        const hs = shardOfGameId(gameId);
        if (hs === this.shard) {
            try { this.host.attach(gameId, userId, c.endpoint); } catch (e) { this.log?.error?.('host.attach failed', { gameId, err: e }); return { ok: false }; }
        } else if (this.bus && this.isShard(hs)) {
            this.bus.control(hs, BusOp.Attach, conn.id, userId, gameId);
        } else {
            return { ok: false };
        }
        return { ok: true };
    }

    _detach(conn, gameId) {
        const hs = shardOfGameId(gameId);
        if (hs === this.shard) {
            try { this.host.detach(gameId, conn.userId, conn.ctx.endpoint); } catch (e) { this.log?.error?.('host.detach failed', { gameId, err: e }); }
        } else if (this.bus && this.isShard(hs)) {
            this.bus.control(hs, BusOp.Detach, conn.id, conn.userId, gameId);
        }
    }

    _remoteKey(shard, connId) { return shard * 0x100000000 + connId; }

    _onBus(kind, from, connId, userId, gameId, payload) {
        switch (kind) {
            case BusKind.ToConn: {
                const conn = this.conns.get(connId);
                if (conn && conn.userId === userId && conn.state === 'ready') conn.sendFrame(payload);
                return;
            }
            case BusKind.ToHost: {
                let msg;
                try { msg = decode(payload, DIR_C2S); } catch { return; }      // validated by the source shard
                const ep = this.remote.get(this._remoteKey(from, connId)) || new RemoteEndpoint(this.bus, from, connId, userId);
                try {
                    this.host.onClientMessage(gameId, userId, msg, ep);
                } catch (e) {
                    this.log?.error?.('host.onClientMessage failed', { gameId, err: e });
                }
                return;
            }
            case BusKind.Control:
                this._onControl(payload[0], from, connId, userId, gameId, payload);
                return;
            default:
        }
    }

    _onControl(op, from, connId, userId, gameId, payload) {
        const key = this._remoteKey(from, connId);
        switch (op) {
            case BusOp.Attach: {
                let ep = this.remote.get(key);
                if (!ep || ep.userId !== userId) { ep = new RemoteEndpoint(this.bus, from, connId, userId); this.remote.set(key, ep); }
                ep.games.add(gameId);
                try { this.host.attach(gameId, userId, ep); } catch (e) { this.log?.error?.('host.attach failed', { gameId, err: e }); }
                return;
            }
            case BusOp.Detach: {
                const ep = this.remote.get(key);
                if (!ep) return;
                ep.games.delete(gameId);
                if (!ep.games.size) this.remote.delete(key);
                try { this.host.detach(gameId, userId, ep); } catch (e) { this.log?.error?.('host.detach failed', { gameId, err: e }); }
                return;
            }
            case BusOp.Rtt: {
                if (payload.length < 3) return;
                const ms = payload.readUInt16LE(1);
                const ep = this.remote.get(key);
                if (ep) ep.rttMs = ms;
                if (typeof this.host.onRtt === 'function') {
                    try { this.host.onRtt(gameId, userId, ms); } catch (e) { this.log?.error?.('host.onRtt failed', { err: e }); }
                }
                return;
            }
            case BusOp.Forfeit:
                this._forfeitLocal(userId);
                return;
            case BusOp.RematchDecline:
                this._declineRematchLocal(gameId, userId);
                return;
            case BusOp.Close: {
                // Sent by a host shard (certain cheat): close our connection.
                const conn = this.conns.get(connId);
                if (conn && conn.userId === userId && payload.length >= 3) conn.close(payload.readUInt16LE(1), '');
                return;
            }
            default:
        }
    }

    /**
     * A shard died: its connections are gone, detach them from the games hosted here.
     * @param {number} shard
     */
    forgetShard(shard) {
        let n = 0;
        for (const [key, ep] of this.remote) {
            if (ep.shard !== shard) continue;
            this.remote.delete(key);
            for (const g of ep.games) {
                try { this.host.detach(g, ep.userId, ep); } catch (e) { this.log?.error?.('host.detach failed', { err: e }); }
                n++;
            }
        }
        return { ok: true, detached: n };
    }

    // ---- primary -> shard -----------------------------------------------------------------------

    /** Writes encoded frames on a connection ('conn.send'). */
    sendTo(connId, frames) {
        const conn = this.conns.get(connId);
        if (!conn || conn.state !== 'ready') return { ok: false };
        for (const f of frames || []) conn.sendFrame(f);
        return { ok: true };
    }

    /** Sends frames then closes a connection ('conn.kick'). */
    kick(connId, closeCode, frames) {
        const conn = this.conns.get(connId);
        if (!conn) return { ok: false };
        for (const f of frames || []) conn.sendFrame(f);
        conn.close(closeCode || CloseCode.Policy, '');
        return { ok: true };
    }

    /**
     * 'auth.invalidate': drops the cached sessions and closes the user's connections opened with
     * one of the revoked tokens (hashes as Buffers, hex or base64/base64url strings).
     */
    invalidateSessions({ userId, tokenHashes }) {
        try { this.auth.invalidate?.({ userId, tokenHashes }); } catch (e) { this.log?.error?.('auth.invalidate failed', { err: e }); }
        const conn = this.byUser.get(userId);
        if (!conn || !conn.ctx?.tokenHash) return { ok: true, closed: 0 };
        const h = conn.ctx.tokenHash;
        const forms = new Set([h.toString('hex'), h.toString('base64'), h.toString('base64url')]);
        const hit = !tokenHashes || tokenHashes.some((x) => (Buffer.isBuffer(x) || x instanceof Uint8Array ? h.equals(Buffer.from(x)) : forms.has(String(x))));
        if (!hit) return { ok: true, closed: 0 };
        conn.sendFrame(encode.Notice({ code: N.SessionRevoked, arg: 0 }));
        this._fatal(conn, 0, E.Unauthorized, CloseCode.Unauthorized);
        return { ok: true, closed: 1 };
    }

    // ---- heartbeat ------------------------------------------------------------------------------

    /** One sweeper tick (exposed for tests). */
    sweep(now = Date.now()) {
        const cfg = this.config;
        // Hello deadlines (FIFO in arrival order).
        const q = this._helloQ;
        while (this._helloHead < q.length) {
            const conn = q[this._helloHead];
            if (conn.state === 'hello' && !conn.ctx.helloStarted) {
                if (now - conn.openedAt < cfg.wsHelloTimeoutMs) break;
                this._hello.labels('timeout').inc();
                this._fatal(conn, 0, E.HelloRequired, CloseCode.HelloTimeout);
            }
            // Ready, closed, or authenticating (the slice below catches a stuck authentication).
            q[this._helloHead++] = null;
        }
        if (this._helloHead > 1024 && this._helloHead * 2 > q.length) { this._helloQ = q.slice(this._helloHead); this._helloHead = 0; }

        // Heartbeat slice. A lap over the connections takes ceil(n / slice) ticks, at most about one
        // interval. When the iterator runs out mid-tick, the tick ends there and the next lap starts
        // with the next tick at the first connection (restarting the lap here would visit a
        // connection twice in one tick; consuming the first entry here would skip it forever).
        const n = this.conns.size;
        if (!n) { this._iter = null; return; }
        const slice = Math.max(1, Math.ceil(n * this.tickMs / cfg.heartbeatIntervalMs));
        const interval = cfg.heartbeatIntervalMs, timeout = cfg.heartbeatTimeoutMs;
        if (!this._iter) this._iter = this.conns.values();
        for (let i = 0; i < slice; i++) {
            let r = this._iter.next();
            if (r.done) {
                this._iter = this.conns.values();
                if (i > 0) break;
                r = this._iter.next();
                if (r.done) break;
            }
            const conn = r.value;
            if (now - conn.lastRecvAt >= timeout) { conn.close(CloseCode.GoingAway, 'timeout'); continue; }
            const c = conn.ctx;
            if (conn.state === 'hello' && now - conn.openedAt >= cfg.wsHelloTimeoutMs * 2) {
                // Authentication stuck (auth service or primary not answering).
                this._hello.labels('internal').inc();
                this._fatal(conn, 1, E.Internal, CloseCode.Internal);
                continue;
            }
            // A lap lasts at most about one interval, so a ping on every visit at least half an
            // interval after the last one keeps pings between interval / 2 and interval + tick
            // apart. (Waiting for interval - tick would skip every other visit whenever a lap is
            // shorter than that, and space the pings about two intervals apart.)
            if (conn.state === 'ready' && now - c.pingAt >= interval / 2) {
                c.pingAt = now;
                c.pingNonce = (c.pingNonce + 1) >>> 0 || 1;
                c.pingSentAt = performance.now();
                conn.sendFrame(encode.S_Ping({ nonce: c.pingNonce, serverTime: now }));
            }
        }
    }

    _reportLoad() {
        let games = 0;
        try { games = this.host.stats?.()?.games ?? 0; } catch { /* ignore */ }
        const lag = this.lagP99();
        this.primary.notify('shard.load', {
            shard: this.shard, conns: this.conns.size, players: this.byUser.size, games, lagP99: lag,
            overloaded: lag > (this.config.shardOverloadLagMs || 250),
        });
    }

    /** Diagnostics. */
    stats() {
        return { conns: this.conns.size, players: this.byUser.size, remoteEndpoints: this.remote.size };
    }
}
