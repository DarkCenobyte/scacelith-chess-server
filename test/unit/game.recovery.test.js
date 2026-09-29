// Recovered games (DESIGN 6.4, server restart): both players get RECOVERY_GRACE_MS to come back
// instead of the normal grace, the grace is journaled with the recovery, and a later normal
// disconnection gets the normal grace again. The clock of the side to move waits for that player,
// RECOVERY_CLOCK_HOLD_MS at most, and the hold replays identically from the journal, from a
// snapshot and after a second crash. A restored game aborted NoShow because its player never came
// back records no conduct incident.

import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { ConfigError, testConfig } from '../../src/config.js';
import { graceFor, recoveryGraceFor, recoveryHoldFor } from '../../src/game/clock.js';
import { GameHost } from '../../src/game/host.js';
import { GameRoom, JournalKind, JournalEvent } from '../../src/game/room.js';
import { FakeChessGame, fakeMove, MemoryJournal, FakeStore, FakeAnticheat, FakePrimary, FakeEndpoint, silentLog } from '../../src/game/testing.js';
import { decode, enums, MSG } from '../../src/protocol/index.js';
import { Registry } from '../../src/metrics.js';
import { openJournal } from '../../src/store/journal.js';

const { GameStatus: GS, EndReason: ER, GameEventKind: EV } = enums;
const W = 0, B = 1;
const CFG = testConfig();
const T0 = 1_800_000_000_000;

const opts = (config = CFG) => ({ config, createChessGame: () => new FakeChessGame() });

// A 5+3 game (normal grace 30 s) whose journal records are collected the way the host appends them.
function journaled(config = CFG) {
    const room = new GameRoom({
        id: 424242, category: '5+3', baseMs: 300000, incMs: 3000, rated: true,
        white: { userId: 5, name: 'white-player', rating: 1720, provisional: false },
        black: { userId: 6, name: 'black-player', rating: 1690, provisional: false },
        createdAt: T0, ...opts(config),
    });
    const log = [room.createdRecord()];
    const run = (out) => { for (const r of out.journal) log.push({ kind: r.kind, at: r.at, payload: Buffer.from(r.payload) }); return out; };
    let t = T0;
    const mv = (dt) => run(room.onMove(room.ply & 1, {
        seq: 1, ply: room.ply, move: fakeMove(room.ply), posHash: room.game.position.digest(), thinkMs: 0, drawOffer: false,
    }, (t += dt)));
    for (let i = 0; i < 6; i++) mv(2000);
    return { room, log, run, t };
}

// Restart: a new process rebuilds the room from the journal and recovers it at `at`.
function restart(log, at, config = CFG) {
    const room = GameRoom.fromJournal(log, opts(config));
    const out = room.recover(at);
    return { room, out, log: [...log, ...out.journal.map((r) => ({ kind: r.kind, at: r.at, payload: Buffer.from(r.payload) }))] };
}

test('recoveryGraceFor: RECOVERY_GRACE_MS, or the normal grace when it is longer', () => {
    assert.equal(recoveryGraceFor(180000, CFG), 90000);
    assert.equal(recoveryGraceFor(5400000, CFG), 90000);             // 90+30: normal grace 60 s
    const short = testConfig({ RECOVERY_GRACE_MS: '20000', RECOVERY_CLOCK_HOLD_MS: '10000' });
    assert.equal(recoveryGraceFor(180000, short), 20000);             // 3+2: normal grace 18 s
    assert.equal(recoveryGraceFor(5400000, short), graceFor(5400000, short));
    assert.equal(graceFor(5400000, short), 60000);
    assert.equal(recoveryGraceFor(180000, {}), 90000);                // default without a configuration
    assert.throws(() => testConfig({ RECOVERY_GRACE_MS: '5000' }), ConfigError);
});

