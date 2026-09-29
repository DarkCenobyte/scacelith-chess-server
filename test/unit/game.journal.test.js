import test from 'node:test';
import assert from 'node:assert/strict';
import { GameRoom, JournalKind, JournalEvent, JournalError, RecordFlag, RECOVERY_GSEQ_JUMP } from '../../src/game/room.js';
import { FakeChessGame, fakeMove } from '../../src/game/testing.js';
import { decode, enums, MSG } from '../../src/protocol/index.js';
import { testConfig } from '../../src/config.js';

const { GameStatus: GS, EndReason: ER, GameEventKind: EV } = enums;
const W = 0, B = 1;
const CFG = testConfig();
const T0 = 1_800_000_000_000;

function opts(script = {}) {
    return { config: CFG, createChessGame: () => new FakeChessGame(script) };
}

function mkRoom(script = {}) {
    return new GameRoom({
        id: 987654321, category: '5+3', baseMs: 300000, incMs: 3000, rated: true, rematchOf: 55,
        white: { userId: 5, name: 'white-player', rating: 1720, provisional: false },
        black: { userId: 6, name: 'black-player', rating: 1690, provisional: false },
        createdAt: T0, ...opts(script),
    });
}

// A room that records every journal record the way the host appends them.
function journaled(script) {
    const room = mkRoom(script);
    const log = [room.createdRecord()];
    const run = (out) => { for (const r of out.journal) log.push({ kind: r.kind, at: r.at, payload: Buffer.from(r.payload) }); return out; };
    const mv = (color, t, extra = {}) => run(room.onMove(color, {
        seq: 1, ply: room.ply, move: fakeMove(room.ply), posHash: room.game.position.digest(), thinkMs: 0, drawOffer: false, ...extra,
    }, t));
    return { room, log, run, mv };
}

// Everything a replay must reproduce (the round-trip averages are not journaled).
function state(room, t) {
    return {
        ply: room.ply,
        moves: Array.from(room.moves.subarray(0, room.ply)),
        spent: Array.from(room.spent.subarray(0, room.ply)),
        clockAfter: Array.from(room.clockAfter.subarray(0, room.ply)),
        quotaAfter: Array.from(room.quotaAfter.subarray(0, room.ply)),
        mflags: Array.from(room.mflags.subarray(0, room.ply)),
        recvTime: Array.from(room.recvTime.subarray(0, room.ply)),
        gseq: room.gseq,
        drawOffer: room.drawOffer,
        drawOffersUsed: [...room.drawOffersUsed],
        drawDeclinedAt: [...room.drawDeclinedAt],
        desyncs: [...room.desyncs],
        connected: [...room.connected],
        disconnectedAt: [...room.disconnectedAt],
        clockMs: [...room.clock.ms],
        quota: [...room.clock.quota],
        turnStart: room.clock.turnStart,
        isOver: room.isOver,
        result: room.result,
        flags: room.flags,
        nextDeadline: room.nextDeadline(),
        digest: room.game.position.digest(),
        snapshotW: room.snapshot(W, t),
        snapshotB: room.snapshot(B, t),
    };
}

// A game with every kind of journaled event.
function busyGame() {
    const j = journaled();
    let t = T0;
    j.mv(W, (t += 1200));
    j.mv(B, (t += 2300));
    j.mv(W, (t += 4100), { thinkMs: 4000 });
    j.run(j.room.onDrawOffer(B, (t += 50)));
    j.run(j.room.onDrawAnswer(W, false, (t += 700)));
    j.mv(B, (t += 3000), { thinkMs: 2500, drawOffer: false });
    j.mv(W, (t += 6000), { drawOffer: true });            // offer with the move
    j.run(j.room.onMove(B, { seq: 1, ply: j.room.ply, move: 1, posHash: 99, thinkMs: 0, drawOffer: false }, (t += 10)));  // desync
    j.run(j.room.onDisconnect(B, (t += 100)));
    j.run(j.room.onReconnect(B, (t += 4000)));
    j.mv(B, (t += 500), { thinkMs: 100000 });              // declines White's offer; implausible thinkMs
    j.run(j.room.onDisconnect(W, (t += 20)));
    j.mv(W, (t += 3000));                                  // (moves while "disconnected" happen with relays)
    return { ...j, t };
}

