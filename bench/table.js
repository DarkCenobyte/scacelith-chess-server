#!/usr/bin/env node
// Markdown tables of loadgen reports (bench/results/*.json), for docs/BENCHMARK.md.
//
//   node bench/table.js bench/results/*.json

import fs from 'node:fs';

const files = process.argv.slice(2);
if (!files.length) { process.stdout.write('usage: node bench/table.js REPORT.json...\n'); process.exit(2); }
const reports = files.map((f) => ({ f, r: JSON.parse(fs.readFileSync(f, 'utf8')) })).sort((a, b) => a.r.startedAt.localeCompare(b.r.startedAt));
const v = (x, d = 1) => (x === undefined || x === null ? '-' : typeof x === 'number' ? Number(x.toFixed(d)).toString() : String(x));
const kb = (b) => (b ? `${(b / 1024).toFixed(1)}` : '-');

const conn = reports.filter(({ r }) => r.params.scenario === 'connect');
if (conn.length) {
    process.stdout.write('| run | workers / load procs | connected (failed) | conn/s | TCP+TLS+101 p50 / p99 ms | Hello->Welcome p50 / p99 ms | server CPU per conn (ms) | server KB/conn RSS (heap) | server RSS total | heartbeat RTT p50 / p99 ms (client) | other procs (cores) |\n');
    process.stdout.write('|---|---|---|---|---|---|---|---|---|---|---|\n');
    for (const { r } of conn) {
        const ramp = r.phases.ramp, hold = r.phases.hold, c = r.results?.connect || {}, m = r.results?.memoryPerConnection;
        const cpuPerConn = ramp.connectionsOk ? ramp.cpu.serverCores * ramp.cpu.seconds / ramp.connectionsOk * 1000 : null;
        process.stdout.write(`| ${r.label || r.params.conns} | ${r.server.workers ?? '-'}${r.server.reusePort ? ' (reuseport)' : ''} / ${r.loadgen.procs} | ${ramp.connectionsOk} (${ramp.connectionsFailed}) | ${ramp.connectionsPerSec} `
            + `| ${v(ramp.latencyMs.connect?.p50, 0)} / ${v(ramp.latencyMs.connect?.p99, 0)} | ${v(ramp.latencyMs.hello?.p50, 0)} / ${v(ramp.latencyMs.hello?.p99, 0)} `
            + `| ${v(cpuPerConn, 2)} | ${kb(m?.rssPerConn)} (${kb(m?.heapPerConn)}) | ${m ? `${m.totalRssMB} MB` : '-'} `
            + `| ${v(hold?.latencyMs?.hb?.p50, 1)} / ${v(hold?.latencyMs?.hb?.p99, 1)} | ${v(ramp.cpu.otherCores, 2)} / ${v(hold?.cpu?.otherCores, 2)} |\n`);
    }
    process.stdout.write('\n');
}

const games = reports.filter(({ r }) => r.params.scenario !== 'connect');
if (games.length) {
    process.stdout.write('| run | games (conns) | pace ms | moves/s (quiet median) | move RTT p50 / p90 / p99 / max ms | server move us p50 / p99 | games ended /min | server cores | loadgen cores | shard lag p99 max ms | machine busy | other procs (cores) |\n');
    process.stdout.write('|---|---|---|---|---|---|---|---|---|---|---|---|\n');
    for (const { r } of games) {
        const g = r.results?.games;
        if (!g) continue;
        const lag = Math.max(0, ...Object.entries(g.serverShards || {}).filter(([k]) => k !== 'primary').map(([, s]) => s.lagP99MaxMs || 0));
        const rt = g.moveRttMs || {};
        process.stdout.write(`| ${r.label || r.params.scenario} | ${r.params.games} (${r.loadgen.clients}) | ${r.params.moveIntervalMs} | ${g.movesPerSec} (${v(g.quietMovesPerSecMedian, 0)}) `
            + `| ${v(rt.p50, 2)} / ${v(rt.p90, 1)} / ${v(rt.p99, 1)} / ${v(rt.max, 0)} | ${v(g.serverMoveProcessingUs?.p50, 0)} / ${v(g.serverMoveProcessingUs?.p99, 0)} `
            + `| ${g.gamesPerMinute} | ${v(g.serverCores, 2)} | ${v(g.loadgenCores, 2)} | ${v(lag, 0)} | ${g.machineBusy !== null ? `${Math.round(g.machineBusy * 100)}%` : '-'} | ${v(r.phases.measure?.cpu?.otherCores, 2)} |\n`);
    }
    process.stdout.write('\n');
}
