import test from 'node:test';
import assert from 'node:assert/strict';
import { GameRoom, REMATCH_WINDOW_MS, CERTAIN_KINDS, MAX_PLIES, RecordFlag } from '../../src/game/room.js';
import { FakeChessGame, fakeMove } from '../../src/game/testing.js';
import { decode, enums, MSG, MoveFlag } from '../../src/protocol/index.js';
import { testConfig } from '../../src/config.js';

const { GameStatus: GS, EndReason: ER, ErrorCode: EC, GameEventKind: EV, Color } = enums;
const W = 0, B = 1;
const CFG = testConfig();
const T0 = 1_800_000_000_000;
const GRACE = 18000;          // 3+2: clamp(180000 / 10, 15000, 60000)
const CAP0 = 150;             // min(quota 2000, default rtt 100 + 50, LAG_COMP_MAX_MS 1000)

function mkRoom({ script = {}, config = CFG, baseMs = 180000, incMs = 2000, rated = true, autoPress } = {}) {
    return new GameRoom({
        id: 123456789, category: '3+2', baseMs, incMs, rated, autoPress,
        white: { userId: 11, name: 'alice', rating: 1500, provisional: false },
        black: { userId: 22, name: 'bob', rating: 1612, provisional: true },
        createdAt: T0, config, createChessGame: () => new FakeChessGame(script),
    });
}

function mv(room, color, now, extra = {}, recvAt = now) {
    return room.onMove(color, {
        seq: 7, ply: room.ply, move: fakeMove(room.ply), posHash: room.game.position.digest(), thinkMs: 0, drawOffer: false, ...extra,
    }, now, recvAt);
}

const dec = (bufs) => bufs.map((b) => decode(b));
const types = (bufs) => dec(bufs).map((m) => m.type);

// Plays the two first moves (no clock): White at T0 + 1000, Black at T0 + 2000.
function opened(opts) {
    const room = mkRoom(opts);
    mv(room, W, T0 + 1000);
    mv(room, B, T0 + 2000);
    return room;
}

test('first moves run no clock; MoveMade is one buffer for both players', () => {
    const room = mkRoom();
    const o1 = mv(room, W, T0 + 5000, { thinkMs: 4000 });
    assert.equal(o1.broadcast.length, 1);
    assert.equal(o1.reply.length, 0);
    const m1 = decode(o1.broadcast[0]);
    assert.equal(m1.type, MSG.MoveMade);
    assert.deepEqual([m1.gseq, m1.ply, m1.spentMs, m1.whiteMs, m1.blackMs, m1.firstMoveMs, m1.serverTime], [1, 0, 0, 180000, 180000, 30000, T0 + 5000]);
    assert.equal(o1.journal.length, 1);
    const o2 = mv(room, B, T0 + 9000);
    const m2 = decode(o2.broadcast[0]);
    assert.deepEqual([m2.gseq, m2.ply, m2.spentMs, m2.whiteMs, m2.blackMs, m2.firstMoveMs], [2, 1, 0, 180000, 180000, 0]);
    // White's clock starts with Black's first move.
    const s = room.snapshot(W, T0 + 10000);
    assert.equal(s.running, W);
    assert.equal(s.whiteMs, 179000);
    assert.equal(s.blackMs, 180000);
    assert.equal(s.firstMoveMs, 0);
    assert.equal(room.nextDeadline(), T0 + 9000 + 180000 + CAP0);
});

test('clocked move: elapsed charged, increment added, compensation from lag only', () => {
    const room = opened();
    // White thinks 10 s and the move arrives 10.03 s after the turn started: 30 ms of lag.
    const o = mv(room, W, T0 + 2000 + 10030, { thinkMs: 10000 });
    const m = decode(o.broadcast[0]);
    assert.equal(m.spentMs, 10000);
    assert.equal(m.whiteMs, 180000 - 10000 + 2000);
    assert.equal(m.blackMs, 180000);
    assert.equal(room.clock.quota[W], Math.min(2000 - 30 + 100, 3000));
    assert.equal(o.anomaly, null);
    // Black's clock runs from White's move.
    assert.equal(room.snapshot(B, T0 + 12030 + 500).blackMs, 179500);
});

