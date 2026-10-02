import assert from 'node:assert/strict';
import net from 'node:net';
import { after, before, beforeEach, describe, it } from 'node:test';
import { Registry } from '../../src/metrics.js';
import { WsServer, isValidCloseCode } from '../../src/net/ws.js';
import { connectWs, encodeClientFrame } from '../../src/net/ws-raw-client.js';

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

describe('ws frames', () => {
    let srv, port, wss, registry;
    let received, closes, conns;
    before(async () => {
        registry = new Registry();
        wss = new WsServer({
            registry, maxMessageBytes: 512, sendBufferLimit: 8192, closeTimeoutMs: 300,
            onConnection: (c) => {
                conns.push(c);
                c.onMessage = (cn, buf) => { received.push(Buffer.from(buf)); if (buf[0] === 0xee) cn.sendFrame(Buffer.from(buf)); };
                c.onClose = (cn, code) => closes.set(cn.id, code);
            },
        });
        srv = net.createServer((s) => wss.handleSocket(s));
        await new Promise((r) => srv.listen(0, '127.0.0.1', r));
        port = srv.address().port;
    });
    beforeEach(() => { received = []; closes = new Map(); conns = []; });
    after(() => { wss.closeAll(); srv.close(); });

    async function waitFor(pred, ms = 2000) {
        const t0 = Date.now();
        while (!pred()) {
            if (Date.now() - t0 > ms) throw new Error('condition not reached');
            await sleep(5);
        }
    }

    it('delivers binary messages and echoes', async () => {
        const c = await connectWs({ port });
        c.send(Buffer.from([0xee, 1, 2]));
        assert.deepEqual([...await c.next()], [0xee, 1, 2]);
        await waitFor(() => received.length === 1);
        c.destroy();
    });

    it('delivers many frames written at once and frames split byte by byte', async () => {
        const c = await connectWs({ port });
        const frames = [];
        for (let i = 0; i < 50; i++) frames.push(encodeClientFrame(2, Buffer.from([i, i + 1, i + 2])));
        c.sendRaw(Buffer.concat(frames));
        await waitFor(() => received.length === 50);
        assert.deepEqual([...received[49]], [49, 50, 51]);
        const big = Buffer.alloc(300, 7);
        const f = encodeClientFrame(2, big);                 // 16-bit length header
        for (let i = 0; i < f.length; i++) { c.sendRaw(f.subarray(i, i + 1)); if (i % 40 === 0) await sleep(1); }
        await waitFor(() => received.length === 51);
        assert.equal(received[50].length, 300);
        assert.ok(received[50].every((b) => b === 7));
        c.destroy();
    });

    it('requires masking (1002)', async () => {
        const c = await connectWs({ port });
        c.sendFrame(2, Buffer.from([1]), { mask: false });
        assert.equal((await c.closed).code, 1002);
        assert.equal(received.length, 0);
    });

    it('rejects RSV bits (no extension is negotiated) (1002)', async () => {
        const c = await connectWs({ port });
        c.sendFrame(2, Buffer.from([1]), { rsv: 4 });
        assert.equal((await c.closed).code, 1002);
    });

    it('rejects text frames (1003)', async () => {
        const c = await connectWs({ port });
        c.sendFrame(1, Buffer.from('hello'));
        assert.equal((await c.closed).code, 1003);
        assert.equal(received.length, 0);
    });

    it('rejects reserved opcodes (1002)', async () => {
        const c = await connectWs({ port });
        c.sendFrame(3, Buffer.from([1]));
        assert.equal((await c.closed).code, 1002);
    });

    it('checks the size from the header before buffering (1009)', async () => {
        const c = await connectWs({ port });
        // Only the header of a 1 MB frame: the server must close without waiting for the payload.
        const header = Buffer.from([0x82, 0x80 | 127, 0, 0, 0, 0, 0, 0x10, 0, 0, 1, 2, 3, 4]);
        c.sendRaw(header);
        assert.equal((await c.closed).code, 1009);
        const c2 = await connectWs({ port });
        c2.sendRaw(Buffer.from([0x82, 0x80 | 126, 0x02, 0x01, 1, 2, 3, 4]));      // 513 bytes announced
        assert.equal((await c2.closed).code, 1009);
    });

    it('accepts a message of exactly the limit', async () => {
        const c = await connectWs({ port });
        c.send(Buffer.alloc(512, 1));
        await waitFor(() => received.length === 1);
        assert.equal(received[0].length, 512);
        c.destroy();
    });

    it('rejects non-minimal lengths and lengths above 2^53 (1002)', async () => {
        const c = await connectWs({ port });
        c.sendRaw(Buffer.from([0x82, 0x80 | 126, 0, 5, 1, 2, 3, 4, 0, 0, 0, 0, 0]));
        assert.equal((await c.closed).code, 1002);
        const c2 = await connectWs({ port });
        c2.sendRaw(Buffer.from([0x82, 0x80 | 127, 0xff, 0, 0, 0, 0, 0, 0, 1, 1, 2, 3, 4]));
        assert.equal((await c2.closed).code, 1002);
    });

    it('reassembles fragmented messages within the limit, with interleaved control frames', async () => {
        const c = await connectWs({ port });
        c.sendFrame(2, Buffer.from([1, 2]), { fin: false });
        c.sendFrame(9, Buffer.from('p'));
        c.sendFrame(0, Buffer.from([3, 4]), { fin: false });
        c.sendFrame(0, Buffer.from([5]), { fin: true });
        await waitFor(() => received.length === 1 && c.pongs.length === 1);
        assert.deepEqual([...received[0]], [1, 2, 3, 4, 5]);
        assert.equal(c.pongs[0].toString(), 'p');
        c.destroy();
    });

    it('limits the sum of the fragments (1009) and their number (1009)', async () => {
        const c = await connectWs({ port });
        c.sendFrame(2, Buffer.alloc(300), { fin: false });
        c.sendFrame(0, Buffer.alloc(300), { fin: true });
        assert.equal((await c.closed).code, 1009);
        assert.equal(received.length, 0);
        const c2 = await connectWs({ port });
        c2.sendFrame(2, Buffer.alloc(0), { fin: false });
        for (let i = 0; i < 70; i++) c2.sendFrame(0, Buffer.alloc(0), { fin: false });
        assert.equal((await c2.closed).code, 1009);
    });

    it('rejects a continuation without a start and a new message inside a fragmented one (1002)', async () => {
        const c = await connectWs({ port });
        c.sendFrame(0, Buffer.from([1]));
        assert.equal((await c.closed).code, 1002);
        const c2 = await connectWs({ port });
        c2.sendFrame(2, Buffer.from([1]), { fin: false });
        c2.sendFrame(2, Buffer.from([2]));
        assert.equal((await c2.closed).code, 1002);
    });

    it('answers pings with the same payload and refuses bad control frames', async () => {
        const c = await connectWs({ port });
        c.ping(Buffer.from('abc'));
        await waitFor(() => c.pongs.length === 1);
        assert.equal(c.pongs[0].toString(), 'abc');
        c.sendFrame(9, Buffer.alloc(126));
        assert.equal((await c.closed).code, 1002);
        const c2 = await connectWs({ port });
        c2.sendFrame(9, Buffer.from('x'), { fin: false });
        assert.equal((await c2.closed).code, 1002);
    });

    it('rate-limits pongs', async () => {
        const c = await connectWs({ port });
        for (let i = 0; i < 20; i++) c.ping(Buffer.from([i]));
        await sleep(100);
        assert.ok(c.pongs.length >= 1 && c.pongs.length <= 6, `pongs: ${c.pongs.length}`);
        c.destroy();
    });

    it('echoes a client close and reports its code', async () => {
        const c = await connectWs({ port });
        c.close(4321, 'bye');
        const r = await c.closed;
        assert.equal(r.code, 4321);
        await waitFor(() => conns.length === 1 && closes.has(conns[0].id));
        assert.equal(closes.get(conns[0].id), 4321);
    });

    it('validates close codes and reasons', async () => {
        assert.ok(isValidCloseCode(1000) && isValidCloseCode(4999) && isValidCloseCode(1011));
        assert.ok(!isValidCloseCode(1005) && !isValidCloseCode(1006) && !isValidCloseCode(999) && !isValidCloseCode(2000) && !isValidCloseCode(5000));
        for (const code of [1005, 999, 2500]) {
            const c = await connectWs({ port });
            const p = Buffer.alloc(2); p.writeUInt16BE(code);
            c.sendFrame(8, p);
            assert.equal((await c.closed).code, 1002, `code ${code}`);
        }
        const c = await connectWs({ port });
        c.sendFrame(8, Buffer.from([0x03]));
        assert.equal((await c.closed).code, 1002);
        const c2 = await connectWs({ port });
        c2.sendFrame(8, Buffer.from([0x03, 0xe8, 0xff, 0xfe]));
        assert.equal((await c2.closed).code, 1007);
        const c3 = await connectWs({ port });
        c3.sendFrame(8, Buffer.alloc(0));
        assert.equal((await c3.closed).code, 1000);                 // no status: answered with 1000
    });

    it('closes from the server side with a code and a reason', async () => {
        const c = await connectWs({ port });
        await waitFor(() => conns.length === 1);
        conns[0].close(4000, 'bye');
        const r = await c.closed;
        assert.equal(r.code, 4000);
        assert.equal(r.reason, 'bye');
        assert.equal(conns[0].sendFrame(Buffer.from([1])), false);
    });

    it('closes slow consumers with 4303', async () => {
        const c = await connectWs({ port });
        await waitFor(() => conns.length === 1);
        c.socket.pause();
        const conn = conns[0];
        const chunk = Buffer.alloc(60000, 1);
        let sent = 0;
        while (sent < 4000) {
            if (!conn.sendFrame(chunk)) break;
            sent++;
            await new Promise((r) => setImmediate(r));
        }
        assert.ok(sent < 4000, 'the connection should have been closed');
        await waitFor(() => closes.has(conn.id), 5000);
        assert.equal(closes.get(conn.id), 4303);
        const slow = registry.metrics.get('scacelith_ws_slow_consumers_total').root.value;
        assert.ok(slow >= 1);
        c.destroy();
    });

    it('reports 1006 when the peer vanishes', async () => {
        const c = await connectWs({ port });
        await waitFor(() => conns.length === 1);
        c.destroy();
        await waitFor(() => closes.has(conns[0].id));
        assert.equal(closes.get(conns[0].id), 1006);
    });
});

