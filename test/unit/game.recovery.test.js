// Recovered games (DESIGN 6.4, server restart): both players get RECOVERY_GRACE_MS to come back
// instead of the normal grace, the grace is journaled with the recovery, and a later normal
// disconnection gets the normal grace again.

import test from 'node:test';
import assert from 'node:assert/strict';
import { ConfigError, testConfig } from '../../src/config.js';
import { graceFor, recoveryGraceFor } from '../../src/game/clock.js';
import { GameRoom, JournalKind } from '../../src/game/room.js';
import { FakeChessGame, fakeMove } from '../../src/game/testing.js';
import { decode, enums } from '../../src/protocol/index.js';

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
    const short = testConfig({ RECOVERY_GRACE_MS: '20000' });
    assert.equal(recoveryGraceFor(180000, short), 20000);             // 3+2: normal grace 18 s
    assert.equal(recoveryGraceFor(5400000, short), graceFor(5400000, short));
    assert.equal(graceFor(5400000, short), 60000);
    assert.equal(recoveryGraceFor(180000, {}), 90000);                // default without a configuration
    assert.throws(() => testConfig({ RECOVERY_GRACE_MS: '5000' }), ConfigError);
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