test('recoveryHoldFor: RECOVERY_CLOCK_HOLD_MS, lower than RECOVERY_GRACE_MS', () => {
    assert.equal(CFG.recoveryClockHoldMs, 20000);
    assert.equal(recoveryHoldFor(180000, CFG), 20000);
    assert.equal(recoveryHoldFor(180000, {}), 20000);                 // default without a configuration
    assert.equal(recoveryHoldFor(180000, testConfig({ RECOVERY_CLOCK_HOLD_MS: '0' })), 0);
    assert.equal(recoveryHoldFor(180000, { recoveryGraceMs: 30000, recoveryClockHoldMs: 50000 }), 30000, 'never beyond the grace');
    assert.throws(() => testConfig({ RECOVERY_CLOCK_HOLD_MS: '-1' }), ConfigError);
    assert.equal(testConfig({ RECOVERY_GRACE_MS: '20000', RECOVERY_CLOCK_HOLD_MS: '19999' }).recoveryClockHoldMs, 19999);
});

test('RECOVERY_CLOCK_HOLD_MS: the default follows a short RECOVERY_GRACE_MS down; only a value set at or above the grace is refused', () => {
    // RECOVERY_GRACE_MS alone, anywhere in its documented range, loads: the default hold of 20 s
    // becomes the grace minus 1 ms when the grace is 20 s or less.
    for (const [grace, hold] of [[15000, 14999], [19999, 19998], [20000, 19999], [20001, 20000], [3600000, 20000]]) {
        const cfg = testConfig({ RECOVERY_GRACE_MS: String(grace) });
        assert.equal(cfg.recoveryClockHoldMs, hold, `RECOVERY_GRACE_MS=${grace}`);
        assert.ok(recoveryHoldFor(180000, cfg) < recoveryGraceFor(180000, cfg));
    }
    // An empty value counts as not set.
    assert.equal(testConfig({ RECOVERY_GRACE_MS: '15000', RECOVERY_CLOCK_HOLD_MS: '' }).recoveryClockHoldMs, 14999);
    // A value the operator set is kept as it is, and refused at or above the grace.
    assert.throws(() => testConfig({ RECOVERY_GRACE_MS: '15000', RECOVERY_CLOCK_HOLD_MS: '20000' }), /RECOVERY_CLOCK_HOLD_MS must be lower than RECOVERY_GRACE_MS/);
    assert.throws(() => testConfig({ RECOVERY_GRACE_MS: '20000', RECOVERY_CLOCK_HOLD_MS: '20000' }), /RECOVERY_CLOCK_HOLD_MS must be lower than RECOVERY_GRACE_MS/);
    assert.equal(testConfig({ RECOVERY_GRACE_MS: '15000', RECOVERY_CLOCK_HOLD_MS: '5000' }).recoveryClockHoldMs, 5000);
    assert.equal(testConfig({ RECOVERY_GRACE_MS: '90000', RECOVERY_CLOCK_HOLD_MS: '60000' }).recoveryClockHoldMs, 60000);
});

test('a restart after a shutdown: the disconnections of the drain do not shorten the recovery grace', () => {
    const { room, log, run, t } = journaled();
    // The drain closes both connections (ShuttingDown): two Disconnect records, normal grace 30 s.
    const d = run(room.onDisconnect(W, t + 3000));
    assert.equal(decode(d.broadcast[0]).arg, 30000);
    run(room.onDisconnect(B, t + 3000));
    // The server comes back 45 s later: the normal grace would already be over.
    const R = t + 48000;
    const { room: copy } = restart(log, R);
    assert.equal(copy.isOver, false);
    assert.deepEqual(copy.connected, [false, false]);
    assert.deepEqual(copy.disconnectGrace, [90000, 90000]);
    assert.equal(copy.nextDeadline(), R + 20000, 'the clock hold of the side to move ends first');
    copy.tick(R + 20000);
    assert.equal(copy.nextDeadline(), R + 90000);
    assert.equal(copy.snapshot(W, R).graceMs, 90000);
    copy.tick(R + 89999);
    assert.equal(copy.isOver, false);
    // White comes back after a minute; Black never does: Black abandons when its recovery grace ends.
    copy.onReconnect(W, R + 60000);
    assert.equal(copy.snapshot(W, R + 60000).graceMs, 30000);         // what White waits for
    const out = copy.tick(R + 90000);
    assert.deepEqual([copy.result.status, copy.result.reason], [GS.WhiteWins, ER.Abandonment]);
    assert.deepEqual(out.conduct, [{ userId: 6, kind: 'abandon' }]);
});

