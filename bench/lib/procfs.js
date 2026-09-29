// Linux /proc readers: CPU time and RSS of processes (server primary, shards, analysis process,
// load processes), process trees, the machine's CPU usage and the open-file limits. Every
// function returns null (or an empty result) when /proc is not available, so the benchmark still
// runs elsewhere with the server's own metrics only.

import { execFileSync } from 'node:child_process';
import fs from 'node:fs';
import os from 'node:os';

let tck = 0;
/** Clock ticks per second (utime/stime unit). */
export function clkTck() {
    if (tck) return tck;
    try { tck = Number(execFileSync('getconf', ['CLK_TCK'], { encoding: 'utf8' }).trim()) || 100; } catch { tck = 100; }
    return tck;
}

let pageSize = 0;
function pageBytes() {
    if (pageSize) return pageSize;
    try { pageSize = Number(execFileSync('getconf', ['PAGESIZE'], { encoding: 'utf8' }).trim()) || 4096; } catch { pageSize = 4096; }
    return pageSize;
}

/**
 * CPU ticks (user + system) and RSS of a process.
 * @param {number} pid
 * @returns {{ ticks: number, rss: number, ppid: number } | null}
 */
export function procStat(pid) {
    try {
        const s = fs.readFileSync(`/proc/${pid}/stat`, 'latin1');
        const f = s.slice(s.lastIndexOf(')') + 2).split(' ');
        // f[0] = state (field 3); utime = field 14, stime = 15, rss = 24 (pages).
        return { ppid: Number(f[1]), ticks: Number(f[11]) + Number(f[12]), rss: Number(f[21]) * pageBytes() };
    } catch {
        return null;
    }
}

/** Pids of every descendant of `root` (not included). */
export function descendants(root) {
    let entries;
    try { entries = fs.readdirSync('/proc'); } catch { return []; }
    const children = new Map();
    for (const e of entries) {
        if (!/^\d+$/.test(e)) continue;
        const st = procStat(Number(e));
        if (!st) continue;
        if (!children.has(st.ppid)) children.set(st.ppid, []);
        children.get(st.ppid).push(Number(e));
    }
    const out = [];
    const stack = [root];
    while (stack.length) {
        for (const c of children.get(stack.pop()) || []) { out.push(c); stack.push(c); }
    }
    return out;
}

/** One environment variable of a process (null when unreadable). */
export function procEnv(pid, key) {
    try {
        for (const kv of fs.readFileSync(`/proc/${pid}/environ`, 'latin1').split('\0')) if (kv.startsWith(`${key}=`)) return kv.slice(key.length + 1);
    } catch { /* gone or not ours */ }
    return null;
}

/** Command line of a process. */
export function procCmdline(pid) {
    try { return fs.readFileSync(`/proc/${pid}/cmdline`, 'latin1').split('\0').filter(Boolean).join(' '); } catch { return ''; }
}

/** Machine CPU counters { busy, total } in ticks (all cores), from /proc/stat. */
export function machineTicks() {
    try {
        const line = fs.readFileSync('/proc/stat', 'latin1').split('\n')[0];
        const v = line.trim().split(/\s+/).slice(1).map(Number);
        const idle = v[3] + (v[4] || 0);
        const total = v.reduce((a, b) => a + b, 0);
        return { busy: total - idle, total };
    } catch {
        return null;
    }
}

/** { soft, hard } open-file limits of this process. */
export function nofileLimits() {
    try {
        const line = fs.readFileSync('/proc/self/limits', 'latin1').split('\n').find((l) => l.startsWith('Max open files'));
        const [soft, hard] = line.replace('Max open files', '').trim().split(/\s+/);
        return { soft: soft === 'unlimited' ? Infinity : Number(soft), hard: hard === 'unlimited' ? Infinity : Number(hard) };
    } catch {
        return null;
    }
}

/** Integer sysctl value (or the raw text), null when unreadable. */
export function sysctl(p) {
    try {
        const t = fs.readFileSync(`/proc/sys/${p}`, 'latin1').trim();
        return /^\d+$/.test(t) ? Number(t) : t;
    } catch {
        return null;
    }
}

/** Description of the machine for the report. */
export function machineInfo() {
    const cpus = os.cpus();
    return {
        hostname: os.hostname(),
        platform: `${os.platform()} ${os.release()}`,
        arch: os.arch(),
        cpuModel: cpus[0]?.model || 'unknown',
        cores: cpus.length,
        memoryGB: Number((os.totalmem() / 2 ** 30).toFixed(1)),
        freeMemoryGB: Number((os.freemem() / 2 ** 30).toFixed(1)),
        node: process.version,
        nofile: nofileLimits(),
        nrOpen: sysctl('fs/nr_open'),
        fileMax: sysctl('fs/file-max'),
        ephemeralPorts: sysctl('net/ipv4/ip_local_port_range'),
        somaxconn: sysctl('net/core/somaxconn'),
    };
}

/**
 * CPU sampler over a set of processes: sample() records ticks; since(prev) gives cores used.
 */
export class CpuSampler {
    constructor() { this.hz = clkTck(); }
    /** @param {Record<string, number>} pids name -> pid */
    sample(pids) {
        const out = { at: performance.now(), procs: {}, machine: machineTicks() };
        for (const [name, pid] of Object.entries(pids)) {
            const st = procStat(pid);
            if (st) out.procs[name] = st;
        }
        return out;
    }
    /** Cores used by each process between two samples, and by the machine. */
    delta(a, b) {
        const dt = (b.at - a.at) / 1000;
        const procs = {};
        for (const [name, s] of Object.entries(b.procs)) {
            const p = a.procs[name];
            if (p) procs[name] = { cores: Number(((s.ticks - p.ticks) / this.hz / dt).toFixed(3)), rss: s.rss };
        }
        let machine = null;
        if (a.machine && b.machine) {
            const busy = b.machine.busy - a.machine.busy, total = b.machine.total - a.machine.total;
            machine = { busyRatio: total ? Number((busy / total).toFixed(3)) : 0, coresBusy: total ? Number((busy / total * os.cpus().length).toFixed(2)) : 0 };
        }
        return { seconds: Number(dt.toFixed(2)), procs, machine };
    }
}
