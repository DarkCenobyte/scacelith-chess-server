// The database side of the automatic sanction of a certain cheat (index.js sanctionCertain): run
// by the shard's store writer thread (src/store/writer.js, 'sanction'), or on the caller's own
// connection when there is no writer (tools, tests). It only uses the store, so the thread does
// not load the rest of the anti-cheat module.

import { refundVictims, refundWindowStart, victimTotals } from './refunds.js';
import { readIntegrity, writeIntegrity, writeStructured, HOUR_MS } from './util.js';

const MAX_EVIDENCE_ITEMS = 50;

/**
 * Bans a player for a certain cheat: ban BAN_DURATION_HOURS (source 'auto') unless an active ban
 * of the same game, or one lasting at least as long, exists (another shard, an earlier anomaly);
 * integrity level 'confirmed' with the evidence appended; security event 'sanction_auto'; the
 * rating refunds of the player's victims (refunds.js; they are idempotent, so they also run when
 * the ban existed). Each step's failure is logged, not thrown.
 * @param {object} store
 * @param {object} config   loadConfig() result (banDurationHours, ratingRefundDays)
 * @param {{ userId: number, gameId?: number, kind: string, at: number }} s
 * @param {object} [log]
 * @returns {{ until: number, created: boolean, sanctionId: number|null, refunds: { victimId: number, points: number }[] }}
 *          refunds: the points given back now, per victim
 */
export function applyCertainSanction(store, config, { userId, gameId = 0, kind, at }, log = null) {
    const reason = `certain_cheat:${kind}`;
    let until = at + config.banDurationHours * HOUR_MS;
    let created = false;
    let sanctionId = null;
    let active = null;
    try { active = store.sanctions.activeBan(userId, at); } catch { active = null; }
    // A ban without an end (permanent) counts as ending in 100 years.
    const activeEnd = active ? Number(active.endsAt ?? active.ends_at ?? 0) || at + 100 * 365 * 24 * HOUR_MS : 0;
    const sameGame = active && gameId && Number(active.gameId ?? active.game_id) === Number(gameId);
    if (active && (sameGame || activeEnd >= until)) {
        // Already banned (another shard, or an earlier anomaly of this game): no second ban.
        until = activeEnd;
        sanctionId = active.id ?? null;
    } else {
        try {
            sanctionId = store.sanctions.create({ userId, kind: 'ban', reason, source: 'auto', gameId: gameId || null, startsAt: at, endsAt: until,
                createdBy: null });
            created = true;
        } catch (e) {
            log?.error?.('automatic ban not stored', { err: e, userId, kind });
        }
    }

    try {
        const prev = readIntegrity(store, userId);
        const ev = { ...prev.evidence };
        ev.certain = [...(Array.isArray(ev.certain) ? ev.certain : []), { kind, gameId: gameId || 0, at, banUntil: until }].slice(-MAX_EVIDENCE_ITEMS);
        writeIntegrity(store, userId, { level: 'confirmed', score: prev.score, evidence: ev, updatedAt: at });
    } catch (e) {
        log?.error?.('integrity not updated after a certain cheat', { err: e, userId });
    }
    if (created) {
        try {
            writeStructured((r) => store.security.insertBatch(r), [{ kind: 'sanction_auto', userId, ip: null, detail: { kind, gameId: gameId || 0, until }, at }], ['detail']);
        } catch (e) { log?.warn?.('security event not stored', { err: e }); }
    }

    let refunds = [];
    const since = refundWindowStart(config, at);
    if (since !== null && store.refunds) {   // a partial store (tests, tools) may have none
        try {
            const given = refundVictims(store, { cheaterId: userId, since, now: at, sanctionId, source: 'auto', log });
            refunds = victimTotals(given);
        } catch (e) {
            log?.error?.('rating refunds not applied', { err: e, userId });
        }
    }
    return { until, created, sanctionId, refunds };
}
