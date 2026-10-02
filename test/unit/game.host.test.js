import test from 'node:test';
import assert from 'node:assert/strict';
import { createAnticheat } from '../../src/anticheat/index.js';
import { GameHost } from '../../src/game/host.js';
import { GameRoom, JournalKind, RecordFlag, REMATCH_WINDOW_MS } from '../../src/game/room.js';
import { FakeChessGame, fakeMove, MemoryJournal, FakeStore, FakeAnticheat, FakePrimary, FakeEndpoint, silentLog } from '../../src/game/testing.js';
import { decode, encode, enums, MSG, CloseCode } from '../../src/protocol/index.js';
import { Registry } from '../../src/metrics.js';
import { GameIdAllocator, shardOfGameId } from '../../src/util/ids.js';
import { testConfig } from '../../src/config.js';

const { GameStatus: GS, EndReason: ER, ErrorCode: EC, GameEventKind: EV } = enums;
const W = 0, B = 1;
const CFG = testConfig();
const T0 = 1_800_000_000_000;
const player = (id) => ({ userId: id, name: `user${id}`, rating: 1500 + id, provisional: false });

function mkHost(o = {}) {
    const clock = { t: o.t ?? T0 };
    const deps = {
        store: 'store' in o ? o.store : new FakeStore(),
        journal: 'journal' in o ? o.journal : new MemoryJournal(),
        anticheat: 'anticheat' in o ? o.anticheat : new FakeAnticheat(),
        primary: 'primary' in o ? o.primary : new FakePrimary(o.handlers),
        registry: new Registry(),
    };
    const host = new GameHost({
        shard: 3, config: o.config || CFG, store: deps.store, journal: deps.journal, anticheat: deps.anticheat,
        primary: deps.primary, log: silentLog, createChessGame: () => new FakeChessGame(o.script || {}),
        now: () => clock.t, metrics: deps.registry, autoStart: false, lastGameId: o.lastGameId,
    });
    return { host, clock, ...deps };
}

function newGame(host, w = 1, b = 2, tc = { baseMs: 180000, incMs: 2000 }) {
    return host.createGame({ white: player(w), black: player(b), rated: true, ...tc });
}

function moveMsg(room, extra = {}) {
    return { type: MSG.Move, seq: 5, game: room.id, ply: room.ply, move: fakeMove(room.ply), posHash: room.game.position.digest(), thinkMs: 0, drawOffer: false, ...extra };
}

function play(host, id, clock, dt = 1000) {
    const room = host.room(id);
    clock.t += dt;
    host.onClientMessage(id, room.playerOf(room.ply & 1).userId, moveMsg(room), null);
}

// ErrorCode values >= 256 (ProtocolViolation 300 .. SlowConsumer 303) do not fit the u8 enum of
// schema.js: the codec writes them truncated and cannot decode them back. Until the schema is
// fixed, such Error frames are read raw here (the code byte is checked against the truncation).
function frame(buf) {
    try { return decode(buf); } catch (err) {
        if (buf[0] !== MSG.Error) throw err;
        return { type: MSG.Error, ref: buf.readUInt32LE(1), code: buf[5] === (EC.CheatDetected & 0xff) ? EC.CheatDetected : -buf[5], fatal: buf[6] === 1, raw: true };
    }
}
const last = (ep) => frame(ep.sent.at(-1));
const frames = (ep) => ep.sent.map(frame);
const kinds = (ep) => frames(ep).map((m) => m.type);

function metricValue(registry, name, label) {
    const m = registry.metrics.get(name);
    if (!m) return undefined;
    for (const c of m.children.values()) if (label === undefined || c.labelValues[0] === label) return m.kind === 'histogram' ? c.count : c.value;
    return undefined;
}

test('create, attach (snapshot), moves broadcast as one buffer, journal appends, metrics', () => {
    const { host, clock, journal, registry } = mkHost();
    const id = newGame(host);
    assert.equal(shardOfGameId(id), 3);
    assert.equal(host.activeGameOf(1), id);
    const ew = new FakeEndpoint(10), eb = new FakeEndpoint(20, 1);
    assert.equal(host.attach(id, 1, ew), true);
    assert.equal(host.attach(id, 2, eb), true);
    const snap = last(ew);
    assert.deepEqual([snap.type, snap.you, snap.category, snap.rated, snap.firstMoveMs], [MSG.GameSnapshot, W, '3+2', true, 30000]);
    assert.equal(last(eb).you, B);
    clock.t += 1500;
    host.onClientMessage(id, 1, moveMsg(host.room(id)), ew);
    assert.equal(ew.sent.at(-1), eb.sent.at(-1), 'the same Buffer for both players');
    assert.equal(last(ew).type, MSG.MoveMade);
    assert.deepEqual(journal.games.get(id).map((r) => r.kind), [JournalKind.Created, JournalKind.Move]);
    assert.equal(metricValue(registry, 'scacelith_game_moves_total'), 1);
    assert.equal(metricValue(registry, 'scacelith_game_move_processing_us'), 1);
    assert.equal(metricValue(registry, 'scacelith_games_active'), 1);
    assert.equal(host.stats().active, 1);
    assert.equal(host.wheel.deadlineOf(host.rooms.get(id)), clock.t + 30000 + 150);
});

test('unknown game and unroutable messages: Error{NotInGame}', () => {
    const { host, anticheat } = mkHost();
    const ep = new FakeEndpoint();
    host.onClientMessage(424242, 1, { type: MSG.Resign, seq: 8, game: 424242 }, ep);
    assert.deepEqual([last(ep).type, last(ep).code, last(ep).ref], [MSG.Error, EC.NotInGame, 8]);
    assert.equal(anticheat.anomalies.length, 0);
    const id = newGame(host);
    host.onClientMessage(id, 1, { type: MSG.QueueJoin, seq: 9 }, ep);
    assert.deepEqual([last(ep).code, last(ep).ref], [EC.NotInGame, 9]);
    assert.equal(host.attach(424242, 1, ep), false);
});

test('a game message from a non-player is foreign_game: sanctioned, the game goes on', () => {
    const { host, anticheat } = mkHost();
    const id = newGame(host);
    const ep = new FakeEndpoint(77);
    host.onClientMessage(id, 99, { type: MSG.Resign, seq: 3, game: id }, ep);
    const msgs = frames(ep);
    assert.deepEqual(msgs.map((m) => [m.type, m.code]), [[MSG.Error, EC.NotInGame], [MSG.Error, EC.CheatDetected]]);
    assert.equal(msgs[1].fatal, true);
    assert.deepEqual(anticheat.anomalies.map((a) => [a.userId, a.gameId, a.kind]), [[99, id, 'foreign_game']]);
    assert.deepEqual(anticheat.sanctions, [{ userId: 99, gameId: id, kind: 'foreign_game' }]);
    assert.equal(ep.closed.code, CloseCode.CheatDetected);
    assert.equal(host.room(id).isOver, false);
});

