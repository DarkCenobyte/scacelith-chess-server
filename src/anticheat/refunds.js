// Rating refunds of the victims of a player banned for cheating (docs/ANTICHEAT.md, rating refunds).
//
// When a player is banned as a cheater (a certain cheat, sanction.js; a moderator's integrity
// confirm, admin.js), every opponent who lost rating points to them in a rated game that ended
// within RATING_REFUND_DAYS before the ban gets exactly those points back, added to their CURRENT
// rating in that category: the games are not recomputed (the later games of everyone involved
// stand as they were played), a win against the cheater is left alone, and a draw that cost points
// is refunded like a loss. Only the change of the K formula is refunded: the game that gave a
// player their first rating moved them from a working rating, which was no rating to lose. The
// store gives one refund per game and victim at most (store.refunds.applyForCheater), so a
// second ban, a moderator's later `refunds apply` or a retry gives nothing twice. The games that
// are not in the database yet when the ban is given (in progress, or ended and waiting for their
// commit) are refunded by the store as they are recorded (store.games.finishBatch), for as long
// as the player is 'confirmed' and banned.
//
// Audit trail: the rating_refunds rows (game, victim, cheater, category, points, time, the ban
// or the moderator) and one security event 'rating_refund' per refund. The victims learn of it
// through Notice{RatingRestored, arg: points} (refund-notices.js, on the primary).

import { writeStructured, DAY_MS } from './util.js';

/**
 * Start of the automatic refund window of a ban at `banAt`: RATING_REFUND_DAYS before it, or null
 * when RATING_REFUND_DAYS is 0 (no automatic refunds).
 * @param {{ ratingRefundDays?: number }} config
 * @param {number} banAt
 * @returns {number|null}
 */
export function refundWindowStart(config, banAt) {
    const days = Math.max(0, Math.floor(Number(config?.ratingRefundDays ?? 60)));
    return days > 0 ? banAt - days * DAY_MS : null;
}

/**
 * Gives the refunds of a cheater's games that ended at `since` or later, and writes their
 * security events. Returns the refunds given now (none for a game already refunded).
 * @param {object} store  store with refunds and security (src/store/index.js)
 * @param {{ cheaterId: number, since: number, now: number, sanctionId?: number|null,
 *           source: 'auto'|'moderator', by?: string|null, log?: object }} o
 * @returns {{ id: number, gameId: number, victimId: number, category: string, points: number }[]}
 */
export function refundVictims(store, { cheaterId, since, now, sanctionId = null, source, by = null, log = null }) {
    const given = store.refunds.applyForCheater({ cheaterId, since, now, sanctionId, source, by });
    if (!given.length) return given;
    const events = given.map((r) => ({
        kind: 'rating_refund', userId: r.victimId, ip: null, at: now,
        detail: { refundId: r.id, gameId: r.gameId, cheaterId, category: r.category, points: r.points, source, sanctionId, by },
    }));
    try {
        writeStructured((rows) => store.security.insertBatch(rows), events, ['detail']);
    } catch (e) {
        log?.warn?.('refund security events not stored', { err: e, cheaterId });
    }
    log?.security?.('rating.refund', { cheaterId, source, sanctionId, by, refunds: given.length, victims: victimTotals(given).length,
        points: given.reduce((n, r) => n + r.points, 0) });
    return given;
}

/**
 * Points given back per victim, in the order of their first refund.
 * @param {{ victimId: number, points: number }[]} given
 * @returns {{ victimId: number, points: number }[]}
 */
export function victimTotals(given) {
    const out = new Map();
    for (const r of given) out.set(r.victimId, (out.get(r.victimId) || 0) + r.points);
    return [...out].map(([victimId, points]) => ({ victimId, points }));
}