test('after coming back, a new disconnection gets the normal grace again', () => {
    const { log, t } = journaled();
    const R = t + 10000;
    const { room } = restart(log, R);
    room.onReconnect(W, R + 1000);
    const d = room.onDisconnect(W, R + 10000);
    const ev = decode(d.broadcast[0]);
    assert.deepEqual([ev.kind, ev.color, ev.arg], [EV.PlayerDisconnected, W, 30000]);
    assert.deepEqual(room.disconnectGrace, [30000, 90000]);
    // Both are away, not together: the first grace to end is White's (R + 40 s, Black's is R + 90 s).
    assert.equal(room.nextDeadline(), R + 40000);
    assert.equal(room.snapshot(B, R + 20000).graceMs, 20000);
    const out = room.tick(R + 40000);
    assert.deepEqual([room.result.status, room.result.reason], [GS.BlackWins, ER.Abandonment]);
    assert.deepEqual(out.conduct, [{ userId: 5, kind: 'abandon' }]);
});

test('both players still away when the recovery grace ends: aborted, unrated', () => {
    const { log, t } = journaled();
    const R = t + 5000;
    const { room } = restart(log, R);
    room.tick(R + 89999);
    assert.equal(room.isOver, false);
    room.tick(R + 90000);
    assert.deepEqual([room.result.status, room.result.reason], [GS.Aborted, ER.BothDisconnected]);
    assert.equal(room.record().rated, false);
});

test('the recovery record keeps the grace; checkpoints carry it; old records and checkpoints still replay', () => {
    const custom = testConfig({ RECOVERY_GRACE_MS: '40000' });
    const { log, t } = journaled(custom);
    const R = t + 7000;
    const first = restart(log, R, custom);
    const rec = first.out.journal[0];
    assert.equal(rec.kind, JournalKind.Event);
    assert.equal(rec.payload.readUInt32LE(8), 40000);
    // Replayed with another configuration: the journaled grace wins (a replay rebuilds the same room).
    const again = GameRoom.fromJournal(first.log, opts(CFG));
    assert.deepEqual(again.disconnectGrace, [40000, 40000]);
    assert.equal(again.nextDeadline(), first.room.nextDeadline());
    // journalState() (created, moves, checkpoint) rebuilds the graces too.
    first.room.onReconnect(B, R + 3000);
    first.room.onDisconnect(B, R + 4000);                             // Black: normal grace, White: recovery grace
    const compact = first.room.journalState();
    const copy = GameRoom.fromJournal(compact, opts(CFG));
    assert.deepEqual(copy.disconnectGrace, first.room.disconnectGrace);
    assert.equal(copy.nextDeadline(), first.room.nextDeadline());
    assert.deepEqual(copy.snapshot(W, R + 5000), first.room.snapshot(W, R + 5000));
    // A 60-byte checkpoint (without the graces) gives both players the normal grace.
    const cp = compact.findIndex((r) => r.kind === JournalKind.Event);
    const old = compact.map((r, i) => (i === cp ? { ...r, payload: r.payload.subarray(0, 60) } : r));
    assert.deepEqual(GameRoom.fromJournal(old, opts(CFG)).disconnectGrace, [30000, 30000]);
    // A recovery record without a grace (arg 0) gives RECOVERY_GRACE_MS of the configuration.
    const zero = first.log.map((r) => ({ ...r, payload: Buffer.from(r.payload) }));
    zero.at(-1).payload.writeUInt32LE(0, 8);
    assert.deepEqual(GameRoom.fromJournal(zero, opts(CFG)).disconnectGrace, [90000, 90000]);
});

// ---- Clock hold after a recovery (RECOVERY_CLOCK_HOLD_MS) ----------------------------------------

const HOLD = 20000;
const pl = (id) => ({ userId: id, name: `user${id}`, rating: 1500, provisional: false });

// Everything a replay must rebuild, and what the players see at `t`.
function fp(room, t) {
    return {
        records: room.journalState().map((r) => [r.kind, r.at, Buffer.from(r.payload).toString('hex')]),
        clockHeld: room.clockHeld, turnStart: room.clock.turnStart, ms: [...room.clock.ms],
        graces: [...room.disconnectGrace], disconnectedAt: [...room.disconnectedAt], connected: [...room.connected],
        next: room.isOver ? null : room.nextDeadline(), w: room.snapshot(W, t), b: room.snapshot(B, t),
    };
}

