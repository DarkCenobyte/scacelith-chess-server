// GIF rendering off the event loop: renderGame (src/gif/render.js) runs on worker threads, one
// job per thread at a time, with a short FIFO queue in front. The games of the process never wait
// for a render: the event loop only posts the job and receives the finished GIF (a transferred
// ArrayBuffer, no copy).
//
//   const pool = createGifPool({ threads: 1, queueMax: 4, timeoutMs: 15000 });
//   try {
//       const gif = await pool.render(job);   // Buffer (image/gif)
//   } catch (e) {
//       if (e.code === 'busy') ...            // 503 server_busy with Retry-After
//       if (e.code === 'render_failed') ...   // bad job (illegal move...) or a failed thread
//   }
//   pool.stats();                             // counters for the metrics
//   await pool.close();
//
// Rejections (Error with a `code`):
//  * 'busy': the queue already holds queueMax jobs; or a job waited timeoutMs in the queue
//    without starting; or the pool is closed (close() rejects the jobs still waiting or running).
//  * 'render_failed': renderGame threw (err.message says why: an invalid FEN, an illegal move, too
//    many moves...), the render ran longer than renderTimeoutMs (the thread is terminated and a
//    new one starts with the next job), or the thread died (out of memory: resourceLimits).
//
// Threads start on first use and stop after idleMs without work, and their memory returns to the
// system: measured on Node 22, a thread adds about 27 MiB to the process RSS after one render
// (its V8 isolate, the module, the piece sprites and square pictures of one size) and about
// 41 MiB once it has rendered all three sizes; the first render of a size in a new thread costs
// about 150-200 ms more (rasterizing the pieces and squares). An idle thread does not keep the
// process alive (unref).
//
// stats(): threads (the limit), live (threads started), running, queued, and the counters
// completed, failed, rejectedFull, rejectedWait, renderTimeouts, threadsStarted, renderMsTotal,
// renderMsMax, lastRenderMs, bytesTotal.

import { Worker } from 'node:worker_threads';

const DEFAULT_WORKER = new URL('./worker.js', import.meta.url);

function failure(code, message) {
    return Object.assign(new Error(message), { code });
}

/**
 * Creates a pool of GIF rendering threads.
 * @param {{ threads?: number, queueMax?: number, timeoutMs?: number, renderTimeoutMs?: number,
 *   idleMs?: number, maxHeapMb?: number, workerUrl?: URL|string }} [o]
 *   threads: renders at the same time (default 1); queueMax: jobs waiting at most (default 4);
 *   timeoutMs: longest wait in the queue (default 15000); renderTimeoutMs: longest render
 *   (default 30000); idleMs: idle time before a thread stops (default 60000, 0 = never);
 *   maxHeapMb: V8 old-generation limit of a thread (default 128); workerUrl: the thread's
 *   module (tests).
 */
