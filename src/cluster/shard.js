// Assembly of one shard from its dependencies (worker-main.js imports the real modules and calls
// this; tests call it with fakes): bus, router, WebSocket server, listeners, IPC handlers,
// process metrics, graceful stop.
//
// Protection per address (net/ipguard.js): one IpGuard per shard, shared by the TLS gate, the API
// wrapper, the WebSocket upgrade and the API handler (worker-main.js creates it before the API
// handler; createShardGuard). Its refusal reports go to the primary ('abuse.report', at most one
// notification per second); the blocks the primary decides come back as 'abuse.block', at once
// for a new block and with every running block after this shard's 'shard.ready'.

import { metrics as defaultRegistry } from '../metrics.js';
import { WS_SUBPROTOCOL } from '../protocol/index.js';
import { ipMatcher } from '../net/ip.js';
import { IpGuard } from '../net/ipguard.js';
import { Listeners, makeClientIp } from '../net/listeners.js';
import { WsServer } from '../net/ws.js';
import { Bus, busToken, unixTransport } from './bus.js';
import { startProcessMetrics } from './proc-metrics.js';
import { Router, TYPE_NAMES } from './router.js';

/**
 * The shard bus, created before the GameHost (which receives it) and before the router (which
 * becomes its message handler in startShard).
 * @param {{ config: object, shard: number, serverId: string, transport?: object, log?: object, registry?: object }} o
 */
export function createBus({ config, shard, serverId, busDir = '', transport = null, log = null, registry = defaultRegistry }) {
    return new Bus({
        shard,
        transport: transport || unixTransport({ runDir: config.runDir, serverId, fallbackDir: busDir }),
        token: busToken(config.serverSecret, serverId),
        onMessage: () => {},
        log,
        registry,
    });
}

/**
 * The protection per address of a shard: its refusals are reported to the primary, which decides
 * the blocks for every shard.
 * @param {{ config: object, primary: { notify(type: string, payload: object): any }, log?: object, registry?: object }} o
 */
export function createShardGuard({ config, primary, log = null, registry = defaultRegistry }) {
    return new IpGuard({ config, registry, log, report: (entries) => primary.notify('abuse.report', { entries }) });
}

/**
 * Starts a shard whose GameHost is already recovered.
 * @param {object} o
 * @param {object} o.config
 * @param {number} o.shard
 * @param {string} o.serverId
 * @param {import('./ipc.js').Ipc} o.primary
 * @param {object} o.host GameHost
 * @param {object} o.auth
 * @param {object} [o.anticheat]
 * @param {object} [o.store]
 * @param {Function|null} [o.apiHandler]
 * @param {Bus} [o.bus] from createBus (created here when absent)
 * @param {object} o.log
 * @param {object} [o.registry]
 * @param {boolean} [o.reusePort]
 * @param {() => (void|Promise<void>)} [o.onStopped] called after a graceful stop (close the store, exit)
 * @param {IpGuard} [o.guard] protection per address (the one given to the API handler); created here when absent
 */
export async function startShard({ config, shard, serverId, primary, host, auth, anticheat = null, store = null, apiHandler = null,
    bus = null, log, registry = defaultRegistry, reusePort, onStopped = null, guard = null }) {
    const proc = startProcessMetrics({ registry });
    const theGuard = guard || createShardGuard({ config, primary, log: log.child('guard'), registry });
    const theBus = bus || createBus({ config, shard, serverId, log: log.child('bus'), registry });
    const router = new Router({ config, shard, host, auth, primary, bus: theBus, anticheat, store, log: log.child('router'), registry, lagP99: proc.lagP99 });
    theBus.onMessage = router.onBus;
    const clientIp = makeClientIp(config);
    const wss = new WsServer({
        maxMessageBytes: config.wsMaxMessageBytes, subprotocol: WS_SUBPROTOCOL, allowOrigins: config.wsAllowedOrigins || [],
        sendBufferLimit: config.wsSendBufferLimit, onConnection: router.onConnection, admission: router.admission,
        clientIp, messageLabel: (t) => TYPE_NAMES[t] || `0x${t.toString(16)}`, log: log.child('ws'), registry,
        handshakeTimeoutMs: Math.min(10000, config.wsHelloTimeoutMs),
        // The /info serverId again, so that a client that reuses an /info answer checks it before Hello.
        upgradeHeaders: serverId ? { 'Scacelith-Server-Id': serverId } : null,
        guard: theGuard,
        // The heads a dedicated WS_PORT cannot read count toward a block unless a trusted proxy
        // sent them (the predicate hardenHttp uses on the API port).
        isTrusted: config.tlsMode === 'proxy' ? ipMatcher(config.trustedProxies || []) : null,
    });
    let ready = false, draining = false, stopping = null;
    const listeners = new Listeners({
        config, apiHandler, wsServer: wss, log: log.child('listen'), ready: () => ready && !draining, reusePort,
        full: () => router.isFull(), registry, guard: theGuard,
    });

    router.bindPrimary(primary);
    primary.on('metrics.snapshot', () => registry.snapshot());
    primary.on('tls.reload', () => ({ ok: listeners.reloadCertificates() }));
    primary.on('abuse.block', (p) => { theGuard.applyBlocks(p?.blocks); return null; });
    primary.on('shutdown', ({ graceMs } = {}) => { stop(graceMs ?? config.shutdownGraceMs); return { ok: true }; });

    async function stop(graceMs = config.shutdownGraceMs) {
        if (stopping) return stopping;
        stopping = (async () => {
            draining = true;
            wss.accepting = false;
            listeners.close();
            log.info('draining', { connections: router.conns.size, graceMs });
            await router.drain(graceMs);
            router.stop();
            try { await host.shutdown?.(); } catch (e) { log.error('host shutdown failed', { err: e }); }
            primary.flush();
            await theBus.close();
            proc.stop();
            theGuard.close();
            // Give the close frames a moment to leave before the process goes.
            await new Promise((r) => setTimeout(r, Math.min(500, wss.closeTimeoutMs)));
            for (const c of wss.connections.values()) c.terminate();
            if (onStopped) await onStopped();
        })();
        return stopping;
    }

    await theBus.start();
    router.start();
    await listeners.listen();
    ready = true;
    primary.notify('shard.ready', { shard });
    log.info('shard ready', { shard, listeners: listeners.addresses(), bus: theBus.transport.describe?.(shard) });
    return { router, bus: theBus, wss, listeners, guard: theGuard, stop, isReady: () => ready && !draining };
}
