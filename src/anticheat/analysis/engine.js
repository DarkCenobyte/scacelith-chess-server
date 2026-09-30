// UCI engine driver (Stockfish or any UCI engine) for the post-game analysis.
//
// One UciEngine owns one engine process: spawned directly from ANALYSIS_ENGINE_PATH (never
// through a shell), one search thread, a small hash, low CPU priority. Commands are strictly
// sequential (one search at a time per engine); an EnginePool hands engines to concurrent
// analyses. Every wait has a timeout: an engine that does not answer is killed and restarted,
// and the caller gets an EngineError it can turn into a failed job.
//
// Scores are reported as the engine gives them: from the side to move's point of view. The
// engine's identity is learnt when it starts: its `id name` and the evaluation network(s) it
// reports (name, net), which are part of the analysis profile (analyzer.js). Where its network
// lives (netMemory) is learnt then too, but it is operational only and not part of the profile:
// it changes the memory the engines take, never their results.

import { spawn } from 'node:child_process';
import os from 'node:os';

/** Error raised by the engine driver; `code` is 'timeout' | 'crashed' | 'spawn' | 'closed'. */
export class EngineError extends Error {
    constructor(code, message) {
        super(message);
        this.name = 'EngineError';
        this.code = code;
    }
}

const SKIP_ONE = new Set(['seldepth', 'nodes', 'nps', 'time', 'hashfull', 'tbhits', 'currmovenumber', 'cpuload', 'currmove', 'sbhits']);
// Nodes searched so far, as an info line reports them.
const NODES = /\bnodes (\d+)/;

// Stockfish names the network(s) it evaluates with when a search starts, e.g. "info string NNUE
// evaluation using nn-1a298aa575a0.nnue (109MiB, ...)" (Stockfish 16: "... nn-5af11540bbfe.nnue
// enabled"; one line per network when it has several).
const NET_LINE = /^info string NNUE evaluation using (\S+)/;

// Stockfish 19 and later keep the network in memory shared by every process of the same
// executable, user and network (on Linux a memfd handed over unix sockets under
// /tmp/stockfish-<uid>/, on Windows a named file mapping), and say with every search where each
// replica lives (one per NUMA node in use): "info string Network replica 1: Shared memory.", or
// "... Local memory. <why>" when sharing failed and the process holds its own copy. Earlier
// versions and other engines say nothing.
const REPLICA_LINE = /^info string Network replica (\d+): (?:(Shared memory|Local memory|No allocation)\.\s*)?(.*)$/;
const REPLICA_MEMORY = { 'Shared memory': 'shared', 'Local memory': 'local', 'No allocation': 'none' };

/**
 * Parses Stockfish's report of where a replica of its network lives. Returns null for any other
 * line. `memory` is 'unknown' for a status this parser does not know (a later wording), whose
 * text is then the `error`.
 * @param {string} line
 * @returns {null | { replica: number, memory: 'shared'|'local'|'none'|'unknown', error: string|null }}
 */
export function parseNetworkReplica(line) {
    const m = REPLICA_LINE.exec(String(line).trim());
    if (!m) return null;
    return { replica: Number(m[1]), memory: m[2] ? REPLICA_MEMORY[m[2]] : 'unknown', error: m[3] || null };
}

/**
 * Where a process's network lives, from the replica reports of one search: 'shared' when every
 * allocated replica is in shared memory, 'local' when one is not (its explanation in `error`),
 * null when the engine reported none.
 * @param {{ memory: string, error: string|null }[]} replicas
 * @returns {{ memory: 'shared'|'local'|null, error: string|null }}
 */
export function networkMemory(replicas) {
    if (!replicas.length) return { memory: null, error: null };
    const unshared = replicas.find((r) => r.memory !== 'shared' && r.memory !== 'none');
    if (unshared) return { memory: 'local', error: unshared.error };
    if (replicas.some((r) => r.memory === 'shared')) return { memory: 'shared', error: null };
    return { memory: 'local', error: 'no replica allocated' };
}

function toInt(s) {
    const v = Number.parseInt(s, 10);
    return Number.isFinite(v) ? v : null;
}

