import assert from 'node:assert/strict';
import { describe, it } from 'node:test';
import { Presence, upgradeReserve } from '../../src/cluster/presence.js';

describe('presence', () => {
    it('claims, replaces (kick) and releases only the live connection', () => {
        const p = new Presence();
        assert.deepEqual(p.claim({ userId: 1, username: 'Alice', shard: 0, connId: 10 }), { previous: null });
        assert.equal(p.userIdByName('alice'), 1);
        const r = p.claim({ userId: 1, username: 'Alice', shard: 2, connId: 20 });
        assert.deepEqual(r.previous, { shard: 0, connId: 10 });
        assert.equal(p.release(1, 10, 0), false);                  // the replaced connection closing
        assert.equal(p.get(1).connId, 20);
        assert.equal(p.release(1, 20, 2), true);
        assert.equal(p.get(1), undefined);
        assert.equal(p.userIdByName('Alice'), 0);
        assert.deepEqual(p.claim({ userId: 3, shard: 1, connId: 5 }), { previous: null });
        assert.deepEqual(p.claim({ userId: 3, shard: 1, connId: 5 }), { previous: null });   // same connection again
    });

    it('limits connections per IPv4 address and per IPv6 /64', () => {
        const p = new Presence({ maxPerIp: 2, maxConnections: 100 });
        assert.equal(p.ipAcquire('192.0.2.1', 0).ok, true);
        assert.equal(p.ipAcquire('::ffff:192.0.2.1', 1).ok, true);          // same client through a dual-stack listener
        assert.deepEqual(p.ipAcquire('192.0.2.1', 0), { ok: false, reason: 'per_ip' });
        assert.equal(p.ipAcquire('192.0.2.2', 0).ok, true);
        assert.equal(p.ipAcquire('2001:db8:0:1::1', 0).ok, true);
        assert.equal(p.ipAcquire('2001:db8:0:1:ffff::2', 0).ok, true);
        assert.deepEqual(p.ipAcquire('2001:db8:0:1:abcd:1:2:3', 1), { ok: false, reason: 'per_ip' });
        assert.equal(p.ipAcquire('2001:db8:0:2::1', 1).ok, true);           // another /64
        assert.equal(p.ipRelease('2001:db8:0:1::1', 0), true);
        assert.equal(p.ipAcquire('2001:db8:0:1::99', 0).ok, true);
        assert.equal(p.ipRelease('203.0.113.1', 0), false);                 // never acquired
        // No count per /48 (docs/PROTOCOL.md, README): each /64 of a /48 has the whole limit; the
        // /48 as a whole is bounded by the request budget and, with native TLS, the TLS gate.
        const q = new Presence({ maxPerIp: 2, maxConnections: 100 });
        for (let net = 1; net <= 6; net++) {
            for (let i = 1; i <= 2; i++) assert.equal(q.ipAcquire(`2001:db8:5:${net}::${i}`, 0).ok, true, `/64 ${net}, ${i}`);
        }
        assert.equal(q.connections, 12);
    });

    it('enforces the global limit and forgets a dead shard', () => {
        // Upgrades may go 16 beyond MAX_CONNECTIONS (the reserve of the players with a game in
        // progress; ControlPlane.presenceClaim applies the exact cap at Hello).
        const p = new Presence({ maxPerIp: 10, maxConnections: 3 });
        assert.equal(p.upgradeCap, 3 + 16);
        p.ipAcquire('10.0.0.1', 0);
        p.ipAcquire('10.0.0.2', 1);
        p.ipAcquire('10.0.0.3', 1);
        for (let i = 0; i < 16; i++) assert.equal(p.ipAcquire(`10.0.1.${i}`, 0).ok, true, `reserve ${i}`);
        assert.deepEqual(p.ipAcquire('10.0.0.4', 0), { ok: false, reason: 'global' });
        for (let i = 0; i < 16; i++) p.ipRelease(`10.0.1.${i}`, 0);
        assert.equal(p.connections, 3);
        p.claim({ userId: 1, username: 'a', shard: 1, connId: 1 });
        p.claim({ userId: 2, username: 'b', shard: 0, connId: 1 });
        assert.deepEqual([...p.shardIps.get(1).values()], [1, 1]);
        assert.deepEqual(p.dropShard(1), [1]);
        assert.equal(p.connections, 1);
        assert.equal(p.shardIps.has(1), false);
        assert.equal(p.ipCount('10.0.0.2'), 0);
        assert.equal(p.get(1), undefined);
        assert.equal(p.get(2).shard, 0);
        assert.equal(p.ipAcquire('10.0.0.4', 0).ok, true);
        assert.equal(p.ipRelease('10.0.0.2', 1), false);                    // released with its shard already
    });

    it('sizes the upgrade reserve: max(16, 2 % of MAX_CONNECTIONS)', () => {
        assert.deepEqual([1, 100, 800, 801, 1000, 200000].map(upgradeReserve), [16, 16, 16, 17, 20, 4000]);
        assert.equal(new Presence({ maxConnections: 200000 }).upgradeCap, 204000);
    });
});
