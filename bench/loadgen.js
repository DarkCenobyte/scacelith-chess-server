#!/usr/bin/env node
// Scacelith load generator and benchmark (docs/BENCHMARK.md).
//
//   node bench/loadgen.js --scenario connect --conns 20000
//   node bench/loadgen.js --scenario games --games 5000 [--move-interval-ms 1000] [--via queue]
//   node bench/loadgen.js --scenario burst --games 500
//   node bench/loadgen.js --scenario games --url wss://host:port/ws --ca cert.pem --tokens tokens.tsv \
//                         --metrics http://127.0.0.1:9464/metrics
//
// Without --url it starts its own server (temporary data directory, self-signed certificate,
// WORKERS shards, proof of work off, high per-IP limits, bench accounts made with
// `bin/admin.js bench-accounts`) and stops it at the end. The clients are spread over --procs load
// processes (each one has its own open-file limit and event loop); this process only
// coordinates: it sends phase commands, merges the statistics the load processes send every
// second, samples the server's /metrics endpoint and (local server, Linux) the CPU time of every
// server and load process from /proc, prints a progress line and writes a JSON report to
// bench/results/. `--help` lists the options.

import { spawn } from 'node:child_process';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const SELF = fileURLToPath(import.meta.url);
const BENCH_DIR = path.dirname(SELF);


// ---- options ------------------------------------------------------------------------------------------

function usage() {
    return `Usage: node bench/loadgen.js --scenario connect|games|burst [options]

Scenarios
  connect   ramp --conns authenticated WSS connections (Hello/Welcome), hold them idle with
            heartbeats for --hold-s; connections/s, failures, handshake and Hello latency,
            server memory per connection, heartbeat round trip
  games     --games pairs play legal random moves at --move-interval-ms per player move;
            move round trip (Move -> own MoveMade), moves/s, games started/finished, rejections,
            server CPU per shard and event-loop lag
  burst     like games with no think time: throughput limit

Load
  --conns N              connections (connect)                              [1000]
  --games N              simultaneous games (games, burst)                  [1000 / 500]
  --procs N              load processes                                     [auto: max(2, clients/15000)]
  --rate N               connection attempts per second, all processes (0 = as fast as possible) [0]
  --inflight N           handshakes in flight per process                   [200]
  --connect-timeout-ms N handshake + Hello deadline                         [30000]
  --hold-s S             connect: idle hold after the ramp                  [30]
  --ping-interval-ms N   client Ping (round trip) per connection, 0 = off   [the server's Welcome.clientPingMs]
  --tls-resume           resume TLS sessions (one ticket per process) instead of full handshakes
  --plain                own server only: TLS_MODE=off + ALLOW_INSECURE_DEV (measures the cost of TLS)

Games
  --via challenge|queue  direct challenges between paired accounts, or the matchmaking queue [challenge]
  --tc M+I               time control                                       [3+2]
  --rated true|false     rated games (rating commits at the end)            [true]
  --move-interval-ms N   think time per move (+-jitter)                     [1000 games, 0 burst]
  --jitter F             think time spread, 0..1 (uniform +-F)              [0.5]
  --max-plies N          resign at this ply (0 = play to the natural end)   [80]
  --gesture-hz N         Gesture messages per second per player in a game, 0 = none [0]
                         (the server relays GESTURE_RATE per second at most: raise it with
                         --server-env GESTURE_RATE=N above 4)
  --between-games-ms N   pause before the next game of a pair               [1000 games, 100 burst]
  --start-rate N         games started per second, all processes (0 = no limit) [1000]
  --warmup-s S           after the games started, before measuring          [10 games, 5 burst]
  --duration-s S         measurement window                                 [60 games, 30 burst]

Server (started by the tool unless --url)
  --workers N|auto       WORKERS (shards)                                   [auto: max(cores, clients/15000)]
  --reuse-port           LISTEN_REUSE_PORT=true
  --server-env K=V       extra server setting (repeatable)
  --data-dir DIR         keep the server data here (not deleted)
  --keep                 keep the temporary data directory
  --server-cpu-prof DIR  V8 CPU profile of every server process, written to DIR at the stop
                         (analyse with node bench/profile-summary.js DIR/*.cpuprofile)
Existing test server
  --url wss://host:port/ws   --ca FILE   --tokens FILE (username<TAB>token or token per line)
  --metrics URL          http://host:port/metrics        --metrics-token T

Output
  --out FILE             JSON report                                        [bench/results/<scenario>-<time>.json]
  --label TEXT           free text stored in the report
  --json                 also print the JSON report on stdout
  --max-run-s S          safety limit of the whole run                      [900]
  --wait-idle-s S        wait up to S s for the other processes of the machine to go quiet [0]
  --min-free-mb N        stop opening connections below this available memory [1500]
`;
}

function parseArgs(argv) {
    const o = { serverEnv: [] };
    const flags = new Set(['reuse-port', 'keep', 'json', 'tls-resume', 'plain', 'help']);
    for (let i = 0; i < argv.length; i++) {
        const a = argv[i];
        if (!a.startsWith('--')) throw new Error(`unexpected argument ${a}`);
        let k = a.slice(2), v;
        const eq = k.indexOf('=');
        if (eq > 0) { v = k.slice(eq + 1); k = k.slice(0, eq); } else if (flags.has(k)) v = true; else v = argv[++i];
        if (v === undefined) throw new Error(`--${k} needs a value`);
        const key = k.replace(/-([a-z])/g, (_, c) => c.toUpperCase());
        if (key === 'serverEnv') o.serverEnv.push(String(v));
        else o[key] = v;
    }
    return o;
}

const num = (v, d) => (v === undefined || v === null || v === '' ? d : Number(v));
const bool = (v, d) => (v === undefined ? d : v === true || /^(1|true|yes|on)$/i.test(String(v)));