const moveOf = (room, extra = {}) => ({ seq: 1, ply: room.ply, move: fakeMove(room.ply), posHash: room.game.position.digest(), thinkMs: 0, drawOffer: false, ...extra });

// Records the journal records of outcomes into `log`.
const recorder = (log) => (out) => {
    for (const r of out.journal) log.push({ kind: r.kind, at: r.at, payload: Buffer.from(r.payload) });
    return out;
};

// A 1+0 game at ply 22 whose side to move, White, has less time left than the clock hold: the
// time scramble of a bullet game.
function bullet() {
    const room = new GameRoom({
        id: 515151, category: '1+0', baseMs: 60000, incMs: 0, rated: true, white: pl(7), black: pl(8), createdAt: T0, ...opts(),
    });
    const log = [room.createdRecord()];
    const run = recorder(log);
    let t = T0;
    for (let i = 0; i < 22; i++) run(room.onMove(room.ply & 1, moveOf(room), (t += i & 1 ? 300 : 4500)));
    assert.equal(room.sideToMove, W);
    return { room, log, t, wMs: room.clock.ms[W] };
}

test('the side to move\'s clock waits for its player: back within the hold, its clock starts at the reconnection', () => {
    const g = bullet();
    assert.ok(g.wMs > 5000 && g.wMs + 2000 < HOLD, `White has ${g.wMs} ms left`);
    const R = g.t + 30000;
    const { room, out, log } = restart(g.log, R);
    const run = recorder(log);
    assert.equal(out.journal[0].payload.length, 16);
    assert.equal(out.journal[0].payload.readUInt32LE(12), HOLD, 'the recovery record keeps the hold');
    assert.deepEqual([room.clockHeld, room.clock.turnStart, room.nextDeadline()], [true, R + HOLD, R + HOLD]);
    // Black is back first: White's clock stays stopped, and Black's snapshot shows it stopped.
    assert.equal(run(room.onReconnect(B, R + 1000)).clockStarted, 2);
    const sb = room.snapshot(B, R + 1000 + g.wMs);
    assert.deepEqual([sb.running, sb.whiteMs], [2, g.wMs]);
    // White comes back later than its time left (it would have lost on time without the hold).
    const back = R + g.wMs + 2000;
    const o = run(room.onReconnect(W, back));
    assert.equal(room.isOver, false);
    assert.equal(o.clockStarted, W, 'the host tells Black that White\'s clock runs');
    assert.deepEqual([room.clockHeld, room.clock.turnStart], [false, back]);
    const s = room.snapshot(B, back + 1000);
    assert.deepEqual([s.running, s.whiteMs], [W, g.wMs - 1000]);
    assert.equal(room.nextDeadline(), back + g.wMs + room.clock.compCap(W));
    assert.deepEqual(fp(GameRoom.fromJournal(log, opts()), back + 1000), fp(room, back + 1000), 'the journal replays to the same room');
    // White moves 1.5 s after coming back: charged from its reconnection.
    const m = run(room.onMove(W, moveOf(room, { thinkMs: 1400 }), back + 1500));
    assert.equal(m.moved, true);
    assert.equal(room.spent[22], 1400);
    assert.equal(room.clockAfter[22], g.wMs - 1400);
});

test('the hold ends without its player: the clock runs, a checkpoint is journaled, the replay agrees', () => {
    const g = bullet();
    const R = g.t + 30000;
    const { room, log } = restart(g.log, R);
    const run = recorder(log);
    run(room.onReconnect(B, R + 1000));
    assert.equal(run(room.tick(R + HOLD - 1)).journal.length, 0);
    const o = run(room.tick(R + HOLD));
    assert.equal(o.clockStarted, W);
    assert.deepEqual(o.journal.map((r) => [r.kind, r.at, r.payload[0], r.payload.length]), [[JournalKind.Event, R + HOLD, JournalEvent.Checkpoint, 68]]);
    assert.equal(room.clockHeld, false);
    const s = room.snapshot(B, R + HOLD + 1000);
    assert.deepEqual([s.running, s.whiteMs], [W, g.wMs - 1000]);
    assert.deepEqual(fp(GameRoom.fromJournal(log, opts()), R + HOLD + 1000), fp(room, R + HOLD + 1000), 'the journal replays to the same room');
    // Still away when its time runs out (before the recovery grace): White loses on time.
    const flagAt = R + HOLD + g.wMs + room.clock.compCap(W);
    assert.equal(room.nextDeadline(), flagAt);
    room.tick(flagAt - 1);
    assert.equal(room.isOver, false);
    room.tick(flagAt);
    assert.deepEqual([room.result.status, room.result.reason], [GS.BlackWins, ER.Timeout]);
});

