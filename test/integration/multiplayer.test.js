// End-to-end multiplayer scenarios against the real server (bin/scacelith-server.js with two
// shards, TLS, SQLite), driven through the Node SDK: pairing, special moves, duplicates, out of
// turn, illegal moves, cheat sanction, reconnection, resignation, time out, clock tampering,
// simultaneous results, protocol abuse, gesture relay, the clock press across a restart. Needs the
// openssl command line (skipped without it).
import test, { before, after } from 'node:test';
import assert from 'node:assert/strict';
import { startServer, haveOpenssl } from './helpers/harness.js';
import { account, connect, player, challengeGame, queueGame, Table, closeAll } from './helpers/players.js';
import { enums, CloseCode, GestureFlag, MoveFlag as MF, uciToMove } from '../../src/protocol/index.js';

const { GameStatus: GS, EndReason: ER, ErrorCode: EC, GameEventKind: EV } = enums;
const skip = !haveOpenssl() && 'openssl not available';

let srv;
let n = 0;
const uniq = (p) => `${p}${++n}`;

before(async () => {
    if (skip) return;
    srv = await startServer({ workers: 2, env: { FIRST_MOVE_TIMEOUT_MS: '20000', DB_COMMIT_MS: '20' } });
});
after(async () => { if (srv) await srv.stop(); });

const endOf = (p, id, since) => p.client.waitFor('GameEnd', (m) => m.game === id, 20000, since === undefined ? undefined : { since });

test('rated queue: castling, en passant, promotion, resignation; ratings committed and public', { skip }, async () => {
    const a = await player(srv, uniq('ann')), b = await player(srv, uniq('bob'));
    try {
        const g = await queueGame(a, b, '3+2');
        const t = new Table(g);
        const w = g.white.client.mark(), bl = g.black.client.mark();
        await t.playAll(['e2e4', 'a7a6', 'e4e5', 'd7d5']);
        const ep = await t.play('e5d6');
        assert.ok(ep.flags & MF.EnPassant, 'en passant flag');
        await t.playAll(['g8f6', 'g1f3', 'b8c6', 'f1e2', 'a6a5']);
        const castle = await t.play('e1g1');
        assert.ok(castle.flags & MF.CastleKing, 'castle flag');
        await t.playAll(['a5a4', 'd6c7', 'a4a3']);
        const promo = await t.play('c7d8q');
        assert.ok(promo.flags & MF.Promotion, 'promotion flag');
        const since = g.black.client.mark();
        g.black.client.resign(g.id);
        const endB = await endOf(g.black, g.id, since);
        const endW = await endOf(g.white, g.id, w);
        assert.equal(endB.status, GS.WhiteWins);
        assert.equal(endB.reason, ER.Resignation);
        assert.deepEqual([endW.status, endW.reason], [endB.status, endB.reason]);
        const ru = await g.white.client.waitFor('RatingUpdate', (m) => m.game === g.id, 10000, { since: w });
        assert.equal(ru.category, '3+2');
        assert.ok(ru.white.after > ru.white.before && ru.black.after < ru.black.before);
        await g.black.client.waitFor('RatingUpdate', (m) => m.game === g.id, 10000, { since: bl });
        // Public record: the moves as played, the result, both names.
        const rec = await a.api.get(`/games/${g.id}`, { token: null });
        assert.equal(rec.status, 200, JSON.stringify(rec.body));
        const text = JSON.stringify(rec.body);
        assert.ok(text.includes('e5d6') && text.includes('e1g1') && text.includes('c7d8q'), text.slice(0, 400));
        const prof = await a.api.get(`/players/${g.white.name}`, { token: null });
        assert.equal(prof.status, 200);
        assert.ok(JSON.stringify(prof.body).includes('3+2'));
    } finally { await closeAll(a, b); }
});

