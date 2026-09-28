// ScacelithClient: the realtime side of the Node SDK (integration tests, load generator, bots).
//
//   const c = await ScacelithClient.connect({ host, wsPort, token, ca });   // Hello -> Welcome
//   c.joinQueue('3+2', true);
//   const snap = await c.waitFor('GameSnapshot');
//   c.move(snap.game, 0, encodeMove(12, 28), posHash, 800);
//   await c.waitFor('MoveMade', (m) => m.ply === 0);
//   c.games.get(snap.game)            // last snapshot + applied MoveMade / GameEvent / GameEnd
//
// It numbers every client message (seq), answers the server's Ping at once, measures its own
// round trip and clock offset with ping(), and dispatches decoded server messages by name
// (on('MoveMade', fn), waitFor(...)). Memory per client is small: pass { history: 0,
// trackGames: false } when opening thousands of connections.

import { WsClient } from './ws-client.js';
import { encode, decode, messageName, MSG, enums, ProtocolError, PROTOCOL_VERSION, SCHEMA_HASH, WS_SUBPROTOCOL, DECODE_S2C } from '../protocol/index.js';

const T = MSG;
const { Color, GameEventKind, GameStatus } = enums;
const ERROR_NAMES = Object.fromEntries(Object.entries(enums.ErrorCode).map(([k, v]) => [v, k]));

// 'Ping' -> 'S_Ping' (a client only receives server messages); other names unchanged.
function eventName(name) {
    if (MSG[name] !== undefined) return name;
    if (MSG['S_' + name] !== undefined) return 'S_' + name;
    return name;
}

/**
 * Error of a refused connection or request. errorCode/errorName: the server's ErrorCode (0 / ''
 * when none); closeCode/closeReason: the WebSocket close (0 when still open); status: the HTTP
 * status when the upgrade itself was refused.
 */
export class ScacelithError extends Error {
    constructor(message, fields = {}) {
        super(message);
        this.name = 'ScacelithError';
        this.errorCode = fields.errorCode || 0;
        this.errorName = this.errorCode ? ERROR_NAMES[this.errorCode] || String(this.errorCode) : '';
        this.fatal = !!fields.fatal;
        this.closeCode = fields.closeCode || 0;
        this.closeReason = fields.closeReason || '';
        this.status = fields.status || 0;
        this.ref = fields.ref || 0;
    }
}

export class ScacelithClient {
    /**
     * @param {object} [opts]
     * @param {string} [opts.clientName] Hello.client (default 'scacelith-node-sdk')
     * @param {number} [opts.history] decoded messages kept for waitFor(..., { since }) (default 32, 0 = none)
     * @param {boolean} [opts.trackGames] keep client.games up to date (default true)
     * @param {boolean} [opts.autoPong] answer the server's Ping (default true)
     */
    constructor(opts = {}) {
        this.opts = opts;
        this.ws = null;
        this.state = 'idle';            // 'connecting' | 'hello' | 'ready' | 'closed'
        this.seq = 0;                   // last seq sent on this connection
        this.welcome = null;
        this.userId = 0;
        this.username = '';
        this.serverName = '';
        this.activeGame = 0;
        this.games = new Map();         // game id -> view (see _snapshot)
        this.currentGame = 0;           // id of the last game a snapshot was received for
        this.queue = null;              // last QueueStatus
        this.challenges = new Map();    // challenge id -> last ChallengeReceived / ChallengeStatus
        this.lastError = null;          // last Error message
        this.rttMs = -1;                // last ping() round trip
        this.clockOffsetMs = 0;         // server clock - local clock (Welcome, then ping())
        this.closeCode = 0;
        this.closeReason = '';
        this.received = 0;              // server messages received on this client
        const h = opts.history ?? 32;
        this._hist = h > 0 ? new Array(h) : null;
        this._ls = null;
        this._waiters = null;
        this._pings = null;
        this._pingNonce = 0;
        this._hello = null;
    }

    /** new ScacelithClient(opts) + connect(opts). */
    static async connect(opts) {
        const c = new ScacelithClient(opts);
        await c.connect(opts);
        return c;
    }