test('lag compensation is bounded by rtt + 50, LAG_COMP_MAX_MS and the quota', () => {
    let room = opened();
    room.onRtt(W, 30);                                    // cap = 80
    let o = mv(room, W, T0 + 2000 + 5000, { thinkMs: 4000 });  // 1000 ms of lag
    assert.equal(decode(o.broadcast[0]).spentMs, 5000 - 80);
    assert.equal(room.clock.quota[W], 2000 - 80 + 100);

    room = opened();
    room.onRtt(W, 5000);                                  // capped at 2000 -> cap = min(2050, 1000, 2000)
    assert.equal(room.clock.rtt[W], 2000);
    o = mv(room, W, T0 + 2000 + 5000, { thinkMs: 0 });
    assert.equal(decode(o.broadcast[0]).spentMs, 5000 - 1000);

    // Quota: 300 ms initially, no gain.
    const cfg = testConfig({ LAG_QUOTA_INITIAL_MS: '300', LAG_QUOTA_GAIN_MS: '0' });
    room = opened({ config: cfg });
    room.onRtt(W, 950); room.onRtt(B, 950);               // rtt cap 1000
    let t = T0 + 2000;
    const spent = [];
    for (let i = 0; i < 3; i++) {
        t += 2000;
        o = mv(room, room.ply & 1, t, { thinkMs: 0 });    // 2000 ms of lag each time
        spent.push(decode(o.broadcast[0]).spentMs);
    }
    // White: comp 300 (whole quota), Black: 300, White again: quota exhausted -> 0.
    assert.deepEqual(spent, [1700, 1700, 2000]);
    assert.equal(room.clock.quota[W], 0);
});

test('thinkMs never adds time; an impossible thinkMs is clock_implausible', () => {
    const room = opened();
    const o = mv(room, W, T0 + 2000 + 3000, { thinkMs: 9000 });
    const m = decode(o.broadcast[0]);
    assert.equal(m.spentMs, 3000, 'charged = elapsed, no compensation');
    assert.equal(o.anomaly.kind, 'clock_implausible');
    assert.equal(o.anomaly.color, W);
    assert.equal(CERTAIN_KINDS.has('clock_implausible'), false);
    // thinkMs = elapsed + 100 is still plausible (clock drift).
    const o2 = mv(room, B, T0 + 5000 + 1000, { thinkMs: 1100 });
    assert.equal(o2.anomaly, null);
    assert.equal(decode(o2.broadcast[0]).spentMs, 1000);
});

test('flag at the exact deadline including the maximal compensation', () => {
    const room = opened();
    const deadline = T0 + 2000 + 180000 + CAP0;
    assert.equal(room.nextDeadline(), deadline);
    assert.equal(room.tick(deadline - 1).broadcast.length, 0);
    const o = room.tick(deadline);
    assert.equal(o.ended, true);
    const end = decode(o.broadcast[0]);
    assert.equal(end.type, MSG.GameEnd);
    assert.deepEqual([end.status, end.reason, end.whiteMs, end.blackMs], [GS.BlackWins, ER.Timeout, 0, 180000]);
    assert.deepEqual(room.result, { status: GS.BlackWins, reason: ER.Timeout, whiteMs: 0, blackMs: 180000, endedAt: deadline });
});

test('a move one millisecond before the deadline counts; at the deadline it is FlagFell', () => {
    let room = opened();
    const deadline = T0 + 2000 + 180000 + CAP0;
    let o = mv(room, W, deadline - 1, { thinkMs: 0 });
    const m = decode(o.broadcast[0]);
    assert.equal(m.type, MSG.MoveMade);
    assert.equal(m.whiteMs, 1 + 2000);

    room = opened();
    o = mv(room, W, deadline, { thinkMs: 0 });
    assert.deepEqual(types(o.broadcast), [MSG.GameEnd]);
    const rej = dec(o.reply);
    assert.equal(rej[0].type, MSG.MoveRejected);
    assert.equal(rej[0].code, EC.FlagFell);
    assert.equal(rej[1].type, MSG.GameSnapshot);
    assert.equal(o.anomaly, null);

    // An honest thinkMs lowers the compensation: flagged before the timer deadline.
    room = opened();
    o = mv(room, W, deadline - 10, { thinkMs: 180000 });
    assert.equal(dec(o.reply)[0].code, EC.FlagFell);
    assert.equal(room.result.reason, ER.Timeout);
});

test('flag against a lone king is a draw (TimeoutVsInsufficient)', () => {
    const room = opened({ script: { canMate: [true, false] } });
    room.tick(T0 + 2000 + 180000 + CAP0);
    assert.deepEqual([room.result.status, room.result.reason], [GS.Draw, ER.TimeoutVsInsufficient]);
});