/**
 * Parses one UCI `info` line into { depth, seldepth, multipv, cp, mate, bound, wdl, pv }.
 * Returns null for lines that carry no score (currmove updates, `info string ...`) and for any
 * line that is not an `info` line. `bound` is null, 'lowerbound' or 'upperbound'; `cp` and
 * `mate` are exclusive (the other one is null).
 * @param {string} line
 * @returns {null | { depth: number, seldepth: number, multipv: number, cp: number|null, mate: number|null, bound: string|null, wdl: number[]|null, pv: string[] }}
 */
export function parseInfoLine(line) {
    const t = String(line).trim().split(/\s+/);
    if (t[0] !== 'info') return null;
    const out = { depth: 0, seldepth: 0, multipv: 1, cp: null, mate: null, bound: null, wdl: null, pv: [] };
    for (let i = 1; i < t.length; i++) {
        const k = t[i];
        if (k === 'string') return null;
        if (k === 'depth') { out.depth = toInt(t[++i]) ?? 0; continue; }
        if (k === 'seldepth') { out.seldepth = toInt(t[++i]) ?? 0; continue; }
        if (k === 'multipv') { out.multipv = toInt(t[++i]) ?? 1; continue; }
        if (k === 'score') {
            const kind = t[++i];
            const v = toInt(t[++i]);
            if (v === null) return null;
            if (kind === 'cp') out.cp = v;
            else if (kind === 'mate') out.mate = v;
            else return null;
            continue;
        }
        if (k === 'lowerbound' || k === 'upperbound') { out.bound = k; continue; }
        if (k === 'wdl') { out.wdl = [toInt(t[i + 1]), toInt(t[i + 2]), toInt(t[i + 3])]; i += 3; continue; }
        if (k === 'pv') { out.pv = t.slice(i + 1); break; }
        if (k === 'refutation' || k === 'currline') break;
        if (SKIP_ONE.has(k)) { i++; continue; }
        // Unknown token: ignore it (engines add their own keys).
    }
    if (out.cp === null && out.mate === null) return null;
    return out;
}

/**
 * Keeps, per multipv index, the most informative line of a search: the deepest one, and at the
 * same depth an exact score over a bound (a later exact line replaces an earlier one).
 * @param {Map<number, object>} acc
 * @param {object} info  parsed info line
 */
export function mergeInfo(acc, info) {
    const prev = acc.get(info.multipv);
    if (!prev || info.depth > prev.depth || (info.depth === prev.depth && (!info.bound || prev.bound))) {
        // A score line without a pv (mate 0 / stalemate at depth 0) is still kept.
        acc.set(info.multipv, info);
    }
}

/**
 * Final lines of a search, sorted by multipv. Lines from an iteration older than the deepest
 * one are dropped (they were not re-searched), except the first line which always exists.
 * @param {Map<number, object>} acc
 * @returns {{ multipv: number, depth: number, cp: number|null, mate: number|null, bound: string|null, move: string|null, pv: string[] }[]}
 */
export function finalLines(acc) {
    const all = [...acc.values()].sort((a, b) => a.multipv - b.multipv);
    if (!all.length) return [];
    const maxDepth = Math.max(...all.map((l) => l.depth));
    return all
        .filter((l, i) => i === 0 || l.depth >= maxDepth - 1)
        .map((l) => ({ multipv: l.multipv, depth: l.depth, cp: l.cp, mate: l.mate, bound: l.bound, move: l.pv[0] || null, pv: l.pv }));
}

/**
 * One engine process.
 */