    /**
     * Opens the WebSocket, sends Hello and resolves with the Welcome message.
     * Rejects with a ScacelithError on an Error from the server, a close, a refused upgrade or a timeout.
     * @param {object} o
     * @param {string} [o.host] default '127.0.0.1'
     * @param {number} [o.wsPort] WebSocket port (alias: port)
     * @param {string} o.token session token from the HTTPS login
     * @param {string|Buffer} [o.ca] CA certificate (self-signed test servers)
     * @param {boolean} [o.insecure] plain ws:// (TLS_MODE=off)
     * @param {boolean} [o.rejectUnauthorized]
     * @param {string} [o.servername]
     * @param {string} [o.path] default '/ws'
     * @param {string} [o.clientName]
     * @param {number} [o.proto] Hello.proto (default PROTOCOL_VERSION)
     * @param {number} [o.schema] Hello.schema (default SCHEMA_HASH)
     * @param {string|null} [o.subprotocol] default 'scacelith.v1'
     * @param {number} [o.timeoutMs] whole connect + Hello (default 10000)
     * @param {string} [o.localAddress]
     * @param {object} [o.headers]
     * @returns {Promise<object>} the Welcome message
     */
    async connect(o = {}) {
        if (this.state === 'connecting' || this.state === 'hello' || this.state === 'ready') throw new Error('client: already connected');
        // Encoded first: an invalid token or client name fails here, before any connection.
        let hello;
        try {
            hello = encode.Hello({
                seq: 1, proto: o.proto ?? PROTOCOL_VERSION, schema: o.schema ?? SCHEMA_HASH,
                client: o.clientName ?? this.opts.clientName ?? 'scacelith-node-sdk', token: o.token ?? '',
            });
        } catch (e) {
            throw new ScacelithError(`client: invalid Hello (${e.reason || e.message})`);
        }
        this.state = 'connecting';
        this.seq = 0;
        this.closeCode = 0;
        this.closeReason = '';
        const timeoutMs = o.timeoutMs ?? 10000;
        const ws = new WsClient({
            host: o.host ?? '127.0.0.1', port: o.wsPort ?? o.port, path: o.path ?? '/ws', ca: o.ca, insecure: !!o.insecure,
            rejectUnauthorized: o.rejectUnauthorized, servername: o.servername, headers: o.headers, localAddress: o.localAddress,
            subprotocol: o.subprotocol === undefined ? WS_SUBPROTOCOL : o.subprotocol, handshakeTimeoutMs: timeoutMs,
            maxMessageBytes: o.maxMessageBytes ?? 1048576,
        });
        this.ws = ws;
        ws.on('message', (data, isBinary) => this._onFrame(data, isBinary));
        ws.on('close', (code, reason) => this._onClose(code, reason));
        ws.on('error', (e) => this._emit('error', e));

        const welcome = new Promise((resolve, reject) => {
            const timer = setTimeout(() => {
                this._hello = null;
                reject(new ScacelithError('client: no Welcome in time'));
                ws.terminate();
            }, timeoutMs);
            this._hello = { resolve, reject, timer, error: null };
        });
        try {
            await ws.connect();
        } catch (e) {
            const h = this._hello;
            if (h) { clearTimeout(h.timer); this._hello = null; }
            this.state = 'closed';
            throw new ScacelithError(`client: ${e.message}`, { status: e.status });
        }
        this.state = 'hello';
        this.seq = 1;
        this._send(hello);
        return welcome;
    }

    // ---- listeners ----

    /** Listens to a server message by name ('MoveMade', 'Ping' = 'S_Ping'...), 'message' (all), 'close', 'error', 'protocolError'. */
    on(name, fn) {
        const ls = this._ls || (this._ls = Object.create(null));
        const n = eventName(name);
        (ls[n] || (ls[n] = [])).push(fn);
        return this;
    }

    off(name, fn) {
        const a = this._ls && this._ls[eventName(name)];
        if (a) { const i = a.indexOf(fn); if (i >= 0) a.splice(i, 1); }
        return this;
    }