test('first-move timeout: no-show abort, conduct noshow, unrated record', () => {
    // The deadline has the margin of a flag (CAP0); the countdown shown to the players does not.
    let room = mkRoom();
    assert.equal(room.nextDeadline(), T0 + 30000 + CAP0);
    assert.equal(room.snapshot(W, T0 + 10000).firstMoveMs, 20000);
    assert.equal(room.tick(T0 + 30000 + CAP0 - 1).ended, false);
    let o = room.tick(T0 + 30000 + CAP0);
    assert.equal(o.ended, true);
    assert.deepEqual([room.result.status, room.result.reason], [GS.Aborted, ER.NoShow]);
    assert.deepEqual(o.conduct, [{ userId: 11, kind: 'noshow' }]);
    assert.equal(room.record().rated, false);

    room = mkRoom();
    mv(room, W, T0 + 4000);
    assert.equal(room.nextDeadline(), T0 + 4000 + 30000 + CAP0);
    o = room.tick(T0 + 34000 + CAP0);
    assert.deepEqual(o.conduct, [{ userId: 22, kind: 'noshow' }]);
    assert.equal(room.result.reason, ER.NoShow);
    // A late first move finds the game over.
    const late = mv(room, B, T0 + 34001 + CAP0);
    assert.equal(dec(late.reply)[0].code, EC.GameOver);
});

test('a first move sent in time over a slow link is accepted within the margin, which follows the round trip', () => {
    const room = mkRoom();
    const o = mv(room, W, T0 + 30000 + CAP0 - 1, { thinkMs: 29900 });
    assert.equal(o.moved, true);
    assert.equal(room.nextDeadline(), T0 + 30000 + CAP0 - 1 + 30000 + CAP0);
    room.onRtt(B, 400);                              // cap: min(quota 2000, 400 + 50, 1000)
    assert.equal(room.nextDeadline(), T0 + 30000 + CAP0 - 1 + 30000 + 450);
});

test('double moves, duplicates and stale plies', () => {
    const room = mkRoom();
    const first = mv(room, W, T0 + 1000);
    const original = first.broadcast[0];
    // Same ply, same move (resent after a reconnection): the original MoveMade, to the sender only.
    const dup = room.onMove(W, { seq: 9, ply: 0, move: fakeMove(0), posHash: 0, thinkMs: 0, drawOffer: false }, T0 + 1500);
    assert.equal(dup.duplicate, true);
    assert.equal(dup.broadcast.length, 0);
    assert.equal(dup.reply.length, 1);
    assert.deepEqual(dup.reply[0], original);
    assert.equal(dup.anomaly, null);
    assert.equal(room.gseq, 1);
    // Same ply, other move: stale (info).
    const stale = room.onMove(W, { seq: 10, ply: 0, move: fakeMove(5), posHash: 0, thinkMs: 0, drawOffer: false }, T0 + 1600);
    assert.equal(dec(stale.reply)[0].code, EC.StalePly);
    assert.equal(stale.anomaly.kind, 'stale_ply');
    // White plays again in the synchronised position: out of turn, certain.
    const again = mv(room, W, T0 + 1700);
    assert.equal(again.broadcast.length, 0);
    assert.deepEqual(types(again.reply), [MSG.MoveRejected, MSG.GameSnapshot]);
    assert.equal(dec(again.reply)[0].code, EC.NotYourTurn);
    assert.deepEqual(again.anomaly && [again.anomaly.kind, again.anomaly.posMatched], ['out_of_turn', true]);
    assert.equal(room.ply, 1);
});

test('out of turn with a non-matching posHash is a desync; 3 desyncs are suspicious', () => {
    const room = mkRoom();
    mv(room, W, T0 + 1000);
    const kinds = [];
    for (let i = 0; i < 3; i++) {
        const o = room.onMove(W, { seq: 1, ply: 1, move: fakeMove(1), posHash: 12345, thinkMs: 0, drawOffer: false }, T0 + 2000 + i);
        assert.equal(dec(o.reply)[0].code, EC.Desync);
        assert.equal(dec(o.reply)[1].type, MSG.GameSnapshot);
        assert.equal(o.anomaly.posMatched, false);
        kinds.push(o.anomaly.kind);
    }
    assert.deepEqual(kinds, ['desync', 'desync', 'repeated_desync']);
    // A future ply with the right hash is a desync as well.
    const fut = room.onMove(B, { seq: 1, ply: 5, move: fakeMove(1), posHash: room.game.position.digest(), thinkMs: 0, drawOffer: false }, T0 + 3000);
    assert.equal(dec(fut.reply)[0].code, EC.Desync);
    assert.equal(room.ply, 1);
});

