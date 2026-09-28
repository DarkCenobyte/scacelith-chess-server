// One load process of bench/loadgen.js. It owns a slice of the accounts, opens their WSS
// connections (raw RFC 6455 over node:tls, one shared SecureContext, a precomputed upgrade
// request), answers the server heartbeat, and in the game scenarios plays legal random moves with
// the correct posHash. Everything it measures goes to the coordinator once a second as deltas
// (counters, sparse histograms, gauges); the coordinator merges the processes.
//
// Lean on purpose, so that the load generator is not the bottleneck:
//   - shared handler functions (no closure per socket), one fixed-shape object per client;
//   - hot messages (Move, Pong, MoveMade, Ping) through the fixed-offset fast path of wire.js,
//     everything else through the generated codec;
//   - think times on one timing wheel (5 ms slots) instead of a timer per move; with no think time
//     (burst) the replies of one event-loop turn are sent together from one setImmediate;
//   - both players of a direct-challenge game live in the same process and share one Position.
//
// Commands from the coordinator (process.send): init, connect, games, stop, close, exit.

import crypto from 'node:crypto';
import tls from 'node:tls';
import { monitorEventLoopDelay, performance } from 'node:perf_hooks';
import { Position } from '../../src/chess/index.js';
import { Hist } from './hist.js';
import {
    T, encode, decode, enums, SCHEMA_HASH, PROTOCOL_VERSION, WS_SUBPROTOCOL,
    clientFrame, closeFrame, moveFrame, pongFrame, pingFrame, readMoveMade, readNonce, selfCheck,
} from './wire.js';

const E = enums.ErrorCode;
const ERR_NAME = Object.fromEntries(Object.entries(E).map(([k, v]) => [v, k]));
const REASON_NAME = Object.fromEntries(Object.entries(enums.EndReason).map(([k, v]) => [v, k]));
const CS = enums.ChallengeState;

// Client states.
const S_IDLE = 0, S_CONNECTING = 1, S_UPGRADING = 2, S_HELLO = 3, S_READY = 4, S_CLOSED = 5;
// Pair states (direct challenges).
const P_IDLE = 0, P_WAIT = 1, P_CHALLENGING = 2, P_PLAYING = 3, P_DEAD = 4;
// Request kinds (Ack / Error correlation).
const R_CREATE = 1, R_ACCEPT = 2, R_QUEUE = 3, R_CANCEL = 4, R_OTHER = 5;
// Timer kinds.
const K_MOVE = 1, K_PING = 2, K_START = 3, K_ACCEPT = 4, K_QUEUE = 5;

let cfg = null;
let secureContext = null;
let upgradeReq = null;
let helloClient = 'scacelith-bench/1';
let tlsSession = null;
const clients = [];
const pairs = [];
const games = new Map();
let stopping = false;
let closing = false;
let gamesOn = false;

const now = () => performance.now();

// ---- statistics -----------------------------------------------------------------------------------

const newCounters = () => ({
    connStarted: 0, connOk: 0, connFail: 0, dropped: 0, closedByUs: 0, helloSent: 0,
    movesSent: 0, movesOk: 0, movesRejected: 0, resigns: 0, plies: 0, strayMoves: 0, resyncs: 0,
    gamesStarted: 0, gamesEnded: 0, snapshots: 0, challenges: 0, challengeRetries: 0, accepts: 0, queueJoins: 0,
    pingsSent: 0, pongs: 0, serverPings: 0, errors: 0, notices: 0, ratingUpdates: 0, writesBlocked: 0, decodeErrors: 0,
    gameEvents: 0, unexpected: 0,
});
let cnt = newCounters();
let maps = { fail: {}, rejected: {}, errors: {}, closes: {}, ends: {}, notices: {} };
const hist = { connect: new Hist(), hello: new Hist(), move: new Hist(), hb: new Hist(), start: new Hist() };
const inc = (m, k) => { m[k] = (m[k] || 0) + 1; };