function options(raw) {
    const scenario = raw.scenario || 'connect';
    if (!['connect', 'games', 'burst'].includes(scenario)) throw new Error('--scenario connect|games|burst');
    const burst = scenario === 'burst';
    const o = {
        scenario,
        conns: num(raw.conns, 1000),
        games: num(raw.games, burst ? 500 : 1000),
        procs: num(raw.procs, 0),
        rate: num(raw.rate, 0),
        inflight: num(raw.inflight, 200),
        connectTimeoutMs: num(raw.connectTimeoutMs, 30000),
        holdS: num(raw.holdS, 30),
        pingIntervalMs: num(raw.pingIntervalMs, -1),     // -1: Welcome.clientPingMs, like the game
        tlsResume: !!raw.tlsResume,
        plain: !!raw.plain,
        via: raw.via || 'challenge',
        tc: raw.tc || '3+2',
        rated: bool(raw.rated, true),
        moveIntervalMs: num(raw.moveIntervalMs, burst ? 0 : 1000),
        jitter: num(raw.jitter, 0.5),
        maxPlies: num(raw.maxPlies, 80),
        gestureHz: num(raw.gestureHz, 0),
        betweenGamesMs: num(raw.betweenGamesMs, burst ? 100 : 1000),
        startRate: num(raw.startRate, 1000),
        warmupS: num(raw.warmupS, burst ? 5 : 10),
        durationS: num(raw.durationS, burst ? 30 : 60),
        workers: raw.workers || 'auto',
        reusePort: !!raw.reusePort,
        serverEnv: raw.serverEnv,
        dataDir: raw.dataDir || null,
        keep: !!raw.keep,
        url: raw.url || null,
        ca: raw.ca || null,
        tokens: raw.tokens || null,
        metrics: raw.metrics || null,
        metricsToken: raw.metricsToken || null,
        out: raw.out || null,
        label: raw.label || '',
        json: !!raw.json,
        maxRunS: num(raw.maxRunS, 900),
        waitIdleS: num(raw.waitIdleS, 0),
        serverCpuProf: raw.serverCpuProf || null,
        minFreeMb: num(raw.minFreeMb, 1500),
    };
    if (!['challenge', 'queue'].includes(o.via)) throw new Error('--via challenge|queue');
    const m = /^(\d+(?:\.\d+)?)\+(\d+)$/.exec(o.tc);
    if (!m) throw new Error('--tc M+I (e.g. 3+2)');
    o.tcSec = [Math.round(Number(m[1]) * 60), Number(m[2])];
    if (o.url && !o.tokens) throw new Error('--url needs --tokens (and usually --ca)');
    if (o.url && o.plain) throw new Error('--plain only with the server started by the tool');
    o.clients = scenario === 'connect' ? o.conns : o.games * 2;
    if (o.jitter < 0 || o.jitter > 1) throw new Error('--jitter 0..1');
    if (!(o.gestureHz >= 0 && o.gestureHz <= 100)) throw new Error('--gesture-hz 0..100');
    return o;
}

// ---- helpers ------------------------------------------------------------------------------------------

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
const fmt = (n, d = 0) => (n === undefined || n === null || Number.isNaN(n) ? '-' : Number(n).toFixed(d));
const mb = (b) => (b / 2 ** 20).toFixed(0);
const t0Run = Date.now();
const elapsed = () => ((Date.now() - t0Run) / 1000).toFixed(0).padStart(4);
const say = (s) => process.stdout.write(`${s}\n`);

