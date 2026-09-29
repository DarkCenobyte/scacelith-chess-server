// Reading the server's Prometheus endpoint (GET /metrics on METRICS_PORT): a small text parser and
// the few aggregations the benchmark needs (per-shard process gauges, counters, histogram deltas).

import http from 'node:http';
import https from 'node:https';

/**
 * GET a metrics URL; resolves with the body text.
 * @param {string} url
 * @param {{ token?: string, timeoutMs?: number }} [o]
 */
export function fetchText(url, { token = null, timeoutMs = 10000 } = {}) {
    return new Promise((resolve, reject) => {
        const mod = url.startsWith('https:') ? https : http;
        const headers = token ? { Authorization: `Bearer ${token}` } : {};
        const req = mod.get(url, { headers, timeout: timeoutMs }, (res) => {
            let body = '';
            res.setEncoding('utf8');
            res.on('data', (c) => { body += c; });
            res.on('end', () => (res.statusCode === 200 ? resolve(body) : reject(new Error(`metrics HTTP ${res.statusCode}`))));
        });
        req.on('error', reject);
        req.on('timeout', () => req.destroy(new Error('metrics timeout')));
    });
}

function parseLabels(s) {
    const out = {};
    const re = /([a-zA-Z_][a-zA-Z0-9_]*)="((?:[^"\\]|\\.)*)"/g;
    let m;
    while ((m = re.exec(s))) out[m[1]] = m[2].replace(/\\n/g, '\n').replace(/\\"/g, '"').replace(/\\\\/g, '\\');
    return out;
}

/**
 * Parses Prometheus text format 0.0.4.
 * @param {string} text
 * @returns {Map<string, Array<{labels: object, value: number}>>}
 */
export function parseProm(text) {
    const out = new Map();
    for (const line of text.split('\n')) {
        if (!line || line[0] === '#') continue;
        const brace = line.indexOf('{');
        const sp = line.lastIndexOf(' ');
        if (sp < 0) continue;
        let name, labels = {};
        if (brace >= 0 && brace < sp) {
            name = line.slice(0, brace);
            labels = parseLabels(line.slice(brace + 1, line.lastIndexOf('}')));
        } else {
            name = line.slice(0, sp).trim();
        }
        const value = Number(line.slice(sp + 1));
        let a = out.get(name);
        if (!a) out.set(name, (a = []));
        a.push({ labels, value });
    }
    return out;
}

/** Sum of every series of a metric (optionally filtered by a label predicate). */
export function sum(m, name, pred = null) {
    let s = 0;
    for (const x of m.get(name) || []) if (!pred || pred(x.labels)) s += x.value;
    return s;
}

/** { labelValue: value } of a metric, keyed by one label (default 'shard'). */
export function byLabel(m, name, label = 'shard') {
    const out = {};
    for (const x of m.get(name) || []) out[x.labels[label] ?? ''] = (out[x.labels[label] ?? ''] || 0) + x.value;
    return out;
}

/**
 * A histogram summed over its label sets: { le: [bounds...], cum: [cumulative counts...], sum, count }.
 * @param {Map} m
 * @param {string} name metric base name (without _bucket)
 */
export function histogram(m, name) {
    const byLe = new Map();
    for (const x of m.get(`${name}_bucket`) || []) {
        const le = x.labels.le === '+Inf' ? Infinity : Number(x.labels.le);
        byLe.set(le, (byLe.get(le) || 0) + x.value);
    }
    const le = [...byLe.keys()].sort((a, b) => a - b);
    return { le, cum: le.map((b) => byLe.get(b)), sum: sum(m, `${name}_sum`), count: sum(m, `${name}_count`) };
}

/** b - a for two histograms of the same metric (a may be null). */
export function histDelta(a, b) {
    if (!a || !a.le.length) return b;
    return { le: b.le, cum: b.cum.map((v, i) => v - (a.cum[i] || 0)), sum: b.sum - a.sum, count: b.count - a.count };
}