test('replaying the journal rebuilds an identical room', () => {
    const { room, log, t } = busyGame();
    const copy = GameRoom.fromJournal(log, opts());
    assert.deepEqual(state(copy, t + 10), state(room, t + 10));
    // Every record kind was exercised.
    const kinds = new Set(log.map((r) => r.kind));
    assert.deepEqual([...kinds].sort(), [JournalKind.Created, JournalKind.Move, JournalKind.Event]);
});

test('a replayed room behaves identically afterwards (same frames, same records)', () => {
    const a = busyGame();
    const b = GameRoom.fromJournal(a.log, opts());
    let t = a.t;
    const steps = [
        (r) => r.onMove(r.ply & 1, { seq: 1, ply: r.ply, move: fakeMove(r.ply), posHash: r.game.position.digest(), thinkMs: 10, drawOffer: false }, (t += 777)),
        (r) => r.onDrawOffer(B, (t += 5)),
        (r) => r.onMove(r.ply & 1, { seq: 1, ply: r.ply, move: fakeMove(r.ply), posHash: r.game.position.digest(), thinkMs: 0, drawOffer: false }, (t += 3000)),
        (r) => r.onDisconnect(B, (t += 5)),                   // > 5 s after White left
        (r) => r.tick((t += 60000)),                          // White's grace expired first: abandonment
        (r) => r.onRematch(W, true, (t += 5)),
    ];
    for (const step of steps) {
        const t0 = t;
        const oa = step(a.room);
        t = t0;
        const ob = step(b);
        assert.deepEqual(ob.broadcast, oa.broadcast);
        assert.deepEqual(ob.reply, oa.reply);
        assert.deepEqual(ob.journal.map((r) => [r.kind, r.at, r.payload]), oa.journal.map((r) => [r.kind, r.at, r.payload]));
        assert.deepEqual([ob.ended, ob.conduct], [oa.ended, oa.conduct]);
    }
    assert.equal(a.room.result.reason, ER.Abandonment);
    assert.deepEqual(b.record(), a.room.record());
});

test('a finished game replays to the same result and record', () => {
    const j = journaled({ endAfter: { 5: { status: GS.WhiteWins, reason: ER.Checkmate } } });
    let t = T0;
    for (let i = 0; i < 5; i++) j.mv(i & 1, (t += 1000));
    assert.equal(j.room.isOver, true);
    assert.equal(j.log.at(-1).kind, JournalKind.Ended);
    const copy = GameRoom.fromJournal(j.log, opts({ endAfter: { 5: { status: GS.WhiteWins, reason: ER.Checkmate } } }));
    assert.deepEqual(copy.result, j.room.result);
    assert.equal(copy.gseq, j.room.gseq);
    assert.deepEqual(copy.record(), j.room.record());
});

test('journalState() is a compact journal that rebuilds the same room', () => {
    const { room, t } = busyGame();
    const compact = room.journalState();
    assert.equal(compact.filter((r) => r.kind === JournalKind.Event).length, 1, 'one checkpoint');
    const copy = GameRoom.fromJournal(compact, opts());
    assert.deepEqual(state(copy, t + 10), state(room, t + 10));
    room.onResign(W, t + 20);
    const ended = GameRoom.fromJournal(room.journalState(), opts());
    assert.deepEqual(ended.record(), room.record());
    assert.equal(ended.gseq, room.gseq);
});