test('a snapshot or compact journal taken during the hold rebuilds the same room, which goes on identically', () => {
    const g = bullet();
    const R = g.t + 30000;
    const first = restart(g.log, R);
    recorder(first.log)(first.room.onReconnect(B, R + 1000));
    const S = R + 5000;
    for (const [label, records] of [['journalState', first.room.journalState()], ['journalSnapshot', [first.room.journalSnapshot(S)]]]) {
        const ref = GameRoom.fromJournal(first.log, opts());
        const copy = GameRoom.fromJournal(records, opts());
        assert.equal(copy.clockHeld, true, label);
        assert.deepEqual(fp(copy, S), fp(ref, S), label);
        for (const at of [R + HOLD, R + HOLD + g.wMs + 150]) {
            const a = ref.tick(at), b = copy.tick(at);
            assert.deepEqual(b.journal.map((r) => [r.kind, r.at, r.payload]), a.journal.map((r) => [r.kind, r.at, r.payload]), `${label} at ${at - R}`);
            assert.deepEqual([b.clockStarted, b.ended, b.broadcast], [a.clockStarted, a.ended, a.broadcast], `${label} at ${at - R}`);
            assert.deepEqual(fp(copy, at), fp(ref, at), `${label} at ${at - R}`);
        }
        assert.deepEqual([copy.result.status, copy.result.reason], [GS.BlackWins, ER.Timeout]);
    }
});

test('a recovered record of an older build (12 bytes) replays without a hold; RECOVERY_CLOCK_HOLD_MS=0 restarts the clock at once', () => {
    const g = bullet();
    const R = g.t + 30000;
    const { log } = restart(g.log, R);
    const old = log.map((r, i) => (i === log.length - 1 ? { ...r, payload: r.payload.subarray(0, 12) } : r));
    const room = GameRoom.fromJournal(old, opts());
    assert.deepEqual([room.clockHeld, room.clock.turnStart, room.disconnectGrace[W]], [false, R, 90000]);
    assert.equal(room.snapshot(B, R + 1000).running, W);
    const none = testConfig({ RECOVERY_CLOCK_HOLD_MS: '0' });
    const r0 = restart(g.log, R, none);
    assert.equal(r0.out.journal[0].payload.readUInt32LE(12), 0);
    assert.deepEqual([r0.room.clockHeld, r0.room.clock.turnStart, r0.room.nextDeadline()], [false, R, R + g.wMs + r0.room.clock.compCap(W)]);
});

test('a restored game at ply 1: no conduct incident when its player never came back, one when it is back and does not move', () => {
    const room0 = new GameRoom({ id: 616161, category: '3+2', baseMs: 180000, incMs: 2000, rated: true, white: pl(3), black: pl(4), createdAt: T0, ...opts() });
    const log = [room0.createdRecord()];
    recorder(log)(room0.onMove(W, moveOf(room0), T0 + 2000));
    const R = T0 + 12000;
    // White is back, Black (to move) never is: its first-move time starts at the end of the hold.
    const a = restart(log, R).room;
    a.onReconnect(W, R + 1000);
    assert.equal(a.nextDeadline(), R + HOLD);
    assert.equal(a.snapshot(W, R + 1000).firstMoveMs, HOLD - 1000 + 30000);
    a.tick(R + HOLD);
    assert.equal(a.nextDeadline(), R + HOLD + 30000);
    const out = a.tick(R + HOLD + 30000);
    assert.deepEqual([a.result.status, a.result.reason], [GS.Aborted, ER.NoShow]);
    assert.deepEqual(out.conduct, [], 'the server broke the connection, not the player');
    assert.equal(a.record().rated, false);
    // Black is back (its first-move time starts then) and does not move: that is a no-show.
    const b = restart(log, R).room;
    b.onReconnect(B, R + 5000);
    assert.equal(b.nextDeadline(), R + 5000 + 30000);
    const out2 = b.tick(R + 35000);
    assert.deepEqual([b.result.status, b.result.reason], [GS.Aborted, ER.NoShow]);
    assert.deepEqual(out2.conduct, [{ userId: 4, kind: 'noshow' }]);
    // A game that was never restored keeps its conduct incident.
    const c = GameRoom.fromJournal(log, opts());
    assert.deepEqual(c.tick(T0 + 2000 + 30000).conduct, [{ userId: 4, kind: 'noshow' }]);
});