async function main() {
    const raw = parseArgs(process.argv.slice(2));
    if (raw.help) { process.stdout.write(usage()); return; }
    const o = options(raw);
    const { Hist } = await import('./lib/hist.js');
    const prom = await import('./lib/prom.js');
    const { CpuSampler, machineInfo } = await import('./lib/procfs.js');
    const { startServer, withNofile } = await import('./lib/server.js');

    const cores = os.cpus().length;
    const procs = o.procs > 0 ? o.procs : Math.max(2, Math.ceil(o.clients / 15000));
    const report = {
        tool: 'scacelith-bench', version: 1, label: o.label, command: `node bench/loadgen.js ${process.argv.slice(2).join(' ')}`,
        startedAt: new Date().toISOString(), machine: machineInfo(), params: { ...o, serverEnv: o.serverEnv },
        loadgen: { procs, clients: o.clients, clientsPerProc: Math.ceil(o.clients / procs) },
        server: {}, phases: {}, timeline: [], notes: [],
    };

    // ---- target -----------------------------------------------------------------------------------
    let srv = null;
    let target;
    const cleanup = [];
    // One cleanup, run once: a signal or a guard arriving while it runs waits for it to finish.
    let cleaning = null;
    const doCleanup = () => (cleaning ||= (async () => {
        for (const f of cleanup.reverse()) { try { await f(); } catch (e) { process.stderr.write(`cleanup: ${e.message}\n`); } }
    })());
    let interrupted = false;
    process.on('SIGINT', () => {
        if (interrupted) { say('\ninterrupted again: exiting now (the server may be left running)'); process.exit(130); }
        interrupted = true;
        say('\ninterrupted: cleaning up'); doCleanup().then(() => process.exit(130));
    });
    process.on('SIGTERM', () => { doCleanup().then(() => process.exit(143)); });
    const safety = setTimeout(() => { process.stderr.write(`--max-run-s ${o.maxRunS} reached: aborting\n`); doCleanup().then(() => process.exit(3)); }, o.maxRunS * 1000);
    safety.unref();

    try {
        if (!o.url) {
            const workers = o.workers === 'auto' ? Math.max(cores, Math.ceil(o.clients / 15000)) : Number(o.workers);
            const env = {};
            if (o.scenario === 'burst') { env.WS_MSG_RATE = '1000'; env.WS_MSG_BURST = '2000'; }
            if (o.plain) { env.TLS_MODE = 'off'; env.ALLOW_INSECURE_DEV = '1'; }
            for (const kv of o.serverEnv) { const i = kv.indexOf('='); env[kv.slice(0, i)] = kv.slice(i + 1); }
            say(`starting a server: ${workers} worker(s)${o.reusePort ? ', SO_REUSEPORT' : ''}, ${o.clients} bench accounts...`);
            srv = await startServer({ workers, reusePort: o.reusePort, accounts: o.clients, env, dataDir: o.dataDir, keep: o.keep, cpuProfDir: o.serverCpuProf, log: (s) => say(`  ${s}`) });
            cleanup.push(async () => { say('stopping the server...'); await srv.stop(); });
            target = { host: '127.0.0.1', port: srv.wsPort, path: '/ws', ca: srv.ca, servername: null, metrics: srv.metricsUrl, metricsToken: null, users: srv.users };
            report.server = { mode: 'local', workers, reusePort: o.reusePort, tls: !o.plain, dataDir: o.dataDir || '(temporary)', env: srv.env, info: pickInfo(srv.info), processes: srv.processes() };
        } else {
            const u = new URL(o.url);
            if (u.protocol !== 'wss:') throw new Error('--url must be wss://host:port/ws');
            const users = fs.readFileSync(o.tokens, 'utf8').split('\n').map((l) => l.trim()).filter(Boolean)
                .map((l) => { const p = l.split('\t'); return p.length > 1 ? { u: p[0], t: p[1] } : { u: '', t: p[0] }; });
            if (users.length < o.clients) throw new Error(`--tokens has ${users.length} accounts, ${o.clients} needed`);
            const isIp = /^[\d.]+$|:/.test(u.hostname);
            target = {
                host: u.hostname, port: Number(u.port || 443), path: u.pathname || '/ws', ca: o.ca ? fs.readFileSync(o.ca, 'utf8') : null,
                servername: isIp ? null : u.hostname, metrics: o.metrics, metricsToken: o.metricsToken, users,
            };
            report.server = { mode: 'external', url: o.url, metrics: o.metrics || null };
        }
        report.server.sameMachine = !o.url || /^(127\.|localhost$|::1$)/.test(target.host);

        // ---- load processes -------------------------------------------------------------------------
        const loopback = /^127\./.test(target.host);           // then each process binds its own 127.1.x.y source address
        const procList = [];
        // Account slices: pairs (games) never straddle two processes.
        const slices = [];
        if (o.scenario === 'connect') {
            const per = Math.ceil(o.clients / procs);
            for (let p = 0; p < procs; p++) slices.push([p * per, Math.min(o.clients, (p + 1) * per)]);
        } else {
            const per = Math.ceil(o.games / procs);
            for (let p = 0; p < procs; p++) slices.push([2 * p * per, 2 * Math.min(o.games, (p + 1) * per)]);
        }
        for (let p = 0; p < procs; p++) {
            const [from, to] = slices[p];
            if (to <= from) continue;
            const { cmd, args } = withNofile([process.execPath, SELF, '--worker']);
            const child = spawn(cmd, args, { stdio: ['ignore', 'inherit', 'inherit', 'ipc'], serialization: 'advanced' });
            const pr = { p, child, from, to, ready: null, closed: false, gauges: {}, exited: null };
            child.on('exit', (code, signal) => { pr.exited = { code, signal }; });
            procList.push(pr);
        }
        cleanup.push(async () => {
            for (const pr of procList) if (!pr.exited) { try { pr.child.send({ cmd: 'exit' }); } catch { /* gone */ } }
            await sleep(300);
            for (const pr of procList) if (!pr.exited) pr.child.kill('SIGKILL');
        });

        // ---- aggregation ----------------------------------------------------------------------------
        const HN = ['connect', 'hello', 'move', 'hb', 'start'];
        const newAgg = () => ({ c: {}, m: {}, h: Object.fromEntries(HN.map((k) => [k, new Hist()])), t: Date.now() });
        const addAgg = (a, s) => {
            for (const [k, v] of Object.entries(s.c)) if (v) a.c[k] = (a.c[k] || 0) + v;
            for (const [mk, mv] of Object.entries(s.m)) {
                const t = (a.m[mk] ||= {});
                for (const [k, v] of Object.entries(mv)) t[k] = (t[k] || 0) + v;
            }
            for (const k of HN) if (s.h[k]) a.h[k].merge(s.h[k]);
        };
        const total = newAgg();
        let phase = newAgg();
        let win = newAgg();
        const onStats = (pr, s) => {
            pr.gauges = s.g;
            addAgg(total, s); addAgg(phase, s); addAgg(win, s);
        };
        const sumG = (k) => procList.reduce((a, pr) => a + (pr.gauges[k] || 0), 0);
        const maxG = (k) => procList.reduce((a, pr) => Math.max(a, pr.gauges[k] || 0), 0);

        const readyP = procList.map((pr) => new Promise((resolve, reject) => {
            pr.child.on('message', (msg) => {
                if (msg.type === 'stats') onStats(pr, msg);
                else if (msg.type === 'ready') { pr.ready = msg; resolve(); } else if (msg.type === 'closed') pr.closed = true;
            });
            pr.child.on('exit', (code) => { if (!pr.ready) reject(new Error(`load process ${pr.p} exited (${code})`)); });
        }));
        const srcIp = (pr) => (loopback ? `127.1.${Math.floor(pr.p / 250)}.${(pr.p % 250) + 1}` : null);
        for (const pr of procList) {
            const accounts = target.users.slice(pr.from, pr.to);
            const ip = srcIp(pr);
            pr.child.send({
                cmd: 'init',
                accounts,
                cfg: {
                    proc: pr.p, scenario: o.scenario, host: target.host, port: target.port, path: target.path,
                    hostHeader: `${target.servername || target.host}:${target.port}`, servername: target.servername, ca: target.ca,
                    localAddress: ip, tlsResume: o.tlsResume, plain: o.plain,
                    rate: o.rate > 0 ? o.rate / procList.length : 0, inflight: o.inflight, connectTimeoutMs: o.connectTimeoutMs,
                    pingIntervalMs: o.pingIntervalMs, via: o.via, tc: o.tcSec, rated: o.rated,
                    moveIntervalMs: o.moveIntervalMs, jitter: o.jitter, maxPlies: o.maxPlies, betweenGamesMs: o.betweenGamesMs,
                    gestureHz: o.scenario === 'connect' ? 0 : o.gestureHz,
                    startRate: o.startRate > 0 ? o.startRate / procList.length * (o.via === 'queue' ? 2 : 1) : 0,
                    clientName: 'scacelith-bench/1',
                },
            });
        }
        await Promise.all(readyP);
        const fp = procList[0].ready.fastPath;
        report.loadgen.fastPath = fp;
        report.loadgen.procs = procList.length;
        report.loadgen.localAddresses = loopback ? procList.map(srcIp) : null;
        if (!fp.fast) say(`note: wire fast path disabled (${fp.reason}); using the codec for every message`);
        say(`${procList.length} load process(es), ${o.clients} clients, target ${target.host}:${target.port}${report.server.sameMachine ? ' (same machine: load generator and server share the CPU)' : ''}`);

        // ---- sampling --------------------------------------------------------------------------------
        const cpu = new CpuSampler();
        const pidMap = () => {
            const m = srv ? { ...srv.processes() } : {};
            for (const pr of procList) if (!pr.exited) m[`load-${pr.p}`] = pr.child.pid;
            return m;
        };
        let pids = pidMap();
        const scrape = async () => {
            if (!target.metrics) return null;
            try { return prom.snapshot(await prom.fetchText(target.metrics, { token: target.metricsToken })); } catch (e) { return { error: e.message }; }
        };
        const mark = async () => ({ at: Date.now(), metrics: await scrape(), cpu: cpu.sample(pids) });
        let lastSample = cpu.sample(pids);
        let phaseName = 'setup';
        let scraping = false;
        const serverSamples = [];
        let lastConnOk = 0, lastWinAt = Date.now();
        const tick = async () => {
            if (scraping) return;
            scraping = true;
            try {
                const m = await scrape();
                const s = cpu.sample(pids);
                const d = cpu.delta(lastSample, s);
                lastSample = s;
                if (m && !m.error) serverSamples.push({ phase: phaseName, at: m.at, shards: m.shards });
                const now = Date.now();
                const dt = (now - lastWinAt) / 1000;
                lastWinAt = now;
                const srvCores = Object.entries(d.procs).filter(([k]) => !k.startsWith('load-')).reduce((a, [, v]) => a + v.cores, 0);
                const lgCores = Object.entries(d.procs).filter(([k]) => k.startsWith('load-')).reduce((a, [, v]) => a + v.cores, 0);
                const srvCpuM = m && !m.error ? Object.values(m.shards).reduce((a, x) => a + (x.cpu || 0), 0) : null;
                const lagSrv = m && !m.error ? Math.max(0, ...Object.values(m.shards).map((x) => x.lagP99 || 0)) : null;
                const rssSrv = m && !m.error ? Object.values(m.shards).reduce((a, x) => a + (x.rss || 0), 0) : null;
                const connOk = total.c.connOk || 0;
                const mv = win.c.movesOk || 0;
                const row = {
                    t: Math.round((now - t0Run) / 1000), phase: phaseName,
                    ready: sumG('ready'), connecting: sumG('connecting'), connPerSec: Math.round((connOk - lastConnOk) / dt),
                    games: sumG('whitePlaying'), movesPerSec: Math.round(mv / dt),
                    moveRttP50Ms: win.h.move.n ? +(win.h.move.quantile(0.5) / 1000).toFixed(2) : null,
                    moveRttP99Ms: win.h.move.n ? +(win.h.move.quantile(0.99) / 1000).toFixed(2) : null,
                    srvCores: d.procs.primary ? +srvCores.toFixed(2) : srvCpuM !== null ? +srvCpuM.toFixed(2) : null,
                    srvLagP99Ms: lagSrv !== null ? +lagSrv.toFixed(1) : null,
                    srvRssMB: rssSrv !== null ? Math.round(rssSrv / 2 ** 20) : null,
                    lgCores: lgCores ? +lgCores.toFixed(2) : +sumG('cpu').toFixed(2),
                    lgLagP99Ms: +maxG('lagP99').toFixed(1),
                    machineBusy: d.machine ? d.machine.busyRatio : null,
                    otherCores: d.machine && d.procs.primary ? +Math.max(0, d.machine.coresBusy - srvCores - lgCores).toFixed(2) : null,
                    memAvailableMB: Math.round(memAvailableMB()),
                };
                lastConnOk = connOk;
                report.timeline.push(row);
                if (row.memAvailableMB < o.minFreeMb / 2) {
                    say(`available memory ${row.memAvailableMB} MB: aborting the run to protect the machine`);
                    doCleanup().then(() => process.exit(4));
                }
                say(`${elapsed()}s ${phaseName.padEnd(7)} conns ${String(row.ready).padStart(6)} (+${row.connPerSec}/s, ${total.c.connFail || 0} failed) `
                    + `games ${String(row.games).padStart(5)} moves/s ${String(row.movesPerSec).padStart(6)} rtt p50/p99 ${fmt(row.moveRttP50Ms, 1)}/${fmt(row.moveRttP99Ms, 1)} ms `
                    + `| server ${fmt(row.srvCores, 2)} cores lag ${fmt(row.srvLagP99Ms, 0)} ms rss ${fmt(row.srvRssMB)} MB `
                    + `| loadgen ${fmt(row.lgCores, 2)} cores lag ${fmt(row.lgLagP99Ms, 0)} ms | cpu busy ${row.machineBusy !== null ? Math.round(row.machineBusy * 100) : '-'}% | free ${row.memAvailableMB} MB`
                    + `${row.otherCores > 0.2 ? ` (other processes ${row.otherCores} cores)` : ''}`);
                win = newAgg();
            } finally {
                scraping = false;
            }
        };
        const ticker = setInterval(() => { tick().catch(() => {}); }, 2000);
        cleanup.push(async () => clearInterval(ticker));

        const bcast = (msg) => { for (const pr of procList) if (!pr.exited) pr.child.send(msg); };
        const phaseResult = (name, a, m0, m1, extra = {}) => {
            const r = {
                seconds: +((m1.at - m0.at) / 1000).toFixed(1),
                counters: a.c, detail: a.m,
                latencyMs: Object.fromEntries(HN.filter((k) => a.h[k].n).map((k) => [k, a.h[k].summary(1000)])),
                ...extra,
            };
            r.server = serverPhase(prom, m0, m1, serverSamples.filter((s) => s.phase === name));
            r.cpu = cpu.delta(m0.cpu, m1.cpu);
            r.cpu.serverCores = +Object.entries(r.cpu.procs).filter(([k]) => !k.startsWith('load-')).reduce((x, [, v]) => x + v.cores, 0).toFixed(2);
            r.cpu.loadgenCores = +Object.entries(r.cpu.procs).filter(([k]) => k.startsWith('load-')).reduce((x, [, v]) => x + v.cores, 0).toFixed(2);
            r.cpu.otherCores = r.cpu.machine && r.cpu.procs.primary ? +Math.max(0, r.cpu.machine.coresBusy - r.cpu.serverCores - r.cpu.loadgenCores).toFixed(2) : null;
            r.loadgen = { lagP99MaxMs: +maxG('lagP99').toFixed(1), rssMB: Math.round(sumG('rss') / 2 ** 20) };
            report.phases[name] = r;
            return r;
        };
        const startPhase = async (name) => {
            phaseName = name;
            phase = newAgg();
            return mark();
        };
        const deadAll = () => procList.every((pr) => pr.exited);

        // ---- quiet machine ----------------------------------------------------------------------------
        if (o.waitIdleS > 0) {
            const until = Date.now() + o.waitIdleS * 1000;
            for (;;) {
                const a = cpu.sample(pidMap());
                await sleep(2000);
                const d = cpu.delta(a, cpu.sample(pidMap()));
                const ours = Object.values(d.procs).reduce((x, v) => x + v.cores, 0);
                const other = d.machine ? d.machine.coresBusy - ours : 0;
                if (other < 0.3) break;
                if (Date.now() > until) { report.notes.push(`machine not idle after ${o.waitIdleS} s (other processes ${other.toFixed(2)} cores): started anyway`); break; }
                say(`waiting for a quiet machine (other processes use ${other.toFixed(2)} cores)...`);
            }
        }

        // ---- baseline ---------------------------------------------------------------------------------
        await sleep(1500);
        pids = pidMap();
        const base = await mark();
        report.server.baseline = base.metrics && !base.metrics.error ? summarizeShards(base.metrics) : base.metrics;

        // ---- ramp ---------------------------------------------------------------------------------------
        say(`ramp: ${o.clients} connections${o.rate ? ` at ${o.rate}/s` : ''} (${o.inflight} handshakes in flight per process)`);
        let m0 = await startPhase('ramp');
        bcast({ cmd: 'connect' });
        let lastProgress = Date.now(), lastDone = 0;
        let halted = false;
        for (;;) {
            await sleep(250);
            const done = (phase.c.connOk || 0) + (phase.c.connFail || 0);
            if (done !== lastDone) { lastDone = done; lastProgress = Date.now(); }
            if (done >= o.clients) break;
            if (halted && done >= (phase.c.connStarted || 0)) break;
            if (!halted && memAvailableMB() < o.minFreeMb) {
                halted = true;
                bcast({ cmd: 'halt' });
                const note = `ramp halted at about ${done} connections: available memory below ${o.minFreeMb} MB`;
                report.notes.push(note);
                say(note);
                await sleep(1200);
            }
            if (Date.now() - lastProgress > 45000) { report.notes.push(`ramp stalled at ${done}/${o.clients}`); break; }
            if (deadAll()) throw new Error('every load process exited');
        }
        await sleep(1200);                              // last stats reports
        let m1 = await mark();
        const lastOkAt = maxG('lastWelcomeAt');
        const rampSeconds = ((lastOkAt || m1.at) - m0.at) / 1000;
        const ramp = phaseResult('ramp', phase, m0, m1, {
            connectionsOk: phase.c.connOk || 0, connectionsFailed: phase.c.connFail || 0,
            failures: phase.m.fail || {}, rampSeconds: +rampSeconds.toFixed(2),
            connectionsPerSec: Math.round((phase.c.connOk || 0) / Math.max(0.001, rampSeconds)),
        });
        say(`ramp done: ${ramp.connectionsOk} connected, ${ramp.connectionsFailed} failed in ${ramp.rampSeconds} s = ${ramp.connectionsPerSec} connections/s`);
        if (ramp.connectionsFailed) say(`  failures: ${JSON.stringify(ramp.failures)}`);

        // Memory with the connections open (idle), after a short settle.
        await sleep(3000);
        const loaded = await mark();
        const memConn = memoryDelta(base, loaded, ramp.connectionsOk);
        report.results = { memoryPerConnection: memConn };
        if (memConn) say(`server memory: +${mb(memConn.rssDelta)} MB RSS, +${mb(memConn.heapDelta)} MB heap for ${ramp.connectionsOk} connections = ${memConn.rssPerConn} B RSS, ${memConn.heapPerConn} B heap, ${memConn.externalPerConn} B external per connection`);

        if (o.scenario === 'connect') {
            // ---- hold ---------------------------------------------------------------------------------
            const pingText = o.pingIntervalMs < 0 ? ' + client Ping every Welcome.clientPingMs'
                : o.pingIntervalMs > 0 ? ` + client Ping every ${o.pingIntervalMs} ms` : ', no client Ping';
            say(`hold: ${o.holdS} s idle (server heartbeat${pingText})`);
            m0 = await startPhase('hold');
            await sleep(o.holdS * 1000);
            await sleep(1100);
            m1 = await mark();
            const hold = phaseResult('hold', phase, m0, m1, { dropped: phase.c.dropped || 0, closes: phase.m.closes || {} });
            report.results.memoryAfterHold = memoryDelta(base, m1, sumG('ready'));
            report.results.connect = {
                connectionsOk: ramp.connectionsOk, connectionsFailed: ramp.connectionsFailed, connectionsPerSec: ramp.connectionsPerSec,
                handshakeMs: ramp.latencyMs.connect, helloMs: ramp.latencyMs.hello, heartbeatRttMs: hold.latencyMs.hb || null,
                serverHeartbeatRttMs: hold.server?.rttMs || null, droppedDuringHold: hold.dropped,
                memoryPerConnection: memConn, serverCpuDuringHoldCores: hold.cpu.serverCores,
            };
        } else {
            // ---- games --------------------------------------------------------------------------------
            say(`games: starting ${o.games} games via ${o.via} (${o.startRate || 'unlimited'}/s), ${o.moveIntervalMs} ms per move, resign at ply ${o.maxPlies || 'never'}`
                + `${o.gestureHz > 0 ? `, ${o.gestureHz} gestures/s per player` : ''}`);
            m0 = await startPhase('start');
            bcast({ cmd: 'games' });
            let lastG = -1, lastGAt = Date.now();
            for (;;) {
                await sleep(250);
                const g = phase.c.gamesStarted || 0;
                if (g !== lastG) { lastG = g; lastGAt = Date.now(); }
                if (g >= o.games) break;
                if (Date.now() - lastGAt > 15000) { report.notes.push(`game start stalled at ${g}/${o.games}`); break; }
                if (deadAll()) throw new Error('every load process exited');
            }
            m1 = await mark();
            const st = phaseResult('start', phase, m0, m1, {});
            st.gamesStarted = phase.c.gamesStarted || 0;
            st.gamesPerSec = Math.round(st.gamesStarted / Math.max(0.001, st.seconds));
            say(`started ${st.gamesStarted} games in ${st.seconds} s (${st.gamesPerSec}/s); start latency p50 ${fmt(st.latencyMs.start?.p50, 1)} ms p99 ${fmt(st.latencyMs.start?.p99, 1)} ms`);

            if (o.warmupS > 0) { say(`warm-up ${o.warmupS} s`); await startPhase('warmup'); await sleep(o.warmupS * 1000); }
            say(`measuring ${o.durationS} s`);
            m0 = await startPhase('measure');
            await sleep(o.durationS * 1000);
            await sleep(1100);
            m1 = await mark();
            const ms = phaseResult('measure', phase, m0, m1, {});
            const secs = ms.seconds;
            const withGames = memoryDelta(base, m1, sumG('ready'));
            // Shared hosts: the 2-second windows of the measurement when the other processes of the
            // machine used less than 0.3 core, and their median throughput.
            const quiet = report.timeline.filter((row) => row.phase === 'measure' && row.otherCores !== null && row.otherCores < 0.3 && row.movesPerSec > 0);
            const med = (a) => { const x = [...a].sort((p, q) => p - q); return x.length ? x[Math.floor(x.length / 2)] : null; };
            report.results.games = {
                activeGames: sumG('whitePlaying'),
                movesPerSec: Math.round((phase.c.movesOk || 0) / secs),
                quietWindows: quiet.length, quietMovesPerSecMedian: med(quiet.map((row) => row.movesPerSec)),
                quietMoveRttP99MsMedian: med(quiet.map((row) => row.moveRttP99Ms).filter((v) => v !== null)),
                movesSent: phase.c.movesSent || 0, movesConfirmed: phase.c.movesOk || 0,
                moveRttMs: ms.latencyMs.move || null,
                serverMoveProcessingUs: ms.server?.moveUs || null,
                gamesStarted: phase.c.gamesStarted || 0, gamesFinished: phase.c.gamesEnded || 0,
                gamesPerMinute: Math.round((phase.c.gamesEnded || 0) / secs * 60),
                endReasons: phase.m.ends || {}, rejected: phase.m.rejected || {}, errors: phase.m.errors || {},
                dropped: phase.c.dropped || 0, resyncs: phase.c.resyncs || 0,
                gesturesSentPerSec: Math.round((phase.c.gesturesSent || 0) / secs),
                gesturesReceivedPerSec: Math.round((phase.c.gesturesIn || 0) / secs),
                serverGesturesRelayedPerSec: ms.server?.gesturesRelayedPerSec ?? null,
                serverGesturesDropped: ms.server?.gesturesDropped || null,
                gameStartMs: st.latencyMs.start || null,
                serverCores: ms.cpu.serverCores, loadgenCores: ms.cpu.loadgenCores, machineBusy: ms.cpu.machine?.busyRatio ?? null,
                serverShards: ms.server?.shards || null,
                memoryWithGames: withGames,
            };
            bcast({ cmd: 'stop' });
        }

        // ---- close ------------------------------------------------------------------------------------
        phaseName = 'close';
        bcast({ cmd: 'close' });
        const tc = Date.now();
        while (!procList.every((pr) => pr.closed || pr.exited) && Date.now() - tc < 20000) await sleep(100);
        clearInterval(ticker);
        report.totals = { counters: total.c, detail: total.m };
        report.finishedAt = new Date().toISOString();
        report.saturation = saturation(report, cores);

        const outFile = o.out || path.join(BENCH_DIR, 'results', `${o.scenario}-${new Date().toISOString().replace(/[:.]/g, '-')}.json`);
        fs.mkdirSync(path.dirname(outFile), { recursive: true });
        fs.writeFileSync(outFile, JSON.stringify(report, null, 2));
        printSummary(report);
        say(`report: ${path.relative(process.cwd(), outFile)}`);
        if (o.json) process.stdout.write(`${JSON.stringify(report)}\n`);
    } catch (e) {
        process.stderr.write(`loadgen: ${e.stack || e.message}\n`);
        if (srv) process.stderr.write(`server log (tail):\n${srv.tail(40)}\n`);
        process.exitCode = 1;
    } finally {
        clearTimeout(safety);
        await doCleanup();
    }
}