const lag = monitorEventLoopDelay({ resolution: 10 });
lag.enable();
let lastCpu = process.cpuUsage();
let lastElu = performance.eventLoopUtilization();
let lastReportAt = now();

function gauges() {
    let open = 0, ready = 0, connecting = 0, whitePlaying = 0, pending = 0;
    for (const c of clients) {
        if (c.state === S_READY) ready++;
        if (c.state >= S_CONNECTING && c.state <= S_HELLO) connecting++;
        if (c.state !== S_IDLE && c.state !== S_CLOSED) open++;
        if (c.game && !c.game.over && c.color === 0) whitePlaying++;
        if (c.pendingPly >= 0) pending++;
    }
    return { open, ready, connecting, whitePlaying, pendingMoves: pending, localGames: games.size };
}

function report(final = false) {
    const t = now();
    const dt = (t - lastReportAt) / 1000;
    lastReportAt = t;
    const cpu = process.cpuUsage(lastCpu);
    lastCpu = process.cpuUsage();
    const elu = performance.eventLoopUtilization(lastElu);
    lastElu = performance.eventLoopUtilization();
    const mem = process.memoryUsage();
    const msg = {
        type: 'stats', proc: cfg.proc, final, dt,
        c: cnt, m: maps,
        h: Object.fromEntries(Object.entries(hist).map(([k, h]) => [k, h.toSparse()])),
        g: {
            ...gauges(),
            cpu: dt > 0 ? (cpu.user + cpu.system) / 1e6 / dt : 0,
            elu: elu.utilization,
            lagP99: lag.count ? lag.percentile(99) / 1e6 : 0,
            lagMax: lag.count ? lag.max / 1e6 : 0,
            rss: mem.rss, heap: mem.heapUsed,
        },
    };
    cnt = newCounters();
    maps = { fail: {}, rejected: {}, errors: {}, closes: {}, ends: {}, notices: {} };
    for (const h of Object.values(hist)) h.reset();
    lag.reset();
    process.send(msg);
}

// ---- timing wheel ---------------------------------------------------------------------------------

const SLOT_MS = 5;
const NSLOTS = 4096;
const slots = Array.from({ length: NSLOTS }, () => []);
let wheelCur = Math.floor(now() / SLOT_MS);

function schedule(atMs, c, kind, gen) {
    let s = Math.floor(atMs / SLOT_MS);
    if (s <= wheelCur) s = wheelCur + 1;
    slots[s % NSLOTS].push(c, kind, gen, atMs);
}

function wheelTick() {
    const target = Math.floor(now() / SLOT_MS);
    while (wheelCur < target) {
        wheelCur++;
        const a = slots[wheelCur % NSLOTS];
        if (!a.length) continue;
        slots[wheelCur % NSLOTS] = [];
        const t = wheelCur * SLOT_MS + SLOT_MS;
        for (let i = 0; i < a.length; i += 4) {
            if (a[i + 3] > t) { schedule(a[i + 3], a[i], a[i + 1], a[i + 2]); continue; }   // more than one lap ahead
            fire(a[i], a[i + 1], a[i + 2]);
        }
    }
}

function fire(c, kind, gen) {
    switch (kind) {
        case K_MOVE: doMove(c, gen); return;
        case K_PING: doPing(c); return;
        case K_START: if (c.pair && c.pair.gen === gen) pairStart(c.pair); return;
        case K_ACCEPT: if (c.pair && c.pair.gen === gen) accept(c, c.pair.challengeId); return;
        case K_QUEUE: if (c.gen2 === gen) queueJoin(c); return;
        default:
    }
}

// Moves without think time: sent together once the current I/O callbacks are done (a GameEnd
// that arrived in the same chunk as the last MoveMade is then seen first).
let soon = [];
let soonScheduled = false;
function flushSoon() {
    soonScheduled = false;
    const a = soon;
    soon = [];
    for (let i = 0; i < a.length; i += 2) doMove(a[i], a[i + 1]);
}

