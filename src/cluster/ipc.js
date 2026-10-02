// Request/response and notifications over a process IPC channel (DESIGN 5.7).
//
// Both sides use the same class: the worker wraps `process`, the primary wraps each cluster
// Worker (or child process). Channels are expected to use `serialization: 'advanced'` so that
// Buffers inside payloads arrive as Buffers.
//
//   const ipc = new Ipc(process);
//   ipc.on('conn.send', ({ connId, frames }) => { ... });            // handler returns the reply (or a promise)
//   const r = await ipc.request('presence.claim', { userId, ... });  // rejects on timeout (5 s) or remote exception
//   ipc.notify('presence.release', { userId, connId });              // fire and forget
//
// Application-level refusals are ordinary replies ({ error: ErrorCode }); a rejected promise
// means the transport failed (timeout, closed channel) or the handler threw (IpcRemoteError).
//
// Messages posted during one event-loop turn are batched into a single process.send (one
// serialisation, one write): QueueStatus refreshes, conn.send bursts and presence traffic under
// load cost one IPC message per turn and peer instead of one per item.
//
// Wire format: { $ipc: 1, i, t, p } request, { $ipc: 2, i, p } / { $ipc: 2, i, e, c } reply,
// { $ipc: 3, t, p } notification, { $ipc: 4, b: [...] } batch. Other messages on the channel
// (cluster internals, foreign code) are ignored.

import v8 from 'node:v8';

const REQ = 1, REP = 2, NOTE = 3, BATCH = 4;

/** The peer did not answer in time. */
export class IpcTimeoutError extends Error {
    constructor(type, ms) { super(`ipc: no reply to ${type} within ${ms} ms`); this.code = 'IPC_TIMEOUT'; this.type = type; }
}
/** The peer's handler threw. */
export class IpcRemoteError extends Error {
    constructor(message, code) { super(message); this.code = code || 'IPC_REMOTE'; }
}
/** The channel is closed. */
export class IpcClosedError extends Error {
    constructor(reason) { super(`ipc: channel closed${reason ? ` (${reason})` : ''}`); this.code = 'IPC_CLOSED'; }
}

export class Ipc {
    /**
     * @param {{ send: Function, on: Function, removeListener?: Function }} channel process, cluster Worker or ChildProcess
     * @param {{ timeoutMs?: number, batch?: boolean, log?: object, name?: string }} [o]
     */
    constructor(channel, { timeoutMs = 5000, batch = true, log = null, name = '' } = {}) {
        this.channel = channel;
        this.timeoutMs = timeoutMs;
        this.batch = batch;
        this.log = log;
        this.name = name;
        this.handlers = new Map();
        this.pending = new Map();
        this.closed = false;
        this._nextId = 1;
        this._queue = [];
        this._scheduled = false;
        this._flush = () => this._flushQueue();
        this._onMessage = (m) => { if (m !== null && typeof m === 'object' && m.$ipc) this._dispatch(m); };
        channel.on('message', this._onMessage);
    }

    /**
     * Registers the handler of a request or notification type. The handler receives
     * (payload, type) and returns the reply or a promise of it.
     * @param {string} type
     * @param {(payload: any, type: string) => any} handler
     */
    on(type, handler) {
        this.handlers.set(type, handler);
        return this;
    }

    /**
     * Sends a request and resolves with the peer's reply.
     * @param {string} type
     * @param {any} [payload]
     * @param {{ timeoutMs?: number }} [o]
     * @returns {Promise<any>}
     */
    request(type, payload = null, { timeoutMs } = {}) {
        if (this.closed) return Promise.reject(new IpcClosedError());
        const id = this._nextId;
        this._nextId = id >= Number.MAX_SAFE_INTEGER ? 1 : id + 1;
        const ms = timeoutMs ?? this.timeoutMs;
        return new Promise((resolve, reject) => {
            const timer = setTimeout(() => {
                this.pending.delete(id);
                reject(new IpcTimeoutError(type, ms));
            }, ms);
            this.pending.set(id, { resolve, reject, timer });
            this._post({ $ipc: REQ, i: id, t: type, p: payload });
        });
    }

    /** Fire-and-forget message (the peer's handler reply, if any, is discarded). */
    notify(type, payload = null) {
        if (this.closed) return false;
        this._post({ $ipc: NOTE, t: type, p: payload });
        return true;
    }

    /** Sends everything queued now (normally done once per event-loop turn). */
    flush() { if (this._queue.length) this._flushQueue(); }