test('a certain cheat forfeits the game when AUTO_SANCTION_CERTAIN_CHEATS is on', () => {
    const { host, clock, anticheat, registry } = mkHost();
    const id = newGame(host);
    const ew = new FakeEndpoint(10), eb = new FakeEndpoint(20);
    host.attach(id, 1, ew); host.attach(id, 2, eb);
    play(host, id, clock);
    ew.clear(); eb.clear();
    clock.t += 100;
    host.onClientMessage(id, 1, moveMsg(host.room(id), { seq: 31 }), ew);   // White again: out of turn
    assert.deepEqual(kinds(ew), [MSG.MoveRejected, MSG.GameSnapshot, MSG.GameEnd, MSG.Error]);
    const err = last(ew);
    assert.deepEqual([err.code, err.fatal, err.ref], [EC.CheatDetected, true, 31]);
    assert.deepEqual(kinds(eb), [MSG.GameEnd]);
    const end = decode(eb.sent[0]);
    assert.deepEqual([end.status, end.reason], [GS.BlackWins, ER.Forfeit]);
    assert.equal(ew.closed.code, CloseCode.CheatDetected);
    assert.deepEqual(anticheat.sanctions.map((s) => s.kind), ['out_of_turn']);
    assert.equal(metricValue(registry, 'scacelith_game_rejects_total', 'NotYourTurn'), 1);
    assert.equal(metricValue(registry, 'scacelith_games_ended_total', 'Forfeit'), 1);
    assert.equal(host.activeGameOf(1), 0);
});

test('without auto-sanction, or for non-certain anomalies, the anomaly is only recorded', () => {
    const { host, clock, anticheat } = mkHost({ config: testConfig({ AUTO_SANCTION_CERTAIN_CHEATS: '0' }) });
    const id = newGame(host);
    const ew = new FakeEndpoint(10);
    host.attach(id, 1, ew);
    play(host, id, clock);
    host.onClientMessage(id, 1, moveMsg(host.room(id)), ew);
    host.onClientMessage(id, 1, { ...moveMsg(host.room(id)), ply: 0, move: fakeMove(9) }, ew);   // stale
    assert.deepEqual(anticheat.anomalies.map((a) => a.kind), ['out_of_turn', 'stale_ply']);
    assert.equal(anticheat.sanctions.length, 0);
    assert.equal(host.room(id).isOver, false);
    assert.equal(ew.closed, null);
});

test('detach / attach: disconnection events, stale detach ignored, snapshot on reconnection', () => {
    const { host, clock } = mkHost();
    const id = newGame(host);
    const ew = new FakeEndpoint(10), eb = new FakeEndpoint(20);
    host.attach(id, 1, ew); host.attach(id, 2, eb);
    play(host, id, clock); play(host, id, clock);
    eb.clear();
    const ew2 = new FakeEndpoint(11);
    host.attach(id, 1, ew2);                       // a new connection replaced the old one
    assert.equal(host.detach(id, 1, ew), false, 'the old connection closing is ignored');
    assert.equal(eb.sent.length, 0);
    clock.t += 1000;
    assert.equal(host.detach(id, 1, ew2), true);
    const ev = last(eb);
    assert.deepEqual([ev.type, ev.kind, ev.color, ev.arg], [MSG.GameEvent, EV.PlayerDisconnected, W, 18000]);
    clock.t += 5000;
    const ew3 = new FakeEndpoint(12);
    host.attach(id, 1, ew3);
    assert.equal(last(eb).kind, EV.PlayerReconnected);
    const s = last(ew3);
    assert.deepEqual([s.type, s.whiteConnected, s.running], [MSG.GameSnapshot, true, W]);
    assert.equal(s.whiteMs, 180000 - 6000);
});

test('grace expiry through the timer wheel: abandonment and conduct.record', () => {
    const { host, clock, primary } = mkHost();
    const id = newGame(host);
    const eb = new FakeEndpoint(20);
    host.attach(id, 2, eb);
    host.attach(id, 1, new FakeEndpoint(10));
    play(host, id, clock); play(host, id, clock);
    host.detach(id, 1, null);
    const t = clock.t;
    host.runTimers(t + 18000 - 1);
    assert.equal(host.room(id).isOver, false);
    host.runTimers(t + 18000 + 9);
    assert.deepEqual([host.room(id).result.reason, host.room(id).result.endedAt], [ER.Abandonment, t + 18000 + 9]);
    assert.equal(last(eb).type, MSG.GameEnd);
    assert.deepEqual(primary.of('conduct.record'), [{ userId: 1, kind: 'abandon' }]);
});

test('timer wheel under 10k rooms: every deadline fires once, never early', (t) => {
    const { host, clock } = mkHost({ store: null, journal: null, anticheat: null, primary: null });
    const N = 10000, expected = new Map();
    const started = performance.now();
    for (let i = 0; i < N; i++) {
        clock.t = T0 + i * 3;
        const id = host.createGame({ white: player(2 * i + 1), black: player(2 * i + 2), baseMs: 15000 + (i % 7) * 1000, incMs: 0, rated: false });
        const kind = i % 3;
        if (kind === 0) {                          // both first moves: White's clock runs out
            play(host, id, clock, 1); play(host, id, clock, 1);
            expected.set(id, [clock.t + 15000 + (i % 7) * 1000 + 150, ER.Timeout]);
        } else if (kind === 1) {                   // White never moves (first-move margin: 150)
            expected.set(id, [clock.t + 30000 + 150, ER.NoShow]);
        } else {                                   // Black never moves
            play(host, id, clock, 1);
            expected.set(id, [clock.t + 30000 + 150, ER.NoShow]);
        }
    }
    assert.equal(host.wheel.size, N);
    let fired = 0;
    for (let at = T0; at <= T0 + 61000; at += 10) { clock.t = at; fired += host.runTimers(at); }
    assert.equal(fired, N, 'one firing per room');
    for (const [id, [deadline, reason]] of expected) {
        const room = host.room(id);
        assert.equal(room.isOver, true);
        assert.equal(room.result.reason, reason);
        const late = room.result.endedAt - deadline;
        assert.ok(late >= 0 && late < 10, `game ${id} ended ${late} ms after its deadline`);
    }
    assert.equal(host.stats().active, 0);
    // Commit everything (no store: immediate), then let the rematch windows expire: all removed.
    while (host.pending.size) host.pollCommits(clock.t);
    for (let at = T0 + 61000; at <= T0 + 61000 + REMATCH_WINDOW_MS + 100; at += 10) { clock.t = at; host.runTimers(at); }
    assert.equal(host.rooms.size, 0);
    assert.equal(host.wheel.size, 0);
    t.diagnostic(`10k rooms, ${fired} deadlines, 12k wheel advances: ${(performance.now() - started).toFixed(0)} ms`);
});