test('checkmate ends the game for both players with the same result', { skip }, async () => {
    const a = await player(srv, uniq('cat')), b = await player(srv, uniq('dan'));
    try {
        const g = await challengeGame(a, b, { baseSec: 300, incSec: 0 });
        const t = new Table(g);
        const ma = a.client.mark(), mb = b.client.mark();
        await t.playAll(['f2f3', 'e7e5', 'g2g4']);
        t.send('d8h4');
        const [ea, eb] = await Promise.all([endOf(a, g.id, ma), endOf(b, g.id, mb)]);
        assert.deepEqual([ea.status, ea.reason], [GS.BlackWins, ER.Checkmate]);
        assert.deepEqual([eb.status, eb.reason], [GS.BlackWins, ER.Checkmate]);
    } finally { await closeAll(a, b); }
});

test('a resent move is idempotent; a stale ply and a desynchronised move are refused, not relayed', { skip }, async () => {
    const a = await player(srv, uniq('eve')), b = await player(srv, uniq('fay'));
    try {
        const g = await challengeGame(a, b);
        const t = new Table(g);
        await t.play('e2e4');
        // Duplicate: White resends its ply-0 move; nothing new is played.
        const ma = a.client.mark();
        t.send('e2e4', { by: a, ply: 0, hash: new Table(g).hash });
        const again = await a.client.waitFor('MoveMade', (m) => m.ply === 0, 3000, { since: ma });
        assert.equal(again.move, uciToMove('e2e4'));
        // Stale ply (Black answers for ply 0): refused with StalePly.
        const s1 = b.client.mark();
        b.client.move(g.id, 0, uciToMove('e7e5'), t.hash, 0, false);
        const rej = await b.client.waitFor('MoveRejected', null, 3000, { since: s1 });
        assert.equal(rej.code, EC.StalePly);
        // Desync (not the player's turn and a position it does not have): refused, connection kept.
        const s2 = a.client.mark();
        t.send('d2d4', { by: a, ply: 1, hash: 12345 });
        const rej2 = await a.client.waitFor('MoveRejected', null, 3000, { since: s2 });
        assert.equal(rej2.code, EC.Desync);
        await a.client.waitFor('GameSnapshot', (m) => m.game === g.id, 3000, { since: s2 });
        // The opponent saw none of it: its only MoveMade is ply 0.
        await t.play('e7e5');
        assert.equal(b.client.games.get(g.id).moves.length, 2);
        assert.equal(a.client.state, 'ready');
    } finally { await closeAll(a, b); }
});

test('an illegal move in a synchronised position is a certain cheat: forfeit, close 4302, ban', { skip }, async () => {
    const cheater = await player(srv, uniq('gil')), honest = await player(srv, uniq('hal'));
    try {
        const g = await challengeGame(cheater, honest);
        const t = new Table(g);
        await t.playAll(['e2e4', 'e7e5']);
        const mh = honest.client.mark();
        const closed = cheater.client.waitFor('close', null, 5000);
        t.send('e1e3');                                 // the king cannot move two squares up
        const c = await closed;
        assert.equal(c.code, CloseCode.CheatDetected);
        const end = await endOf(honest, g.id, mh);
        assert.deepEqual([end.status, end.reason], [GS.BlackWins, ER.Forfeit]);
        // The move was never relayed.
        assert.equal(honest.client.games.get(g.id).moves.length, 2);
        // Banned: the next connection is refused.
        await assert.rejects(connect(srv, cheater.token), (e) => e.closeCode === CloseCode.Banned || e.errorCode === EC.Banned);
    } finally { await closeAll(cheater, honest); }
});

test('out of turn with the right position is a certain cheat too', { skip }, async () => {
    const cheater = await player(srv, uniq('ida')), honest = await player(srv, uniq('jon'));
    try {
        const g = await challengeGame(honest, cheater);   // cheater is Black
        const t = new Table(g);
        await t.play('d2d4');
        await t.play('d7d5');
        const mh = honest.client.mark();
        const closed = cheater.client.waitFor('close', null, 5000);
        t.send('e7e6', { by: cheater });                  // White to move
        assert.equal((await closed).code, CloseCode.CheatDetected);
        const end = await endOf(honest, g.id, mh);
        assert.deepEqual([end.status, end.reason], [GS.WhiteWins, ER.Forfeit]);
    } finally { await closeAll(cheater, honest); }
});

