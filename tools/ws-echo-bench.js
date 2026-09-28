#!/usr/bin/env node
// Benchmark of the WebSocket layer (src/net/ws.js): connections per second, memory per idle
// connection and echoed messages per second.
//
//   node tools/ws-echo-bench.js [--conns 10000] [--concurrency 256] [--clients 200]
//                               [--window 32] [--size 16] [--seconds 5] [--json]
//
// The server runs in a child process (node --expose-gc): a plain TCP listener handing sockets to
// a WsServer whose connections echo every binary message. This process is the load generator: a
// minimal client (precomputed handshake and masked frames, replies only counted) so that the
// server is the bottleneck. Both share the machine, so on a small host the numbers are a lower
// bound of what the server alone can do. Loopback TCP, no TLS.
//
//   connections/s   N handshakes (GET + 101) with `concurrency` in flight
//   memory          (heap + external) and RSS growth of the server for N idle connections, after
//                   a forced GC, divided by N
//   messages/s      `clients` connections, each keeping `window` messages of `size` bytes in
//                   flight; every echo received is counted (one message in and one out per count)

import { fork } from 'node:child_process';
import crypto from 'node:crypto';
import net from 'node:net';
import { fileURLToPath } from 'node:url';

const SELF = fileURLToPath(import.meta.url);

function args(argv) {
    const o = { conns: 10000, concurrency: 256, clients: 200, window: 32, size: 16, seconds: 5, json: false };
    for (let i = 0; i < argv.length; i++) {
        const a = argv[i];
        if (a === '--json') { o.json = true; continue; }
        const k = a.replace(/^--/, '');
        if (!(k in o)) throw new Error(`unknown option ${a}`);
        o[k] = Number(argv[++i]);
    }
    return o;
}

// ---- server (child) ---------------------------------------------------------------------------------

async function serverMain() {
    const { WsServer } = await import('../src/net/ws.js');
    const { Registry } = await import('../src/metrics.js');
    const wss = new WsServer({
        registry: new Registry(), maxMessageBytes: 4096, sendBufferLimit: 1 << 20,
        onConnection: (c) => { c.onMessage = (cn, buf) => { cn.sendFrame(buf); }; },
    });
    const srv = net.createServer({ noDelay: true }, (s) => wss.handleSocket(s));
    srv.listen({ port: 0, host: '127.0.0.1', backlog: 4096 }, () => process.send({ port: srv.address().port }));
    let cpu0 = process.cpuUsage(), t0 = performance.now();
    process.on('message', (m) => {
        if (m.cmd === 'mem') {
            for (let i = 0; i < 3; i++) globalThis.gc?.();
            const u = process.memoryUsage();
            process.send({ mem: { rss: u.rss, heap: u.heapUsed, external: u.external + (u.arrayBuffers || 0) }, conns: wss.size });
        } else if (m.cmd === 'cpuStart') {
            cpu0 = process.cpuUsage(); t0 = performance.now();
            process.send({ ok: true });
        } else if (m.cmd === 'cpu') {
            const d = process.cpuUsage(cpu0);
            process.send({ cpu: (d.user + d.system) / 1000 / (performance.now() - t0) });
        } else if (m.cmd === 'exit') {
            wss.closeAll();
            srv.close();
            setTimeout(() => process.exit(0), 50);
        }
    });
}

// ---- load generator ---------------------------------------------------------------------------------

function handshake(port) {
    const key = crypto.randomBytes(16).toString('base64');
    return Buffer.from(`GET /ws HTTP/1.1\r\nHost: 127.0.0.1:${port}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n`
        + `Sec-WebSocket-Key: ${key}\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Protocol: scacelith.v1\r\n\r\n`);
}

/** Opens one WebSocket; resolves with the socket once the 101 head is read. */
function open(port, req) {
    return new Promise((resolve, reject) => {
        const s = net.connect({ port, host: '127.0.0.1', noDelay: true });
        let head = '';
        const onData = (c) => {
            head += c.toString('latin1');
            const end = head.indexOf('\r\n\r\n');
            if (end < 0) return;
            s.off('data', onData);
            if (!head.startsWith('HTTP/1.1 101')) { s.destroy(); reject(new Error(head.split('\r\n')[0])); return; }
            resolve(s);
        };
        s.on('data', onData);
        s.once('error', reject);
        s.once('connect', () => s.write(req));
    });
}

