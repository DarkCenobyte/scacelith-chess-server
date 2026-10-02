// Small numeric helpers of the analysis and the scoring model (pure functions, no I/O).

/** Win probability in percent for a centipawn score (lichess' logistic model). */
export function winPercent(cp) {
    const c = Math.max(-1000, Math.min(1000, cp));
    return 50 + 50 * (2 / (1 + Math.exp(-0.00368208 * c)) - 1);
}

/**
 * Accuracy of one move (0..100) from the mover's win-probability drop, as lichess computes it
 * (including its +1 "imperfect analysis" bonus).
 * @param {number} winBefore  mover's win% before the move (best play)
 * @param {number} winAfter   mover's win% after the move played
 */
export function moveAccuracy(winBefore, winAfter) {
    const d = Math.max(0, winBefore - winAfter);
    const a = 103.1668100711649 * Math.exp(-0.04354415386753951 * d) - 3.166924740191411 + 1;
    return Math.max(0, Math.min(100, a));
}

/** Arithmetic mean (NaN for an empty list). */
export function mean(xs) {
    if (!xs.length) return NaN;
    let s = 0;
    for (const x of xs) s += x;
    return s / xs.length;
}

/** Harmonic mean of positive values (values below `floor` count as `floor`). */
export function harmonicMean(xs, floor = 1) {
    if (!xs.length) return NaN;
    let s = 0;
    for (const x of xs) s += 1 / Math.max(floor, x);
    return xs.length / s;
}

/** Sample standard deviation (0 for fewer than two values). */
export function stdev(xs) {
    if (xs.length < 2) return 0;
    const m = mean(xs);
    let s = 0;
    for (const x of xs) s += (x - m) * (x - m);
    return Math.sqrt(s / (xs.length - 1));
}

/** Ranks with ties averaged (1-based). */
export function ranks(xs) {
    const idx = xs.map((x, i) => i).sort((a, b) => xs[a] - xs[b]);
    const r = new Array(xs.length);
    for (let i = 0; i < idx.length;) {
        let j = i;
        while (j + 1 < idx.length && xs[idx[j + 1]] === xs[idx[i]]) j++;
        const avg = (i + j) / 2 + 1;
        for (let k = i; k <= j; k++) r[idx[k]] = avg;
        i = j + 1;
    }
    return r;
}

/** Pearson correlation; null when a side has no variance or fewer than 3 points. */
export function pearson(xs, ys) {
    const n = xs.length;
    if (n < 3 || ys.length !== n) return null;
    const mx = mean(xs), my = mean(ys);
    let sxy = 0, sxx = 0, syy = 0;
    for (let i = 0; i < n; i++) {
        const dx = xs[i] - mx, dy = ys[i] - my;
        sxy += dx * dy; sxx += dx * dx; syy += dy * dy;
    }
    if (sxx <= 0 || syy <= 0) return null;
    return sxy / Math.sqrt(sxx * syy);
}

/** Spearman rank correlation (Pearson of the tie-averaged ranks); null when undefined. */
export function spearman(xs, ys) {
    if (xs.length !== ys.length || xs.length < 3) return null;
    return pearson(ranks(xs), ranks(ys));
}

/** Coefficient of variation (sd / mean); null when the mean is not positive. */
export function coefficientOfVariation(xs) {
    if (xs.length < 2) return null;
    const m = mean(xs);
    if (!(m > 0)) return null;
    return stdev(xs) / m;
}

/**
 * Welford accumulator state { n, mean, m2 } updated with one value (returns a new object).
 * @param {{n:number, mean:number, m2:number}|null} s
 * @param {number} x
 */
export function welfordAdd(s, x) {
    const n = (s?.n || 0) + 1;
    const prevMean = s?.mean || 0;
    const delta = x - prevMean;
    const m = prevMean + delta / n;
    const m2 = (s?.m2 || 0) + delta * (x - m);
    return { n, mean: m, m2 };
}

/** Sample variance of a Welford state (0 below two values). */
export function welfordVariance(s) {
    return s && s.n > 1 ? s.m2 / (s.n - 1) : 0;
}

/** Clamps x into [lo, hi]. */
export function clamp(x, lo, hi) { return x < lo ? lo : x > hi ? hi : x; }

/** Upper tail of the standard normal distribution, P(Z >= z) (Abramowitz-Stegun 7.1.26). */
export function normalTail(z) {
    const x = Math.abs(z) / Math.SQRT2;
    const t = 1 / (1 + 0.3275911 * x);
    const erfc = t * (0.254829592 + t * (-0.284496736 + t * (1.421413741 + t * (-1.453152027 + t * 1.061405429)))) * Math.exp(-x * x);
    return z >= 0 ? erfc / 2 : 1 - erfc / 2;
}