test('connection lost mid-game: the opponent is told, the player reconnects and resumes', { skip }, async () => {
    const a = await player(srv, uniq('kim')), b = await player(srv, uniq('lou'));
    try {
        const g = await challengeGame(a, b);
        const t = new Table(g);
        await t.playAll(['e2e4', 'c7c5', 'g1f3']);
        const mb = b.client.mark();
        a.client.ws.terminate();                          // abrupt loss, no close frame
        const ev = await b.client.waitFor('GameEvent', (m) => m.kind === EV.PlayerDisconnected, 5000, { since: mb });
        assert.equal(ev.color, 0);
        assert.ok(ev.arg >= 15000, `grace ${ev.arg}`);
        // Black keeps playing while White is away.
        const mb2 = b.client.mark();
        t.send('d7d6');
        await b.client.waitFor('MoveMade', (m) => m.ply === 3, 3000, { since: mb2 });
        t.rules.play(uciToMove('d7d6'));
        // White comes back: Welcome names the game, a snapshot carries all 4 moves.
        const c2 = await connect(srv, a.token);
        assert.equal(c2.welcome.activeGame, g.id);
        const snap = await c2.waitFor('GameSnapshot', (m) => m.game === g.id, 5000, { since: 0 });
        assert.equal(snap.moves.length, 4);
        assert.equal(snap.whiteConnected, true);
        await b.client.waitFor('GameEvent', (m) => m.kind === EV.PlayerReconnected, 5000, { since: mb2 });
        a.client = c2;
        await t.play('d2d4');
    } finally { await closeAll(a, b); }
});

test('a second connection replaces the first (4007) and gets the game', { skip }, async () => {
    const a = await player(srv, uniq('max')), b = await player(srv, uniq('ned'));
    try {
        const g = await challengeGame(a, b);
        const old = a.client;
        const closed = old.waitFor('close', null, 5000);
        const c2 = await connect(srv, a.token);
        assert.equal((await closed).code, CloseCode.Replaced);
        assert.equal(c2.welcome.activeGame, g.id);
        a.client = c2;
        await c2.waitFor('GameSnapshot', (m) => m.game === g.id, 5000, { since: 0 });
        await new Table(g).play('e2e4');
    } finally { await closeAll(a, b); }
});

test('flag fall: the side whose clock runs out loses on time (server clock)', { skip }, async () => {
    const a = await player(srv, uniq('oli')), b = await player(srv, uniq('pat'));
    try {
        const g = await challengeGame(a, b, { baseSec: 15, incSec: 0 });
        const t = new Table(g);
        const ma = a.client.mark();
        await t.playAll(['e2e4', 'e7e5']);              // White's clock starts with Black's first move
        const end = await endOf(a, g.id, ma);            // nobody moves: 15 s later White's flag falls
        assert.deepEqual([end.status, end.reason], [GS.BlackWins, ER.Timeout]);
        assert.equal(end.whiteMs, 0);
    } finally { await closeAll(a, b); }
});

test('a forged think time gains nothing: the server charges the time it measured', { skip }, async () => {
    const a = await player(srv, uniq('quin')), b = await player(srv, uniq('rae'));
    try {
        const g = await challengeGame(a, b, { baseSec: 60, incSec: 0 });
        const t = new Table(g);
        await t.playAll(['e2e4', 'e7e5']);
        await new Promise((r) => setTimeout(r, 1500));
        const made = await t.play('g1f3', { thinkMs: 1 });   // claims 1 ms after 1.5 s of thinking
        assert.ok(made.spentMs >= 1000, `charged ${made.spentMs} ms`);
        assert.ok(made.whiteMs <= 59000, `white has ${made.whiteMs} ms`);
    } finally { await closeAll(a, b); }
});