    /**
     * Closes this side: pending requests are rejected, later calls fail.
     * @param {string} [reason]
     */
    close(reason) {
        if (this.closed) return;
        this.flush();
        this.closed = true;
        this.channel.removeListener?.('message', this._onMessage);
        const err = new IpcClosedError(reason);
        for (const [, p] of this.pending) { clearTimeout(p.timer); p.reject(err); }
        this.pending.clear();
    }

    _post(msg) {
        if (!this.batch) { this._write(msg); return; }
        this._queue.push(msg);
        if (!this._scheduled) { this._scheduled = true; setImmediate(this._flush); }
    }

    _flushQueue() {
        this._scheduled = false;
        const q = this._queue;
        if (!q.length) return;
        this._queue = [];
        this._write(q.length === 1 ? q[0] : { $ipc: BATCH, b: q });
    }

    _write(msg) {
        try {
            this.channel.send(msg, undefined, undefined, (err) => { if (err) this._sendFailed(msg, err); });
        } catch (err) {
            this._sendFailed(msg, err);
        }
    }

    _sendFailed(msg, err) {
        // Requests in a lost message fail now rather than at their timeout.
        const list = msg.$ipc === BATCH ? msg.b : [msg];
        for (const m of list) {
            if (m.$ipc !== REQ) continue;
            const p = this.pending.get(m.i);
            if (p) { this.pending.delete(m.i); clearTimeout(p.timer); p.reject(new IpcClosedError(err.code || err.message)); }
        }
        this.log?.debug?.('ipc send failed', { name: this.name, err });
    }

    _reply(id, value) {
        if (this.closed) return;
        this._post({ $ipc: REP, i: id, p: value === undefined ? null : value });
    }

    _replyError(id, err) {
        if (this.closed) return;
        this._post({ $ipc: REP, i: id, e: String(err?.message || err), c: err?.code });
    }

    _dispatch(m) {
        switch (m.$ipc) {
            case REQ: {
                const h = this.handlers.get(m.t);
                if (!h) { this._replyError(m.i, new IpcRemoteError(`no handler for ${m.t}`, 'IPC_NO_HANDLER')); return; }
                let r;
                try { r = h(m.p, m.t); } catch (e) {
                    this.log?.error?.('ipc handler failed', { type: m.t, err: e });
                    this._replyError(m.i, e);
                    return;
                }
                if (r !== null && typeof r === 'object' && typeof r.then === 'function') {
                    r.then((v) => this._reply(m.i, v), (e) => {
                        this.log?.error?.('ipc handler failed', { type: m.t, err: e });
                        this._replyError(m.i, e);
                    });
                } else this._reply(m.i, r);
                return;
            }
            case REP: {
                const p = this.pending.get(m.i);
                if (!p) return;                                 // late reply after a timeout
                this.pending.delete(m.i);
                clearTimeout(p.timer);
                if (m.e !== undefined) p.reject(new IpcRemoteError(m.e, m.c));
                else p.resolve(m.p);
                return;
            }
            case NOTE: {
                const h = this.handlers.get(m.t);
                if (!h) return;
                try {
                    const r = h(m.p, m.t);
                    if (r !== null && typeof r === 'object' && typeof r.then === 'function') {
                        r.then(undefined, (e) => this.log?.error?.('ipc handler failed', { type: m.t, err: e }));
                    }
                } catch (e) {
                    this.log?.error?.('ipc handler failed', { type: m.t, err: e });
                }
                return;
            }
            case BATCH:
                for (const x of m.b) this._dispatch(x);
                return;
            default:
        }
    }
}

/**
 * Two connected in-memory channels (tests, single-process setups). Messages are delivered
 * asynchronously and structured-cloned, like a real IPC channel.
 * @returns {[object, object]}
 */
export function channelPair() {
    const mk = () => {
        const listeners = new Set();
        return {
            peer: null,
            connected: true,
            send(msg, _h, _o, cb) {
                if (!this.connected) { const e = new Error('channel closed'); e.code = 'ERR_IPC_CHANNEL_CLOSED'; if (cb) { cb(e); return false; } throw e; }
                const copy = v8.deserialize(v8.serialize(msg));     // what 'advanced' serialization does (Buffers stay Buffers)
                const peer = this.peer;
                setImmediate(() => { for (const l of peer._listeners) l(copy); cb?.(null); });
                return true;
            },
            on(ev, fn) { if (ev === 'message') listeners.add(fn); return this; },
            removeListener(ev, fn) { if (ev === 'message') listeners.delete(fn); return this; },
            disconnect() { this.connected = false; this.peer.connected = false; },
            _listeners: listeners,
        };
    };
    const a = mk(), b = mk();
    a.peer = b; b.peer = a;
    return [a, b];
}
