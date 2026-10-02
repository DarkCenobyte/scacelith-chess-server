// Shared helpers of the anti-cheat module.

/** Integrity levels, weakest first. */
export const LEVELS = Object.freeze(['none', 'suspected', 'high_confidence', 'confirmed']);

/** Rank of a level (unknown values count as 'none'). */
export function levelRank(level) {
    const i = LEVELS.indexOf(level);
    return i < 0 ? 0 : i;
}

/**
 * Structured value read back from the store: objects as they are, JSON text parsed (twice when
 * it was stored already encoded), anything unreadable -> fallback.
 * @param {*} v
 * @param {*} [fallback=null]
 */
export function parseMaybeJson(v, fallback = null) {
    let x = v;
    for (let i = 0; i < 2 && typeof x === 'string'; i++) {
        try { x = JSON.parse(x); } catch { return fallback; }
    }
    if (x === undefined || x === null) return fallback;
    return x;
}

/**
 * Calls a store write with structured values; when the store refuses to bind an object
 * (TypeError from node:sqlite), retries once with the given fields JSON-encoded. The Store API
 * does not say whether free-form columns (detail, evidence, features) take objects or text.
 * @param {(value: *) => *} write   receives the argument to pass
 * @param {*} value                 object or array of objects
 * @param {string[]} fields         fields to encode on retry (empty: encode the value itself)
 */
export function writeStructured(write, value, fields = []) {
    try {
        return write(value);
    } catch (e) {
        if (!(e instanceof TypeError)) throw e;
        const enc = (o) => {
            if (!fields.length) return JSON.stringify(o);
            const c = { ...o };
            for (const f of fields) if (c[f] !== undefined && c[f] !== null && typeof c[f] !== 'string') c[f] = JSON.stringify(c[f]);
            return c;
        };
        return write(Array.isArray(value) ? value.map(enc) : enc(value));
    }
}

/**
 * Integrity record with defaults (the store returns null for a player never scored). A store error
 * gives the defaults too, unless `strict` (a caller that writes the record back must not write
 * the defaults over it).
 */
export function readIntegrity(store, userId, strict = false) {
    let r = null;
    try { r = store.integrity.get(userId); } catch (e) { if (strict) throw e; r = null; }
    return {
        level: r?.level || 'none',
        score: Number(r?.score) || 0,
        evidence: parseMaybeJson(r?.evidence, {}) || {},
        updatedAt: r?.updatedAt ?? null,
        reviewedBy: r?.reviewedBy ?? null,
    };
}

/** Writes an integrity record, tolerating stores that want the evidence as text. */
export function writeIntegrity(store, userId, fields) {
    return writeStructured((f) => store.integrity.set(userId, f), fields, ['evidence']);
}

/**
 * Runs fn() in one store transaction when the store has them (store.transaction: BEGIN
 * IMMEDIATE), else directly. An integrity record read, changed and written back inside it cannot
 * overwrite what another process (the analysis, a shard's automatic sanction, the admin CLI)
 * wrote in between.
 */
export function inTx(store, fn) {
    return typeof store.transaction === 'function' ? store.transaction(fn) : fn();
}

export const HOUR_MS = 3600000;
export const DAY_MS = 86400000;