function maskedFrame(payload) {
    const mask = crypto.randomBytes(4);
    const len = payload.length;
    const hl = len < 126 ? 2 : 4;
    const f = Buffer.alloc(hl + 4 + len);
    f[0] = 0x82;
    if (len < 126) f[1] = 0x80 | len;
    else { f[1] = 0x80 | 126; f.writeUInt16BE(len, 2); }
    mask.copy(f, hl);
    for (let i = 0; i < len; i++) f[hl + 4 + i] = payload[i] ^ mask[i & 3];
    return f;
}

function ask(child, msg, key) {
    return new Promise((resolve) => {
        const h = (m) => { if (key in m) { child.off('message', h); resolve(m); } };
        child.on('message', h);
        child.send(msg);
    });
}

async function driverMain(o) {
    const child = fork(SELF, ['--server'], { execArgv: ['--expose-gc'], stdio: ['ignore', 'inherit', 'inherit', 'ipc'] });
    const { port } = await new Promise((resolve) => child.once('message', resolve));
    const req = handshake(port);
    const out = { node: process.version, cpus: (await import('node:os')).cpus().length, ...o };

    // 1. Connections per second + memory per idle connection.
    const base = (await ask(child, { cmd: 'mem' }, 'mem')).mem;
    const sockets = [];
    let next = 0, failed = 0;
    const t0 = performance.now();
    await Promise.all(Array.from({ length: Math.min(o.concurrency, o.conns) }, async () => {
        while (next < o.conns) {
            next++;
            try { sockets.push(await open(port, req)); } catch { failed++; }
        }
    }));
    const dt = (performance.now() - t0) / 1000;
    out.connectionsOpened = sockets.length;
    out.connectFailures = failed;
    out.connectionsPerSec = Math.round(sockets.length / dt);
    await new Promise((r) => setTimeout(r, 300));
    const loaded = await ask(child, { cmd: 'mem' }, 'mem');
    out.serverConnections = loaded.conns;
    const n = Math.max(1, loaded.conns);
    out.bytesPerIdleConnHeapExternal = Math.round((loaded.mem.heap + loaded.mem.external - base.heap - base.external) / n);
    out.bytesPerIdleConnRss = Math.round((loaded.mem.rss - base.rss) / n);

    // 2. Echo throughput on `clients` of those connections.
    const clients = sockets.slice(0, Math.min(o.clients, sockets.length));
    const frame = maskedFrame(crypto.randomBytes(o.size));
    const echoLen = (o.size < 126 ? 2 : 4) + o.size;
    const batch = Buffer.concat(Array.from({ length: o.window }, () => frame));
    let received = 0, running = true;
    for (const s of clients) {
        let pending = 0;
        s.on('data', (c) => {
            pending += c.length;
            const k = Math.floor(pending / echoLen);
            if (!k) return;
            pending -= k * echoLen;
            received += k;
            if (!running) return;
            // Refill the window: k echoes came back, send k more.
            if (k === o.window) s.write(batch);
            else s.write(k < o.window ? batch.subarray(0, k * frame.length) : Buffer.concat(Array.from({ length: k }, () => frame)));
        });
    }
    await ask(child, { cmd: 'cpuStart' }, 'ok');
    const t1 = performance.now();
    for (const s of clients) s.write(batch);
    await new Promise((r) => setTimeout(r, o.seconds * 1000));
    running = false;
    const dt2 = (performance.now() - t1) / 1000;
    out.echoClients = clients.length;
    out.messagesPerSec = Math.round(received / dt2);
    out.serverCpuDuringEcho = Number((await ask(child, { cmd: 'cpu' }, 'cpu')).cpu.toFixed(2));

    for (const s of sockets) s.destroy();
    child.send({ cmd: 'exit' });
    await new Promise((r) => child.once('exit', r));

    if (o.json) { process.stdout.write(`${JSON.stringify(out)}\n`); return; }
    process.stdout.write([
        `node ${out.node}, ${out.cpus} CPUs (server and load generator share them)`,
        `connections/s        ${out.connectionsPerSec} (${out.connectionsOpened} opened, ${out.connectFailures} failed, ${o.concurrency} in flight)`,
        `memory per idle conn ${out.bytesPerIdleConnHeapExternal} B heap+external, ${out.bytesPerIdleConnRss} B RSS (${out.serverConnections} connections)`,
        `echo messages/s      ${out.messagesPerSec} (${out.echoClients} clients x ${o.window} in flight, ${o.size}-byte messages, server CPU ${out.serverCpuDuringEcho} cores)`,
        '',
    ].join('\n'));
}

if (process.argv[2] === '--server') serverMain();
else driverMain(args(process.argv.slice(2))).catch((e) => { process.stderr.write(`${e.stack || e}\n`); process.exit(1); });
