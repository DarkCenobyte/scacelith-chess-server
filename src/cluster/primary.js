// Assembly of the primary from its dependencies (primary-main.js imports the real modules and
// calls this; tests call it with fakes): shard supervisor, control plane, metrics endpoint.

import { metrics as defaultRegistry } from '../metrics.js';
import { ControlPlane } from './control-plane.js';
import { OnceStore, SlidingWindowLimiter } from './limits.js';
import { createMetricsServer } from './metrics-server.js';
import { Presence } from './presence.js';
import { startProcessMetrics } from './proc-metrics.js';
import { ShardSupervisor } from './supervisor.js';

/**
 * @param {object} o
 * @param {object} o.config
 * @param {object} o.log
 * @param {(shard: number) => object} o.fork
 * @param {object} o.matchmaker
 * @param {object} o.challenges
 * @param {object} [o.conduct]
 * @param {Function} [o.activeBan]
 * @param {Function} [o.ratingOf]
 * @param {number[]} [o.shards] default: SHARD_BASE .. SHARD_BASE + WORKERS - 1
 * @param {object} [o.registry]
 */
export async function startPrimary({ config, log, fork, matchmaker, challenges, conduct = null, activeBan = null, ratingOf = null,
    shards, registry = defaultRegistry }) {
    const shardNumbers = shards || Array.from({ length: config.workers }, (_, i) => config.shardBase + i);
    const proc = startProcessMetrics({ registry });
    const presence = new Presence({ maxConnections: config.maxConnections, maxPerIp: config.maxConnectionsPerIp });
    let cp = null;
    let stopping = false;
    const supervisor = new ShardSupervisor({
        shards: shardNumbers, fork, log: log.child('supervisor'),
        onUp: (s, ipc) => cp.bind(s, ipc),
        onDown: (s) => cp.shardDown(s),
    });
    const directory = {
        notify: (s, t, p) => supervisor.notify(s, t, p),
        request: (s, t, p, o) => supervisor.request(s, t, p, o),
        broadcast: (t, p) => supervisor.broadcast(t, p),
        list: () => supervisor.list().filter((s) => cp.readyShards.has(s)),
    };
    cp = new ControlPlane({
        config, presence, matchmaker, challenges, conduct, limiter: new SlidingWindowLimiter(), once: new OnceStore(),
        shards: directory, activeBan, ratingOf, log: log.child('control'), registry,
    });
    registry.gaugeFn('scacelith_shards_ready', 'Shards ready', () => cp.readyShards.size);
    registry.gaugeFn('scacelith_shard_restarts', 'Shard restarts since the start', () => supervisor.restarts);

    const ready = () => !stopping && shardNumbers.every((s) => cp.readyShards.has(s));
    let metricsServer = null, metricsPort = 0;
    if (config.metricsPort) {
        metricsServer = createMetricsServer({
            config, ready, log: log.child('metrics'),
            collect: async () => {
                const parts = [{ shard: 'primary', snapshot: registry.snapshot() }];
                for (const { shard, reply } of await supervisor.requestAll('metrics.snapshot', null, { timeoutMs: 2000 })) {
                    if (reply) parts.push({ shard, snapshot: reply });
                }
                return parts;
            },
        });
    }

    supervisor.start();
    cp.start();
    if (metricsServer) metricsPort = await metricsServer.listen();

    return {
        controlPlane: cp,
        supervisor,
        presence,
        metricsPort,
        ready,
        /** Forwards a certificate reload to every shard. */
        reloadTls() { supervisor.broadcast('tls.reload', null); },
        /** Graceful stop: every shard drains (ServerShutdown notice, graceMs), then exits. */
        async stop(graceMs = config.shutdownGraceMs) {
            if (stopping) return;
            stopping = true;
            cp.stop();
            await supervisor.stop(graceMs);
            if (metricsServer) await metricsServer.close();
            proc.stop();
        },
    };
}