test('recover(): both players disconnected with the recovery grace, downtime not charged, the clock held', () => {
    const { room, log, t } = busyGame();
    // The side to move is Black; its clock value at the last journaled move:
    assert.equal(room.sideToMove, B);
    const blackMs = room.clock.ms[B];
    const restartAt = t + 3_600_000;                 // the server was down for an hour
    const HOLD = 20000;                              // RECOVERY_CLOCK_HOLD_MS
    const copy = GameRoom.fromJournal(log, opts());
    const out = copy.recover(restartAt);
    assert.equal(out.ended, false);
    assert.equal(out.journal.length, 1);
    assert.equal(copy.gseq, room.gseq + RECOVERY_GSEQ_JUMP);
    assert.deepEqual(copy.connected, [false, false]);
    assert.ok(copy.flags & RecordFlag.Recovered);
    // Black is away: its clock does not run during the hold, and snapshots show it stopped.
    const s = copy.snapshot(W, restartAt + 1000);
    assert.deepEqual([s.blackMs, s.running], [blackMs, 2]);
    assert.equal(s.whiteConnected, false);
    assert.equal(s.blackConnected, false);
    assert.equal(s.graceMs, 90000 - 1000);           // RECOVERY_GRACE_MS (the normal 5+3 grace is 30 s)
    assert.equal(out.journal[0].payload.readUInt32LE(8), 90000, 'the recovery record keeps the grace');
    assert.equal(out.journal[0].payload.readUInt32LE(12), HOLD, 'and the hold');
    assert.equal(copy.nextDeadline(), restartAt + HOLD, 'the end of the hold');
    // The recovery record replays too (before the ticks below end the hold and the game).
    const again = GameRoom.fromJournal([...log, ...out.journal], opts());
    assert.deepEqual(again.connected, [false, false]);
    assert.equal(again.clock.turnStart, restartAt + HOLD);
    assert.equal(again.clockHeld, true);
    assert.equal(again.gseq, room.gseq + RECOVERY_GSEQ_JUMP);
    assert.equal(again.nextDeadline(), restartAt + HOLD);
    // Nobody comes back: Black's clock runs once the hold is over (a journaled checkpoint)...
    const released = copy.tick(restartAt + HOLD);
    assert.equal(released.clockStarted, B);
    assert.deepEqual(released.journal.map((r) => [r.kind, r.payload[0]]), [[JournalKind.Event, 7]]);
    assert.equal(copy.clockHeld, false);
    const s2 = copy.snapshot(W, restartAt + HOLD + 1000);
    assert.deepEqual([s2.blackMs, s2.running], [blackMs - 1000, B]);
    const replayed = GameRoom.fromJournal([...log, ...out.journal, ...released.journal], opts());
    assert.deepEqual(state(replayed, restartAt + HOLD + 1000), state(copy, restartAt + HOLD + 1000));
    // ... then both are aborted (disconnected together) when the recovery grace ends, unrated.
    assert.equal(copy.nextDeadline(), restartAt + 90000);
    copy.tick(restartAt + 90000 - 1);
    assert.equal(copy.isOver, false);
    copy.tick(restartAt + 90000);
    assert.deepEqual([copy.result.status, copy.result.reason], [GS.Aborted, ER.BothDisconnected]);
});

test('recover(): one player comes back, the other abandons', () => {
    const { log, t } = busyGame();
    const copy = GameRoom.fromJournal(log, opts());
    const at = t + 50000;
    copy.recover(at);
    const o = copy.onReconnect(W, at + 2000);
    assert.equal(decode(o.broadcast[0]).kind, EV.PlayerReconnected);
    copy.tick(at + 30000);                           // the normal grace of a 5+3 game: not yet
    assert.equal(copy.isOver, false);
    copy.tick(at + 90000);
    assert.deepEqual([copy.result.status, copy.result.reason], [GS.WhiteWins, ER.Abandonment]);
});

test('recover() ends a game whose ended record was torn off', () => {
    const script = { endAfter: { 3: { status: GS.WhiteWins, reason: ER.Checkmate } } };
    const j = journaled(script);
    let t = T0;
    for (let i = 0; i < 3; i++) j.mv(i & 1, (t += 1000));
    const torn = j.log.filter((r) => r.kind !== JournalKind.Ended);
    const copy = GameRoom.fromJournal(torn, opts(script));
    assert.equal(copy.isOver, false);
    const out = copy.recover(t + 99999);
    assert.equal(out.ended, true);
    assert.deepEqual([copy.result.status, copy.result.reason, copy.result.endedAt], [GS.WhiteWins, ER.Checkmate, t]);
    assert.equal(out.journal[0].kind, JournalKind.Ended);
    assert.equal(copy.rematchOpen, false);
});