test('illegal move: certain in a synchronised position, desync otherwise; never sent to the opponent', () => {
    const bad = fakeMove(1) ^ 0x40;   // some other bit pattern
    const room = mkRoom({ script: { illegal: [bad] } });
    mv(room, W, T0 + 1000);
    const o = room.onMove(B, { seq: 3, ply: 1, move: bad, posHash: room.game.position.digest(), thinkMs: 0, drawOffer: false }, T0 + 2000);
    assert.equal(o.broadcast.length, 0);
    assert.equal(dec(o.reply)[0].code, EC.IllegalMove);
    assert.deepEqual([o.anomaly.kind, o.anomaly.color, o.anomaly.posMatched], ['illegal_move', B, true]);
    assert.ok(CERTAIN_KINDS.has('illegal_move'));
    const o2 = room.onMove(B, { seq: 4, ply: 1, move: bad, posHash: 1, thinkMs: 0, drawOffer: false }, T0 + 2100);
    assert.equal(dec(o2.reply)[0].code, EC.Desync);
    assert.equal(room.ply, 1);
});

test('promotion bits and flags pass through', () => {
    const room = mkRoom({ script: { flags: { 2: MoveFlag.Capture | MoveFlag.Check } } });
    mv(room, W, T0 + 1000);
    mv(room, B, T0 + 2000);
    const promo = fakeMove(2, 5);
    const o = mv(room, W, T0 + 3000, { move: promo });
    const m = decode(o.broadcast[0]);
    assert.equal(m.move, promo);
    assert.equal(m.flags, MoveFlag.Promotion | MoveFlag.Capture | MoveFlag.Check);
    const s = room.snapshot(B, T0 + 3000);
    assert.equal(s.moves[2].move, promo);
    room.onResign(B, T0 + 4000);
    assert.equal(room.record().moves[2], promo);
});

test('abort only before the own first move; conduct abort; unrated', () => {
    let room = mkRoom();
    mv(room, W, T0 + 1000);
    let o = room.onAbort(W, T0 + 1500, 44);
    const err = dec(o.reply)[0];
    assert.deepEqual([err.type, err.code, err.ref, err.game], [MSG.Error, EC.AbortNotAllowed, 44, room.id]);
    o = room.onAbort(B, T0 + 1600);
    assert.equal(o.ended, true);
    assert.deepEqual([room.result.status, room.result.reason], [GS.Aborted, ER.Aborted]);
    assert.deepEqual(o.conduct, [{ userId: 22, kind: 'abort' }]);
    assert.equal(room.record().rated, false);

    room = mkRoom();
    o = room.onAbort(B, T0 + 100);         // Black before White's first move: allowed
    assert.equal(room.result.reason, ER.Aborted);
    room = opened();
    assert.equal(dec(room.onAbort(B, T0 + 3000).reply)[0].code, EC.AbortNotAllowed);
});

test('resignation; a resignation after the flag deadline loses to the flag', () => {
    let room = opened();
    let o = room.onResign(W, T0 + 5000);
    assert.equal(o.ended, true);
    const end = decode(o.broadcast[0]);
    assert.deepEqual([end.status, end.reason, end.whiteMs], [GS.BlackWins, ER.Resignation, 177000]);
    assert.equal(room.record().rated, true);
    assert.equal(dec(room.onResign(B, T0 + 6000, 5).reply)[0].code, EC.GameOver);

    // White's flag deadline has passed but the timer has not fired yet: Black resigns too late.
    room = opened();
    const deadline = T0 + 2000 + 180000 + CAP0;
    o = room.onResign(B, deadline + 4);
    assert.equal(room.result.reason, ER.Timeout);
    assert.equal(room.result.status, GS.BlackWins);
    assert.deepEqual(types(o.broadcast), [MSG.GameEnd]);
    assert.equal(dec(o.reply)[0].code, EC.GameOver);
});

test('draw offer alone, declined by answer, accepted by answer', () => {
    const room = opened();
    let o = room.onDrawOffer(W, T0 + 3000);
    let ev = decode(o.broadcast[0]);
    assert.deepEqual([ev.type, ev.kind, ev.color, ev.gseq], [MSG.GameEvent, EV.DrawOffered, W, 3]);
    assert.equal(room.snapshot(B, T0 + 3000).drawOffer, W);
    assert.equal(room.onDrawOffer(W, T0 + 3100).broadcast.length, 0, 'already standing');
    assert.equal(dec(room.onDrawAnswer(W, true, T0 + 3200, 3).reply)[0].code, EC.NoPendingOffer);
    o = room.onDrawAnswer(B, false, T0 + 3300);
    ev = decode(o.broadcast[0]);
    assert.deepEqual([ev.kind, ev.color], [EV.DrawDeclined, B]);
    assert.equal(room.drawOffer, Color.None);
    o = room.onDrawOffer(B, T0 + 3400);
    o = room.onDrawAnswer(W, true, T0 + 3500);
    assert.deepEqual([room.result.status, room.result.reason], [GS.Draw, ER.Agreement]);
});

