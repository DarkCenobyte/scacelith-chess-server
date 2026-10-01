// Entry module of the GIF rendering threads that routes/gif.js starts (src/gif/pool.js workerUrl):
// src/gif/worker.js, with the thread's own scheduling priority lowered to the lowest (nice 19) on
// Linux, so that a render only takes the CPU time the games leave: a worker process's event loop
// (its games, at nice 0) and its rendering thread share the machine's cores, and with WORKERS=auto
// (one worker per core) a render at the same priority would take up to half of a core from a
// worker's games for as long as it lasts. On Linux the nice value belongs to each thread
// (setpriority on the calling thread, `os.setPriority(0, ...)`), so the rest of the process keeps
// its priority. Elsewhere the call would lower the whole process: it is not made there.
//
// The priority is lowered once the modules are loaded (they are read before this module runs), so
// that no other thread of the process is started from this one at the low priority (threads
// inherit the nice value of the thread that creates them; a render only computes).
//
// Under a full CPU a render then waits for the games, and the requests behind it may run out of
// GIF_QUEUE_TIMEOUT_MS: 503 server_busy, with the render quotas given back. Imported by the main
// thread (a test), the module changes nothing.

import os from 'node:os';
import { isMainThread } from 'node:worker_threads';
import '../../gif/worker.js';

/** The nice value of the rendering threads on Linux (os.constants.priority.PRIORITY_LOW). */
export const GIF_THREAD_NICE = os.constants.priority.PRIORITY_LOW;

if (!isMainThread && process.platform === 'linux') {
    try { os.setPriority(0, GIF_THREAD_NICE); } catch { /* not permitted: render at the normal priority */ }
}