test('timer rescheduling: moves push the deadline, rtt changes move it', () => {
    const { host, clock } = mkHost();
    const id = newGame(host);
    const entry = host.rooms.get(id);
    play(host, id, clock); play(host, id, clock);
    const t1 = clock.t;
    assert.equal(host.wheel.deadlineOf(entry), t1 + 180000 + 150);
    host.onRtt(id, 1, 600);
    assert.equal(host.wheel.deadlineOf(entry), t1 + 180000 + 650);
    play(host, id, clock, 500);
    assert.equal(host.wheel.deadlineOf(entry), clock.t + 180000 + 150);
});

test('finished games are committed in one batch DB_COMMIT_MS after the first end; then RatingUpdate, committed, game.ended', () => {
    const { host, clock, store, journal, primary, registry } = mkHost();
    const ids = [], eps = [];
    for (let i = 0; i < 5; i++) {
        const id = newGame(host, 10 + 2 * i, 11 + 2 * i);
        const ew = new FakeEndpoint(100 + i), eb = new FakeEndpoint(200 + i);
        host.attach(id, 10 + 2 * i, ew); host.attach(id, 11 + 2 * i, eb);
        ids.push(id); eps.push([ew, eb]);
        play(host, id, clock, 10); play(host, id, clock, 10);
    }
    const tEnd = clock.t;
    for (let i = 0; i < 5; i++) {
        clock.t = tEnd + i * 5;
        host.onClientMessage(ids[i], 10 + 2 * i, { type: MSG.Resign, seq: 2, game: ids[i] }, eps[i][0]);
    }
    assert.equal(host.pending.size, 5);
    host.pollCommits(tEnd + 49);
    assert.equal(store.batches.length, 0);
    host.pollCommits(tEnd + 50);
    assert.equal(store.batches.length, 1);
    assert.deepEqual(store.batches[0].map((r) => r.id), ids);
    const rec = store.batches[0][0];
    assert.ok(rec.moves instanceof Uint16Array && rec.spentMs instanceof Uint32Array && rec.clockMs instanceof Uint32Array);
    assert.equal(rec.rated, true);
    for (const [ew, eb] of eps) {
        assert.equal(last(ew).type, MSG.RatingUpdate);
        assert.equal(ew.sent.at(-1), eb.sent.at(-1));
        assert.deepEqual([last(ew).white.before, last(ew).white.after, last(ew).category], [last(ew).white.before, last(ew).white.before + 8, '3+2']);
    }
    for (const id of ids) assert.ok(journal.done.has(id));
    const ended = primary.of('game.ended');
    assert.equal(ended.length, 5);
    assert.deepEqual(ended[0], { gameId: ids[0], whiteId: 10, blackId: 11, status: GS.BlackWins, reason: ER.Resignation, rated: true, category: '3+2', rematchOffer: 2 });
    assert.equal(metricValue(registry, 'scacelith_game_commit_batch_size'), 1);
    assert.equal(host.pending.size, 0);
    // Rooms stay until the rematch window closes.
    assert.ok(host.room(ids[0]));
    host.runTimers(tEnd + REMATCH_WINDOW_MS + 30);
    assert.equal(host.room(ids[0]), null);
});

test('a failed commit is retried with backoff; the journal keeps the game meanwhile', () => {
    const { host, clock, store, journal, registry } = mkHost();
    const id = newGame(host);
    play(host, id, clock); play(host, id, clock);
    host.onClientMessage(id, 2, { type: MSG.Resign, seq: 2, game: id }, null);
    const t = clock.t;
    const poll = (dt) => { clock.t = t + dt; return host.pollCommits(clock.t); };
    store.failures = 2;
    poll(50);                                       // fails: retry in 100 ms
    assert.equal(journal.done.has(id), false);
    poll(149);
    assert.equal(store.failures, 1, 'no attempt during the backoff');
    poll(150);                                      // fails: retry in 200 ms
    poll(349);
    assert.equal(store.batches.length, 0);
    poll(350);
    assert.deepEqual(store.committedIds, [id]);
    assert.equal(journal.done.has(id), true);
    assert.equal(metricValue(registry, 'scacelith_game_commit_errors_total'), 2);
    assert.equal(host.backoffMs, 0);
});

test('asynchronous store: one commit in flight at a time', async () => {
    const store = new FakeStore();
    store.async = true;
    store.failures = 1;
    const { host, clock, journal } = mkHost({ store });
    const a = newGame(host, 1, 2), b = newGame(host, 3, 4);
    host.onClientMessage(a, 1, { type: MSG.Abort, seq: 2, game: a }, null);
    const t = clock.t;
    const poll = (dt) => { clock.t = t + dt; return host.pollCommits(clock.t); };
    assert.equal(await poll(50), false);            // fails: retry in 100 ms
    host.onClientMessage(b, 3, { type: MSG.Abort, seq: 2, game: b }, null);
    assert.equal(poll(100), null, 'still backing off');
    const p = poll(150);
    assert.equal(poll(151), null, 'in flight');
    assert.equal(await p, true);
    assert.deepEqual(store.committedIds.sort(), [a, b].sort());
    assert.ok(journal.done.has(a) && journal.done.has(b));
    assert.equal(store.batches[0][0].rated, false, 'aborted games are unrated');
});