    _emit(name, a) {
        const ls = this._ls && this._ls[name];
        if (!ls) return;
        for (const fn of ls.length === 1 ? ls : ls.slice()) fn(a);
    }

    /** Number of server messages received so far: pass it as { since } to waitFor to include later ones already received. */
    mark() { return this.received; }

    /**
     * Resolves with the next server message called `name` that satisfies `predicate` ('close'
     * resolves with { code, reason }). With { since: mark } messages received after that mark
     * (still in the history) count too.
     * @param {string} name
     * @param {(msg: object) => boolean} [predicate]
     * @param {number} [timeoutMs] default 5000
     * @param {{ since?: number }} [opts]
     */
    waitFor(name, predicate = null, timeoutMs = 5000, opts = undefined) {
        const n = eventName(name);
        if (opts && opts.since !== undefined && this._hist) {
            const size = this._hist.length;
            for (let k = Math.max(opts.since + 1, this.received - size + 1); k <= this.received; k++) {
                const msg = this._hist[(k - 1) % size];
                if (messageName(msg.type) === n && (!predicate || predicate(msg))) return Promise.resolve(msg);
            }
        }
        if (n === 'close' && this.state === 'closed') return Promise.resolve({ code: this.closeCode, reason: this.closeReason });
        if (this.state === 'closed' || this.state === 'idle') {
            return Promise.reject(new ScacelithError(`client: not connected (waiting for ${n})`, { closeCode: this.closeCode, closeReason: this.closeReason }));
        }
        return new Promise((resolve, reject) => {
            const w = { name: n, predicate, resolve, reject, timer: null };
            w.timer = setTimeout(() => {
                this._removeWaiter(w);
                reject(new ScacelithError(`client: timeout waiting for ${n}`));
            }, timeoutMs);
            (this._waiters || (this._waiters = [])).push(w);
        });
    }

    _removeWaiter(w) {
        const a = this._waiters;
        const i = a ? a.indexOf(w) : -1;
        if (i >= 0) a.splice(i, 1);
    }

    // ---- sending ----

    _send(buf) {
        if (!this.ws || this.state === 'closed' || this.state === 'idle') return false;
        return this.ws.send(buf);
    }

    /**
     * Encodes and sends a client message with the next seq; returns that seq.
     * @param {string} name encode name ('Move', 'C_Ping'...)
     * @param {object} fields every field but seq
     */
    send(name, fields = {}) {
        const seq = ++this.seq;
        const enc = encode[name] ?? encode['C_' + name];
        if (!enc) throw new Error(`client: unknown message ${name}`);
        this._send(enc({ ...fields, seq }));
        return seq;
    }

    /** Sends bytes as one binary message, untouched (seq not consumed): protocol-abuse tests. */
    sendRaw(buf) { return this._send(buf); }

    /** Sends a text message (a protocol violation). */
    sendText(text) { return this._send(String(text)); }

    joinQueue(category, rated = true) { const seq = ++this.seq; this._send(encode.QueueJoin({ seq, category, rated })); return seq; }
    leaveQueue() { const seq = ++this.seq; this._send(encode.QueueLeave({ seq })); return seq; }

    /**
     * Challenges a player (target = username) or creates a private game (target = '').
     * Accepts (target, baseSec, incSec, rated, color) or one object with those names.
     */
    challenge(target, baseSec, incSec, rated = false, color = enums.ColorPref.Random) {
        if (target && typeof target === 'object') ({ target = '', baseSec, incSec, rated = false, color = enums.ColorPref.Random } = target);
        const seq = ++this.seq;
        this._send(encode.ChallengeCreate({ seq, target: target ?? '', baseSec, incSec, rated, color }));
        return seq;
    }

