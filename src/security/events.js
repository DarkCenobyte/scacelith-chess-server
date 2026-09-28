// Security events (failed logins, lockouts, proof of work, password resets, MFA changes, session
// revocations...): logged at once through log.security() and persisted in batches, every
// `flushMs`, with store.security.insertBatch (docs/DESIGN.md section 7: batched 1 s, retained
// RETENTION_SECURITY_DAYS).
//
// The queue is bounded: under a flood the oldest unsaved events are dropped (and counted), the
// log line of each event has already been written.

import { ipForLog } from '../log.js';
import { metrics } from '../metrics.js';

const eventsTotal = metrics.counter('scacelith_auth_security_events_total', 'Security events recorded', ['kind']);
const droppedTotal = metrics.counter('scacelith_auth_security_events_dropped_total', 'Security events dropped before being saved');

/**
 * @param {{ store: object, log: object, flushMs?: number, maxQueue?: number, now?: () => number }} opts
 * @returns {{ record(kind: string, fields?: { userId?: number|null, ip?: string|null, detail?: object }): void,
 *             flush(): void, close(): void, pending(): number }}
 */
export function createSecurityEvents({ store, log, flushMs = 1000, maxQueue = 10000, now = Date.now }) {
    let queue = [];
    let timer = null;
    const counters = new Map();

    function flush() {
        if (timer) { clearTimeout(timer); timer = null; }
        if (!queue.length) return;
        const batch = queue;
        queue = [];
        try {
            store.security.insertBatch(batch);
        } catch (err) {
            log.error('security events not saved', { count: batch.length, err });
        }
    }

    function schedule() {
        if (timer) return;
        timer = setTimeout(flush, flushMs);
        timer.unref?.();
    }

    /**
     * Records one event. `detail` must not hold credentials (it is persisted as JSON text).
     */
    function record(kind, { userId = null, ip = null, detail = null } = {}) {
        let c = counters.get(kind);
        if (!c) { c = eventsTotal.labels(kind); counters.set(kind, c); }
        c.inc();
        log.security(kind, { userId: userId ?? undefined, ip: ipForLog(ip), ...(detail || {}) });
        if (queue.length >= maxQueue) { queue.shift(); droppedTotal.inc(); }
        queue.push({ kind, userId: userId ?? null, ip: ip || null, detail: detail ? JSON.stringify(detail) : null, at: now() });
        schedule();
    }

    return {
        record,
        flush,
        close: flush,
        pending: () => queue.length,
    };
}