test('recovery after a crash: running games restored, ended-but-uncommitted games committed', () => {
    const journal = new MemoryJournal();
    const store = new FakeStore();
    store.failures = 1000;                          // the database is down: nothing gets committed
    const a = mkHost({ journal, store });
    const running = newGame(a.host, 1, 2);
    const done = newGame(a.host, 3, 4);
    for (let i = 0; i < 5; i++) play(a.host, running, a.clock, 700);
    for (let i = 0; i < 4; i++) play(a.host, done, a.clock, 300);
    a.host.onClientMessage(done, 4, { type: MSG.Resign, seq: 2, game: done }, null);
    a.host.pollCommits(a.clock.t + 100);
    assert.equal(store.batches.length, 0);
    const expectedRecord = a.host.room(done).record();
    const before = a.host.room(running);
    const blackMs = before.clock.ms[B];
    // Crash: a new process with the same journal, an hour later.
    const primary = new FakePrimary();
    const b = mkHost({ journal, primary, t: a.clock.t + 3_600_000 });
    assert.equal(b.host.recover(), 2);
    const room = b.host.room(running);
    assert.equal(room.ply, 5);
    assert.deepEqual(room.connected, [false, false]);
    assert.equal(b.host.activeGameOf(1), running);
    assert.deepEqual(primary.of('game.recovered'), [{ gameId: running, whiteId: 1, blackId: 2, shard: 3 }]);
    // The ended game is committed as it was.
    b.host.pollCommits(b.clock.t + 50);
    assert.deepEqual(b.store.batches[0], [expectedRecord]);
    assert.ok(journal.done.has(done));
    // Black, the side to move, comes back 2 s later: its clock was held until then (nothing
    // charged) and starts at the reconnection, from its journaled value.
    assert.equal(room.nextDeadline(), b.clock.t + 20000, 'the clock hold (RECOVERY_CLOCK_HOLD_MS) ends first');
    const eb = new FakeEndpoint(5);
    b.clock.t += 2000;
    b.host.attach(running, 2, eb);
    const s = last(eb);
    assert.deepEqual([s.type, s.running, s.blackMs, s.blackConnected, s.whiteConnected], [MSG.GameSnapshot, B, blackMs, true, false]);
    assert.equal(room.snapshot(B, b.clock.t + 1500).blackMs, blackMs - 1500);
    // White never comes back: abandonment after the recovery grace (RECOVERY_GRACE_MS, 90 s),
    // not the normal 18 s of a 3+2 game.
    assert.equal(s.graceMs, 90000 - 2000);
    b.host.runTimers(b.clock.t - 2000 + 18000);
    assert.equal(room.isOver, false);
    b.host.runTimers(b.clock.t - 2000 + 90000);
    assert.deepEqual([room.result.status, room.result.reason], [GS.BlackWins, ER.Abandonment]);
});

test('recovery: a journal that cannot be replayed ends ServerAborted; an unreadable one is dropped', () => {
    const journal = new MemoryJournal();
    const a = mkHost({ journal });
    const g1 = newGame(a.host, 1, 2), g2 = newGame(a.host, 3, 4);
    for (let i = 0; i < 4; i++) { play(a.host, g1, a.clock, 100); play(a.host, g2, a.clock, 100); }
    journal.games.get(g1)[3].payload.writeUInt16LE(0, 2);               // a move the rules refuse (a1a1)
    journal.games.get(g2)[0].payload = Buffer.from('not json');
    const b = mkHost({ journal, t: a.clock.t + 1000 });
    assert.equal(b.host.recover(), 1);
    const r1 = b.host.room(g1);
    assert.deepEqual([r1.result.status, r1.result.reason, r1.ply], [GS.Aborted, ER.ServerAborted, 2]);
    assert.equal(b.host.room(g2), null);
    assert.ok(journal.done.has(g2), 'dropped');
    b.host.pollCommits(b.clock.t + 50);
    assert.deepEqual(b.store.committedIds, [g1]);
    assert.equal(b.store.batches[0][0].rated, false);
});

test('game ids are never given again after a restart with the clock behind (journal and database seeds)', () => {
    const used = new GameIdAllocator(9).next(T0);                // another shard's id
    const ids = new GameIdAllocator(3);
    ids.seed(used);
    for (const t of [T0 - 120000, T0, T0 + 0.5]) assert.ok(ids.next(t) > used);
    for (const bad of [0, -1, 1.5, NaN]) ids.seed(bad);
    assert.ok(ids.next(T0 + 1) > used);
    // A restart in the same millisecond, then two minutes behind: after every game of the journal.
    const journal = new MemoryJournal();
    const a = mkHost({ journal });
    const g1 = newGame(a.host, 1, 2), g2 = newGame(a.host, 3, 4);
    for (const dt of [0, -120000]) {
        const b = mkHost({ journal, t: a.clock.t + dt });
        b.host.recover();
        assert.ok(newGame(b.host, 5, 6) > Math.max(g1, g2));
    }
    // The database's largest id (a committed game is no longer in the journal).
    const c = mkHost({ t: a.clock.t - 120000, lastGameId: g2 });
    assert.ok(newGame(c.host, 5, 6) > g2);
    // A clock ahead of the seeds gives the same ids as without them.
    const d = mkHost({ t: a.clock.t + 1000, lastGameId: g2 }), e = mkHost({ t: a.clock.t + 1000 });
    assert.equal(newGame(d.host, 5, 6), newGame(e.host, 5, 6));
});

test('rematch agreement is sent to the primary with colours swapped', async () => {
    const { host, clock, primary } = mkHost({ handlers: { 'game.rematch': () => ({ error: 'user_unavailable' }) } });
    const id = newGame(host, 1, 2);
    const ew = new FakeEndpoint(1), eb = new FakeEndpoint(2);
    host.attach(id, 1, ew); host.attach(id, 2, eb);
    play(host, id, clock); play(host, id, clock);
    host.onClientMessage(id, 1, { type: MSG.Resign, seq: 3, game: id }, ew);
    host.onClientMessage(id, 2, { type: MSG.Rematch, seq: 4, game: id, accept: true }, eb);
    assert.equal(last(ew).kind, EV.RematchOffered);
    host.onClientMessage(id, 1, { type: MSG.Rematch, seq: 5, game: id, accept: true }, ew);
    const [req] = primary.of('game.rematch');
    assert.deepEqual([req.gameId, req.white.userId, req.black.userId, req.baseMs, req.incMs, req.rated, req.category], [id, 2, 1, 180000, 2000, true, '3+2']);
    await new Promise((r) => setImmediate(r));
    assert.deepEqual([last(ew).type, last(ew).code], [MSG.Error, EC.RematchUnavailable]);
});

test('declineRematch (a queue join) closes the rematch window and counts no refusal, whatever the game\'s state', () => {
    const { host, clock, registry } = mkHost();
    const id = newGame(host, 1, 2);
    const ew = new FakeEndpoint(1), eb = new FakeEndpoint(2);
    host.attach(id, 1, ew); host.attach(id, 2, eb);
    play(host, id, clock); play(host, id, clock);
    host.declineRematch(id, 1);                                  // still running
    assert.equal(host.room(id).isOver, false);
    host.onClientMessage(id, 1, { type: MSG.Resign, seq: 3, game: id }, ew);
    ew.clear(); eb.clear();
    host.declineRematch(id, 1);
    assert.deepEqual(frames(eb).map((m) => [m.type, m.kind]), [[MSG.GameEvent, EV.RematchDeclined]]);
    assert.equal(ew.sent.length, 1, 'only the broadcast');
    assert.equal(host.room(id).rematchOpen, false);
    host.declineRematch(id, 2);                                  // window closed
    host.pollCommits(clock.t + 1000);
    host.runTimers(clock.t + REMATCH_WINDOW_MS + 30);
    assert.equal(host.room(id), null);
    host.declineRematch(id, 1);                                  // game gone
    assert.equal(metricValue(registry, 'scacelith_game_rejects_total'), undefined);
    host.onClientMessage(id, 1, { type: MSG.Rematch, seq: 4, game: id, accept: false }, ew);
    assert.equal(metricValue(registry, 'scacelith_game_rejects_total', 'NotInGame'), 1, 'a client\'s request still counts');
});