    createPrivateGame(baseSec, incSec, rated = false, color = enums.ColorPref.Random) { return this.challenge('', baseSec, incSec, rated, color); }
    acceptChallenge(id) { const seq = ++this.seq; this._send(encode.ChallengeAccept({ seq, id })); return seq; }
    declineChallenge(id) { const seq = ++this.seq; this._send(encode.ChallengeDecline({ seq, id })); return seq; }
    cancelChallenge(id) { const seq = ++this.seq; this._send(encode.ChallengeCancel({ seq, id })); return seq; }
    joinCode(code) { const seq = ++this.seq; this._send(encode.ChallengeJoinCode({ seq, code })); return seq; }

    /** Move intent (u16 move, posHash of the position it is played in). Returns its seq. */
    move(game, ply, move, posHash, thinkMs = 0, drawOffer = false) {
        const seq = ++this.seq;
        this._send(encode.Move({ seq, game, ply, move, posHash, thinkMs, drawOffer }));
        return seq;
    }

    resign(game) { const seq = ++this.seq; this._send(encode.Resign({ seq, game })); return seq; }
    offerDraw(game) { const seq = ++this.seq; this._send(encode.DrawOffer({ seq, game })); return seq; }
    answerDraw(game, accept) { const seq = ++this.seq; this._send(encode.DrawAnswer({ seq, game, accept })); return seq; }
    claimDraw(game) { const seq = ++this.seq; this._send(encode.DrawClaim({ seq, game })); return seq; }
    abort(game) { const seq = ++this.seq; this._send(encode.Abort({ seq, game })); return seq; }
    resync(game) { const seq = ++this.seq; this._send(encode.Resync({ seq, game })); return seq; }
    rematch(game, accept = true) { const seq = ++this.seq; this._send(encode.Rematch({ seq, game, accept })); return seq; }

    /**
     * Resolves with the Ack of request `seq`, rejects with a ScacelithError on an Error quoting it.
     * @param {number} seq
     * @param {number} [timeoutMs]
     */
    expectAck(seq, timeoutMs = 5000) {
        return new Promise((resolve, reject) => {
            if (this.state === 'closed' || this.state === 'idle') { reject(new ScacelithError('client: not connected', { closeCode: this.closeCode })); return; }
            let done = false;
            const finish = (fn, v) => {
                if (done) return;
                done = true;
                clearTimeout(timer);
                this.off('Ack', onAck);
                this.off('Error', onErr);
                this.off('close', onClose);
                fn(v);
            };
            const onAck = (m) => { if (m.ref === seq) finish(resolve, m); };
            const onErr = (m) => {
                if (m.ref === seq) finish(reject, new ScacelithError(`client: request ${seq} refused: ${ERROR_NAMES[m.code] || m.code}`, { errorCode: m.code, fatal: m.fatal, ref: m.ref }));
            };
            const onClose = (c) => finish(reject, new ScacelithError(`client: closed while waiting for the answer to ${seq}`, { closeCode: c.code, closeReason: c.reason }));
            const timer = setTimeout(() => finish(reject, new ScacelithError(`client: no answer to request ${seq}`)), timeoutMs);
            this.on('Ack', onAck);
            this.on('Error', onErr);
            this.on('close', onClose);
        });
    }

    /**
     * Measures the round trip and the server clock offset with a client Ping.
     * @returns {Promise<{ rttMs: number, offsetMs: number, serverTime: number }>}
     */
    ping(timeoutMs = 5000) {
        const nonce = this._pingNonce = (this._pingNonce + 1) >>> 0;
        const t0 = performance.now(), wall = Date.now();
        return new Promise((resolve, reject) => {
            if (this.state !== 'ready' && this.state !== 'hello') { reject(new ScacelithError('client: not connected')); return; }
            const p = { t0, wall, resolve, reject, timer: null };
            p.timer = setTimeout(() => { this._pings.delete(nonce); reject(new ScacelithError('client: no Pong in time')); }, timeoutMs);
            (this._pings || (this._pings = new Map())).set(nonce, p);
            this._send(encode.C_Ping({ seq: ++this.seq, nonce }));
        });
    }

    /** Estimate of the server's clock (epoch ms). */
    serverNow() { return Date.now() + this.clockOffsetMs; }