/** MemAvailable of the machine in MB (os.freemem() elsewhere). */
function memAvailableMB() {
    try {
        const m = /MemAvailable:\s+(\d+) kB/.exec(fs.readFileSync('/proc/meminfo', 'latin1'));
        if (m) return Number(m[1]) / 1024;
    } catch { /* not Linux */ }
    return os.freemem() / 2 ** 20;
}

function pickInfo(info) {
    if (!info) return null;
    const { name, serverId, protocol, wsPort, limits, categories } = info;
    return { name, serverId, protocol, wsPort, limits, categories: categories?.length };
}

function summarizeShards(m) {
    const out = {};
    for (const [shard, s] of Object.entries(m.shards)) out[shard] = { rssMB: +((s.rss || 0) / 2 ** 20).toFixed(1), heapMB: +((s.heap || 0) / 2 ** 20).toFixed(1), conns: s.conns ?? null };
    return out;
}

/** Server memory growth between two marks, per connection. */
function memoryDelta(a, b, conns) {
    if (!a.metrics || !b.metrics || a.metrics.error || b.metrics.error || !conns) return null;
    const tot = (m, k) => Object.values(m.shards).reduce((x, s) => x + (s[k] || 0), 0);
    const rss = tot(b.metrics, 'rss') - tot(a.metrics, 'rss');
    const heap = tot(b.metrics, 'heap') - tot(a.metrics, 'heap');
    const ext = tot(b.metrics, 'external') - tot(a.metrics, 'external');
    const perShard = {};
    for (const [shard, s] of Object.entries(b.metrics.shards)) {
        const s0 = a.metrics.shards[shard] || {};
        perShard[shard] = { conns: s.conns ?? null, rssMB: +((s.rss || 0) / 2 ** 20).toFixed(1), rssDeltaMB: +(((s.rss || 0) - (s0.rss || 0)) / 2 ** 20).toFixed(1), heapMB: +((s.heap || 0) / 2 ** 20).toFixed(1) };
    }
    return {
        connections: conns, rssDelta: rss, heapDelta: heap, externalDelta: ext,
        rssPerConn: Math.round(rss / conns), heapPerConn: Math.round(heap / conns), externalPerConn: Math.round(ext / conns),
        totalRssMB: Math.round(tot(b.metrics, 'rss') / 2 ** 20), perShard,
        note: 'process RSS / V8 heap sums over the server processes (primary included) from /metrics, no forced GC',
    };
}