test('asynchronous store (writer thread): one bad record does not block the batch either', async () => {
    const { host, clock, store, journal } = mkHost();
    store.async = true;
    const ids = [newGame(host, 1, 2), newGame(host, 3, 4), newGame(host, 5, 6)];
    for (const id of ids) {
        play(host, id, clock); play(host, id, clock);
        host.onClientMessage(id, host.room(id).black.userId, { type: MSG.Resign, seq: 2, game: id }, null);
    }
    store.badIds.add(ids[1]);
    clock.t += 100;
    const res = host.pollCommits(clock.t);
    assert.equal(typeof res.then, 'function');
    assert.equal(await res, false);
    assert.equal(host.commitInFlight, null);
    assert.deepEqual(store.committedIds.sort(), [ids[0], ids[2]].sort());
    assert.ok(journal.done.has(ids[0]) && journal.done.has(ids[2]));
    assert.equal(journal.done.has(ids[1]), false, 'the bad game stays in the journal');
    assert.equal(host.stats().pendingCommits, 1);
});

test('the anti-cheat\'s buffered anomalies are written before the finished game goes to the database when one is not info (end-of-game signal)', () => {
    const order = [];
    const acStore = { anomalies: { insertBatch(rows) { for (const r of rows) order.push(`anomaly ${r.kind} ${r.gameId}`); } } };
    const anticheat = createAnticheat({ config: CFG, store: acStore, log: silentLog, flushMs: 60000 });
    const store = new FakeStore();
    const finishBatch = store.games.finishBatch;
    store.games.finishBatch = (records) => { for (const r of records) order.push(`commit ${r.id}`); return finishBatch(records); };
    const { host, clock } = mkHost({ store, anticheat });
    try {
        // Only an info anomaly buffered: the commit does not write it (the analysis queue policy
        // reads only the others); it waits for the anti-cheat's own timer.
        const id0 = newGame(host, 5, 6);
        anticheat.recordAnomaly({ userId: 5, gameId: id0, kind: 'stale_ply' });
        assert.deepEqual([anticheat.pendingCount, anticheat.pendingSignalCount], [1, 0]);
        host.onClientMessage(id0, 5, { type: MSG.Abort, seq: 2, game: id0 }, null);
        host.pollCommits(clock.t + 50);
        assert.deepEqual(order, [`commit ${id0}`]);
        assert.equal(anticheat.pendingCount, 1);
        const id = newGame(host);
        play(host, id, clock); play(host, id, clock);
        // White's last move claims more thinking time than it had (suspicious: buffered up to a
        // second), then White resigns.
        clock.t += 1000;
        const room = host.room(id);
        host.onClientMessage(id, 1, moveMsg(room, { thinkMs: 5000 }), null);
        assert.equal(room.ply, 3);
        assert.deepEqual([anticheat.pendingCount, anticheat.pendingSignalCount], [2, 1]);
        host.onClientMessage(id, 1, { type: MSG.Resign, seq: 2, game: id }, null);
        host.pollCommits(clock.t + 50);
        assert.deepEqual(order.slice(1), [`anomaly stale_ply ${id0}`, `anomaly clock_implausible ${id}`, `commit ${id}`]);
        assert.deepEqual([anticheat.pendingCount, anticheat.pendingSignalCount], [0, 0]);
        // Nothing buffered: no extra write before the next commit.
        const id2 = newGame(host, 3, 4);
        host.onClientMessage(id2, 3, { type: MSG.Abort, seq: 2, game: id2 }, null);
        host.pollCommits(clock.t + 200);
        assert.deepEqual(order.slice(4), [`commit ${id2}`]);
    } finally {
        anticheat.close();
    }
});

test('abort is forwarded as conduct.record; forfeitUser; shutdown flushes commits and the journal', async () => {
    const { host, clock, primary, store, journal } = mkHost();
    const a = newGame(host, 1, 2);
    host.onClientMessage(a, 2, { type: MSG.Abort, seq: 2, game: a }, null);
    assert.deepEqual(primary.of('conduct.record'), [{ userId: 2, kind: 'abort' }]);
    const b = newGame(host, 5, 6);
    play(host, b, clock); play(host, b, clock);
    assert.equal(host.forfeitUser(6), true);
    assert.deepEqual([host.room(b).result.status, host.room(b).result.reason], [GS.WhiteWins, ER.Forfeit]);
    assert.equal(host.forfeitUser(6), false);
    assert.equal(host.pending.size, 2);
    await host.shutdown();
    assert.equal(host.pending.size, 0);
    assert.deepEqual(store.committedIds.sort(), [a, b].sort());
    assert.equal(journal.flushes, 1);
});

test('Resync binds an endpoint that is not attached yet and answers with a snapshot', () => {
    const { host } = mkHost();
    const id = newGame(host);
    const ep = new FakeEndpoint(3);
    host.onClientMessage(id, 2, { type: MSG.Resync, seq: 4, game: id }, ep);
    assert.equal(last(ep).type, MSG.GameSnapshot);
    assert.equal(host.rooms.get(id).ep[B], ep);
});

test('GameRoom.fromJournal works from the host journal records', () => {
    const { host, clock, journal } = mkHost();
    const id = newGame(host);
    for (let i = 0; i < 6; i++) play(host, id, clock, 900);
    const copy = GameRoom.fromJournal(journal.games.get(id), { config: CFG, createChessGame: () => new FakeChessGame() });
    assert.deepEqual(copy.snapshot(W, clock.t), host.room(id).snapshot(W, clock.t));
});

test('one bad record does not block the batch: the others are committed one by one', () => {
    const { host, clock, store, journal } = mkHost();
    const ids = [newGame(host, 1, 2), newGame(host, 3, 4), newGame(host, 5, 6)];
    for (const id of ids) {
        play(host, id, clock); play(host, id, clock);
        host.onClientMessage(id, host.room(id).black.userId, { type: MSG.Resign, seq: 2, game: id }, null);
    }
    store.badIds.add(ids[1]);
    clock.t += 100;
    assert.equal(host.pollCommits(clock.t), false);
    assert.deepEqual(store.committedIds.sort(), [ids[0], ids[2]].sort());
    assert.ok(journal.done.has(ids[0]) && journal.done.has(ids[2]));
    assert.equal(journal.done.has(ids[1]), false, 'the bad game stays in the journal');
    assert.equal(host.stats().pendingCommits, 1);
});

// ---- gesture relay ----------------------------------------------------------------------------------

