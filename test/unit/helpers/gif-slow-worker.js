// A stand-in for src/gif/worker.js in the pool tests: answers a job { sleepMs, fail, crash, bytes }
// after blocking its thread for sleepMs (like a long render), with a small fake GIF, an error, or
// by exiting the thread.

import { parentPort } from 'node:worker_threads';

parentPort.on('message', ({ id, job }) => {
    const until = Date.now() + (job.sleepMs ?? 0);
    while (Date.now() < until) { /* busy, like a render */ }
    if (job.crash) process.exit(3);
    if (job.fail) {
        parentPort.postMessage({ id, ok: false, name: 'RangeError', message: job.fail });
        return;
    }
    const gif = new Uint8Array(job.bytes ?? 8);
    gif.set([0x47, 0x49, 0x46, 0x38, 0x39, 0x61].slice(0, gif.length));
    parentPort.postMessage({ id, ok: true, gif: gif.buffer, ms: job.sleepMs ?? 0 }, [gif.buffer]);
});