test('a move declines the pending offer; offers made with a move; limits', () => {
    const room = opened();
    // White moves with an offer: MoveMade.drawOffer.
    let o = mv(room, W, T0 + 3000, { drawOffer: true });
    assert.equal(decode(o.broadcast[0]).drawOffer, true);
    assert.equal(room.drawOffer, W);
    // Black moves instead of answering: declined (event after the MoveMade).
    o = mv(room, B, T0 + 4000);
    const [m, ev] = dec(o.broadcast);
    assert.deepEqual([m.type, m.gseq, ev.type, ev.kind, ev.color, ev.gseq], [MSG.MoveMade, 4, MSG.GameEvent, EV.DrawDeclined, B, 5]);
    assert.equal(room.drawOffer, Color.None);
    // White may not offer again before 10 plies after the decline.
    assert.equal(dec(room.onDrawOffer(W, T0 + 4100, 12).reply)[0].code, EC.DrawOfferLimit);
    o = mv(room, W, T0 + 5000, { drawOffer: true, seq: 13 });
    assert.equal(decode(o.broadcast[0]).drawOffer, false, 'the move goes on without the offer');
    const e = dec(o.reply)[0];
    assert.deepEqual([e.type, e.code, e.ref], [MSG.Error, EC.DrawOfferLimit, 13]);
    let t = T0 + 5000;
    while (room.ply < 4 + 10) mv(room, room.ply & 1, (t += 100));
    assert.equal(room.sideToMove, W);
    assert.equal(room.onDrawOffer(W, (t += 100)).broadcast.length, 1, '10 plies later: allowed');
    // Offering while the opponent's offer stands is an agreement.
    o = room.onDrawOffer(B, t + 50);
    assert.deepEqual([room.result.status, room.result.reason], [GS.Draw, ER.Agreement]);
});

test('DRAW_OFFERS_PER_GAME limits the offers of each player', () => {
    const room = opened({ config: testConfig({ DRAW_OFFERS_PER_GAME: '2' }) });
    let t = T0 + 3000;
    for (let i = 0; i < 2; i++) {
        assert.equal(room.onDrawOffer(B, (t += 10)).broadcast.length, 1);
        room.onDrawAnswer(W, false, (t += 10));
        while (room.ply < 2 + 10 * (i + 1)) mv(room, room.ply & 1, (t += 10));
    }
    assert.equal(dec(room.onDrawOffer(B, (t += 10), 99).reply)[0].code, EC.DrawOfferLimit);
    assert.deepEqual(room.drawOffersUsed, [0, 2]);
});

test('draw claims: threefold, fifty moves, nothing to claim', () => {
    let room = opened({ script: { threefoldAt: [4], fiftyAt: [6] } });
    let o = room.onDrawClaim(W, T0 + 3000, 21);
    assert.equal(dec(o.reply)[0].code, EC.NothingToClaim);
    assert.equal(o.anomaly.kind, 'nothing_to_claim');
    assert.equal(CERTAIN_KINDS.has('nothing_to_claim'), false);
    mv(room, W, T0 + 3100); mv(room, B, T0 + 3200);
    o = room.onDrawClaim(W, T0 + 3300);
    assert.deepEqual([room.result.status, room.result.reason], [GS.Draw, ER.ThreefoldClaim]);
    room = opened({ script: { fiftyAt: [4] } });
    mv(room, W, T0 + 3100); mv(room, B, T0 + 3200);
    room.onDrawClaim(B, T0 + 3300);
    assert.equal(room.result.reason, ER.FiftyMoveClaim);
});

test('automatic end from the rules (mate)', () => {
    const room = mkRoom({ script: { endAfter: { 3: { status: GS.WhiteWins, reason: ER.Checkmate } } } });
    mv(room, W, T0 + 1000); mv(room, B, T0 + 2000);
    const o = mv(room, W, T0 + 3000);
    assert.deepEqual(types(o.broadcast), [MSG.MoveMade, MSG.GameEnd]);
    assert.equal(o.ended, true);
    assert.deepEqual([room.result.status, room.result.reason], [GS.WhiteWins, ER.Checkmate]);
    assert.equal(room.nextDeadline(), T0 + 3000 + REMATCH_WINDOW_MS);
});

test('disconnection: reconnection within the grace keeps the game', () => {
    const room = opened();
    let o = room.onDisconnect(B, T0 + 3000);
    const ev = decode(o.broadcast[0]);
    assert.deepEqual([ev.kind, ev.color, ev.arg], [EV.PlayerDisconnected, B, GRACE]);
    assert.equal(room.snapshot(W, T0 + 4000).graceMs, GRACE - 1000);
    assert.equal(room.snapshot(W, T0 + 4000).blackConnected, false);
    assert.equal(room.nextDeadline(), T0 + 3000 + GRACE);
    o = room.onReconnect(B, T0 + 3000 + GRACE - 1);
    assert.equal(decode(o.broadcast[0]).kind, EV.PlayerReconnected);
    assert.equal(room.tick(T0 + 3000 + GRACE + 1).ended, false);
    assert.equal(room.nextDeadline(), T0 + 2000 + 180000 + CAP0);
    // A second disconnect-reconnect is idempotent.
    assert.equal(room.onReconnect(B, T0 + 30000).broadcast.length, 0);
});

