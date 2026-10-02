// Notice{RatingRestored} of the rating refunds (src/anticheat/refund-notices.js) through the
// control plane: out of a game only (at once when the victim is connected and idle, after their
// game ends, or right after Welcome at their next connection unless it resumes a game), marked
// notified only once the shard has written it.

import test from 'node:test';
import assert from 'node:assert/strict';
import { createFakeStore } from '../../src/anticheat/testing/fake-store.js';
import { RefundNotices } from '../../src/anticheat/refund-notices.js';
import { ControlPlane } from '../../src/cluster/control-plane.js';
import { OnceStore, SlidingWindowLimiter } from '../../src/cluster/limits.js';
import { Presence } from '../../src/cluster/presence.js';
import { testConfig } from '../../src/config.js';
import { Challenges } from '../../src/match/challenges.js';
import { Registry } from '../../src/metrics.js';
import { decode, enums, MSG } from '../../src/protocol/index.js';
import { GameIdAllocator } from '../../src/util/ids.js';

const N = enums.NoticeCode;
const cfg = testConfig();
const T = Date.UTC(2026, 8, 1, 12);
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const settle = () => new Promise((r) => setImmediate(r));

// Shards whose 'conn.send' requests answer like the router: { ok } (ready says whether the
// connection has had its Welcome).
class Shards {
    constructor() { this.sent = []; this.ready = new Set(); this.alloc = new GameIdAllocator(0); }
    notify(shard, type, payload) { this.sent.push({ type, payload }); }
    broadcast() {}
    list() { return [0]; }
    async request(shard, type, payload) {
        if (type === 'game.create') return { ok: true, gameId: this.alloc.next() };
        if (type !== 'conn.send') return null;
        if (!this.ready.has(payload.connId)) return { ok: false };
        this.sent.push({ type, payload });
        return { ok: true };
    }
    notices(connId) {
        return this.sent.filter((s) => s.type === 'conn.send' && s.payload.connId === connId)
            .flatMap((s) => s.payload.frames.map(decode)).filter((m) => m.type === MSG.Notice && m.code === N.RatingRestored).map((m) => m.arg);
    }
}

function setup() {
    const store = createFakeStore();
    const shards = new Shards();
    const presence = new Presence({ maxConnections: cfg.maxConnections, maxPerIp: cfg.maxConnectionsPerIp });
    const cp = new ControlPlane({
        config: cfg, presence, matchmaker: { join: () => ({ ok: true }), leave: () => false, has: () => false, tick: () => [] },
        challenges: new Challenges({ config: cfg, now: () => T }), limiter: new SlidingWindowLimiter({ now: () => T }),
        once: new OnceStore({ now: () => T }), shards, refunds: store.refunds, now: () => T, registry: new Registry(),
    });
    const refund = (victimId, points, gameId = victimId * 100 + store._.refunds.length) => {
        store._.refunds.push({ id: 1000 + store._.refunds.length, gameId, victimId, cheaterId: 99, category: '3+2', points, createdAt: T,
            sanctionId: 1, source: 'auto', createdBy: null, notifiedAt: null });
    };
    const connect = (userId, { ready = true } = {}) => {
        const connId = userId * 10 + presence.connections;
        if (ready) shards.ready.add(connId);
        const r = cp.presenceClaim({ userId, username: `u${userId}`, shard: 0, connId, ip: `10.0.0.${userId}` }, 0);
        assert.equal(r.ok, true);
        return { connId, activeGame: r.activeGame };
    };
    return { store, shards, cp, refund, connect };
}

const notified = (store, victimId) => store._.refunds.filter((r) => r.victimId === victimId).map((r) => r.notifiedAt);

test('a connected, idle victim is told at once, with the total of their refunds, then marked notified', async () => {
    const { store, shards, cp, refund, connect } = setup();
    const { connId } = connect(1);
    refund(1, 10);
    refund(1, 5);
    refund(2, 7);                           // offline victim
    cp.sanctionApplied({ userId: 99, until: T + 3600000, reason: 'certain_cheat:illegal_move', refunds: 2 });
    await settle(); await settle();
    assert.deepEqual(shards.notices(connId), [15]);
    assert.deepEqual(notified(store, 1), [T, T]);
    assert.deepEqual(notified(store, 2), [null], 'the offline victim waits');
    // Nothing is sent twice.
    cp.refundNotices.poll();
    await settle();
    assert.deepEqual(shards.notices(connId), [15]);
    cp.stop();
});