/** An endpoint with the router's droppable send; `full` makes it refuse (a backlog). */
class DroppableEndpoint extends FakeEndpoint {
    constructor(connId, shard) { super(connId, shard); this.full = false; this.droppable = 0; }
    sendDroppable(buf) {
        if (this.full) return false;
        this.droppable++;
        this.sent.push(buf);
        return true;
    }
}

const GESTURE = { ply: 2, touch: 12, aim: 28, placed: 0, flags: 5, yaw: -1234, pitch: 321, lean: 40 };

test('gestures go to the opponent only, as droppable S_Gesture frames; nothing is journaled or timed', () => {
    const { host, clock, journal, registry, anticheat } = mkHost();
    const id = newGame(host);
    const ew = new FakeEndpoint(10), eb = new DroppableEndpoint(20, 1);   // Black on another shard
    host.attach(id, 1, ew); host.attach(id, 2, eb);
    play(host, id, clock); play(host, id, clock);
    const entry = host.rooms.get(id), room = host.room(id);
    const before = { records: journal.games.get(id).length, gseq: room.gseq, deadline: host.wheel.deadlineOf(entry), snap: room.snapshot(W, clock.t) };
    ew.clear(); eb.clear();
    const frame = encode.C_Gesture({ seq: 41, game: id, ...GESTURE });
    assert.equal(host.relayGesture(id, 1, frame), true);
    assert.equal(ew.sent.length, 0, 'never back to the sender');
    assert.equal(eb.droppable, 1);
    assert.deepEqual(frames(eb), [{ type: MSG.S_Gesture, game: id, ...GESTURE }]);
    // Black's gesture to White's endpoint, which has no droppable send.
    assert.equal(host.relayGesture(id, 2, encode.C_Gesture({ seq: 42, game: id, ...GESTURE, yaw: 7 })), true);
    assert.equal(last(ew).yaw, 7);
    assert.deepEqual(
        { records: journal.games.get(id).length, gseq: room.gseq, deadline: host.wheel.deadlineOf(entry), snap: room.snapshot(W, clock.t) },
        before);
    assert.equal(anticheat.anomalies.length, 0);
    assert.equal(metricValue(registry, 'scacelith_gestures_relayed_total'), 2);
    assert.equal(host.stats().gestures, 2);
});

test('gestures are dropped without an anomaly when they cannot be relayed, and relayed through the rematch window', () => {
    const { host, clock, registry, anticheat } = mkHost();
    const id = newGame(host);
    const ew = new FakeEndpoint(10), eb = new DroppableEndpoint(20);
    host.attach(id, 1, ew); host.attach(id, 2, eb);
    const g = (userId, extra = {}) => host.relayGesture(id, userId, encode.C_Gesture({ seq: 5, game: id, ...GESTURE, ...extra }));
    const dropped = (reason) => metricValue(registry, 'scacelith_gestures_dropped_total', reason);
    assert.equal(g(99), false);
    assert.equal(dropped('not_player'), 1);
    eb.full = true;
    assert.equal(g(1), false);
    assert.equal(dropped('backlog'), 1);
    eb.full = false;
    assert.equal(host.relayGesture(id, 1, encode.C_Gesture({ seq: 5, game: id, ...GESTURE }).subarray(0, 20)), false);
    assert.equal(dropped('malformed'), 1);
    assert.equal(host.relayGesture(id + 1, 1, encode.C_Gesture({ seq: 5, game: id + 1, ...GESTURE })), false);
    assert.equal(dropped('no_game'), 1);
    clock.t += 1000;
    host.onClientMessage(id, 1, { type: MSG.Resign, seq: 3, game: id }, ew);
    assert.equal(host.room(id).isOver, true);
    assert.equal(g(1), true, 'the room still exists: the rematch window');
    host.detach(id, 2, eb);
    assert.equal(g(1), false);
    assert.equal(dropped('no_opponent'), 1);
    assert.equal(anticheat.anomalies.length, 0);
});

// ---- clock press ------------------------------------------------------------------------------------

test('autoPress: AUTO_PRESS_CLOCK unless the spec sets it; snapshot, created record, rematch and record flag', async () => {
    const created = (journal, id) => JSON.parse(journal.games.get(id)[0].payload.toString('utf8'));
    let { host, journal } = mkHost();
    let id = newGame(host);
    let ep = new FakeEndpoint(1);
    host.attach(id, 1, ep);
    assert.equal(last(ep).autoPress, true);
    assert.equal(created(journal, id).autoPress, true);
    id = host.createGame({ white: player(3), black: player(4), rated: true, baseMs: 180000, incMs: 2000, autoPress: false });
    assert.equal(host.room(id).autoPress, false);

    ({ host, journal } = mkHost({ config: testConfig({ AUTO_PRESS_CLOCK: 'false' }) }));
    id = newGame(host);
    ep = new FakeEndpoint(1);
    host.attach(id, 1, ep);
    assert.equal(last(ep).autoPress, false);
    assert.equal(created(journal, id).autoPress, false);
    assert.equal(host.room(host.createGame({ white: player(3), black: player(4), rated: false, baseMs: 60000, incMs: 0, autoPress: true })).autoPress, true);

    const m = mkHost({ config: testConfig({ AUTO_PRESS_CLOCK: 'false' }), handlers: { 'game.rematch': () => ({ ok: true }) } });
    id = newGame(m.host, 1, 2);
    const ew = new FakeEndpoint(1), eb = new FakeEndpoint(2);
    m.host.attach(id, 1, ew); m.host.attach(id, 2, eb);
    play(m.host, id, m.clock); play(m.host, id, m.clock);
    m.host.onClientMessage(id, 1, { type: MSG.Resign, seq: 3, game: id }, ew);
    m.host.onClientMessage(id, 2, { type: MSG.Rematch, seq: 4, game: id, accept: true }, eb);
    m.host.onClientMessage(id, 1, { type: MSG.Rematch, seq: 5, game: id, accept: true }, ew);
    assert.equal(m.primary.of('game.rematch')[0].autoPress, false);
    m.clock.t += 100;
    m.host.pollCommits(m.clock.t);
    await new Promise((r) => setImmediate(r));
    const rec = m.store.batches.flat().find((r) => r.id === id);
    assert.equal(rec.flags & RecordFlag.ManualPress, RecordFlag.ManualPress);
});

// ---- stall credit -----------------------------------------------------------------------------------