/** Server-side view of a phase: counter deltas, histogram deltas, per-shard CPU / lag / memory. */
function serverPhase(prom, m0, m1, samples) {
    const a = m0.metrics, b = m1.metrics;
    if (!a || !b || a.error || b.error) return null;
    const secs = (b.at - a.at) / 1000;
    const d = (k) => b.totals[k] - a.totals[k];
    const shards = {};
    for (const [shard, s] of Object.entries(b.shards)) {
        const cpus = samples.map((x) => x.shards[shard]?.cpu).filter((v) => v !== undefined);
        const lags = samples.map((x) => x.shards[shard]?.lagP99).filter((v) => v !== undefined);
        const lagMax = samples.map((x) => x.shards[shard]?.lagMax).filter((v) => v !== undefined);
        shards[shard] = {
            cpuMean: cpus.length ? +(cpus.reduce((x, y) => x + y, 0) / cpus.length).toFixed(3) : null,
            cpuMax: cpus.length ? +Math.max(...cpus).toFixed(3) : null,
            lagP99MeanMs: lags.length ? +(lags.reduce((x, y) => x + y, 0) / lags.length).toFixed(1) : null,
            lagP99MaxMs: lags.length ? +Math.max(...lags).toFixed(1) : (s.lagP99 ?? null),
            lagMaxMs: lagMax.length ? +Math.max(...lagMax).toFixed(1) : (s.lagMax ?? null),
            rssMB: +((s.rss || 0) / 2 ** 20).toFixed(1), heapMB: +((s.heap || 0) / 2 ** 20).toFixed(1),
            conns: s.conns ?? null, games: s.games ?? null,
        };
    }
    return {
        seconds: +secs.toFixed(1),
        movesPerSec: +(d('moves') / secs).toFixed(1),
        gamesCreated: d('gamesCreated'), gamesEnded: d('gamesEnded'), gamesCommitted: d('gamesCommitted'), rejects: d('rejects'),
        relayedPerSec: +(d('relayed') / secs).toFixed(1),
        gesturesRelayedPerSec: +(d('gesturesRelayed') / secs).toFixed(1),
        gesturesDropped: Object.fromEntries(Object.entries(b.totals.gesturesDropped)
            .map(([k, v]) => [k, v - (a.totals.gesturesDropped[k] || 0)]).filter(([, v]) => v > 0)),
        bytesInPerSec: Math.round(d('bytesIn') / secs), bytesOutPerSec: Math.round(d('bytesOut') / secs),
        slowConsumers: d('slowConsumers'), handshakesRejected: d('handshakesRejected'),
        moveUs: prom.histSummary(prom.histDelta(a.hist.moveUs, b.hist.moveUs)),
        rttMs: prom.histSummary(prom.histDelta(a.hist.rttMs, b.hist.rttMs)),
        helloMs: prom.histSummary(prom.histDelta(a.hist.helloMs, b.hist.helloMs)),
        handshakeMs: prom.histSummary(prom.histDelta(a.hist.handshakeMs, b.hist.handshakeMs)),
        journalFlushMs: prom.histSummary(prom.histDelta(a.hist.journalFlushMs, b.hist.journalFlushMs)),
        commitMs: prom.histSummary(prom.histDelta(a.hist.commitMs, b.hist.commitMs)),
        commitBatchMs: prom.histSummary(prom.histDelta(a.hist.commitBatchMs, b.hist.commitBatchMs)),
        commitBatchSize: prom.histSummary(prom.histDelta(a.hist.commitBatchSize, b.hist.commitBatchSize)),
        storeBusy: d('storeBusy'), commitErrors: d('commitErrors'), journalErrors: d('journalErrors'),
        dropped: b.totals.dropped, closes: b.totals.closes, hello: b.totals.hello, anomalies: b.totals.anomalies,
        shards,
    };
}