test('both players resign at the same moment: one result, the same for both', { skip }, async () => {
    const a = await player(srv, uniq('sam')), b = await player(srv, uniq('tia'));
    try {
        const g = await challengeGame(a, b);
        const t = new Table(g);
        await t.playAll(['e2e4', 'e7e5']);
        const ma = a.client.mark(), mb = b.client.mark();
        a.client.resign(g.id);
        b.client.resign(g.id);
        const [ea, eb] = await Promise.all([endOf(a, g.id, ma), endOf(b, g.id, mb)]);
        assert.equal(ea.reason, ER.Resignation);
        assert.deepEqual([ea.status, ea.reason, ea.gseq], [eb.status, eb.reason, eb.gseq]);
        await new Promise((r) => setTimeout(r, 300));
        const ends = (c) => c.games.get(g.id).status;
        assert.equal(ends(a.client), ends(b.client));
    } finally { await closeAll(a, b); }
});

test('draw offer and acceptance', { skip }, async () => {
    const a = await player(srv, uniq('uma')), b = await player(srv, uniq('vic'));
    try {
        const g = await challengeGame(a, b);
        const t = new Table(g);
        await t.playAll(['e2e4', 'e7e5']);
        const mb = b.client.mark(), ma = a.client.mark();
        a.client.offerDraw(g.id);
        await b.client.waitFor('GameEvent', (m) => m.kind === EV.DrawOffered, 3000, { since: mb });
        b.client.answerDraw(g.id, true);
        const end = await endOf(a, g.id, ma);
        assert.deepEqual([end.status, end.reason], [GS.Draw, ER.Agreement]);
    } finally { await closeAll(a, b); }
});

test('protocol abuse: garbage, text frames and floods close the connection; the server stays up', { skip }, async () => {
    const acc = await account(srv, uniq('wes'));
    try {
        let c = await connect(srv, acc.token);
        let closed = c.waitFor('close', null, 5000);
        c.sendRaw(Buffer.from([0xee, 1, 2, 3]));
        assert.ok([CloseCode.ProtocolViolation, CloseCode.ProtocolError].includes((await closed).code));

        c = await connect(srv, acc.token);
        closed = c.waitFor('close', null, 5000);
        c.sendText('hello');
        assert.equal((await closed).code, CloseCode.Unsupported);

        c = await connect(srv, acc.token);
        closed = c.waitFor('close', null, 10000);
        for (let i = 0; i < 400; i++) c.send('Ping', { nonce: i, clientTime: Date.now() });
        assert.equal((await closed).code, CloseCode.Flood);

        const info = await acc.api.info();
        assert.equal(info.status, 200);
        const c2 = await connect(srv, acc.token);
        await c2.close();
    } finally { await closeAll(acc); }
});

test('wrong protocol schema and bad token are refused at Hello', { skip }, async () => {
    const acc = await account(srv, uniq('xan'));
    try {
        await assert.rejects(connect(srv, acc.token, { schema: 1 }), (e) => e.errorCode === EC.UnsupportedProtocol || e.closeCode === CloseCode.UnsupportedProtocol);
        await assert.rejects(connect(srv, 'sct_' + 'A'.repeat(43)), (e) => e.errorCode === EC.Unauthorized || e.closeCode === CloseCode.Unauthorized);
    } finally { await closeAll(acc); }
});