export function createGifPool({
    threads = 1, queueMax = 4, timeoutMs = 15000, renderTimeoutMs = 30000, idleMs = 60000, maxHeapMb = 128,
    workerUrl = DEFAULT_WORKER,
} = {}) {
    threads = Math.max(1, Math.floor(threads));
    queueMax = Math.max(0, Math.floor(queueMax));
    const queue = [];             // { job, resolve, reject, timer, queuedAt }
    const slots = [];             // { worker, task, idleTimer }
    let nextId = 1;
    let closed = false;
    const counters = {
        completed: 0, failed: 0, rejectedFull: 0, rejectedWait: 0, renderTimeouts: 0, threadsStarted: 0,
        renderMsTotal: 0, renderMsMax: 0, lastRenderMs: 0, bytesTotal: 0,
    };

    const spawn = () => {
        // execArgv []: the thread needs none of the process's flags (and some, like --input-type,
        // would stop it).
        const worker = new Worker(workerUrl, { execArgv: [], resourceLimits: { maxOldGenerationSizeMb: maxHeapMb } });
        counters.threadsStarted++;
        const slot = { worker, task: null, idleTimer: null };
        worker.unref();
        worker.on('message', (msg) => {
            const task = slot.task;
            if (!task || !msg || msg.id !== task.id) return;
            finishTask(slot);
            if (msg.ok) {
                const gif = Buffer.from(msg.gif);
                counters.completed++;
                counters.lastRenderMs = msg.ms;
                counters.renderMsTotal += msg.ms;
                if (msg.ms > counters.renderMsMax) counters.renderMsMax = msg.ms;
                counters.bytesTotal += gif.length;
                task.resolve(gif);
            } else {
                counters.failed++;
                task.reject(failure('render_failed', `GIF render failed: ${msg.message}`));
            }
            pump();
        });
        const died = (err) => {
            const i = slots.indexOf(slot);
            if (i < 0) return;
            slots.splice(i, 1);
            clearTimeout(slot.idleTimer);
            const task = slot.task;
            if (task) {
                clearTimeout(task.timer);
                slot.task = null;
                counters.failed++;
                task.reject(closed ? failure('busy', 'GIF renderer closed')
                    : failure('render_failed', `GIF render failed: the rendering thread stopped (${err && err.message ? err.message : err})`));
            }
            pump();
        };
        worker.on('error', died);
        worker.on('exit', (code) => died(new Error(`exit code ${code}`)));
        slots.push(slot);
        return slot;
    };

    const finishTask = (slot) => {
        clearTimeout(slot.task.timer);
        slot.task = null;
        slot.worker.unref();
        if (idleMs > 0) {
            clearTimeout(slot.idleTimer);
            slot.idleTimer = setTimeout(() => {
                if (!slot.task && slots.includes(slot)) {
                    slots.splice(slots.indexOf(slot), 1);
                    slot.worker.terminate().catch(() => {});
                }
            }, idleMs);
            slot.idleTimer.unref();
        }
    };

    const start = (slot, entry) => {
        clearTimeout(entry.timer);
        clearTimeout(slot.idleTimer);
        const id = nextId++;
        const task = { id, resolve: entry.resolve, reject: entry.reject, timer: null };
        slot.task = task;
        slot.worker.ref();
        task.timer = setTimeout(() => {
            if (slot.task !== task) return;
            counters.renderTimeouts++;
            const i = slots.indexOf(slot);
            if (i >= 0) slots.splice(i, 1);
            slot.task = null;
            counters.failed++;
            task.reject(failure('render_failed', `GIF render failed: longer than ${renderTimeoutMs} ms`));
            slot.worker.terminate().catch(() => {});
            pump();
        }, renderTimeoutMs);
        task.timer.unref();
        slot.worker.postMessage({ id, job: entry.job });
    };

    const pump = () => {
        while (queue.length > 0 && !closed) {
            let slot = slots.find((s) => s.task === null);
            if (!slot) {
                if (slots.length >= threads) return;
                slot = spawn();
            }
            start(slot, queue.shift());
        }
    };

    return {
        /**
         * Renders a job (renderGame's argument) on a pool thread.
         * @param {object} job
         * @returns {Promise<Buffer>}
         */
        render(job) {
            if (closed) return Promise.reject(failure('busy', 'GIF renderer closed'));
            const running = slots.filter((s) => s.task !== null).length;
            const canStart = queue.length === 0 && running < threads;
            if (!canStart && queue.length >= queueMax) {
                counters.rejectedFull++;
                return Promise.reject(failure('busy', 'GIF renderer busy (queue full)'));
            }
            return new Promise((resolve, reject) => {
                const entry = { job, resolve, reject, timer: null };
                entry.timer = setTimeout(() => {
                    const i = queue.indexOf(entry);
                    if (i < 0) return;
                    queue.splice(i, 1);
                    counters.rejectedWait++;
                    reject(failure('busy', `GIF renderer busy (no thread free within ${timeoutMs} ms)`));
                }, timeoutMs);
                entry.timer.unref();
                queue.push(entry);
                pump();
            });
        },

        /** @returns {object} counters and gauges for the metrics */
        stats() {
            return {
                threads,
                live: slots.length,
                running: slots.filter((s) => s.task !== null).length,
                queued: queue.length,
                ...counters,
            };
        },

        /**
         * Stops the threads; jobs still waiting or running are rejected ('busy').
         * @returns {Promise<void>}
         */
        async close() {
            if (closed) return;
            closed = true;
            for (const e of queue.splice(0)) {
                clearTimeout(e.timer);
                e.reject(failure('busy', 'GIF renderer closed'));
            }
            const all = slots.splice(0);
            for (const s of all) {
                clearTimeout(s.idleTimer);
                if (s.task) {
                    clearTimeout(s.task.timer);
                    s.task.reject(failure('busy', 'GIF renderer closed'));
                    s.task = null;
                }
            }
            await Promise.all(all.map((s) => s.worker.terminate().catch(() => {})));
        },
    };
}