// ---- client -----------------------------------------------------------------------------------------

class Client {
    constructor(i, acct) {
        this.i = i;
        this.token = acct.t;
        this.username = acct.u || '';
        this.userId = 0;
        this.sock = null;
        this.state = S_IDLE;
        this.rbuf = null;
        this.seq = 0;
        this.mask = crypto.randomBytes(4);
        this.t0 = 0;
        this.tHello = 0;
        this.err = '';
        this.closeCode = 0;
        this.game = null;
        this.color = -1;
        this.turnAt = 0;
        this.gen = 0;         // invalidates scheduled moves
        this.gen2 = 0;        // invalidates scheduled queue joins
        this.pendingPly = -1;
        this.sentAt = 0;
        this.pingNonce = 0;
        this.pingAt = 0;
        this.reqs = null;     // Map seq -> kind
        this.pair = null;
        this.queuedAt = 0;
        this.acceptTries = 0;
    }
}

function send(c, frame) {
    if (c.state === S_CLOSED || !c.sock) return;
    if (!c.sock.write(frame)) cnt.writesBlocked++;
}

function req(c, seq, kind) {
    if (!c.reqs) c.reqs = new Map();
    c.reqs.set(seq, kind);
    if (c.reqs.size > 64) c.reqs.delete(c.reqs.keys().next().value);
}

function connect(c) {
    c.state = S_CONNECTING;
    c.t0 = now();
    c.rbuf = null;
    cnt.connStarted++;
    const o = { host: cfg.host, port: cfg.port, secureContext, servername: cfg.servername || undefined };
    if (cfg.localAddress) o.localAddress = cfg.localAddress;
    if (cfg.tlsResume && tlsSession) o.session = tlsSession;
    const s = tls.connect(o);
    s._c = c;
    c.sock = s;
    s.setNoDelay(true);
    s.on('secureConnect', onSecure);
    s.on('data', onData);
    s.on('error', onError);
    s.on('close', onClose);
    if (cfg.tlsResume && !tlsSession) s.once('session', onSession);
}

function onSession(sess) { if (!tlsSession) tlsSession = sess; }
function onSecure() {
    const c = this._c;
    if (c.state !== S_CONNECTING) return;
    c.state = S_UPGRADING;
    this.write(upgradeReq);
}
function onData(chunk) {
    const c = this._c;
    if (c.state === S_UPGRADING) upgradeData(c, chunk);
    else if (c.state === S_HELLO || c.state === S_READY) frames(c, chunk);
}
function onError(e) { const c = this._c; if (!c.err) c.err = e.code || e.message || 'error'; }
function onClose() { closed(this._c); }

let inflight = 0;

function closed(c) {
    const prev = c.state;
    if (prev === S_CLOSED) return;
    c.state = S_CLOSED;
    c.sock = null;
    c.rbuf = null;
    if (prev < S_READY) {
        inflight--;
        cnt.connFail++;
        inc(maps.fail, c.err || (c.closeCode ? `close_${c.closeCode}` : prev === S_UPGRADING ? 'closed_during_upgrade' : prev === S_HELLO ? 'closed_before_welcome' : 'closed'));
    } else if (closing) {
        cnt.closedByUs++;
    } else {
        cnt.dropped++;
        inc(maps.closes, String(c.closeCode || 1006));
    }
    c.game = null;
    c.pendingPly = -1;
    if (c.pair) c.pair.state = P_DEAD;
    pumpRamp();
}

function fail(c, reason) {
    if (!c.err) c.err = reason;
    if (c.sock) c.sock.destroy();
}