/** Quantile of a cumulative bucket histogram (linear interpolation inside the bucket, like Prometheus). */
export function histQuantile(h, q) {
    if (!h || !h.count) return 0;
    const total = h.cum[h.cum.length - 1];
    if (!total) return 0;
    const rank = q * total;
    for (let i = 0; i < h.le.length; i++) {
        if (h.cum[i] >= rank) {
            const lo = i === 0 ? 0 : h.le[i - 1];
            const hi = h.le[i];
            const prev = i === 0 ? 0 : h.cum[i - 1];
            if (hi === Infinity) return lo;
            const inBucket = h.cum[i] - prev;
            return lo + (hi - lo) * (inBucket ? (rank - prev) / inBucket : 0);
        }
    }
    return h.le[h.le.length - 2] ?? 0;
}

/** Summary of a histogram delta: { n, mean, p50, p90, p99 } (bucket interpolation: coarse). */
export function histSummary(h, digits = 2) {
    const r = (x) => Number(x.toFixed(digits));
    if (!h || !h.count) return { n: 0 };
    return { n: h.count, mean: r(h.sum / h.count), p50: r(histQuantile(h, 0.5)), p90: r(histQuantile(h, 0.9)), p99: r(histQuantile(h, 0.99)) };
}

/**
 * The benchmark's view of one /metrics scrape.
 * @param {string} text
 */
export function snapshot(text) {
    const m = parseProm(text);
    const shards = {};
    const per = (name, key) => {
        for (const [shard, v] of Object.entries(byLabel(m, name))) (shards[shard] ||= {})[key] = v;
    };
    per('scacelith_process_cpu_ratio', 'cpu');
    per('scacelith_process_rss_bytes', 'rss');
    per('scacelith_process_heap_used_bytes', 'heap');
    per('scacelith_process_external_bytes', 'external');
    per('scacelith_process_event_loop_delay_p99_ms', 'lagP99');
    per('scacelith_process_event_loop_delay_max_ms', 'lagMax');
    per('scacelith_ws_connections', 'conns');
    per('scacelith_ws_players', 'players');
    per('scacelith_games_active', 'games');
    return {
        at: Date.now(),
        shards,
        totals: {
            conns: sum(m, 'scacelith_ws_connections'),
            players: sum(m, 'scacelith_ws_players'),
            presence: sum(m, 'scacelith_presence_online'),
            gamesActive: sum(m, 'scacelith_games_active'),
            moves: sum(m, 'scacelith_game_moves_total'),
            gamesCreated: sum(m, 'scacelith_games_created_total'),
            gamesEnded: sum(m, 'scacelith_games_ended_total'),
            gamesCommitted: sum(m, 'scacelith_store_games_committed_total'),
            rejects: sum(m, 'scacelith_game_rejects_total'),
            msgsIn: sum(m, 'scacelith_ws_messages_in_total'),
            msgsOut: sum(m, 'scacelith_ws_messages_out_total'),
            bytesIn: sum(m, 'scacelith_ws_bytes_in_total'),
            bytesOut: sum(m, 'scacelith_ws_bytes_out_total'),
            relayed: sum(m, 'scacelith_ws_relayed_total'),
            busFramesOut: sum(m, 'scacelith_bus_frames_out_total'),
            dropped: byLabel(m, 'scacelith_ws_dropped_total', 'reason'),
            closes: byLabel(m, 'scacelith_ws_closes_total', 'code'),
            hello: byLabel(m, 'scacelith_ws_hello_total', 'result'),
            anomalies: byLabel(m, 'scacelith_ws_anomalies_total', 'kind'),
            slowConsumers: sum(m, 'scacelith_ws_slow_consumers_total'),
            handshakesRejected: sum(m, 'scacelith_ws_handshakes_rejected_total'),
            storeBusy: sum(m, 'scacelith_store_busy_total'),
            commitErrors: sum(m, 'scacelith_game_commit_errors_total'),
            journalErrors: sum(m, 'scacelith_journal_errors_total'),
        },
        hist: {
            moveUs: histogram(m, 'scacelith_game_move_processing_us'),
            rttMs: histogram(m, 'scacelith_ws_rtt_ms'),
            helloMs: histogram(m, 'scacelith_ws_hello_ms'),
            handshakeMs: histogram(m, 'scacelith_ws_handshake_ms'),
            journalFlushMs: histogram(m, 'scacelith_journal_flush_ms'),
            commitMs: histogram(m, 'scacelith_game_commit_latency_ms'),
            commitBatchMs: histogram(m, 'scacelith_store_commit_batch_ms'),
            commitBatchSize: histogram(m, 'scacelith_game_commit_batch_size'),
        },
    };
}
