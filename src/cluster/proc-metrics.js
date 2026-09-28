// Process health metrics (every process: primary and shards): event-loop delay, CPU and memory,
// declared {perShard: true} so the primary's merged /metrics shows one series per process.
// The values are refreshed when a snapshot is taken (gaugeFn), plus a 1 s sampler for the CPU
// ratio and the event-loop window, so reading them costs nothing on the hot path.

import { monitorEventLoopDelay } from 'node:perf_hooks';
import { metrics as defaultRegistry } from '../metrics.js';

/**
 * Starts the process metrics.
 * @param {{ registry?: object, windowMs?: number }} [o]
 * @returns {{ lagP99(): number, lagMax(): number, cpuRatio(): number, stop(): void }}
 */
export function startProcessMetrics({ registry = defaultRegistry, windowMs = 10000 } = {}) {
    const h = monitorEventLoopDelay({ resolution: 10 });
    h.enable();
    let p50 = 0, p99 = 0, max = 0, cpu = 0;
    let lastCpu = process.cpuUsage();
    let lastAt = performance.now();
    let windowStart = lastAt;

    const sample = () => {
        const now = performance.now();
        const c = process.cpuUsage(lastCpu);
        const dt = now - lastAt;
        if (dt > 0) cpu = (c.user + c.system) / 1000 / dt;
        lastCpu = process.cpuUsage();
        lastAt = now;
        if (h.count > 0) {
            p50 = h.percentile(50) / 1e6;
            p99 = h.percentile(99) / 1e6;
            max = h.max / 1e6;
        }
        if (now - windowStart >= windowMs) { h.reset(); windowStart = now; }
    };
    const timer = setInterval(sample, 1000);
    timer.unref();

    const o = { perShard: true, merge: 'last' };
    registry.gaugeFn('scacelith_process_event_loop_delay_p50_ms', 'Event-loop delay, median over the last window', () => p50, o);
    registry.gaugeFn('scacelith_process_event_loop_delay_p99_ms', 'Event-loop delay, 99th percentile over the last window', () => p99, o);
    registry.gaugeFn('scacelith_process_event_loop_delay_max_ms', 'Event-loop delay, maximum over the last window', () => max, o);
    registry.gaugeFn('scacelith_process_cpu_ratio', 'CPU time / wall time over the last second (1 = one core)', () => cpu, o);
    registry.gaugeFn('scacelith_process_rss_bytes', 'Resident set size', () => process.memoryUsage.rss(), o);
    registry.gaugeFn('scacelith_process_heap_used_bytes', 'V8 heap in use', () => process.memoryUsage().heapUsed, o);
    registry.gaugeFn('scacelith_process_external_bytes', 'Memory of Buffers and other external objects', () => process.memoryUsage().external, o);
    registry.gaugeFn('scacelith_process_uptime_seconds', 'Process uptime', () => process.uptime(), o);

    return {
        lagP99: () => p99,
        lagMax: () => max,
        cpuRatio: () => cpu,
        stop() { clearInterval(timer); h.disable(); },
    };
}