export class UciEngine {
    /**
     * @param {object} o
     * @param {string} o.path            engine executable (no shell, no arguments from users)
     * @param {string[]} [o.args]
     * @param {number} [o.threads=1]
     * @param {number} [o.hashMb=16]
     * @param {number} [o.timeoutMs=60000]  longest search before the engine is restarted
     * @param {number} [o.handshakeTimeoutMs=10000]  uci / isready answers
     * @param {boolean} [o.lowPriority=true]
     * @param {object} [o.log]
     */
    constructor({ path, args = [], threads = 1, hashMb = 16, timeoutMs = 60000, handshakeTimeoutMs = 10000, lowPriority = true, log = null }) {
        if (!path) throw new EngineError('spawn', 'engine path is empty');
        this.path = path;
        this.args = args;
        this.threads = threads;
        this.hashMb = hashMb;
        this.timeoutMs = timeoutMs;
        this.handshakeTimeoutMs = handshakeTimeoutMs;
        this.lowPriority = lowPriority;
        this.log = log;
        this.proc = null;
        this.name = '';
        this.nets = [];                 // evaluation networks the running engine reported, in order
        this.netMemory = null;          // where its network lives: 'shared', 'local', null (not reported)
        this.netMemoryError = null;     // why it is not shared, as the engine says
        this.options = new Map();      // option name -> current value (to avoid resending)
        this.buffer = '';
        this.onLine = null;             // current consumer of output lines
        this.onExit = null;
        this.starting = null;
        this.closed = false;
        this.restarts = 0;
        this.starts = 0;                // processes started and past the handshake
        this.searches = 0;
    }

    get alive() { return !!this.proc && this.proc.exitCode === null && !this.proc.killed; }

    /**
     * The evaluation network(s) the running engine reported ('+'-joined file names), null before
     * it started or when it reports none (an engine without NNUE, another engine).
     */
    get net() { return this.nets.length ? this.nets.join('+') : null; }

    /** Starts the process (idempotent), completes the UCI handshake and learns its name and network. */
    async start() {
        if (this.closed) throw new EngineError('closed', 'engine closed');
        if (this.alive && !this.starting) return this;
        if (this.starting) return this.starting;
        this.starting = this._start().finally(() => { this.starting = null; });
        return this.starting;
    }

    async _start() {
        let proc;
        try {
            proc = spawn(this.path, this.args, { stdio: ['pipe', 'pipe', 'ignore'], shell: false, windowsHide: true });
        } catch (e) {
            throw new EngineError('spawn', `cannot start engine: ${e.message}`);
        }
        this.proc = proc;
        this.buffer = '';
        this.options.clear();
        this.nets = [];                 // a restarted engine may be another build: it reports anew
        this.netMemory = null;
        this.netMemoryError = null;
        proc.stdout.setEncoding('utf8');
        proc.stdout.on('data', (chunk) => this._onData(chunk));
        proc.stdin.on('error', () => { /* EPIPE when the engine died: reported through 'exit' */ });
        proc.on('exit', (code, signal) => {
            // A process killed on purpose (timeout) is reported by whoever killed it.
            if (this.proc !== proc) return;
            this.proc = null;
            const cb = this.onExit;
            this.onExit = null;
            this.onLine = null;
            if (cb) cb(new EngineError('crashed', `engine exited (code ${code}, signal ${signal})`));
        });
        const spawned = new Promise((resolve, reject) => {
            proc.once('spawn', resolve);
            proc.once('error', (e) => {
                if (this.proc === proc) this.proc = null;
                reject(new EngineError('spawn', `cannot start engine: ${e.message}`));
            });
        });
        await spawned;
        if (this.lowPriority) {
            try { os.setPriority(proc.pid, os.constants.priority.PRIORITY_LOW); } catch { /* not permitted: keep going */ }
        }
        const id = await this._command('uci', (line, st) => {
            if (line.startsWith('id name ')) st.name = line.slice(8).trim();
            return line === 'uciok' ? st.name || '' : undefined;
        }, this.handshakeTimeoutMs);
        this.name = id || 'unknown engine';
        this._setOption('Threads', this.threads);
        this._setOption('Hash', this.hashMb);
        await this.ready();
        // The network, and where it lives, are only reported when a search starts: a one-ply
        // search of the initial position reports them before any analysis, so every record names
        // its full profile. (Stockfish repeats both with every search; they do not change while
        // the process lives.)
        this._send('position startpos');
        const replicas = [];
        await this._command('go depth 1', (line) => {
            if (line.startsWith('info string ')) {
                const net = NET_LINE.exec(line)?.[1];
                if (net && !this.nets.includes(net)) this.nets.push(net);
                const replica = parseNetworkReplica(line);
                if (replica) replicas.push(replica);
                return undefined;
            }
            return line.startsWith('bestmove') ? true : undefined;
        }, this.handshakeTimeoutMs);
        ({ memory: this.netMemory, error: this.netMemoryError } = networkMemory(replicas));
        this.starts++;
        return this;
    }