test('the first move after a recovery: thinkMs counted from the previous move is not clock_implausible', () => {
    const g = bullet();
    const R = g.t + 30000;
    const at = R + 4000;
    const since = at - g.room.recvTime[21];          // the client's turn began with Black's last move
    const a = restart(g.log, R).room;
    a.onReconnect(W, R + 3000);
    const ok = a.onMove(W, moveOf(a, { thinkMs: since - 50 }), at);
    assert.equal(ok.moved, true);
    assert.equal(ok.anomaly, null);
    assert.equal(a.spent[22], 1000, 'charged from the reconnection');
    const b = restart(g.log, R).room;
    b.onReconnect(W, R + 3000);
    const bad = b.onMove(W, moveOf(b, { thinkMs: since + 101 }), at);
    assert.equal(bad.moved, true);
    assert.equal(bad.anomaly && bad.anomaly.kind, 'clock_implausible', 'longer than the time since the previous move');
});

// A host on `journal` whose clock is `clock.t`.
function hostOn(journal, clock, store = new FakeStore()) {
    return new GameHost({
        shard: 0, config: CFG, store, journal, anticheat: new FakeAnticheat(), primary: new FakePrimary(), log: silentLog,
        createChessGame: () => new FakeChessGame(), now: () => clock.t, metrics: new Registry(), autoStart: false,
    });
}
function move(host, id, extra = {}) {
    const room = host.room(id);
    host.onClientMessage(id, room.playerOf(room.ply & 1).userId, { type: MSG.Move, game: id, ...moveOf(room, extra) }, null);
}

test('GameHost: the opponent sees the held clock stopped, then running once it starts (end of the hold, or reconnection)', () => {
    for (const how of ['hold', 'reconnection']) {
        const journal = new MemoryJournal();
        const clock = { t: T0 };
        const a = hostOn(journal, clock);
        const id = a.createGame({ white: pl(1), black: pl(2), rated: true, baseMs: 180000, incMs: 2000 });
        for (let i = 0; i < 4; i++) { clock.t += 1000; move(a, id); }          // White to move
        const wMs = a.room(id).clock.ms[W];
        clock.t += 10000;
        const R = clock.t;
        const b = hostOn(journal, clock);
        assert.equal(b.recover(), 1);
        const eb = new FakeEndpoint(2);
        clock.t = R + 1000;
        b.attach(id, 2, eb);
        const s0 = eb.msgs().at(-1);
        assert.deepEqual([s0.type, s0.running, s0.whiteMs, s0.whiteConnected], [MSG.GameSnapshot, 2, wMs, false], how);
        eb.clear();
        if (how === 'hold') {
            clock.t = R + HOLD;
            b.runTimers(clock.t + 10);
            const m = eb.msgs();
            assert.deepEqual(m.map((x) => x.type), [MSG.GameSnapshot], how);
            assert.deepEqual([m[0].running, m[0].whiteMs], [W, wMs], how);
        } else {
            const ew = new FakeEndpoint(1);
            clock.t = R + 5000;
            b.attach(id, 1, ew);
            const m = eb.msgs();
            assert.deepEqual(m.map((x) => [x.type, x.kind]), [[MSG.GameEvent, EV.PlayerReconnected], [MSG.GameSnapshot, undefined]], how);
            assert.deepEqual([m[1].running, m[1].whiteMs, m[1].whiteConnected], [W, wMs, true], how);
            const sw = ew.msgs().at(-1);
            assert.deepEqual([sw.type, sw.running, sw.whiteMs], [MSG.GameSnapshot, W, wMs], how);
            b.runTimers(R + HOLD + 10);
            assert.equal(eb.sent.length, 2, 'nothing more at the end of the hold');
        }
        assert.equal(b.room(id).clockHeld, false, how);
    }
});