describe('ws: a peer that half-closes without a close frame and stops reading', () => {
    it('is destroyed after closeTimeoutMs, and its admission slot is given back once', async () => {
        let conn = null, closedCode = 0, releases = 0;
        const wss = new WsServer({
            registry: new Registry(), sendBufferLimit: 64 << 20, closeTimeoutMs: 300,
            admission: { acquire: () => true, release: () => { releases++; } },
            onConnection: (c) => { conn = c; c.onClose = (cn, code) => { closedCode = code; }; },
        });
        const srv = net.createServer((s) => wss.handleSocket(s));
        await new Promise((r) => srv.listen(0, '127.0.0.1', r));
        const client = await connectWs({ port: srv.address().port });
        try {
            client.socket.removeAllListeners('data');
            client.socket.pause();
            // More than the socket buffers hold: the rest stays queued in user space, so the
            // server's end() cannot finish while the client does not read.
            const chunk = Buffer.alloc(60000, 7);
            for (let i = 0; i < 400; i++) conn.sendFrame(chunk);
            await sleep(100);
            assert.ok(conn.bufferedBytes > 0);
            client.socket.end();
            const t0 = Date.now();
            while (closedCode === 0) {
                assert.ok(Date.now() - t0 < 1800, 'destroyed after closeTimeoutMs');
                await sleep(10);
            }
            assert.equal(closedCode, 1006);
            assert.equal(wss.size, 0);
            assert.equal(releases, 1);
        } finally { client.destroy(); srv.close(); }
    });
});