function upgradeData(c, chunk) {
    const b = c.rbuf ? Buffer.concat([c.rbuf, chunk]) : chunk;
    const end = b.indexOf('\r\n\r\n');
    if (end < 0) {
        if (b.length > 8192) fail(c, 'bad_upgrade_response');
        else c.rbuf = b;
        return;
    }
    c.rbuf = null;
    // "HTTP/1.1 101 ..."
    if (!(b[9] === 0x31 && b[10] === 0x30 && b[11] === 0x31)) { fail(c, `http_${b.toString('latin1', 9, 12)}`); return; }
    if (!cfg.acceptChecked) {
        const head = b.toString('latin1', 0, end).toLowerCase();
        if (!head.includes(`sec-websocket-accept: ${cfg.expectedAccept.toLowerCase()}`)) { fail(c, 'bad_accept'); return; }
        cfg.acceptChecked = true;
    }
    const t = now();
    hist.connect.add((t - c.t0) * 1000);
    c.state = S_HELLO;
    c.tHello = t;
    c.seq = 1;
    send(c, clientFrame(encode.Hello({ seq: 1, proto: PROTOCOL_VERSION, schema: SCHEMA_HASH, client: helloClient, token: c.token }), c.mask));
    cnt.helloSent++;
    if (end + 4 < b.length) frames(c, b.subarray(end + 4));
}

function frames(c, chunk) {
    let b = chunk;
    if (c.rbuf) { b = Buffer.concat([c.rbuf, chunk]); c.rbuf = null; }
    const n = b.length;
    let o = 0;
    while (n - o >= 2) {
        let len = b[o + 1] & 0x7f, h = 2;
        if (len === 126) { if (n - o < 4) break; len = b.readUInt16BE(o + 2); h = 4; }
        else if (len === 127) { if (n - o < 10) break; len = b.readUInt32BE(o + 6); h = 10; }
        if (b[o + 1] & 0x80) h += 4;                     // servers never mask
        if (n - o < h + len) break;
        const op = b[o] & 0x0f;
        const p = o + h;
        o = p + len;
        if (op === 2) {
            message(c, b, p, len);
            if (c.state === S_CLOSED) return;
        } else if (op === 8) {
            c.closeCode = len >= 2 ? b.readUInt16BE(p) : 1005;
            if (c.sock) { try { c.sock.end(closeFrame(c.mask)); } catch { /* gone */ } }
            return;
        } else if (op === 9) {
            const payload = Buffer.from(b.subarray(p, p + len));
            const f = clientFrame(payload, c.mask);
            f[0] = 0x8a;                                  // pong
            send(c, f);
        }
    }
    if (o < n) c.rbuf = Buffer.from(b.subarray(o));
}

const MM = { game: 0, ply: 0, move: 0 };

function message(c, b, p, len) {
    const type = b[p];
    if (type === T.MoveMade) { onMoveMade(c, b, p, len); return; }
    if (type === T.S_Ping) {
        cnt.serverPings++;
        send(c, pongFrame(++c.seq, readNonce(b, p, len, 'S_Ping'), c.mask));
        return;
    }
    if (type === T.S_Pong) {
        const nonce = readNonce(b, p, len, 'S_Pong');
        if (c.pingAt && nonce === c.pingNonce) { hist.hb.add((now() - c.pingAt) * 1000); c.pingAt = 0; cnt.pongs++; }
        return;
    }
    let m;
    try {
        m = decode(b.subarray(p, p + len), { dir: 's2c' });
    } catch {
        cnt.decodeErrors++;
        return;
    }
    switch (type) {
        case T.Welcome: onWelcome(c, m); return;
        case T.Error: onErrorMsg(c, m); return;
        case T.Ack: if (c.reqs) c.reqs.delete(m.ref); return;
        case T.Notice: cnt.notices++; inc(maps.notices, String(m.code)); return;
        case T.QueueStatus: return;
        case T.ChallengeReceived: onChallengeReceived(c, m); return;
        case T.ChallengeStatus: onChallengeStatus(c, m); return;
        case T.GameSnapshot: onSnapshot(c, m); return;
        case T.MoveRejected: onMoveRejected(c, m); return;
        case T.GameEvent: cnt.gameEvents++; return;
        case T.GameEnd: onGameEnd(c, m); return;
        case T.RatingUpdate: cnt.ratingUpdates++; return;
        default: cnt.unexpected++;
    }
}

