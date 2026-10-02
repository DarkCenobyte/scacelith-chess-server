// Assembly of the primary from its dependencies (primary-main.js imports the real modules and
// calls this; tests call it with fakes): shard supervisor, control plane, metrics endpoint.
//
// The global rate limiter runs on the monotonic clock of the game hosts (game/clock.js): a step
// back of the wall clock would otherwise hold every key's window open and refuse its users until
// the clock caught up (the values exchanged with the shards are durations). The single-use keys
// stay on the wall clock, which their users' expiries (proof-of-work challenges) are written in.

import { now as clockNow } from '../game/clock.js';
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
 * @param {Function} [o.acceptsChallenges]
 * @param {object} [o.refunds] store.refunds (the rating refund notices)
 * @param {number[]} [o.shards] default: SHARD_BASE .. SHARD_BASE + WORKERS - 1
 * @param {object} [o.registry]
 * @param {() => Promise<Array<{shard: string, snapshot: object[]}>>} [o.extraMetrics] snapshots of other
 *        processes to serve with the shards' (the analysis process)
 */
export async function startPrimary({ config, log, fork, matchmaker, challenges, conduct = null, activeBan = null, ratingOf = null,
    acceptsChallenges = null, refunds = null, shards, registry = defaultRegistry, extraMetrics = null }) {
    const shardNumbers = shards || Array.from({ length: config.workers }, (_, i) => config.shardBase + i);
    const proc = startProcessMetrics({ registry });
    const presence = new Presence({ maxConnections: config.maxConnections, maxPerIp: config.maxConnectionsPerIp });
    let cp = null;
    let stopping = false;
    const supervisor = new ShardSupervisor({
        shards: shardNumbers, fork, log: log.child('supervisor'),
        onUp: (s, ipc) => {
            ipc.on('config.snapshot', () => config.rawValues);     // config.js primaryConfig
            cp.bind(s, ipc);
        },
        onDown: (s) => cp.shardDown(s),
    });
    const directory = {
        notify: (s, t, p) => supervisor.notify(s, t, p),
        request: (s, t, p, o) => supervisor.request(s, t, p, o),
        broadcast: (t, p) => supervisor.broadcast(t, p),
        list: () => supervisor.list().filter((s) => cp.readyShards.has(s)),
    };
    cp = new ControlPlane({
        config, presence, matchmaker, challenges, conduct, limiter: new SlidingWindowLimiter({ now: clockNow }), once: new OnceStore(),
        shards: directory, activeBan, ratingOf, acceptsChallenges, refunds, log: log.child('control'), registry,
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
                const [replies, extra] = await Promise.all([
                    supervisor.requestAll('metrics.snapshot', null, { timeoutMs: 2000 }),
                    extraMetrics ? extraMetrics().catch(() => []) : [],
                ]);
                for (const { shard, reply } of replies) if (reply) parts.push({ shard, snapshot: reply });
                return parts.concat(extra);
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