test('gestures are relayed live to the opponent only, never echoed, and moves stay in step', { skip }, async () => {
    const a = await player(srv, uniq('gus')), b = await player(srv, uniq('ida'));
    try {
        assert.deepEqual([a.client.welcome.gestureRate, a.client.welcome.gestureBurst], [4, 8]);
        const g = await challengeGame(a, b);
        assert.equal(a.client.games.get(g.id).autoPress, true, 'AUTO_PRESS_CLOCK default');
        const t = new Table(g);
        const ma = a.client.mark(), mb = b.client.mark();
        a.client.gesture(g.id, { ply: 0, touch: 12, aim: 28, yaw: -300, pitch: 100, lean: 20, flags: GestureFlag.Glance });
        const got = await b.client.waitFor('Gesture', (m) => m.game === g.id, 5000, { since: mb });
        assert.deepEqual([got.ply, got.touch, got.aim, got.placed, got.flags, got.yaw, got.pitch, got.lean], [0, 12, 28, 0, 1, -300, 100, 20]);
        b.client.gesture(g.id, { ply: 0, yaw: 450, flags: GestureFlag.Side });
        const back = await a.client.waitFor('Gesture', (m) => m.game === g.id, 5000, { since: ma });
        assert.deepEqual([back.yaw, back.flags, back.touch], [450, GestureFlag.Side, 64]);
        await t.playAll(['e2e4', 'e7e5']);
        await assert.rejects(a.client.waitFor('Gesture', (m) => m.yaw === -300, 300, { since: ma }), /timeout/, 'never echoed to the sender');
        assert.equal(a.client.state, 'ready');
        assert.equal(b.client.state, 'ready');
    } finally { await closeAll(a, b); }
});

test('a restored game keeps its clock press when AUTO_PRESS_CLOCK changed across the restart', { skip }, async () => {
    const own = await startServer({ workers: 2, keep: true, env: { AUTO_PRESS_CLOCK: 'false' } });
    let s2 = null, c = null, d = null;
    const a = await player(own, uniq('jon')), b = await player(own, uniq('kim'));
    try {
        const g = await challengeGame(a, b);
        assert.equal(a.client.games.get(g.id).autoPress, false);
        const t = new Table(g);
        await t.playAll(['d2d4', 'd7d5']);
        await new Promise((r) => setTimeout(r, 200));      // > JOURNAL_FLUSH_MS
        await own.stop();
        s2 = await startServer({ workers: 2, dataDir: own.dir, env: { AUTO_PRESS_CLOCK: 'true' } });
        const ca = await connect(s2, a.token), cb = await connect(s2, b.token);
        const snap = await ca.waitFor('GameSnapshot', (m) => m.game === g.id, 5000, { since: 0 });
        assert.deepEqual([snap.moves.length, snap.autoPress], [2, false], 'the journaled setting wins');
        a.client = ca; b.client = cb;
        await cb.waitFor('GameSnapshot', (m) => m.game === g.id, 5000, { since: 0 });
        const m = ca.mark();
        ca.resign(g.id);
        await ca.waitFor('GameEnd', (e) => e.game === g.id, 5000, { since: m });
        // A new game follows the new setting.
        c = await player(s2, uniq('lea')); d = await player(s2, uniq('max'));
        const g2 = await challengeGame(c, d);
        assert.equal(c.client.games.get(g2.id).autoPress, true);
    } finally {
        await closeAll(a, b, c, d);
        if (s2) await s2.stop();
        const fs = await import('node:fs');
        fs.rmSync(own.dir, { recursive: true, force: true });
    }
});

test('server crash (SIGKILL) mid-game: after the restart both players get the game back', { skip }, async () => {
    const own = await startServer({ workers: 2, keep: true });
    let s2 = null;
    const a = await player(own, uniq('yul')), b = await player(own, uniq('zed'));
    try {
        const g = await challengeGame(a, b);
        const t = new Table(g);
        await t.playAll(['e2e4', 'e7e5', 'g1f3', 'b8c6']);
        await new Promise((r) => setTimeout(r, 200));      // > JOURNAL_FLUSH_MS
        await own.crash();
        s2 = await startServer({ workers: 2, dataDir: own.dir });
        const ca = await connect(s2, a.token), cb = await connect(s2, b.token);
        assert.equal(ca.welcome.activeGame, g.id);
        const snap = await ca.waitFor('GameSnapshot', (m) => m.game === g.id, 5000, { since: 0 });
        assert.equal(snap.moves.length, 4);
        a.client = ca; b.client = cb;
        await cb.waitFor('GameSnapshot', (m) => m.game === g.id, 5000, { since: 0 });
        await t.play('f1b5');
    } finally {
        await closeAll(a, b);
        if (s2) await s2.stop();
        const fs = await import('node:fs');
        fs.rmSync(own.dir, { recursive: true, force: true });
    }
});
