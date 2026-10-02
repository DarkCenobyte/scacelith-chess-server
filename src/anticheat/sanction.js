// The database side of the automatic sanction of a certain cheat (index.js sanctionCertain): run
// by the shard's store writer thread (src/store/writer.js, 'sanction'), or on the caller's own
// connection when there is no writer (tools, tests). It only uses the store, so the thread does
// not load the rest of the anti-cheat module.

import { banRefunds, CheatBanReason, refundVictims, refundWindowStart, victimTotals } from './refunds.js';
import { readIntegrity, writeIntegrity, writeStructured, HOUR_MS } from './util.js';

const MAX_EVIDENCE_ITEMS = 50;

/**
 * Bans a player for a certain cheat: ban BAN_DURATION_HOURS (source 'auto') unless an active ban
 * for cheating that refunds (refunds.js banRefunds) exists for the same game or lasts at least as
 * long (another shard, an earlier anomaly, a moderator's integrity confirm); integrity level
 * 'confirmed' with the evidence appended; security event 'sanction_auto'; the rating refunds of
 * the player's victims (refunds.js; they are idempotent, so they also run when the ban existed).
 * Each step's failure is logged, not thrown; a ban that cannot be stored stops there (failed:
 * nothing else is written), so that the next certain anomaly of the game tries again.
 * @param {object} store
 * @param {object} config   loadConfig() result (banDurationHours, ratingRefundDays)
 * @param {{ userId: number, gameId?: number, kind: string, at: number }} s
 * @param {object} [log]
 * @returns {{ until: number, created: boolean, failed?: boolean, sanctionId: number|null, refunds: { victimId: number, points: number }[] }}
 *          refunds: the points given back now, per victim
 */
export function applyCertainSanction(store, config, { userId, gameId = 0, kind, at }, log = null) {
    const reason = `${CheatBanReason.certain}${kind}`;
    let until = at + config.banDurationHours * HOUR_MS;
    let created = false;
    let sanctionId = null;
    // Only a ban that refunds can stand for this one: under a ban for something else (`user ban`)
    // or an integrity confirm with --no-refund, the games recorded later would not be refunded
    // (store.games.finishBatch).
    let bans = [];
    try { bans = store.sanctions.active(userId, at).filter(banRefunds); } catch { bans = []; }
    // A ban without an end (permanent) counts as ending in 100 years.
    const endOf = (b) => Number(b.endsAt ?? b.ends_at ?? 0) || at + 100 * 365 * 24 * HOUR_MS;
    const sameGame = (b) => gameId && Number(b.gameId ?? b.game_id) === Number(gameId);
    const active = bans.find(sameGame) ?? bans.reduce((a, b) => (a && endOf(a) >= endOf(b) ? a : b), null);
    if (active && (sameGame(active) || endOf(active) >= until)) {
        // Already banned for cheating (another shard, an earlier anomaly of this game, a longer
        // ban): no second ban.
        until = endOf(active);
        sanctionId = active.id ?? null;
    } else {
        try {
            sanctionId = store.sanctions.create({ userId, kind: 'ban', reason, source: 'auto', gameId: gameId || null, startsAt: at, endsAt: until,
                createdBy: null });
            created = true;
        } catch (e) {
            log?.error?.('automatic ban not stored', { err: e, userId, kind });
            // Not 'confirmed' and refunded without the ban that justifies it.
            return { until: 0, created: false, failed: true, sanctionId: null, refunds: [] };
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