test('configuration: AUTO_PRESS_CLOCK, GAME_STALL_MIN_MS and GAME_STALL_CREDIT_MAX_MS', () => {
    assert.deepEqual([CFG.autoPressClock, CFG.gameStallMinMs, CFG.gameStallCreditMaxMs], [true, 30, 5000]);
    assert.equal(testConfig({ AUTO_PRESS_CLOCK: 'false' }).autoPressClock, false);
    assert.throws(() => testConfig({ GAME_STALL_MIN_MS: '4' }), /GAME_STALL_MIN_MS: at least 5/);
    assert.throws(() => testConfig({ GAME_STALL_CREDIT_MAX_MS: '60001' }), /GAME_STALL_CREDIT_MAX_MS: at most 60000/);
    const { host } = mkHost({ config: testConfig({ GAME_STALL_MIN_MS: '100', GAME_STALL_CREDIT_MAX_MS: '0' }) });
    host.heartbeat(T0);
    assert.equal(host.stallCredit(T0 + 105), 0, 'under 10 + 100 ms: not a stall');
    assert.equal(host.stallCredit(T0 + 2000), 0, 'GAME_STALL_CREDIT_MAX_MS=0 gives nothing back');
});

// A game where White's clock runs (both first moves played), and its flag deadline.
function running(o = {}) {
    const h = mkHost(o);
    const id = newGame(h.host);
    const ew = new FakeEndpoint(10), eb = new FakeEndpoint(20);
    h.host.attach(id, 1, ew); h.host.attach(id, 2, eb);
    play(h.host, id, h.clock); play(h.host, id, h.clock);
    const deadline = h.host.wheel.deadlineOf(h.host.rooms.get(id));
    return { ...h, id, ew, eb, deadline };
}
const drain = () => new Promise((r) => setImmediate(r));

test('stall credit: a move read after a stall beats the flag that fell during it', async () => {
    const { host, clock, registry, id, ew, eb, deadline } = running();
    clock.t = deadline - 1000;
    assert.equal(host.heartbeat(clock.t), false);             // the last beat before the stall
    clock.t = deadline + 2000;                                // 3 s without a beat
    assert.equal(host.heartbeat(clock.t), true);              // detected: the timers wait for the sockets
    assert.equal(host.room(id).isOver, false);
    host.onClientMessage(id, 1, moveMsg(host.room(id)), ew);
    const m = last(eb);
    assert.equal(m.type, MSG.MoveMade);
    assert.equal(m.serverTime, clock.t);
    await drain();                                            // the timers run after the stall
    assert.equal(host.room(id).isOver, false);
    assert.equal(host.wheel.deadlineOf(host.rooms.get(id)), clock.t + 180000 + 150, 'Black\'s clock starts at the MoveMade');
    assert.equal(metricValue(registry, 'scacelith_game_stall_ms'), 1);
    assert.equal(metricValue(registry, 'scacelith_game_stall_credit_ms_total'), clock.t - (deadline - 1000 + 10));
    assert.equal(host.stallCredit(clock.t + 5), 0, 'no credit once the timers ran');
    assert.equal(host.stallDuring(deadline - 1500), true);
    assert.equal(host.stallDuring(clock.t + 1, clock.t + 5), false);
});

test('stall credit: without a move the flag falls when the timers run after the stall; without a stall at once', async () => {
    let s = running();
    s.clock.t = s.deadline - 1000;
    s.host.heartbeat(s.clock.t);
    s.clock.t = s.deadline + 2000;
    s.host.heartbeat(s.clock.t);
    assert.equal(s.host.room(s.id).isOver, false);
    await drain();
    assert.deepEqual([s.host.room(s.id).result.reason, s.host.room(s.id).result.endedAt], [ER.Timeout, s.deadline + 2000]);
    // The control case: the same move without a detected stall flags.
    s = running();
    s.clock.t = s.deadline + 2000;
    s.host.onClientMessage(s.id, 1, moveMsg(s.host.room(s.id)), s.ew);
    assert.equal(s.host.room(s.id).result.reason, ER.Timeout);
    assert.ok(frames(s.ew).some((f) => f.type === MSG.MoveRejected && f.code === EC.FlagFell));
});

test('stall credit: none for a gap under GAME_STALL_MIN_MS, capped at GAME_STALL_CREDIT_MAX_MS, and from a late beat', async () => {
    let s = running();
    const t0 = s.deadline - 1000;
    s.host.heartbeat(t0);
    assert.equal(s.host.heartbeat(t0 + 10 + 20), false);        // 20 ms late: no stall
    assert.equal(s.host.stallCredit(t0 + 30), 0);
    s.host.heartbeat(t0 + 40);
    assert.equal(s.host.heartbeat(t0 + 40 + 10 + 31), true);
    assert.equal(s.host.stallCredit(t0 + 81), 81 - 50);
    await drain();

    s = running({ config: testConfig({ GAME_STALL_CREDIT_MAX_MS: '1000' }) });
    s.host.heartbeat(s.deadline - 3000);
    s.clock.t = s.deadline + 1500;
    s.host.heartbeat(s.clock.t);
    assert.equal(s.host.stallCredit(s.clock.t), 1000);
    s.host.onClientMessage(s.id, 1, moveMsg(s.host.room(s.id)), s.ew);
    assert.equal(s.host.room(s.id).result.reason, ER.Timeout, 'a stall longer than the cap is not all given back');
    await drain();

    // The interval has not noticed yet: a message handled while the beat is late gets the credit.
    s = running();
    s.host.heartbeat(s.deadline - 200);
    s.clock.t = s.deadline + 300;
    assert.equal(s.host.stallCredit(s.clock.t), 490);
    s.host.onClientMessage(s.id, 1, moveMsg(s.host.room(s.id)), s.ew);
    assert.equal(last(s.eb).type, MSG.MoveMade);
});

test('stall credit: the opponent\'s close or Resync read in the same drain does not flag a queued move', async () => {
    const { host, clock, id, ew, eb, deadline } = running();
    clock.t = deadline - 1000;
    host.heartbeat(clock.t);
    clock.t = deadline + 2000;
    host.heartbeat(clock.t);
    host.onClientMessage(id, 2, { type: MSG.Resync, seq: 7, game: id }, eb);
    assert.equal(last(eb).status, GS.Ongoing);
    host.detach(id, 2, eb);
    assert.equal(host.room(id).isOver, false);
    host.onClientMessage(id, 1, moveMsg(host.room(id)), ew);
    assert.equal(host.room(id).ply, 3);
    const ew2 = new FakeEndpoint(11);
    host.attach(id, 2, ew2);
    assert.equal(last(ew2).type, MSG.GameSnapshot);
    await drain();
    assert.equal(host.room(id).isOver, false);
});

test('stall credit: a first-move timeout that fell during a stall aborts without a no-show; timer lateness is measured', async () => {
    const { host, clock, primary, registry } = mkHost();
    const id = newGame(host);
    host.attach(id, 1, new FakeEndpoint(1));
    host.heartbeat(T0 + 29000);
    clock.t = T0 + 30000 + 150 + 1000;
    host.heartbeat(clock.t);
    await drain();
    assert.deepEqual([host.room(id).result.status, host.room(id).result.reason], [GS.Aborted, ER.NoShow]);
    assert.deepEqual(primary.of('conduct.record'), []);
    assert.equal(metricValue(registry, 'scacelith_game_timer_late_ms'), 1);
    // Without a stall the no-show is recorded.
    const h = mkHost();
    const id2 = newGame(h.host);
    h.host.runTimers(T0 + 30000 + 150);
    assert.equal(h.host.room(id2).result.reason, ER.NoShow);
    assert.deepEqual(h.primary.of('conduct.record'), [{ userId: 1, kind: 'noshow' }]);
});

