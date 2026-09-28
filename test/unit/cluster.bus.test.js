import assert from 'node:assert/strict';
import crypto from 'node:crypto';
import fs from 'node:fs';
import net from 'node:net';
import os from 'node:os';
import path from 'node:path';
import { after, before, describe, it } from 'node:test';
import { Bus, BusKind, BusOp, busToken, tcpTransport, unixTransport } from '../../src/cluster/bus.js';
import { Registry } from '../../src/metrics.js';

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
async function waitFor(pred, ms = 3000) {
    const t0 = Date.now();
    while (!pred()) {
        if (Date.now() - t0 > ms) throw new Error('condition not reached');
        await sleep(5);
    }
}

function mkBus(shard, dir, token, got, registry = new Registry(), transport) {
    const bus = new Bus({
        shard, token, registry,
        transport: transport || unixTransport({ runDir: dir, serverId: 'test' }),
        onMessage: (kind, from, connId, userId, gameId, payload) => got.push({ kind, from, connId, userId, gameId, payload: Buffer.from(payload) }),
    });
    return bus;
}

describe('shard bus (unix sockets)', () => {
    let dir, token, a, b, gotA, gotB, regA;
    before(async () => {
        dir = fs.mkdtempSync(path.join(os.tmpdir(), 'scacelith-bus-'));
        token = busToken(crypto.randomBytes(32), 'server-1');
        gotA = []; gotB = [];
        regA = new Registry();
        a = mkBus(0, dir, token, gotA, regA);
        b = mkBus(1, dir, token, gotB);
        await a.start();
        await b.start();
    });
    after(async () => { await a.close(); await b.close(); fs.rmSync(dir, { recursive: true, force: true }); });

    it('relays frames with the header fields (id53 game ids included)', async () => {
        const gameId = 2 ** 52 + 12345;
        a.send(1, BusKind.ToHost, 0xfffffff0, 42, gameId, Buffer.from([0x20, 1, 2, 3]));
        await waitFor(() => gotB.length === 1);
        assert.deepEqual({ ...gotB[0], payload: [...gotB[0].payload] }, { kind: BusKind.ToHost, from: 0, connId: 0xfffffff0, userId: 42, gameId, payload: [0x20, 1, 2, 3] });
        assert.ok((fs.statSync(path.join(dir, 'bus-1.sock')).mode & 0o777) === 0o600);
    });

    it('batches the frames of one turn into one write, in order', async () => {
        gotB.length = 0;
        await sleep(20);
        const writes = regA.metrics.get('scacelith_bus_writes_total').root;
        const before = writes.value;
        for (let i = 0; i < 1000; i++) a.send(1, BusKind.ToConn, i, 7, 0, Buffer.from([i & 0xff, i >> 8]));
        await waitFor(() => gotB.length === 1000);
        assert.equal(writes.value - before, 1);
        for (let i = 0; i < 1000; i++) assert.equal(gotB[i].connId, i);
    });

    it('carries control ops with their data, both directions', async () => {
        gotA.length = 0;
        const rtt = Buffer.alloc(2); rtt.writeUInt16LE(87);
        b.control(0, BusOp.Rtt, 5, 6, 77, rtt);
        b.control(0, BusOp.Attach, 5, 6, 77);
        await waitFor(() => gotA.length === 2);
        assert.equal(gotA[0].kind, BusKind.Control);
        assert.equal(gotA[0].payload[0], BusOp.Rtt);
        assert.equal(gotA[0].payload.readUInt16LE(1), 87);
        assert.equal(gotA[1].payload[0], BusOp.Attach);
        assert.equal(gotA[1].from, 1);
    });

    it('reassembles large frames split across reads', async () => {
        gotB.length = 0;
        const big = crypto.randomBytes(300000);
        a.send(1, BusKind.ToConn, 1, 1, 1, big);
        a.send(1, BusKind.ToConn, 2, 1, 1, Buffer.from([9]));
        await waitFor(() => gotB.length === 2);
        assert.ok(gotB[0].payload.equals(big));
        assert.deepEqual([...gotB[1].payload], [9]);
    });

    it('refuses a link with a wrong token', async () => {
        const got = [];
        const intruder = mkBus(2, dir, busToken(crypto.randomBytes(32), 'server-1'), got);
        await intruder.start();
        gotB.length = 0;
        intruder.send(1, BusKind.ToHost, 1, 1, 1, Buffer.from([1]));
        await sleep(100);
        assert.equal(gotB.length, 0);
        await intruder.close();
    });

    it('refuses a link whose first frame is not a hello', async () => {
        gotB.length = 0;
        const s = net.connect(path.join(dir, 'bus-1.sock'));
        await new Promise((r) => s.once('connect', r));
        const f = Buffer.alloc(22);
        f.writeUInt32LE(18, 0); f[4] = BusKind.ToHost;
        s.write(f);
        await new Promise((r) => s.once('close', r));
        assert.equal(gotB.length, 0);
    });

    it('queues while a peer is down and reconnects when it is back', async () => {
        await b.close();
        await waitFor(() => !a.stats().out.find((l) => l.peer === 1).connected);   // frames already in a dying socket are lost (at-most-once)
        gotB.length = 0;
        const got2 = [];
        a.send(1, BusKind.ToHost, 11, 12, 13, Buffer.from([1]));
        a.send(1, BusKind.ToHost, 14, 12, 13, Buffer.from([2]));
        await sleep(150);                                       // a few failed attempts (backoff)
        b = mkBus(1, dir, token, got2);
        await b.start();
        await waitFor(() => got2.length === 2, 5000);
        assert.deepEqual(got2.map((m) => m.connId), [11, 14]);
    });

    it('drops frames beyond the queue bound of an unreachable peer', () => {
        const bus = new Bus({ shard: 5, token, registry: new Registry(), transport: unixTransport({ runDir: dir, serverId: 'test' }), onMessage: () => {}, maxQueueBytes: 1000 });
        let ok = 0;
        for (let i = 0; i < 100; i++) if (bus.send(9, BusKind.ToHost, 1, 1, 1, Buffer.alloc(50))) ok++;
        assert.ok(ok > 0 && ok < 100, `accepted ${ok}`);
        bus.close();
    });
});

describe('shard bus (tcp transport)', () => {
    it('relays over TCP with the same framing', async () => {
        const ports = [];
        for (let i = 0; i < 2; i++) {
            ports.push(await new Promise((resolve) => { const s = net.createServer(); s.listen(0, '127.0.0.1', () => { const p = s.address().port; s.close(() => resolve(p)); }); }));
        }
        const token = busToken(crypto.randomBytes(32), 'x');
        const addressOf = (shard) => ({ host: '127.0.0.1', port: ports[shard] });
        const got = [];
        const a = mkBus(0, null, token, [], new Registry(), tcpTransport({ addressOf, bindHost: '127.0.0.1' }));
        const b = mkBus(1, null, token, got, new Registry(), tcpTransport({ addressOf, bindHost: '127.0.0.1' }));
        await a.start();
        await b.start();
        a.send(1, BusKind.ToConn, 3, 4, 5, Buffer.from('hi'));
        await waitFor(() => got.length === 1);
        assert.equal(got[0].payload.toString(), 'hi');
        await a.close();
        await b.close();
    });
});
