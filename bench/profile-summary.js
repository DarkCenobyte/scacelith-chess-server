#!/usr/bin/env node
// Summary of V8 CPU profiles (.cpuprofile, from `node --cpu-prof` or loadgen --server-cpu-prof):
// where the time goes (self time by function and by area), how busy the event loop was, and the
// longest stretches spent without leaving one stack: a synchronous call that blocks the event
// loop (a SQLite transaction waiting for the lock or an fsync, a long garbage collection...)
// shows up there as one run of identical samples.
//
//   node bench/profile-summary.js [--top 25] [--stalls 8] [--min-stall-ms 20]
//                                 [--callers FUNCTION [--depth 6]] FILE.cpuprofile...

import fs from 'node:fs';
import path from 'node:path';

const args = process.argv.slice(2);
const opt = { top: 25, stalls: 8, minStallMs: 20, callers: null, depth: 6 };
const files = [];
for (let i = 0; i < args.length; i++) {
    if (args[i] === '--top') opt.top = Number(args[++i]);
    else if (args[i] === '--stalls') opt.stalls = Number(args[++i]);
    else if (args[i] === '--min-stall-ms') opt.minStallMs = Number(args[++i]);
    else if (args[i] === '--callers') opt.callers = args[++i];
    else if (args[i] === '--depth') opt.depth = Number(args[++i]);
    else files.push(args[i]);
}
if (!files.length) {
    process.stdout.write('usage: node bench/profile-summary.js [--top N] [--stalls N] [--min-stall-ms N] FILE.cpuprofile...\n');
    process.exit(2);
}

