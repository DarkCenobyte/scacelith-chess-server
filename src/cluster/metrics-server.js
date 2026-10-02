// Metrics and health endpoint of the primary (METRICS_PORT on METRICS_BIND, plain HTTP: keep it
// private). GET /metrics merges every shard's registry snapshot (IPC 'metrics.snapshot') with the
// primary's own (Prometheus text 0.0.4); /healthz answers while the process runs; /readyz when
// every shard is ready and the server is not shutting down. With METRICS_TOKEN, /metrics needs
// `Authorization: Bearer <token>` (the same text as in the configuration, compared as written).

import crypto from 'node:crypto';
import http from 'node:http';
import { mergeSnapshots, render } from '../metrics.js';

// The bearer and the token are compared as SHA-256 digests: equal lengths for timingSafeEqual,
// whatever the length of the bearer.
const sha256 = (s) => crypto.createHash('sha256').update(s, 'utf8').digest();

/**
 * @param {object} o
 * @param {object} o.config
 * @param {() => Promise<Array<{shard: number|string, snapshot: object[]}>>} o.collect snapshots of every process
 * @param {() => boolean} o.ready
 * @param {object} [o.log]
 * @returns {{ server: http.Server, listen(): Promise<number>, close(): Promise<void> }}
 */
export function createMetricsServer({ config, collect, ready, log = null }) {
    // Trimmed: a value from the environment keeps its spaces, and a bearer holds none.
    const token = config.metricsToken ? sha256(String(config.metricsToken).trim()) : null;
    const authorized = (req) => {
        if (!token) return true;
        const m = /^Bearer\s+(\S+)$/i.exec(req.headers.authorization || '');
        if (!m) return false;
        return crypto.timingSafeEqual(sha256(m[1]), token);
    };
    const text = (res, status, body, type = 'text/plain; charset=utf-8', extra = {}) => {
        res.writeHead(status, { 'Content-Type': type, 'Content-Length': Buffer.byteLength(body), 'Cache-Control': 'no-store', ...extra });
        res.end(body);
    };
    const server = http.createServer(async (req, res) => {
        const url = req.url.split('?')[0];
        if (req.method !== 'GET' && req.method !== 'HEAD') return text(res, 405, 'method not allowed\n', undefined, { Allow: 'GET, HEAD' });
        if (url === '/healthz') return text(res, 200, 'ok\n');
        if (url === '/readyz') { const ok = ready(); return text(res, ok ? 200 : 503, ok ? 'ready\n' : 'not ready\n'); }
        if (url !== '/metrics') return text(res, 404, 'not found\n');
        if (!authorized(req)) return text(res, 401, 'unauthorized\n', undefined, { 'WWW-Authenticate': 'Bearer' });
        try {
            const parts = await collect();
            return text(res, 200, render(mergeSnapshots(parts)), 'text/plain; version=0.0.4; charset=utf-8');
        } catch (e) {
            log?.error?.('metrics collection failed', { err: e });
            return text(res, 500, 'metrics unavailable\n');
        }
    });
    server.headersTimeout = 5000;
    server.requestTimeout = 10000;
    return {
        server,
        listen() {
            return new Promise((resolve, reject) => {
                server.once('error', reject);
                server.listen(config.metricsPort, config.metricsBind, () => { server.removeListener('error', reject); resolve(server.address().port); });
            });
        },
        close() { return new Promise((resolve) => { server.close(() => resolve()); server.closeAllConnections?.(); }); },
    };
}