function onWelcome(c, m) {
    if (c.state !== S_HELLO) return;
    c.state = S_READY;
    inflight--;
    c.userId = m.userId;
    c.username = m.username;
    hist.hello.add((now() - c.tHello) * 1000);
    cnt.connOk++;
    if (cfg.pingIntervalMs > 0) schedule(now() + Math.random() * cfg.pingIntervalMs, c, K_PING, 0);
    pumpRamp();
}

function onErrorMsg(c, m) {
    cnt.errors++;
    const name = ERR_NAME[m.code] || String(m.code);
    if (c.state === S_HELLO) { c.err = `error_${name}`; return; }    // fatal: the server closes
    const kind = c.reqs ? c.reqs.get(m.ref) : undefined;
    if (kind !== undefined) c.reqs.delete(m.ref);
    inc(maps.errors, `${kindName(kind)}:${name}`);
    if (!gamesOn || stopping) return;
    const pair = c.pair;
    switch (kind) {
        case R_CREATE:
            if (pair && pair.state === P_CHALLENGING) retryPair(pair, 1000);
            return;
        case R_ACCEPT:
            if (pair && pair.state === P_CHALLENGING) {
                if (m.code === E.AlreadyInGame && c.acceptTries < 40) { schedule(now() + 100, c, K_ACCEPT, pair.gen); return; }
                cancelChallenge(pair);
                retryPair(pair, 1000);
            }
            return;
        case R_QUEUE:
            c.gen2++;
            schedule(now() + 1000, c, K_QUEUE, c.gen2);
            return;
        default:
    }
}

function kindName(k) {
    switch (k) {
        case R_CREATE: return 'challengeCreate';
        case R_ACCEPT: return 'challengeAccept';
        case R_QUEUE: return 'queueJoin';
        case R_CANCEL: return 'challengeCancel';
        case R_OTHER: return 'other';
        default: return 'unsolicited';
    }
}

function doPing(c) {
    if (c.state !== S_READY || closing) return;
    c.pingNonce = (c.pingNonce + 1) >>> 0;
    c.pingAt = now();
    send(c, pingFrame(++c.seq, c.pingNonce, c.mask));
    cnt.pingsSent++;
    schedule(now() + cfg.pingIntervalMs, c, K_PING, 0);
}

// ---- ramp -------------------------------------------------------------------------------------------

let rampNext = 0;
let rampStart = 0;
let rampTimer = null;

function pumpRamp() {
    if (!rampStart || closing) return;
    let allowed = cfg.rate > 0 ? Math.floor((now() - rampStart) / 1000 * cfg.rate) - rampNext : Infinity;
    while (rampNext < clients.length && inflight < cfg.inflight && allowed-- > 0) {
        inflight++;
        connect(clients[rampNext++]);
    }
    if (rampNext >= clients.length && rampTimer) { clearInterval(rampTimer); rampTimer = null; }
}

// Handshakes that never finish (lost SYN, stuck TLS, no Welcome) count as failures.
function sweepConnecting() {
    const t = now();
    for (const c of clients) {
        if (c.state >= S_CONNECTING && c.state <= S_HELLO && t - c.t0 > cfg.connectTimeoutMs) fail(c, 'timeout');
    }
}

// ---- games ------------------------------------------------------------------------------------------

function think() {
    const base = cfg.moveIntervalMs;
    if (base <= 0) return 0;
    const j = cfg.jitter;
    return base * (1 - j + 2 * j * Math.random());
}