/** What limited the run, from the measures (heuristic, stated as such in the report). */
function saturation(r, cores) {
    const out = [];
    const ph = r.phases.measure || r.phases.hold || r.phases.ramp;
    if (!ph) return out;
    const busy = ph.cpu?.machine?.busyRatio;
    if (busy !== undefined && busy !== null) {
        out.push(`machine CPU busy ${Math.round(busy * 100)}% of ${cores} cores (server ${ph.cpu.serverCores} cores, load generator ${ph.cpu.loadgenCores} cores)`);
        if (busy > 0.9) out.push('CPU-bound: the machine is saturated (server and load generator compete for the same cores)');
        if ((ph.cpu.otherCores || 0) > 0.2) out.push(`other processes of the machine used ${ph.cpu.otherCores} cores during the window (shared host): the results are pessimistic`);
    }
    const shards = ph.server?.shards || {};
    const hot = Object.entries(shards).filter(([, s]) => (s.cpuMax || 0) > 0.9);
    if (hot.length) out.push(`shards near one full core: ${hot.map(([k, s]) => `${k} ${s.cpuMax}`).join(', ')}`);
    const lagged = Object.entries(shards).filter(([, s]) => (s.lagP99MaxMs || 0) > 50);
    if (lagged.length) out.push(`server event-loop lag p99 above 50 ms: ${lagged.map(([k, s]) => `${k} ${s.lagP99MaxMs} ms`).join(', ')}`);
    if ((ph.loadgen?.lagP99MaxMs || 0) > 50) out.push(`load generator event-loop lag p99 ${ph.loadgen.lagP99MaxMs} ms: client-side latencies include it`);
    const ramp = r.phases.ramp;
    if (ramp && ramp.connectionsFailed) out.push(`connection failures: ${JSON.stringify(ramp.failures)}`);
    const nof = r.machine.nofile;
    if (nof && ramp && ramp.failures && Object.keys(ramp.failures).some((k) => /EMFILE|ENFILE/.test(k))) out.push(`file descriptor limit reached (nofile ${nof.soft}/${nof.hard} per process)`);
    if (ramp && Object.keys(ramp.failures || {}).some((k) => /EADDRNOTAVAIL/.test(k))) out.push('ephemeral ports exhausted (EADDRNOTAVAIL)');
    return out;
}