    _onData(chunk) {
        this.buffer += chunk;
        let nl;
        while ((nl = this.buffer.indexOf('\n')) >= 0) {
            const line = this.buffer.slice(0, nl).replace(/\r$/, '');
            this.buffer = this.buffer.slice(nl + 1);
            if (this.onLine) this.onLine(line);
        }
        if (this.buffer.length > 1 << 20) this.buffer = '';    // runaway output without newline
    }

    _send(cmd) {
        if (!this.alive) throw new EngineError('crashed', 'engine is not running');
        this.proc.stdin.write(cmd + '\n');
    }

    _setOption(name, value) {
        if (this.options.get(name) === value) return;
        this._send(value === undefined ? `setoption name ${name}` : `setoption name ${name} value ${value}`);
        if (value !== undefined) this.options.set(name, value);
    }

    // Sends `cmd` and feeds every output line to `handler(line, state)` until it returns a value
    // other than undefined. On timeout, `onTimeout(state)` may try to recover (e.g. send `stop`,
    // after which the handler sees state.timedOut and throws); without an answer within 2 s more
    // the process is killed.
    _command(cmd, handler, timeoutMs, onTimeout) {
        return new Promise((resolve, reject) => {
            const state = {};
            let timer = null, graceTimer = null;
            const done = (err, value) => {
                clearTimeout(timer);
                clearTimeout(graceTimer);
                this.onLine = null;
                this.onExit = null;
                if (err) reject(err); else resolve(value);
            };
            this.onExit = (err) => done(err);
            this.onLine = (line) => {
                let v;
                try { v = handler(line, state); } catch (e) { done(e); return; }
                if (v !== undefined) done(null, v);
            };
            timer = setTimeout(() => {
                state.timedOut = true;
                if (onTimeout) {
                    try { onTimeout(state); } catch { /* engine gone */ }
                    graceTimer = setTimeout(() => { this._kill(); done(new EngineError('timeout', `engine did not answer "${cmd.split(' ')[0]}"`)); }, 2000);
                } else {
                    this._kill();
                    done(new EngineError('timeout', `engine did not answer "${cmd.split(' ')[0]}"`));
                }
            }, timeoutMs);
            try { this._send(cmd); } catch (e) { done(e); }
        });
    }

    _kill() {
        const p = this.proc;
        this.proc = null;
        this.onLine = null;
        if (p && p.exitCode === null) {
            try { p.kill('SIGKILL'); } catch { /* already gone */ }
        }
    }

    /** Waits for `readyok`. */
    async ready(timeoutMs = this.handshakeTimeoutMs) {
        await this._command('isready', (line) => (line === 'readyok' ? true : undefined), timeoutMs);
    }

    /** `ucinewgame` + isready (clears the engine's game state). */
    async newGame() {
        await this.start();
        this._send('ucinewgame');
        await this.ready();
    }

    /** Empties the transposition table, so a search does not profit from earlier (deeper) ones. */
    async clearHash() {
        await this.start();
        this._send('setoption name Clear Hash');
        await this.ready();
    }

    /**
     * Searches a position to a fixed depth, within `nodes` nodes when given.
     * @param {string[]} moves  UCI moves from the start position (or from `fen`)
     * @param {{ depth: number, multiPv?: number, fen?: string, nodes?: number|null }} opts
     * @returns {Promise<{ lines: object[], bestmove: string|null, nodeLimited: boolean }>}  bestmove null when the side to
     *          move has no legal move; nodeLimited: the node limit ended the search before the depth was complete (the
     *          lines are those of an unfinished search)
     */
    async analyse(moves, { depth, multiPv = 1, fen = null, nodes = null } = {}) {
        await this.start();
        try {
            return await this._analyse(moves, depth, multiPv, fen, nodes);
        } catch (e) {
            if (e instanceof EngineError && (e.code === 'timeout' || e.code === 'crashed') && !e.recovered) {
                // The next call starts a fresh process.
                this.restarts++;
                this.log?.warn('engine killed, restarts on next use', { reason: e.code, restarts: this.restarts });
                this._kill();
            }
            throw e;
        }
    }