test('abandonment after the grace; draw when the opponent cannot mate; no-show before 2 plies', () => {
    let room = opened();
    room.onDisconnect(B, T0 + 3000);
    assert.equal(room.tick(T0 + 3000 + GRACE - 1).ended, false);
    let o = room.tick(T0 + 3000 + GRACE);
    assert.deepEqual([room.result.status, room.result.reason], [GS.WhiteWins, ER.Abandonment]);
    assert.deepEqual(o.conduct, [{ userId: 22, kind: 'abandon' }]);

    room = opened({ script: { canMate: [false, true] } });
    room.onDisconnect(B, T0 + 3000);
    o = room.tick(T0 + 3000 + GRACE);
    assert.deepEqual([room.result.status, room.result.reason], [GS.Draw, ER.AbandonmentVsInsufficient]);
    assert.deepEqual(o.conduct, [{ userId: 22, kind: 'abandon' }]);

    room = mkRoom();
    mv(room, W, T0 + 1000);
    room.onDisconnect(W, T0 + 1500);
    o = room.tick(T0 + 1500 + GRACE);
    assert.deepEqual([room.result.status, room.result.reason], [GS.Aborted, ER.NoShow]);
    assert.deepEqual(o.conduct, [{ userId: 11, kind: 'noshow' }]);

    // The disconnected player's clock keeps running: the flag comes first.
    room = opened({ baseMs: 10000, incMs: 0 });
    room.onDisconnect(W, T0 + 2500);                     // grace 15000
    room.tick(T0 + 2000 + 10000 + CAP0);
    assert.equal(room.result.reason, ER.Timeout);
});

test('both disconnected within 5 s: aborted after the longer grace; otherwise the first one abandons', () => {
    let room = opened();
    room.onDisconnect(W, T0 + 3000);
    room.onDisconnect(B, T0 + 7000);
    assert.equal(room.nextDeadline(), T0 + 7000 + GRACE);
    assert.equal(room.tick(T0 + 3000 + GRACE).ended, false);
    const o = room.tick(T0 + 7000 + GRACE);
    assert.deepEqual([room.result.status, room.result.reason], [GS.Aborted, ER.BothDisconnected]);
    assert.deepEqual(o.conduct, []);
    assert.equal(room.record().rated, false);

    room = opened();
    room.onDisconnect(W, T0 + 3000);
    room.onDisconnect(B, T0 + 8001);
    room.tick(T0 + 3000 + GRACE);
    assert.deepEqual([room.result.status, room.result.reason], [GS.BlackWins, ER.Abandonment]);
});

test('rematch: offer and agreement (colours swapped), decline, expiry, leaving', () => {
    let room = opened();
    assert.equal(dec(room.onRematch(W, true, T0 + 2500, 1).reply)[0].code, EC.RematchUnavailable);
    room.onResign(B, T0 + 3000);
    let o = room.onRematch(B, true, T0 + 4000);
    assert.deepEqual([decode(o.broadcast[0]).kind, decode(o.broadcast[0]).color], [EV.RematchOffered, B]);
    assert.equal(room.snapshot(W, T0 + 4000).rematch, B);
    o = room.onRematch(W, true, T0 + 5000);
    assert.equal(o.broadcast.length, 0);
    assert.deepEqual(o.rematch, {
        gameId: room.id, white: room.black, black: room.white, category: '3+2', baseMs: 180000, incMs: 2000, rated: true, autoPress: true,
    });
    assert.equal(room.nextDeadline(), Infinity);
    assert.equal(dec(room.onRematch(W, true, T0 + 5100, 2).reply)[0].code, EC.RematchUnavailable);

    room = opened();
    room.onResign(B, T0 + 3000);
    room.onRematch(W, true, T0 + 4000);
    o = room.tick(T0 + 3000 + REMATCH_WINDOW_MS);
    const ev = decode(o.broadcast[0]);
    assert.deepEqual([ev.kind, ev.color], [EV.RematchDeclined, Color.None]);
    assert.equal(room.rematchOpen, false);
    assert.equal(room.nextDeadline(), Infinity);

    room = opened();
    room.onResign(B, T0 + 3000);
    room.onRematch(W, true, T0 + 4000);
    o = room.onRematch(B, false, T0 + 4500);
    assert.deepEqual([decode(o.broadcast[0]).kind, decode(o.broadcast[0]).color], [EV.RematchDeclined, B]);
    assert.equal(room.rematchOpen, false);

    room = opened();
    room.onResign(B, T0 + 3000);
    room.onRematch(W, true, T0 + 4000);
    o = room.onDisconnect(B, T0 + 4200);
    assert.equal(decode(o.broadcast[0]).kind, EV.RematchDeclined);
    assert.equal(o.journal.length, 0, 'nothing is journaled after the end');
    assert.equal(room.rematchOpen, false);
});

