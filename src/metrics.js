// Metrics registry (Prometheus text exposition, no dependency).
//
// Every process (primary and each worker) has its own registry. Hot paths call pre-bound
// instruments, so recording a value allocates nothing:
//
//   const moves = metrics.counter('scacelith_moves_total', 'Moves accepted');
//   moves.inc();
//   const rejected = metrics.counter('scacelith_moves_rejected_total', 'Moves refused', ['code']);
//   const illegal = rejected.labels('illegal_move');   // bind once, at module load
//   illegal.inc();
//   const rtt = metrics.histogram('scacelith_rtt_ms', 'Client round trip', [5, 10, 25, 50, 100, 250, 500, 1000]);
//   rtt.observe(42);
//   metrics.gauge('scacelith_connections', 'Open WebSocket connections').set(n);
//   metrics.gaugeFn('scacelith_games_active', 'Games in progress', () => host.size);
//
// The primary merges the workers' snapshot() (sums counters and histograms, sums gauges unless
// declared with {merge: 'max'} or 'last', and adds a {shard} label for gauges declared with
// {perShard: true}) and serves render() on METRICS_PORT.

class Child {
    constructor(kind, buckets) {
        this.kind = kind;
        this.value = 0;
        if (kind === 'histogram') { this.buckets = buckets; this.counts = new Float64Array(buckets.length + 1); this.sum = 0; this.count = 0; }
    }
    inc(v = 1) { this.value += v; }
    dec(v = 1) { this.value -= v; }
    set(v) { this.value = v; }
    observe(v) {
        const b = this.buckets;
        let i = 0;
        while (i < b.length && v > b[i]) i++;
        this.counts[i]++;
        this.sum += v;
        this.count++;
    }
}

class Metric {
    constructor(kind, name, help, labelNames = [], opts = {}) {
        this.kind = kind; this.name = name; this.help = help; this.labelNames = labelNames; this.opts = opts;
        this.buckets = opts.buckets || null;
        this.children = new Map();
        this.fn = opts.fn || null;
        if (!labelNames.length) this.root = this.labels();
    }
    // Binds label values (in labelNames order). Cache the returned child.
    labels(...values) {
        const key = values.join('\u0001');
        let c = this.children.get(key);
        if (!c) { c = new Child(this.kind, this.buckets); c.labelValues = values; this.children.set(key, c); }
        return c;
    }
    inc(v) { this.root.inc(v); }
    dec(v) { this.root.dec(v); }
    set(v) { this.root.set(v); }
    observe(v) { this.root.observe(v); }
}

export class Registry {
    constructor() { this.metrics = new Map(); }
    _get(kind, name, help, labelNames, opts) {
        let m = this.metrics.get(name);
        if (m) { if (m.kind !== kind) throw new Error(`metric ${name} redefined as ${kind}`); return m; }
        m = new Metric(kind, name, help, labelNames, opts);
        this.metrics.set(name, m);
        return m;
    }
    counter(name, help, labelNames = [], opts = {}) { return this._get('counter', name, help, labelNames, opts); }
    gauge(name, help, labelNames = [], opts = {}) { return this._get('gauge', name, help, labelNames, opts); }
    gaugeFn(name, help, fn, opts = {}) { return this._get('gauge', name, help, [], { ...opts, fn }); }
    histogram(name, help, buckets, labelNames = [], opts = {}) { return this._get('histogram', name, help, labelNames, { ...opts, buckets }); }

    // Plain, structured-clone friendly object (sent from the workers to the primary).
    snapshot() {
        const out = [];
        for (const m of this.metrics.values()) {
            if (m.fn) { try { m.root.value = +m.fn() || 0; } catch { /* keep last */ } }
            const children = [];
            for (const c of m.children.values()) {
                children.push(m.kind === 'histogram'
                    ? { l: c.labelValues, counts: Array.from(c.counts), sum: c.sum, count: c.count }
                    : { l: c.labelValues, v: c.value });
            }
            out.push({ kind: m.kind, name: m.name, help: m.help, labelNames: m.labelNames, buckets: m.buckets, merge: m.opts.merge || 'sum', perShard: !!m.opts.perShard, children });
        }
        return out;
    }
}

// Merges snapshots: [{shard, snapshot}] -> snapshot.
export function mergeSnapshots(parts) {
    const byName = new Map();
    for (const { shard, snapshot } of parts) {
        for (const m of snapshot) {
            let t = byName.get(m.name);
            if (!t) {
                t = { ...m, labelNames: m.perShard ? [...m.labelNames, 'shard'] : m.labelNames, children: new Map() };
                byName.set(m.name, t);
            }
            for (const c of m.children) {
                const l = m.perShard ? [...c.l, String(shard)] : c.l;
                const key = l.join('\u0001');
                const prev = t.children.get(key);
                if (!prev) { t.children.set(key, m.kind === 'histogram' ? { l, counts: [...c.counts], sum: c.sum, count: c.count } : { l, v: c.v }); continue; }
                if (m.kind === 'histogram') {
                    for (let i = 0; i < c.counts.length; i++) prev.counts[i] += c.counts[i];
                    prev.sum += c.sum; prev.count += c.count;
                } else if (m.merge === 'max') prev.v = Math.max(prev.v, c.v);
                else if (m.merge === 'last') prev.v = c.v;
                else prev.v += c.v;
            }
        }
    }
    return [...byName.values()].map((t) => ({ ...t, children: [...t.children.values()] }));
}

function esc(v) { return String(v).replace(/\\/g, '\\\\').replace(/\n/g, '\\n').replace(/"/g, '\\"'); }
function labelStr(names, values, extra) {
    const parts = names.map((n, i) => `${n}="${esc(values[i])}"`);
    if (extra) parts.push(extra);
    return parts.length ? `{${parts.join(',')}}` : '';
}

// Prometheus text format 0.0.4.
export function render(snapshot) {
    let s = '';
    for (const m of snapshot) {
        s += `# HELP ${m.name} ${m.help}\n# TYPE ${m.name} ${m.kind}\n`;
        for (const c of m.children) {
            if (m.kind === 'histogram') {
                let cum = 0;
                for (let i = 0; i < m.buckets.length; i++) {
                    cum += c.counts[i];
                    s += `${m.name}_bucket${labelStr(m.labelNames, c.l, `le="${m.buckets[i]}"`)} ${cum}\n`;
                }
                cum += c.counts[m.buckets.length];
                s += `${m.name}_bucket${labelStr(m.labelNames, c.l, 'le="+Inf"')} ${cum}\n`;
                s += `${m.name}_sum${labelStr(m.labelNames, c.l)} ${c.sum}\n${m.name}_count${labelStr(m.labelNames, c.l)} ${c.count}\n`;
            } else {
                s += `${m.name}${labelStr(m.labelNames, c.l)} ${c.v}\n`;
            }
        }
    }
    return s;
}

// The process registry.
export const metrics = new Registry();