    /** Closing handshake; resolves when the connection is closed. */
    close(code = 1000, reason = '') {
        if (!this.ws || this.state === 'closed' || this.state === 'idle') return Promise.resolve();
        return this.ws.close(code, reason);
    }

    /** Drops the TCP connection without a closing handshake (network loss simulation). */
    terminate() { if (this.ws) this.ws.terminate(); }

    // ---- receiving ----

    _onFrame(data, isBinary) {
        if (!isBinary) { this._protocolError(new ProtocolError('text message'), data, 1003); return; }
        let msg;
        try {
            msg = decode(data, DECODE_S2C);
        } catch (e) {
            if (!(e instanceof ProtocolError)) throw e;
            this._protocolError(e, data, 1002);
            return;
        }
        this.received++;
        if (this._hist) this._hist[(this.received - 1) % this._hist.length] = msg;
        this._apply(msg);
        const name = messageName(msg.type);
        this._emit(name, msg);
        this._emit('message', msg);
        const ws = this._waiters;
        if (ws && ws.length) {
            for (const w of ws.slice()) {
                if (w.name !== name) continue;
                let ok = true;
                if (w.predicate) { try { ok = w.predicate(msg); } catch (e) { ok = false; this._removeWaiter(w); clearTimeout(w.timer); w.reject(e); continue; } }
                if (!ok) continue;
                this._removeWaiter(w);
                clearTimeout(w.timer);
                w.resolve(msg);
            }
        }
    }

    _protocolError(err, data, closeCode) {
        this._emit('protocolError', { error: err, data: Buffer.from(data) });
        if (this.ws && this.ws.readyState === 1) this.ws.close(closeCode, err.reason || 'protocol error');
    }

    _apply(msg) {
        switch (msg.type) {
            case T.Welcome: {
                this.welcome = msg;
                this.userId = msg.userId;
                this.username = msg.username;
                this.serverName = msg.serverName;
                this.activeGame = msg.activeGame;
                this.clockOffsetMs = msg.serverTime - Date.now();
                this.state = 'ready';
                const h = this._hello;
                if (h) { this._hello = null; clearTimeout(h.timer); h.resolve(msg); }
                break;
            }
            case T.Error: {
                this.lastError = msg;
                const h = this._hello;
                if (h) {
                    const err = new ScacelithError(`client: server refused the connection: ${ERROR_NAMES[msg.code] || msg.code}`, { errorCode: msg.code, fatal: msg.fatal, ref: msg.ref });
                    // A fatal error is followed by the close: reject then, with both codes.
                    if (msg.fatal) h.error = err;
                    else { this._hello = null; clearTimeout(h.timer); h.reject(err); }
                }
                break;
            }
            case T.S_Ping:
                if (this.opts.autoPong !== false) this._send(encode.C_Pong({ seq: ++this.seq, nonce: msg.nonce }));
                break;
            case T.S_Pong: {
                const p = this._pings && this._pings.get(msg.nonce);
                if (p) {
                    this._pings.delete(msg.nonce);
                    clearTimeout(p.timer);
                    const rttMs = performance.now() - p.t0;
                    const offsetMs = msg.serverTime - (p.wall + rttMs / 2);
                    this.rttMs = rttMs;
                    this.clockOffsetMs = offsetMs;
                    p.resolve({ rttMs, offsetMs, serverTime: msg.serverTime });
                }
                break;
            }
            case T.QueueStatus: this.queue = msg; break;
            case T.ChallengeReceived: case T.ChallengeStatus: this.challenges.set(msg.id, msg); break;
            default:
                if (this.opts.trackGames !== false) this._applyGame(msg);
        }
    }