test('a snapshot taken while the side to move is still away, then a second crash: the hold, the graces and the clocks survive', async () => {
    const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'scacelith-hold-'));
    const copy = fs.mkdtempSync(path.join(os.tmpdir(), 'scacelith-hold-copy-'));
    // segmentBytes 1: every flushed batch goes to its own segment, so compaction starts at once.
    const JOPTS = { shard: 0, log: silentLog, flushMs: 1000, fsync: false, segmentBytes: 1, compactSegments: 1 };
    const clock = { t: T0 };
    try {
        // 1. A 5+3 game at ply 7 (Black to move); then the process dies.
        const j0 = await openJournal({ dir, ...JOPTS });
        const h0 = hostOn(j0, clock);
        const id = h0.createGame({ white: pl(1), black: pl(2), rated: true, baseMs: 300000, incMs: 3000 });
        for (let i = 0; i < 7; i++) { clock.t += 1500; move(h0, id); }
        const clocks = [...h0.room(id).clock.ms];
        await j0.close();

        // 2. Restart at R1: White comes back, Black does not; the game is compacted meanwhile.
        clock.t += 60000;
        const R1 = clock.t;
        const j1 = await openJournal({ dir, ...JOPTS });
        const h1 = hostOn(j1, clock);
        assert.equal(h1.recover(), 1);
        const room = h1.room(id);
        assert.deepEqual([room.clockHeld, room.clock.turnStart], [true, R1 + HOLD]);
        clock.t = R1 + 2000;
        h1.attach(id, 1, new FakeEndpoint(1));
        await j1.flush();
        clock.t = R1 + 4000;
        assert.equal(h1.compactJournal(clock.t), 1, 'the journal asked for a snapshot of the game');
        await j1.flush();

        // 3. A crash at S, during the hold: a copy of the directory is reopened.
        const S = R1 + 6000;
        clock.t = S;
        fs.cpSync(path.join(dir, 'shard-0'), path.join(copy, 'shard-0'), { recursive: true });
        const j2 = await openJournal({ dir: copy, ...JOPTS });
        const records = j2.recover().get(id);
        assert.equal(records[0].kind, JournalKind.Snapshot, 'the game starts from its snapshot');
        const replay = GameRoom.fromJournal(records, opts());
        assert.deepEqual(fp(replay, S), fp(room, S), 'the room as it was at the crash');
        assert.deepEqual([replay.clockHeld, replay.clock.turnStart, replay.clock.ms], [true, R1 + HOLD, clocks]);
        assert.deepEqual([replay.connected, replay.disconnectedAt, replay.disconnectGrace], [[true, false], [R1, R1], [90000, 90000]]);
        assert.equal(replay.snapshot(W, S).graceMs, 90000 - 6000);

        // 4. The second restart, at R2: a new hold and a new grace from R2, the journaled clocks.
        clock.t = S + 30000;
        const R2 = clock.t;
        const h2 = hostOn(j2, clock);
        assert.equal(h2.recover(), 1);
        const r2 = h2.room(id);
        assert.deepEqual([r2.clockHeld, r2.clock.turnStart, r2.clock.ms], [true, R2 + HOLD, clocks]);
        assert.deepEqual([r2.connected, r2.disconnectedAt, r2.disconnectGrace], [[false, false], [R2, R2], [90000, 90000]]);
        assert.equal(r2.nextDeadline(), R2 + HOLD);
        // Black comes back: its clock starts then, with the time it had before the first crash.
        clock.t = R2 + 5000;
        const eb = new FakeEndpoint(2);
        h2.attach(id, 2, eb);
        const s = eb.msgs().at(-1);
        assert.deepEqual([s.type, s.running, s.blackMs, s.graceMs], [MSG.GameSnapshot, B, clocks[B], 90000 - 5000]);
        await j2.close();
        await j1.close();
    } finally {
        fs.rmSync(dir, { recursive: true, force: true });
        fs.rmSync(copy, { recursive: true, force: true });
    }
});