test('a victim in a game is told after it ends, not during it', async () => {
    const { store, shards, cp, refund, connect } = setup();
    const a = connect(1), b = connect(2);
    const g = await cp.createGame({ white: { userId: 1 }, black: { userId: 2 }, category: '3+2', baseMs: 180000, incMs: 2000, rated: true }, 0, 'challenge');
    assert.ok(g.ok);
    refund(1, 12);
    cp.refundNotices.poll();
    await settle(); await settle();
    assert.deepEqual(shards.notices(a.connId), [], 'no notice during the game');
    assert.deepEqual(notified(store, 1), [null]);
    cp.gameEnded({ gameId: g.gameId, whiteId: 1, blackId: 2 });
    await settle(); await settle();
    assert.deepEqual(shards.notices(a.connId), [12]);
    assert.deepEqual(shards.notices(b.connId), []);
    assert.deepEqual(notified(store, 1), [T]);
    cp.stop();
});

test('an offline victim is told right after Welcome at the next connection; a connection that resumes a game waits for its end', async () => {
    const { store, shards, cp, refund, connect } = setup();
    refund(3, 8);
    refund(4, 9);
    cp.start();                              // reads the refunds already waiting (an admin command, a restart)
    // Victim 3 connects: the claim is answered before Welcome (not ready: { ok: false }), the
    // notice follows once the connection is ready.
    const c3 = connect(3, { ready: false });
    await sleep(300);
    assert.deepEqual(shards.notices(c3.connId), []);
    assert.deepEqual(notified(store, 3), [null], 'not marked while not written');
    shards.ready.add(c3.connId);
    await sleep(300);
    assert.deepEqual(shards.notices(c3.connId), [8]);
    assert.deepEqual(notified(store, 3), [T]);

    // Victim 4 comes back to a game in progress (after a restart, for example).
    const other = connect(5);
    const g = await cp.createGame({ white: { userId: 4 }, black: { userId: 5 }, category: '3+2', baseMs: 180000, incMs: 2000, rated: true }, 0, 'challenge');
    const c4 = connect(4);
    assert.equal(c4.activeGame, g.gameId);
    await sleep(300);
    assert.deepEqual(shards.notices(c4.connId), []);
    cp.gameEnded({ gameId: g.gameId, whiteId: 4, blackId: 5 });
    await settle(); await settle();
    assert.deepEqual(shards.notices(c4.connId), [9]);
    assert.deepEqual(shards.notices(other.connId), []);
    cp.stop();
});

test('a notice never written: RETRIES tries, then one per poll, a new series at the next connection', async () => {
    const store = createFakeStore();
    store._.refunds.push({ id: 1, gameId: 5, victimId: 7, cheaterId: 9, category: '3+2', points: 4, createdAt: T, sanctionId: null,
        source: 'moderator', createdBy: 'mod', notifiedAt: null });
    let sends = 0;
    const n = new RefundNotices({ refunds: store.refunds, canNotify: () => true, now: () => T, retryMs: 1, send: async () => { sends++; return false; } });
    n.poll();
    await sleep(200);
    assert.equal(sends, 21, 'the first try and RETRIES (20) more');
    n.poll();
    await sleep(100);
    assert.equal(sends, 22, 'one try per poll once the series is used up');
    n.poll();
    await sleep(100);
    assert.equal(sends, 23);
    n.connected(7, null);
    await sleep(200);
    assert.equal(sends, 43, 'a new series of RETRIES after a new connection');
    assert.equal(store._.refunds[0].notifiedAt, null);
    n.stop();
});

test('a notice that is not written is not marked notified and is tried again', async () => {
    const store = createFakeStore();
    store._.refunds.push({ id: 1, gameId: 5, victimId: 7, cheaterId: 9, category: '3+2', points: 4, createdAt: T, sanctionId: null,
        source: 'moderator', createdBy: 'mod', notifiedAt: null });
    let answers = [false, false, true];
    const sent = [];
    const n = new RefundNotices({
        refunds: store.refunds, canNotify: () => true, now: () => T, retryMs: 5,
        send: async (userId, frames) => { sent.push(decode(frames[0]).arg); return answers.shift(); },
    });
    n.poll();
    await sleep(60);
    assert.deepEqual(sent, [4, 4, 4]);
    assert.equal(store._.refunds[0].notifiedAt, T);
    // A send that throws (IPC timeout) is retried too.
    store._.refunds.push({ id: 2, gameId: 6, victimId: 7, cheaterId: 9, category: '3+2', points: 3, createdAt: T, sanctionId: null,
        source: 'moderator', createdBy: 'mod', notifiedAt: null });
    answers = [true];
    let fail = true;
    n.send = async (userId, frames) => { if (fail) { fail = false; throw new Error('timeout'); } sent.push(decode(frames[0]).arg); return answers.shift(); };
    n.poll();
    await sleep(60);
    assert.deepEqual(sent, [4, 4, 4, 3]);
    assert.equal(store._.refunds[1].notifiedAt, T);
    n.stop();
});