    async _analyse(moves, depth, multiPv, fen, nodes) {
        this._setOption('MultiPV', multiPv);
        const pos = (fen ? `position fen ${fen}` : 'position startpos') + (moves.length ? ` moves ${moves.join(' ')}` : '');
        this._send(pos);
        const acc = new Map();
        const limit = nodes > 0 ? Math.floor(nodes) : 0;
        let reached = 0, searched = 0;
        this.searches++;
        return this._command(`go depth ${depth | 0}${limit ? ` nodes ${limit}` : ''}`, (line, state) => {
            if (line.startsWith('info ')) {
                const info = parseInfoLine(line);
                if (info) { mergeInfo(acc, info); reached = Math.max(reached, info.depth); }
                const n = NODES.exec(line);
                if (n) searched = Math.max(searched, Number(n[1]));
                return undefined;
            }
            if (line.startsWith('bestmove')) {
                if (state.timedOut) {
                    // Stopped early: the partial result is not the fixed-depth answer; refuse it
                    // but keep the (responsive) engine.
                    const err = new EngineError('timeout', `search of depth ${depth} exceeded ${this.timeoutMs} ms`);
                    err.recovered = true;
                    throw err;
                }
                const parts = line.split(/\s+/);
                const best = parts[1] && parts[1] !== '(none)' && parts[1] !== '0000' ? parts[1] : null;
                // Stopped by the limit: the last lines report the limit reached (Stockfish prints the
                // unfinished iteration when it stops), or the last complete iteration is shallower
                // than the depth asked for (it printed nothing when it stopped).
                const nodeLimited = !!limit && best !== null && (searched >= limit || reached < depth);
                return { lines: finalLines(acc), bestmove: best, nodeLimited };
            }
            return undefined;
        }, this.timeoutMs, () => this._send('stop'));
    }

    /** Asks the engine to quit, then kills it if it lingers. */
    async close() {
        this.closed = true;
        const p = this.proc;
        if (!p) return;
        await new Promise((resolve) => {
            const t = setTimeout(() => { this._kill(); resolve(); }, 1000);
            p.once('exit', () => { clearTimeout(t); resolve(); });
            try { p.stdin.write('quit\n'); p.stdin.end(); } catch { clearTimeout(t); this._kill(); resolve(); }
        });
        this.proc = null;
    }
}

/**
 * A fixed set of engines handed out one analysis at a time.
 */
export class EnginePool {
    /**
     * @param {{ size: number, factory: () => UciEngine }} o
     */
    constructor({ size, factory }) {
        this.engines = [];
        this.idle = [];
        this.waiters = [];
        for (let i = 0; i < Math.max(1, size); i++) {
            const e = factory(i);
            this.engines.push(e);
            this.idle.push(e);
        }
        this.closed = false;
    }

    get size() { return this.engines.length; }

    /** @returns {Promise<UciEngine>} */
    acquire() {
        if (this.closed) return Promise.reject(new EngineError('closed', 'pool closed'));
        const e = this.idle.pop();
        if (e) return Promise.resolve(e);
        return new Promise((resolve, reject) => this.waiters.push({ resolve, reject }));
    }

    release(engine) {
        const w = this.waiters.shift();
        if (w) w.resolve(engine); else this.idle.push(engine);
    }

    /** Runs fn(engine) with an engine of the pool. */
    async use(fn) {
        const e = await this.acquire();
        try { return await fn(e); } finally { this.release(e); }
    }

    async close() {
        this.closed = true;
        for (const w of this.waiters.splice(0)) w.reject(new EngineError('closed', 'pool closed'));
        await Promise.all(this.engines.map((e) => e.close()));
    }
}