test('gseq increments on every broadcast event and the snapshot carries it', () => {
    const room = opened();
    room.onDrawOffer(B, T0 + 2500);
    room.onDisconnect(W, T0 + 2600);
    room.onReconnect(W, T0 + 2700);
    const o = mv(room, W, T0 + 3000);
    const seqs = dec(o.broadcast).map((m) => m.gseq);
    assert.deepEqual(seqs, [6, 7]);
    const s = decode(room.snapshotBuffer(B, T0 + 3000));
    assert.equal(s.gseq, 7);
    assert.equal(s.you, B);
    assert.equal(s.moves.length, 3);
    assert.equal(s.white.name, 'alice');
    assert.equal(s.black.rating, 1612);
    assert.equal(s.startedAt, T0);
    assert.equal(s.running, B);
    assert.equal(s.status, GS.Ongoing);
});

test('the game ends ServerAborted at the protocol ply limit', () => {
    const room = mkRoom();
    let t = T0;
    while (!room.isOver) mv(room, room.ply & 1, (t += 1));
    assert.equal(room.ply, MAX_PLIES);
    assert.equal(room.result.reason, ER.ServerAborted);
    assert.equal(room.record().moves.length, MAX_PLIES);
});

test('forfeit and serverAbort', () => {
    let room = opened();
    const o = room.forfeit(B, T0 + 3000);
    assert.deepEqual([room.result.status, room.result.reason], [GS.WhiteWins, ER.Forfeit]);
    assert.equal(o.ended, true);
    assert.equal(room.record().rated, true);
    assert.ok(room.record().flags & 4);
    assert.equal(room.forfeit(W, T0 + 3001).broadcast.length, 0);
    room = opened();
    room.serverAbort(T0 + 3000);
    assert.deepEqual([room.result.status, room.result.reason], [GS.Aborted, ER.ServerAborted]);
    assert.equal(room.rematchOpen, false);
});

test('record() matches the finished-game record of DESIGN 5.5', () => {
    const room = opened();
    mv(room, W, T0 + 4000, { thinkMs: 2000 });
    room.onResign(B, T0 + 5000);
    const r = room.record();
    assert.ok(r.moves instanceof Uint16Array);
    assert.ok(r.spentMs instanceof Uint32Array);
    assert.ok(r.clockMs instanceof Uint32Array);
    assert.deepEqual(Array.from(r.spentMs), [0, 0, 2000]);
    assert.deepEqual(Array.from(r.clockMs), [180000, 180000, 180000]);
    assert.deepEqual(
        [r.id, r.category, r.rated, r.baseMs, r.incMs, r.whiteId, r.blackId, r.whiteName, r.blackName, r.whiteRating, r.blackRating, r.startedAt, r.endedAt, r.status, r.reason, r.rematchOf, r.flags],
        [123456789, '3+2', true, 180000, 2000, 11, 22, 'alice', 'bob', 1500, 1612, T0, T0 + 5000, GS.WhiteWins, ER.Resignation, 0, 1]);
    assert.throws(() => mkRoom().record());
});

test('autoPress: in the snapshot, the created record and the rematch; ManualPress in the record flags', () => {
    let room = opened();
    assert.equal(room.autoPress, true);
    assert.equal(room.snapshot(W, T0 + 3000).autoPress, true);
    room.onResign(B, T0 + 5000);
    assert.equal(room.record().flags & RecordFlag.ManualPress, 0);

    room = opened({ autoPress: false });
    assert.equal(room.snapshot(B, T0 + 3000).autoPress, false);
    assert.equal(decode(room.snapshotBuffer(W, T0 + 3000)).autoPress, false);
    assert.equal(JSON.parse(room.createdRecord().payload.toString('utf8')).autoPress, false);
    room.onResign(B, T0 + 5000);
    assert.equal(room.record().flags, RecordFlag.RatedRequested | RecordFlag.ManualPress);
    room.onRematch(W, true, T0 + 6000);
    const o = room.onRematch(B, true, T0 + 7000);
    assert.equal(o.rematch.autoPress, false, 'a rematch keeps the finished game\'s setting');
});