function turn(c) {
    c.turnAt = now();
    c.gen++;
    if (stopping) return;
    if (cfg.moveIntervalMs <= 0) {
        soon.push(c, c.gen);
        if (!soonScheduled) { soonScheduled = true; setImmediate(flushSoon); }
    } else {
        schedule(now() + think(), c, K_MOVE, c.gen);
    }
}

function doMove(c, gen) {
    if (gen !== c.gen || c.state !== S_READY || stopping) return;
    const g = c.game;
    if (!g || g.over || g.pos.side !== c.color) return;
    if ((cfg.maxPlies > 0 && g.ply >= cfg.maxPlies) || g.ply >= 1190) {
        send(c, clientFrame(encode.Resign({ seq: ++c.seq, game: g.id }), c.mask));
        cnt.resigns++;
        return;
    }
    const moves = g.pos.legalMoves();
    if (!moves.length) return;                         // mate or stalemate: GameEnd is on its way
    const mv = moves[(Math.random() * moves.length) | 0];
    const t = now();
    send(c, moveFrame(++c.seq, g.id, g.ply, mv, g.pos.digest(), Math.max(0, Math.round(t - c.turnAt)), c.mask));
    c.pendingPly = g.ply;
    c.sentAt = t;
    cnt.movesSent++;
}

function onMoveMade(c, b, p, len) {
    readMoveMade(b, p, len, MM);
    const g = c.game;
    if (!g || g.id !== MM.game) { cnt.strayMoves++; return; }
    if (MM.ply === g.ply) {
        try { g.pos.play(MM.move); } catch { resync(c, g); return; }
        g.ply++;
        cnt.plies++;
    } else if (MM.ply > g.ply) {
        resync(c, g);
        return;
    }
    if (c.pendingPly === MM.ply) {
        hist.move.add((now() - c.sentAt) * 1000);
        c.pendingPly = -1;
        cnt.movesOk++;
    }
    if (!g.over && g.pos.side === c.color) turn(c);
}

function resync(c, g) {
    cnt.resyncs++;
    send(c, clientFrame(encode.Resync({ seq: ++c.seq, game: g.id }), c.mask));
}

function onMoveRejected(c, m) {
    cnt.movesRejected++;
    inc(maps.rejected, ERR_NAME[m.code] || String(m.code));
    if (c.pendingPly === m.ply) c.pendingPly = -1;
    // Desync: a GameSnapshot follows and rebuilds the position.
}

function onSnapshot(c, m) {
    cnt.snapshots++;
    let g = games.get(m.game);
    if (!g) {
        g = { id: m.game, pos: Position.start(), ply: 0, over: false };
        games.set(m.game, g);
    }
    if (m.you === 0 && c.game !== g) cnt.gamesStarted++;     // White's first snapshot of this game
    if (g.ply !== m.moves.length) {
        g.pos = Position.start();
        for (const r of m.moves) g.pos.play(r.move);
        g.ply = m.moves.length;
    }
    c.game = g;
    c.color = m.you;
    c.pendingPly = -1;
    c.acceptTries = 0;
    if (c.queuedAt) { hist.start.add((now() - c.queuedAt) * 1000); c.queuedAt = 0; }
    const pair = c.pair;
    if (pair && pair.state === P_CHALLENGING && ++pair.snaps === 2) {
        pair.state = P_PLAYING;
        pair.gameId = m.game;
        hist.start.add((now() - pair.t0) * 1000);
    }
    if (m.status !== enums.GameStatus.Ongoing) { endLocal(c, g, m.reason); return; }
    if (g.pos.side === c.color) turn(c);
}

function endLocal(c, g, reason) {
    if (!g.over) {
        g.over = true;
        games.delete(g.id);
    }
    if (c.color === 0) { cnt.gamesEnded++; inc(maps.ends, REASON_NAME[reason] || String(reason)); }
    c.game = null;
    c.pendingPly = -1;
    c.gen++;
}

