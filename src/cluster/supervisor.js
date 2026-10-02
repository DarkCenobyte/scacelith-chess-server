// Shard supervisor (primary): forks one worker per shard, gives each an Ipc endpoint, restarts a
// crashed shard under the same shard number (its journal is replayed by the new process) with a
// backoff (250 ms, 1 s, 5 s, 15 s, then 30 s; reset after 60 s of uptime), and stops them all
// gracefully. It also is the "shards directory" the control plane talks to.

import { Ipc } from './ipc.js';

const BACKOFF_MS = [250, 1000, 5000, 15000, 30000];

export class ShardSupervisor {
    /**
     * @param {object} o
     * @param {number[]} o.shards shard numbers of this instance
     * @param {(shard: number) => object} o.fork starts a worker (cluster.fork / child_process.fork): send, on('message'|'exit'), kill
     * @param {(shard: number, ipc: Ipc) => void} [o.onUp] a worker started (bind handlers here)
     * @param {(shard: number, info: {code: number|null, signal: string|null}) => void} [o.onDown] a worker exited
     * @param {object} [o.log]
     * @param {object} [o.ipcOptions]
     */
    constructor({ shards, fork, onUp = null, onDown = null, log = null, ipcOptions = {} }) {
        this.shardNumbers = shards;
        this.fork = fork;
        this.onUp = onUp;
        this.onDown = onDown;
        this.log = log;
        this.ipcOptions = ipcOptions;
        /** @type {Map<number, {worker: object, ipc: Ipc, startedAt: number, attempts: number, timer: any, exited: boolean}>} */
        this.workers = new Map();
        this.stopping = false;
        this.restarts = 0;
    }

    start() {
        for (const s of this.shardNumbers) this._spawn(s, 0);
    }

    _spawn(shard, attempts) {
        if (this.stopping) return;
        let worker;
        try {
            worker = this.fork(shard);
        } catch (e) {
            this.log?.error?.('fork failed', { shard, err: e });
            this._scheduleRestart(shard, attempts);
            return;
        }
        const ipc = new Ipc(worker, { ...this.ipcOptions, name: `shard${shard}`, log: this.log });
        const entry = { worker, ipc, startedAt: Date.now(), attempts, timer: null, exited: false };
        this.workers.set(shard, entry);
        worker.on('exit', (code, signal) => {
            entry.exited = true;
            ipc.close('worker exited');
            if (this.workers.get(shard) === entry) this.workers.delete(shard);
            // exitCode, not `code`: the log hides a field of that name (log.js, credentials).
            if (!this.stopping) this.log?.error?.('shard exited', { shard, exitCode: code, signal });
            try { this.onDown?.(shard, { code, signal }); } catch (e) { this.log?.error?.('onDown failed', { err: e }); }
            if (!this.stopping) {
                const ranLong = Date.now() - entry.startedAt > 60000;
                this._scheduleRestart(shard, ranLong ? 0 : attempts + 1);
            }
        });
        worker.on('error', (e) => this.log?.warn?.('worker channel error', { shard, err: e }));
        try { this.onUp?.(shard, ipc); } catch (e) { this.log?.error?.('onUp failed', { err: e }); }
    }

    _scheduleRestart(shard, attempts) {
        if (this.stopping) return;
        const delay = BACKOFF_MS[Math.min(attempts, BACKOFF_MS.length - 1)];
        this.restarts++;
        this.log?.info?.('restarting shard', { shard, delayMs: delay });
        const t = setTimeout(() => this._spawn(shard, attempts), delay);
        this.workers.set(shard, { worker: null, ipc: null, startedAt: 0, attempts, timer: t, exited: true });
    }

    /** Shards with a running worker. */
    list() {
        const out = [];
        for (const [s, e] of this.workers) if (e.ipc && !e.exited && !e.ipc.closed) out.push(s);
        return out;
    }

    ipcOf(shard) {
        const e = this.workers.get(shard);
        return e && e.ipc && !e.exited ? e.ipc : null;
    }

    // ---- shards directory (control plane) -------------------------------------------------------

    notify(shard, type, payload) {
        const ipc = this.ipcOf(shard);
        return ipc ? ipc.notify(type, payload) : false;
    }

    request(shard, type, payload, opts) {
        const ipc = this.ipcOf(shard);
        if (!ipc) return Promise.reject(new Error(`shard ${shard} is not running`));
        return ipc.request(type, payload, opts);
    }

    broadcast(type, payload) {
        for (const s of this.list()) this.notify(s, type, payload);
    }

    /** Requests every running shard; failures become null entries. */
    async requestAll(type, payload, opts) {
        const shards = this.list();
        const res = await Promise.all(shards.map((s) => this.request(s, type, payload, opts).then((r) => r, () => null)));
        return shards.map((shard, i) => ({ shard, reply: res[i] }));
    }

    /**
     * Graceful stop: 'shutdown' to every worker, then waits for them to exit (killing the ones
     * still running after graceMs + timeoutMs).
     */
    async stop(graceMs, timeoutMs = 15000) {
        this.stopping = true;
        const waits = [];
        for (const [, e] of this.workers) {
            clearTimeout(e.timer);
            if (!e.worker || e.exited) continue;
            waits.push(new Promise((resolve) => {
                if (e.exited) return resolve();
                e.worker.once('exit', () => resolve());
                const t = setTimeout(() => { try { e.worker.kill('SIGKILL'); } catch { /* gone */ } }, graceMs + timeoutMs);
                t.unref();
                return undefined;
            }));
            try { e.ipc.notify('shutdown', { graceMs }); e.ipc.flush(); } catch { /* channel closed */ }
        }
        await Promise.all(waits);
    }
}