test('stall credit: a move that waited in a socket is timed from its credited arrival, the next turn starts at once', () => {
    const deadline = T0 + 2000 + 180000 + CAP0;
    const now = deadline + 2000, recvAt = deadline - 500;
    let room = opened();
    const o = mv(room, W, now, { thinkMs: 0 }, recvAt);
    assert.equal(o.moved, true);
    const m = decode(o.broadcast[0]);
    // elapsed 179650 from the credited arrival, all of it lag: compensation = the cap (150).
    assert.deepEqual([m.spentMs, m.whiteMs, m.serverTime], [179500, 180000 - 179500 + 2000, now]);
    assert.deepEqual([room.recvTime[2], room.clock.turnStart], [now, now], 'the stall is charged to nobody');
    assert.equal(room.clock.quota[W], 2000 - CAP0 + 100);
    assert.equal(room.nextDeadline(), now + 180000 + CAP0);
    // Without the credit, the same move flags.
    room = opened();
    assert.equal(dec(mv(room, W, now, { thinkMs: 0 }).reply)[0].code, EC.FlagFell);
    // The arrival is never taken before the latest move (nor after `now`).
    room = opened();
    const early = mv(room, W, T0 + 2500, { thinkMs: 0 }, T0 - 5000);
    assert.equal(decode(early.broadcast[0]).spentMs, 0);
});

test('stall credit: the implausible-thinkMs test keeps the real elapsed time', () => {
    const room = opened();
    // White thinks 6 s; the move waits in a socket from T0 + 5000 (the stall's start, 3 s into the turn) to T0 + 9000.
    const o = mv(room, W, T0 + 9000, { thinkMs: 6000 }, T0 + 5000);
    assert.equal(o.anomaly, null);
    assert.equal(decode(o.broadcast[0]).spentMs, 3000, 'thinkMs is clamped to the credited elapsed time');
    // A thinkMs longer than the real elapsed time is still implausible.
    const o2 = mv(room, B, T0 + 12000, { thinkMs: 5000 }, T0 + 10000);
    assert.equal(o2.anomaly.kind, 'clock_implausible');
});

test('stall credit: a resignation, a draw agreement or an abort that waited beats the flag', () => {
    const deadline = T0 + 2000 + 180000 + CAP0;
    let room = opened();
    let o = room.onResign(B, deadline + 4, 0, deadline - 100);
    assert.deepEqual([room.result.status, room.result.reason, room.result.endedAt], [GS.WhiteWins, ER.Resignation, deadline - 100]);
    assert.equal(o.ended, true);

    room = opened();
    room.onDrawOffer(W, T0 + 3000);
    room.onDrawAnswer(B, true, deadline + 1000, 0, deadline - 1000);
    assert.deepEqual([room.result.status, room.result.reason], [GS.Draw, ER.Agreement]);

    room = mkRoom();
    room.onAbort(W, T0 + 30000 + CAP0 + 500, 0, T0 + 29000);
    assert.deepEqual([room.result.status, room.result.reason], [GS.Aborted, ER.Aborted]);
    // A first move that waited is accepted too.
    room = mkRoom();
    assert.equal(mv(room, W, T0 + 30000 + CAP0 + 2000, { thinkMs: 29000 }, T0 + 29500).moved, true);
});

test('stall credit: a disconnection read after a stall does not let the opponent\'s deadline overtake it', () => {
    const deadline = T0 + 2000 + 180000 + CAP0;
    const room = opened();
    const o = room.onDisconnect(B, deadline + 1000, deadline - 1000);
    assert.equal(o.ended, false);
    assert.equal(decode(o.broadcast[0]).kind, EV.PlayerDisconnected);
    // White's move waited in the same drain: accepted.
    assert.equal(mv(room, W, deadline + 1001, { thinkMs: 0 }, deadline - 1000).moved, true);
    // Resync and reconnection process the deadlines due at the credited arrival only.
    const r = opened();
    assert.equal(r.onResync(B, deadline + 1000, deadline - 1).ended, false);
    assert.equal(r.onResync(B, deadline + 1000).ended, true);
});

test('stall credit: a first-move timeout that fell during a stall aborts without a no-show', () => {
    let room = mkRoom();
    let o = room.tick(T0 + 30000 + CAP0 + 2000, T0 + 30000);
    assert.deepEqual([room.result.status, room.result.reason], [GS.Aborted, ER.NoShow]);
    assert.deepEqual(o.conduct, []);
    // A stall that began after the deadline changes nothing.
    room = mkRoom();
    o = room.tick(T0 + 30000 + CAP0 + 2000, T0 + 30000 + CAP0 + 1);
    assert.deepEqual(o.conduct, [{ userId: 11, kind: 'noshow' }]);
});