function onGameEnd(c, m) {
    const g = c.game;
    if (!g || g.id !== m.game) return;
    endLocal(c, g, m.reason);
    if (!gamesOn || stopping) return;
    if (cfg.via === 'queue') {
        c.gen2++;
        schedule(now() + cfg.betweenGamesMs * (0.5 + Math.random()), c, K_QUEUE, c.gen2);
        return;
    }
    const pair = c.pair;
    if (pair && pair.state === P_PLAYING && pair.gameId === m.game) {
        pair.n++;
        pair.state = P_WAIT;
        pair.gen++;
        schedule(now() + cfg.betweenGamesMs * (0.5 + Math.random()), pair.a, K_START, pair.gen);
    }
}

function pairStart(pair) {
    if (stopping || closing) return;
    if (pair.a.state !== S_READY || pair.b.state !== S_READY) { pair.state = P_DEAD; return; }
    const ch = pair.n % 2 === 0 ? pair.a : pair.b;
    const tg = ch === pair.a ? pair.b : pair.a;
    pair.state = P_CHALLENGING;
    pair.gen++;
    pair.t0 = now();
    pair.snaps = 0;
    pair.challengeId = 0;
    pair.challenger = ch;
    tg.acceptTries = 0;
    const seq = ++ch.seq;
    const [base, inc2] = cfg.tc;
    send(ch, clientFrame(encode.ChallengeCreate({ seq, target: tg.username, baseSec: base, incSec: inc2, rated: cfg.rated, color: enums.ColorPref.Random }), ch.mask));
    req(ch, seq, R_CREATE);
    cnt.challenges++;
}

function retryPair(pair, delay) {
    pair.state = P_WAIT;
    pair.gen++;
    cnt.challengeRetries++;
    schedule(now() + delay * (0.5 + Math.random()), pair.a, K_START, pair.gen);
}

function cancelChallenge(pair) {
    const ch = pair.challenger;
    if (!ch || !pair.challengeId || ch.state !== S_READY) return;
    const seq = ++ch.seq;
    send(ch, clientFrame(encode.ChallengeCancel({ seq, id: pair.challengeId }), ch.mask));
    req(ch, seq, R_CANCEL);
}

function accept(c, id) {
    if (c.state !== S_READY || !id) return;
    c.acceptTries++;
    const seq = ++c.seq;
    send(c, clientFrame(encode.ChallengeAccept({ seq, id }), c.mask));
    req(c, seq, R_ACCEPT);
    cnt.accepts++;
}

function onChallengeReceived(c, m) {
    const pair = c.pair;
    const partner = pair ? (pair.a === c ? pair.b : pair.a) : null;
    if (pair && pair.state === P_CHALLENGING && partner && m.from.userId === partner.userId) {
        pair.challengeId = m.id;
        accept(c, m.id);
        return;
    }
    cnt.unexpected++;
    const seq = ++c.seq;
    send(c, clientFrame(encode.ChallengeDecline({ seq, id: m.id }), c.mask));
    req(c, seq, R_OTHER);
}

function onChallengeStatus(c, m) {
    const pair = c.pair;
    if (!pair || pair.challenger !== c) return;
    if (m.state === CS.Pending) { if (pair.state === P_CHALLENGING) pair.challengeId = m.id; return; }
    if (m.state === CS.Accepted) return;
    if (pair.state === P_CHALLENGING && m.id === pair.challengeId) {
        inc(maps.errors, `challengeStatus:${m.state}`);
        retryPair(pair, 1000);
    }
}

function queueJoin(c) {
    if (c.state !== S_READY || stopping || closing || c.game) return;
    const seq = ++c.seq;
    const [base, inc2] = cfg.tc;
    send(c, clientFrame(encode.QueueJoin({ seq, category: `${base / 60}+${inc2}`, rated: cfg.rated }), c.mask));
    req(c, seq, R_QUEUE);
    c.queuedAt = now();
    cnt.queueJoins++;
}