function printSummary(r) {
    const L = [];
    L.push('');
    L.push(`==== ${r.params.scenario} summary ====`);
    L.push(`machine: ${r.machine.cores} x ${r.machine.cpuModel}, ${r.machine.memoryGB} GB, ${r.machine.node}, nofile ${r.machine.nofile ? `${r.machine.nofile.soft}/${r.machine.nofile.hard}` : '?'}`);
    L.push(`server: ${r.server.mode}${r.server.workers ? `, ${r.server.workers} workers` : ''}${r.server.reusePort ? ', SO_REUSEPORT' : ''}${r.server.tls === false ? ', TLS off (--plain)' : ''}; load: ${r.loadgen.procs} processes, ${r.loadgen.clients} clients${r.server.sameMachine ? ' (same machine)' : ''}`);
    const ramp = r.phases.ramp;
    if (ramp) {
        L.push(`connections: ${ramp.connectionsOk} ok, ${ramp.connectionsFailed} failed, ${ramp.connectionsPerSec}/s over ${ramp.rampSeconds} s`);
        const h = ramp.latencyMs;
        if (h.connect) L.push(`  TCP+TLS+upgrade ms: p50 ${h.connect.p50} p90 ${h.connect.p90} p99 ${h.connect.p99} max ${h.connect.max}`);
        if (h.hello) L.push(`  Hello->Welcome ms:  p50 ${h.hello.p50} p90 ${h.hello.p90} p99 ${h.hello.p99} max ${h.hello.max}`);
        L.push(`  CPU during ramp: server ${ramp.cpu.serverCores} cores, load generator ${ramp.cpu.loadgenCores} cores`);
    }
    const mem = r.results?.memoryPerConnection;
    if (mem) L.push(`server memory per idle connection: ${mem.rssPerConn} B RSS, ${mem.heapPerConn} B heap, ${mem.externalPerConn} B external (total RSS ${mem.totalRssMB} MB)`);
    const c = r.results?.connect;
    if (c) {
        if (c.heartbeatRttMs) L.push(`heartbeat round trip (client Ping->Pong) ms: p50 ${c.heartbeatRttMs.p50} p99 ${c.heartbeatRttMs.p99} max ${c.heartbeatRttMs.max} (${c.heartbeatRttMs.n} samples)`);
        if (c.serverHeartbeatRttMs?.n) L.push(`server-measured heartbeat RTT ms: p50 ${c.serverHeartbeatRttMs.p50} p99 ${c.serverHeartbeatRttMs.p99}`);
        L.push(`dropped during hold: ${c.droppedDuringHold}; server CPU while idle: ${c.serverCpuDuringHoldCores} cores`);
    }
    const g = r.results?.games;
    if (g) {
        L.push(`games: ${g.activeGames} active, ${g.movesPerSec} moves/s confirmed, ${g.gamesFinished} finished (${g.gamesPerMinute}/min), ${g.gamesStarted} started in the window`);
        if (g.quietWindows) L.push(`  quiet 2 s windows (other processes < 0.3 core): ${g.quietWindows}, median ${g.quietMovesPerSecMedian} moves/s, median p99 ${g.quietMoveRttP99MsMedian} ms`);
        if (g.moveRttMs) L.push(`  move round trip ms: p50 ${g.moveRttMs.p50} p90 ${g.moveRttMs.p90} p99 ${g.moveRttMs.p99} p99.9 ${g.moveRttMs.p999} max ${g.moveRttMs.max}`);
        if (g.serverMoveProcessingUs?.n) L.push(`  server move processing us (bucketed): p50 ${g.serverMoveProcessingUs.p50} p99 ${g.serverMoveProcessingUs.p99}`);
        if (g.gameStartMs) L.push(`  game start (challenge -> both snapshots) ms: p50 ${g.gameStartMs.p50} p99 ${g.gameStartMs.p99}`);
        L.push(`  rejected: ${JSON.stringify(g.rejected)} errors: ${JSON.stringify(g.errors)} dropped: ${g.dropped}`);
        if (g.gesturesSentPerSec) L.push(`  gestures/s: sent ${g.gesturesSentPerSec}, relayed ${g.serverGesturesRelayedPerSec ?? '-'} (server), received ${g.gesturesReceivedPerSec}; dropped by the server: ${JSON.stringify(g.serverGesturesDropped || {})}`);
        L.push(`  CPU: server ${g.serverCores} cores, load generator ${g.loadgenCores} cores, machine busy ${g.machineBusy !== null ? Math.round(g.machineBusy * 100) : '-'}%`);
        for (const [k, s] of Object.entries(g.serverShards || {})) L.push(`    ${k.padEnd(8)} cpu mean ${s.cpuMean} max ${s.cpuMax}  lag p99 mean ${s.lagP99MeanMs} max ${s.lagP99MaxMs} ms  rss ${s.rssMB} MB  conns ${s.conns ?? '-'} games ${s.games ?? '-'}`);
    }
    if (r.saturation?.length) { L.push('limits:'); for (const s of r.saturation) L.push(`  - ${s}`); }
    if (r.notes?.length) for (const n of r.notes) L.push(`note: ${n}`);
    process.stdout.write(`${L.join('\n')}\n`);
}

// Entry point (at the end: the helpers above must be initialised first).
if (process.argv[2] === '--worker') {
    await import('./lib/worker.js');
} else {
    await main();
}