function short(url) {
    if (!url) return '';
    const u = url.replace(/^file:\/\//, '');
    const i = u.indexOf('/dedicated-server/');
    return i >= 0 ? u.slice(i + 18) : u.replace(/^node:/, 'node:');
}

function frameName(n) {
    const cf = n.callFrame;
    const fn = cf.functionName || '(anonymous)';
    return cf.url ? `${fn} ${short(cf.url)}:${cf.lineNumber + 1}` : fn;
}

// Coarse area of a leaf frame.
function area(n) {
    const cf = n.callFrame;
    const fn = cf.functionName;
    const u = cf.url || '';
    if (fn === '(garbage collector)') return 'gc';
    if (fn === '(idle)') return 'idle';
    if (fn === '(program)') return 'program (native, outside JS)';
    if (/node:internal\/(tls|crypto)|_tls_/.test(u)) return 'tls (JS side)';
    if (/node:internal\/(net|stream|streams)|node:net|node:stream/.test(u)) return 'net/streams';
    if (/node:sqlite|sqlite/.test(u) || /^(run|all|get|exec|prepare)$/.test(fn) && !u) return 'sqlite (native)';
    if (/node:/.test(u)) return 'node internals (other)';
    if (/\/src\/net\//.test(u)) return 'src/net';
    if (/\/src\/cluster\//.test(u)) return 'src/cluster';
    if (/\/src\/game\//.test(u)) return 'src/game';
    if (/\/src\/chess\//.test(u)) return 'src/chess';
    if (/\/src\/protocol\//.test(u)) return 'src/protocol';
    if (/\/src\/store\//.test(u)) return 'src/store';
    if (/\/src\/auth\/|\/src\/security\//.test(u)) return 'src/auth';
    if (/\/src\//.test(u)) return 'src/other';
    if (!u) return `native: ${fn}`;
    return 'other';
}

for (const file of files) {
    const p = JSON.parse(fs.readFileSync(file, 'utf8'));
    const byId = new Map(p.nodes.map((n) => [n.id, n]));
    const parent = new Map();
    for (const n of p.nodes) for (const c of n.children || []) parent.set(c, n.id);
    const total = (p.endTime - p.startTime) / 1000;
    const self = new Map();
    const areas = new Map();
    let idle = 0;
    const deltas = p.timeDeltas;
    // Sample i's duration: the delta to the next sample.
    for (let i = 0; i < p.samples.length; i++) {
        const d = (i + 1 < deltas.length ? deltas[i + 1] : 0) / 1000;
        const n = byId.get(p.samples[i]);
        const a = area(n);
        if (a === 'idle') { idle += d; continue; }
        const k = frameName(n);
        self.set(k, (self.get(k) || 0) + d);
        areas.set(a, (areas.get(a) || 0) + d);
    }
    const busy = total - idle;
    const out = [];
    out.push(`== ${path.basename(file)}: ${(total / 1000).toFixed(1)} s profiled, busy ${(busy / 1000).toFixed(1)} s (${(busy / total * 100).toFixed(0)}%)`);
    out.push('  by area (self time):');
    for (const [a, ms] of [...areas].sort((x, y) => y[1] - x[1]).slice(0, 14)) out.push(`    ${(ms / busy * 100).toFixed(1).padStart(5)}%  ${(ms / 1000).toFixed(2).padStart(7)} s  ${a}`);
    out.push(`  top ${opt.top} functions (self time):`);
    for (const [k, ms] of [...self].sort((x, y) => y[1] - x[1]).slice(0, opt.top)) out.push(`    ${(ms / busy * 100).toFixed(1).padStart(5)}%  ${ms.toFixed(0).padStart(7)} ms  ${k}`);

    // --callers NAME: the call paths leading to a function (self time), aggregated.
    if (opt.callers) {
        const paths = new Map();
        let sum = 0;
        for (let i = 0; i < p.samples.length; i++) {
            const n = byId.get(p.samples[i]);
            if (n.callFrame.functionName !== opt.callers) continue;
            const d = (i + 1 < deltas.length ? deltas[i + 1] : 0) / 1000;
            const stack = [];
            for (let id = parent.get(n.id); id !== undefined && stack.length < opt.depth; id = parent.get(id)) {
                const f = byId.get(id);
                if (f.callFrame.functionName === '(root)') break;
                stack.push(frameName(f));
            }
            const k = stack.join(' < ');
            paths.set(k, (paths.get(k) || 0) + d);
            sum += d;
        }
        out.push(`  callers of ${opt.callers} (${(sum / 1000).toFixed(2)} s):`);
        for (const [k, ms] of [...paths].sort((x, y) => y[1] - x[1]).slice(0, 12)) out.push(`    ${(ms / sum * 100).toFixed(1).padStart(5)}%  ${k}`);
    }

    // Longest runs of identical samples.
    const runs = [];
    let start = 0;
    for (let i = 1; i <= p.samples.length; i++) {
        if (i < p.samples.length && p.samples[i] === p.samples[start]) continue;
        let ms = 0;
        for (let j = start; j < i; j++) ms += (j + 1 < deltas.length ? deltas[j + 1] : 0) / 1000;
        const n = byId.get(p.samples[start]);
        if (ms >= opt.minStallMs && area(n) !== 'idle') {
            let at = 0;
            for (let j = 0; j <= start; j++) at += deltas[j];
            runs.push({ ms, node: p.samples[start], at: at / 1e6 });
        }
        start = i;
    }
    runs.sort((a, b) => b.ms - a.ms);
    const stallTotal = runs.reduce((a, r) => a + r.ms, 0);
    out.push(`  event loop held by one stack for >= ${opt.minStallMs} ms: ${runs.length} times, ${(stallTotal / 1000).toFixed(2)} s in all; longest:`);
    for (const r of runs.slice(0, opt.stalls)) {
        const stack = [];
        for (let id = r.node; id !== undefined && stack.length < 9; id = parent.get(id)) {
            const n = byId.get(id);
            if (n.callFrame.functionName === '(root)') break;
            stack.push(frameName(n));
        }
        out.push(`    ${r.ms.toFixed(0).padStart(5)} ms at +${r.at.toFixed(1)} s: ${stack.join(' < ')}`);
    }
    process.stdout.write(`${out.join('\n')}\n`);
}