test('cancelGame: a game the primary gave up creating ends ServerAborted, with no no-show', async () => {
    const { host, primary } = mkHost();
    const id = newGame(host);
    assert.equal(host.cancelGame(id), true);
    assert.deepEqual([host.room(id).result.status, host.room(id).result.reason], [GS.Aborted, ER.ServerAborted]);
    assert.equal(host.activeGameOf(1), 0);
    assert.equal(host.cancelGame(id), false);
    host.runTimers(T0 + 30000 + 150);
    await drain();
    assert.deepEqual(primary.of('conduct.record'), []);
});

test('stall credit: a move that reached its socket while the backlog of a stall was read beats the flag', async () => {
    for (const moves of [true, false]) {
        const { host, clock, id, ew, eb, deadline } = running();
        clock.t = deadline - 1010;
        host.heartbeat(clock.t);                                  // the last beat before the stall
        clock.t = deadline - 400;
        assert.equal(host.heartbeat(clock.t), true);              // a 600 ms stall, detected
        // The poll phase reads the stall's backlog for 600 ms. White's move reaches its socket at
        // deadline - 100, after that poll phase looked at it: it waits for the next one.
        clock.t = deadline + 200;
        await drain();                                            // the timers due by the detecting beat
        assert.equal(host.room(id).isOver, false, 'the flag falls after the deadline the poll phase covered');
        clock.t = deadline + 202;
        assert.equal(host.heartbeat(clock.t), true);              // that poll phase was a stall of its own
        if (moves) {
            clock.t = deadline + 203;
            assert.equal(host.stallCredit(clock.t), 593);
            host.onClientMessage(id, 1, moveMsg(host.room(id)), ew);
            assert.equal(last(eb).type, MSG.MoveMade);
        }
        await drain();
        const room = host.room(id);
        if (moves) assert.deepEqual([room.isOver, room.ply], [false, 3]);
        else assert.deepEqual([room.result.reason, room.result.endedAt], [ER.Timeout, deadline + 202]);
    }
});

test('stall credit: the forfeit of a certain cheat read after a stall takes the arrival of the request that revealed it', async () => {
    // An anti-cheat that answers later (a promise): the forfeit is then a new event, credited the same.
    class LaterAnticheat extends FakeAnticheat {
        recordAnomaly(a) { return Promise.resolve(super.recordAnomaly(a)); }
    }
    for (const answer of ['now', 'later']) {
        for (const plies of [2, 0]) {
            const anticheat = answer === 'now' ? new FakeAnticheat() : new LaterAnticheat();
            const { host, clock, primary } = mkHost({ anticheat });
            const id = newGame(host);
            const ew = new FakeEndpoint(10), eb = new FakeEndpoint(20);
            host.attach(id, 1, ew); host.attach(id, 2, eb);
            for (let i = 0; i < plies; i++) play(host, id, clock);
            const dl = host.wheel.deadlineOf(host.rooms.get(id));    // White's flag or first-move deadline
            host.heartbeat(dl - 1000);
            clock.t = dl + 2000;                                     // a 3 s stall, the deadline 1 s into it
            host.heartbeat(clock.t);
            const arrival = host.stallStart(clock.t);
            // Black's out-of-turn move waited in its socket from before White's deadline.
            host.onClientMessage(id, 2, moveMsg(host.room(id), { seq: 31 }), eb);
            await Promise.resolve();                                 // the later answer, before the timers run
            const r = host.room(id).result, what = `${answer} after ${plies} plies`;
            assert.deepEqual([r.status, r.reason, r.endedAt], [GS.WhiteWins, ER.Forfeit, arrival], what);
            assert.deepEqual(anticheat.sanctions.map((x) => [x.userId, x.kind]), [[2, 'out_of_turn']], what);
            assert.equal(eb.closed.code, CloseCode.CheatDetected, what);
            await drain();
            assert.deepEqual(primary.of('conduct.record'), [], what);
        }
    }
});

test('stall credit: a first-move timeout that fell during a stall longer than the credit records no no-show when a request finds it', async () => {
    for (const first of ['resync', 'move', 'close', 'attach', 'rematch', 'forfeit']) {
        const { host, clock, primary } = mkHost();
        const id = newGame(host);
        const ew = new FakeEndpoint(1), eb = new FakeEndpoint(2);
        host.attach(id, 1, ew); host.attach(id, 2, eb);
        const dl = host.wheel.deadlineOf(host.rooms.get(id));    // White's first-move deadline
        host.heartbeat(dl - 2010);
        clock.t = dl + 18000;                                    // a 20 s stall, the deadline 2 s into it
        host.heartbeat(clock.t);
        assert.equal(host.stallCredit(clock.t), 5000);           // the request counts as arrived 3 s after the deadline
        if (first === 'resync') host.onClientMessage(id, 2, { type: MSG.Resync, seq: 7, game: id }, eb);
        else if (first === 'move') host.onClientMessage(id, 1, moveMsg(host.room(id)), ew);
        else if (first === 'close') host.detach(id, 2, eb);
        else if (first === 'attach') host.attach(id, 2, new FakeEndpoint(3));
        else if (first === 'rematch') host.onClientMessage(id, 2, { type: MSG.Rematch, seq: 7, game: id, accept: true }, eb);
        else assert.equal(host.forfeitUser(2), true);           // a sanction from the primary
        assert.deepEqual([host.room(id).result.status, host.room(id).result.reason], [GS.Aborted, ER.NoShow], first);
        await drain();
        assert.deepEqual(primary.of('conduct.record'), [], first);
    }
    // A deadline due before the stall began (at the beat that did not come) records one all the same.
    const { host, clock, primary } = mkHost();
    const id = newGame(host);
    const eb = new FakeEndpoint(2);
    host.attach(id, 1, new FakeEndpoint(1)); host.attach(id, 2, eb);
    const dl = host.wheel.deadlineOf(host.rooms.get(id));
    host.heartbeat(dl - 5);
    clock.t = dl + 18000;
    host.heartbeat(clock.t);
    host.onClientMessage(id, 2, { type: MSG.Resync, seq: 7, game: id }, eb);
    assert.equal(host.room(id).result.reason, ER.NoShow);
    assert.deepEqual(primary.of('conduct.record'), [{ userId: 1, kind: 'noshow' }]);
    await drain();
});
