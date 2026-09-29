// Log-linear histogram of non-negative integers (the load generator records microseconds).
// Exact below 64, then 32 sub-buckets per power of two: about 3 % resolution up to 2^31 (35 min).
// Recording is one index computation and one increment, no allocation. Histograms of the load
// processes are shipped to the coordinator as sparse [index, count] pairs and merged by adding.

const SUB_BITS = 5;
const SUB = 1 << SUB_BITS;               // 32 sub-buckets per power of two
const LINEAR = SUB * 2;                  // 0..63 are exact
const MAX_EXP = 30;                      // values are clamped to 2^31 - 1
export const NBUCKETS = LINEAR + (MAX_EXP - SUB_BITS) * SUB + SUB;

/** Bucket index of a value (clamped to [0, 2^31 - 1]). */
export function bucketOf(v) {
    v = v <= 0 ? 0 : v >= 0x7fffffff ? 0x7fffffff : Math.round(v);
    if (v < LINEAR) return v;
    const e = 31 - Math.clz32(v);
    return LINEAR + (e - SUB_BITS - 1) * SUB + ((v >>> (e - SUB_BITS)) & (SUB - 1));
}

/** Representative value (middle) of a bucket. */
export function valueOf(i) {
    if (i < LINEAR) return i;
    const k = i - LINEAR;
    const e = SUB_BITS + 1 + Math.floor(k / SUB);
    const width = 2 ** (e - SUB_BITS);
    return (SUB + (k % SUB)) * width + width / 2;
}

export class Hist {
    constructor() {
        this.counts = new Float64Array(NBUCKETS);
        this.n = 0;
        this.sum = 0;
        this.max = 0;
        this.min = Infinity;
    }

    add(v) {
        this.counts[bucketOf(v)]++;
        this.n++;
        this.sum += v;
        if (v > this.max) this.max = v;
        if (v < this.min) this.min = v;
    }

    reset() {
        this.counts.fill(0);
        this.n = 0; this.sum = 0; this.max = 0; this.min = Infinity;
    }

    /** Sparse, structured-clone and JSON friendly form: { n, sum, max, min, b: [i, count, ...] }. */
    toSparse() {
        const b = [];
        const c = this.counts;
        for (let i = 0; i < c.length; i++) if (c[i]) b.push(i, c[i]);
        return { n: this.n, sum: this.sum, max: this.max, min: this.n ? this.min : 0, b };
    }

    /** Adds a sparse histogram (from toSparse) into this one. */
    merge(s) {
        if (!s || !s.n) return this;
        for (let k = 0; k < s.b.length; k += 2) this.counts[s.b[k]] += s.b[k + 1];
        this.n += s.n;
        this.sum += s.sum;
        if (s.max > this.max) this.max = s.max;
        if (s.min < this.min) this.min = s.min;
        return this;
    }

    /** Quantile (0..1); the bucket's middle value, the exact maximum for q = 1. */
    quantile(q) {
        if (!this.n) return 0;
        if (q >= 1) return this.max;
        const rank = Math.max(1, Math.ceil(q * this.n));
        let cum = 0;
        const c = this.counts;
        for (let i = 0; i < c.length; i++) {
            cum += c[i];
            if (cum >= rank) return Math.min(valueOf(i), this.max);
        }
        return this.max;
    }

    mean() { return this.n ? this.sum / this.n : 0; }

    /**
     * Summary in the given unit: { n, mean, p50, p90, p99, p999, max } (values divided by `div`,
     * e.g. 1000 to turn microseconds into milliseconds).
     */
    summary(div = 1, digits = 3) {
        const r = (x) => Number((x / div).toFixed(digits));
        return {
            n: this.n, mean: r(this.mean()), p50: r(this.quantile(0.5)), p90: r(this.quantile(0.9)),
            p99: r(this.quantile(0.99)), p999: r(this.quantile(0.999)), max: r(this.max),
        };
    }
}