    // Light authoritative view of each game: the last GameSnapshot with the later MoveMade,
    // GameEvent, GameEnd and RatingUpdate applied (standard start: White moves at even plies).
    _applyGame(msg) {
        if (msg.type === T.GameSnapshot) {
            const prev = this.games.get(msg.game);
            const { type, game, ...rest } = msg;
            const view = { id: game, ...rest, moves: msg.moves.slice(), ply: msg.moves.length, lastRejected: null, desync: false, ratings: prev ? prev.ratings : null };
            this.games.set(game, view);
            this.currentGame = game;
            return;
        }
        const g = msg.game !== undefined ? this.games.get(msg.game) : undefined;
        if (!g) return;
        switch (msg.type) {
            case T.MoveMade: {
                if (msg.gseq > g.gseq) g.gseq = msg.gseq;
                if (msg.ply < g.moves.length) return;                      // duplicate (idempotent resend)
                if (msg.ply > g.moves.length) { g.desync = true; return; }  // missed a move: resync
                const mover = msg.ply & 1 ? Color.Black : Color.White;
                g.moves.push({ move: msg.move, spentMs: msg.spentMs, clockMs: mover === Color.White ? msg.whiteMs : msg.blackMs });
                g.ply = g.moves.length;
                g.whiteMs = msg.whiteMs;
                g.blackMs = msg.blackMs;
                g.serverTime = msg.serverTime;
                g.firstMoveMs = msg.firstMoveMs;
                g.drawOffer = msg.drawOffer ? mover : Color.None;
                g.running = msg.firstMoveMs > 0 ? Color.None : mover ^ 1;
                g.lastMove = msg;
                break;
            }
            case T.MoveRejected: g.lastRejected = msg; break;
            case T.GameEvent:
                if (msg.gseq > g.gseq) g.gseq = msg.gseq;
                switch (msg.kind) {
                    case GameEventKind.DrawOffered: g.drawOffer = msg.color; break;
                    case GameEventKind.DrawDeclined: g.drawOffer = Color.None; break;
                    case GameEventKind.PlayerDisconnected:
                        if (msg.color === Color.White) g.whiteConnected = false; else if (msg.color === Color.Black) g.blackConnected = false;
                        g.graceMs = msg.arg;
                        break;
                    case GameEventKind.PlayerReconnected:
                        if (msg.color === Color.White) g.whiteConnected = true; else if (msg.color === Color.Black) g.blackConnected = true;
                        break;
                    case GameEventKind.RematchOffered: g.rematch = msg.color; break;
                    case GameEventKind.RematchDeclined: g.rematch = Color.None; break;
                }
                break;
            case T.GameEnd:
                if (msg.gseq > g.gseq) g.gseq = msg.gseq;
                g.status = msg.status;
                g.reason = msg.reason;
                g.whiteMs = msg.whiteMs;
                g.blackMs = msg.blackMs;
                g.serverTime = msg.serverTime;
                g.running = Color.None;
                g.drawOffer = Color.None;
                break;
            case T.RatingUpdate: g.ratings = { category: msg.category, white: msg.white, black: msg.black }; break;
        }
    }

    _onClose(code, reason) {
        this.state = 'closed';
        this.closeCode = code;
        this.closeReason = reason;
        const h = this._hello;
        if (h) {
            this._hello = null;
            clearTimeout(h.timer);
            const base = h.error;
            const err = new ScacelithError(base ? base.message : `client: connection closed before Welcome (${code}${reason ? ' ' + reason : ''})`,
                { errorCode: base ? base.errorCode : 0, fatal: base ? base.fatal : false, closeCode: code, closeReason: reason, ref: base ? base.ref : 0 });
            h.reject(err);
        }
        if (this._pings) {
            for (const p of this._pings.values()) { clearTimeout(p.timer); p.reject(new ScacelithError('client: closed', { closeCode: code, closeReason: reason })); }
            this._pings.clear();
        }
        const ws = this._waiters;
        this._waiters = null;
        if (ws) {
            for (const w of ws) {
                clearTimeout(w.timer);
                if (w.name === 'close') w.resolve({ code, reason });
                else w.reject(new ScacelithError(`client: closed while waiting for ${w.name} (${code}${reason ? ' ' + reason : ''})`, { closeCode: code, closeReason: reason }));
            }
        }
        this._emit('close', { code, reason });
    }
}

/** True when the game view is over (any status but Ongoing). */
export function isOver(view) { return !!view && view.status !== GameStatus.Ongoing; }