test('bad journals: strict replay throws, lenient replay stops at the bad record', () => {
    const { log } = busyGame();
    const bad = log.map((r) => ({ ...r, payload: Buffer.from(r.payload) }));
    const i = bad.findIndex((r, k) => k > 3 && r.kind === JournalKind.Move);
    bad[i].payload.writeUInt16LE(999, 0);            // wrong ply
    assert.throws(() => GameRoom.fromJournal(bad, opts()), JournalError);
    const partial = GameRoom.fromJournal(bad, { ...opts(), strict: false });
    assert.ok(partial.replayError instanceof JournalError);
    assert.equal(partial.ply, bad.slice(0, i).filter((r) => r.kind === JournalKind.Move).length);
    assert.throws(() => GameRoom.fromJournal([{ kind: JournalKind.Created, at: 0, payload: Buffer.from('{oops') }], opts()), JournalError);
    assert.throws(() => GameRoom.fromJournal([], opts()), JournalError);
    assert.throws(() => GameRoom.fromJournal(log.slice(1), opts()), JournalError);
});

test('journal payload formats', () => {
    const j = journaled();
    const created = JSON.parse(j.log[0].payload.toString('utf8'));
    assert.deepEqual(Object.keys(created).sort(), ['baseMs', 'black', 'category', 'createdAt', 'id', 'incMs', 'rated', 'rematchOf', 'v', 'white']);
    j.mv(W, T0 + 1000);
    const mvRec = j.log[1];
    assert.equal(mvRec.kind, JournalKind.Move);
    assert.equal(mvRec.payload.length, 32);
    assert.equal(mvRec.at, T0 + 1000);
    assert.equal(mvRec.payload.readUInt16LE(2), fakeMove(0));
    assert.equal(mvRec.payload.readDoubleLE(24), T0 + 1000);
    j.run(j.room.onDisconnect(B, T0 + 1500));
    assert.equal(j.log[2].payload.length, 12);
    assert.equal(j.log[2].payload.readUInt32LE(8), 30000);
    assert.equal(j.log[2].payload[2], 0);
    // The recovered record: 16 bytes, byte 2 = 1 (each player's first reconnection restarts its
    // first-move timer); a checkpoint's presence byte: 1 White connected, 2 Black connected, 4 clock
    // held, 8 White and 16 Black not back since the recovery.
    const rec = GameRoom.fromJournal(j.log, opts());
    const recovered = rec.recover(T0 + 1800).journal[0].payload;
    assert.deepEqual([recovered.length, recovered[0], recovered[1], recovered[2], recovered[3]], [16, JournalEvent.Recovered, 2, 1, 0]);
    const presence = () => rec.journalState().find((r) => r.kind === JournalKind.Event).payload[2];
    assert.equal(presence(), 4 | 8 | 16);
    rec.onReconnect(W, T0 + 1900);
    assert.equal(presence(), 1 | 4 | 16);
    j.run(j.room.onResign(W, T0 + 2000));
    const end = j.log[3];
    assert.equal(end.kind, JournalKind.Ended);
    assert.equal(end.payload.length, 24);
    assert.deepEqual([end.payload[0], end.payload[1], end.payload[2]], [GS.BlackWins, ER.Resignation, W]);
    // A MoveMade resent from the stored data is byte-identical to the original.
    const room = mkRoom();
    const o = room.onMove(W, { seq: 1, ply: 0, move: fakeMove(0), posHash: room.game.position.digest(), thinkMs: 0, drawOffer: true }, T0 + 10);
    assert.equal(decode(o.broadcast[0]).type, MSG.MoveMade);
    assert.deepEqual(room._encodeMoveMade(0), o.broadcast[0]);
});
