#!/usr/bin/env node
// What a connection refused before TLS still costs the server (docs/SIZING.md, "Protection per
// address", the last row of "Cost of the checks"): a TLS server behind the TlsGate and an IpGuard
// runs in a child process, this process opens N connections to it from the loopback and the child
// reports its CPU time (user + system, kernel work included) divided by N.
//
//   node bench/gate-cost.js [--conns 20000] [--parallel 32]
//
// Modes, one child each:
//   blocked    the loopback address is blocked: each connection is reset at accept (the guard's
//              two Map lookups, the RST), before any byte is read;
//   bad_hello  the address is not blocked: each connection is admitted, then reset when its first
//              record turns out not to be TLS (the client sends an HTTP line), the cheapest path
//              through the gate's first stage.
// The other per-address checks (the request budget, the requests in progress, a new connection's
// rate and open counts) are timed alone by the micro-benchmark of test/unit/net.ipguard.test.js.
// Needs the openssl command line (a throw-away P-256 certificate).

import { execFileSync, fork } from 'node:child_process';
import fs from 'node:fs';
import net from 'node:net';
import os from 'node:os';
import path from 'node:path';
import tls from 'node:tls';
import { fileURLToPath } from 'node:url';

const SELF = fileURLToPath(import.meta.url);

if (process.argv[2] === '--child') await child(process.argv[3], process.argv[4]);
else await main();

async function child(mode, dir) {
    const { IpGuard } = await import('../src/net/ipguard.js');
    const { TlsGate } = await import('../src/net/listeners.js');
    const { testConfig } = await import('../src/config.js');
    const { Registry } = await import('../src/metrics.js');
    const registry = new Registry();
    const guard = new IpGuard({ config: testConfig({ IP_CONN_RATE: '100000', IP_MAX_CONNECTIONS: '1000000' }), workers: 1, registry, report: () => {} });
    if (mode === 'blocked') guard.applyBlocks([['127.0.0.1', 3600000, 1]]);
    const gate = new TlsGate({ registry, guard, maxPending: 100000, maxPendingPerIp: 50000, maxWaitingPerIp: 100000 });
    const server = tls.createServer({ key: fs.readFileSync(path.join(dir, 'key.pem')), cert: fs.readFileSync(path.join(dir, 'cert.pem')) }, (s) => {
        s.on('error', () => {});
        s.end();
    });
    server.on('tlsClientError', () => {});
    gate.attach(server);
    server.listen(0, '127.0.0.1', () => process.send({ port: server.address().port }));
    let base = null;
    process.on('message', (m) => {
        if (m === 'mark') { base = process.cpuUsage(); process.send({ ok: 1 }); }
        if (m === 'read') { const u = process.cpuUsage(base); process.send({ us: u.user + u.system }); }
        if (m === 'exit') process.exit(0);
    });
}

async function main() {
    const args = process.argv.slice(2);
    const opt = { conns: 20000, parallel: 32 };
    for (let i = 0; i < args.length; i++) {
        if (args[i] === '--conns') opt.conns = Number(args[++i]);
        else if (args[i] === '--parallel') opt.parallel = Number(args[++i]);
        else { process.stdout.write('usage: node bench/gate-cost.js [--conns N] [--parallel N]\n'); process.exit(2); }
    }
    const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'scacelith-gate-cost-'));
    try {
        execFileSync('openssl', ['req', '-x509', '-newkey', 'ec', '-pkeyopt', 'ec_paramgen_curve:P-256', '-nodes',
            '-keyout', path.join(dir, 'key.pem'), '-out', path.join(dir, 'cert.pem'), '-days', '1', '-subj', '/CN=gate.test'], { stdio: 'ignore' });
        for (const mode of ['blocked', 'bad_hello']) await run(mode, dir, opt);
    } finally {
        fs.rmSync(dir, { recursive: true, force: true });
    }
}

async function run(mode, dir, { conns, parallel }) {
    const c = fork(SELF, ['--child', mode, dir]);
    const port = await new Promise((r) => c.once('message', (m) => r(m.port)));
    const ask = (m) => new Promise((r) => { c.once('message', r); c.send(m); });
    const one = () => new Promise((resolve) => {
        const s = net.connect(port, '127.0.0.1');
        s.on('error', () => resolve());
        s.on('close', () => resolve());
        s.on('connect', () => s.write('GET / HTTP/1.1\r\n\r\n'));
    });
    for (let i = 0; i < 500; i++) await one();               // warm-up
    await ask('mark');
    const t0 = Date.now();
    let i = 0;
    await Promise.all(Array.from({ length: parallel }, async () => { while (i++ < conns) await one(); }));
    const { us } = await ask('read');
    process.stdout.write(`${mode}: ${conns} connections in ${Date.now() - t0} ms, server CPU ${(us / conns).toFixed(1)} µs per connection\n`);
    c.send('exit');
}