// Game starts are spread at cfg.startRate per second (this process's share).
let startQueue = [];
let startHead = 0;
let startBegin = 0;
let startDone = 0;
let startTimer = null;

function pumpStarts() {
    const due = cfg.startRate > 0 ? Math.floor((now() - startBegin) / 1000 * cfg.startRate) : Infinity;
    while (startHead < startQueue.length && startDone < due) {
        const x = startQueue[startHead++];
        startDone++;
        if (cfg.via === 'queue') queueJoin(x);
        else pairStart(x);
    }
    if (startHead >= startQueue.length) { clearInterval(startTimer); startTimer = null; startQueue = []; }
}

function startGames() {
    gamesOn = true;
    if (cfg.via === 'queue') startQueue = clients.filter((c) => c.state === S_READY);
    else startQueue = pairs.filter((p) => p.a.state === S_READY && p.b.state === S_READY);
    startHead = 0;
    startDone = 0;
    startBegin = now();
    startTimer = setInterval(pumpStarts, 5);
    pumpStarts();
}

// ---- lifecycle --------------------------------------------------------------------------------------

function closeAll() {
    closing = true;
    stopping = true;
    for (const c of clients) {
        if (!c.sock) continue;
        if (c.state === S_READY || c.state === S_HELLO) {
            try { c.sock.end(closeFrame(c.mask)); } catch { c.sock.destroy(); }
        } else {
            c.sock.destroy();
        }
    }
    const t0 = now();
    const iv = setInterval(() => {
        const open = clients.some((c) => c.sock);
        if (!open || now() - t0 > 5000) {
            clearInterval(iv);
            for (const c of clients) if (c.sock) c.sock.destroy();
            setTimeout(() => { report(true); process.send({ type: 'closed', proc: cfg.proc }); }, 50);
        }
    }, 100);
}

process.on('message', (msg) => {
    switch (msg.cmd) {
        case 'init': {
            cfg = msg.cfg;
            cfg.tc = cfg.tc || [180, 2];
            const check = selfCheck();
            secureContext = cfg.ca ? tls.createSecureContext({ ca: cfg.ca }) : tls.createSecureContext({});
            const key = crypto.randomBytes(16).toString('base64');
            cfg.expectedAccept = crypto.createHash('sha1').update(`${key}258EAFA5-E914-47DA-95CA-C5AB0DC85B11`).digest('base64');
            upgradeReq = Buffer.from(`GET ${cfg.path} HTTP/1.1\r\nHost: ${cfg.hostHeader}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n`
                + `Sec-WebSocket-Key: ${key}\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Protocol: ${WS_SUBPROTOCOL}\r\n\r\n`, 'latin1');
            helloClient = cfg.clientName || helloClient;
            for (let i = 0; i < msg.accounts.length; i++) clients.push(new Client(i, msg.accounts[i]));
            if (cfg.scenario !== 'connect' && cfg.via !== 'queue') {
                for (let i = 0; i + 1 < clients.length; i += 2) {
                    const p = { a: clients[i], b: clients[i + 1], state: P_IDLE, gen: 0, n: 0, t0: 0, snaps: 0, challengeId: 0, challenger: null, gameId: 0 };
                    clients[i].pair = p;
                    clients[i + 1].pair = p;
                    pairs.push(p);
                }
            }
            setInterval(wheelTick, SLOT_MS);
            setInterval(sweepConnecting, 1000);
            setInterval(() => report(false), cfg.reportMs || 1000);
            process.send({ type: 'ready', proc: cfg.proc, fastPath: check });
            return;
        }
        case 'connect':
            rampStart = now();
            rampTimer = setInterval(pumpRamp, 5);
            pumpRamp();
            return;
        case 'games':
            startGames();
            return;
        case 'stop':
            stopping = true;
            return;
        case 'close':
            closeAll();
            return;
        case 'exit':
            process.exit(0);
            return;
        default:
    }
});

process.on('disconnect', () => process.exit(0));
