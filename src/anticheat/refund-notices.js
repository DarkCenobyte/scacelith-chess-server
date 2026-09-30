// Notice{RatingRestored, arg: points} of the rating refunds (refunds.js), sent by the primary's
// control plane to each refunded victim, out of a game only (the notice must not interrupt one):
//   * when the refund is applied, if the victim is connected and neither playing nor starting a
//     game ('sanction.applied' from a shard makes the primary look at once; the refunds of an
//     admin command are found by the poll, every REFUND_POLL_MS);
//   * otherwise right after the victim's current game ends ('game.ended');
//   * otherwise at the victim's next connection, right after Welcome, unless that connection
//     resumes a game: then after that game ends.
// One notice carries every refund of the victim not notified yet (the total of their points).
// A refund is marked notified (store.refunds.markNotified) only once the shard reports the notice
// written on the victim's connection ('conn.send' answered { ok: true }). A connection that is not
// ready yet (the claim is answered before Welcome is written) answers { ok: false }: the notice is
// tried again RETRY_MS later, RETRIES times, and then at each poll while the victim can get it.

import { encode, enums } from '../protocol/index.js';

const N = enums.NoticeCode;

/** Interval at which the primary looks for refunds not notified yet. */
export const REFUND_POLL_MS = 5000;
const RETRY_MS = 250;
const RETRIES = 20;
const PAGE = 1000;

export class RefundNotices {
    /**
     * @param {object} o
     * @param {object} o.refunds   store.refunds: pendingSince(afterId, limit), pendingFor(victimId), markNotified(ids, now)
     * @param {(userId: number) => boolean} o.canNotify   connected, and neither playing nor starting a game
     * @param {(userId: number, frames: Buffer[]) => Promise<boolean>} o.send   true once written on the connection
     * @param {object} [o.log]
     * @param {() => number} [o.now]
     * @param {number} [o.pollMs]
     * @param {number} [o.retryMs]
     */
    constructor({ refunds, canNotify, send, log = null, now = Date.now, pollMs = REFUND_POLL_MS, retryMs = RETRY_MS }) {
        this.refunds = refunds;
        this.canNotify = canNotify;
        this.send = send;
        this.log = log;
        this.now = now;
        this.pollMs = pollMs;
        this.retryMs = retryMs;
        /** @type {Map<number, number>} victim -> refunds seen for them (a change during a send means new ones) */
        this.waiting = new Map();
        this.lastId = 0;
        this.inFlight = new Set();
        /** @type {Map<number, { left: number, timer: object|null }>} */
        this.retries = new Map();
        this.timer = null;
    }

    /** Reads the refunds already waiting, then polls every pollMs. */
    start() {
        this.poll();
        this.timer = setInterval(() => this.poll(), this.pollMs);
        this.timer.unref?.();
    }

    stop() {
        if (this.timer) { clearInterval(this.timer); this.timer = null; }
        for (const r of this.retries.values()) if (r.timer) clearTimeout(r.timer);
        this.retries.clear();
    }

    /** Takes the refunds not notified yet that are new since the last poll, then notifies every waiting victim who can be. */
    poll() {
        try {
            for (;;) {
                const rows = this.refunds.pendingSince(this.lastId, PAGE);
                for (const r of rows) {
                    this.lastId = Math.max(this.lastId, r.id);
                    this.waiting.set(r.victimId, (this.waiting.get(r.victimId) || 0) + 1);
                }
                if (rows.length < PAGE) break;
            }
        } catch (e) {
            this.log?.error?.('refunds to notify not read', { err: e });
        }
        for (const userId of this.waiting.keys()) this.notify(userId);
    }

    /** A connection of userId was admitted (presence.claim): after Welcome, unless it resumes a game. */
    connected(userId, activeGame) {
        if (activeGame || !this.waiting.has(userId)) return;
        const r = this.retries.get(userId);
        if (r?.timer) clearTimeout(r.timer);
        this.retries.delete(userId);
        this._retry(userId);
    }

    /** These players' game ended. */
    gameEnded(userIds) {
        for (const u of userIds) if (u && this.waiting.has(u)) this.notify(u);
    }

    /** Sends the victim's notice now if they can get it. */
    notify(userId) {
        if (!this.waiting.has(userId) || this.inFlight.has(userId) || !this.canNotify(userId)) return;
        const seen = this.waiting.get(userId);
        let pending;
        try { pending = this.refunds.pendingFor(userId); } catch (e) {
            this.log?.error?.('refunds to notify not read', { err: e, userId });
            return;
        }
        if (!pending.ids.length) { this._forget(userId); return; }
        this.inFlight.add(userId);
        const frames = [encode.Notice({ code: N.RatingRestored, arg: pending.points })];
        Promise.resolve().then(() => this.send(userId, frames)).then((ok) => {
            this.inFlight.delete(userId);
            if (!ok) { this._retry(userId); return; }
            try { this.refunds.markNotified(pending.ids, this.now()); } catch (e) {
                this.log?.error?.('refunds notified but not marked: the notice is sent again after a restart of the primary', { err: e, userId });
            }
            this.log?.info?.('rating refund notified', { userId, points: pending.points, refunds: pending.ids.length });
            if (this.waiting.get(userId) === seen) this._forget(userId);
            else this.notify(userId);
        }, (e) => {
            this.inFlight.delete(userId);
            this.log?.warn?.('rating refund notice not delivered', { err: e, userId });
            this._retry(userId);
        });
    }

    _forget(userId) {
        this.waiting.delete(userId);
        const r = this.retries.get(userId);
        if (r?.timer) clearTimeout(r.timer);
        this.retries.delete(userId);
    }

    _retry(userId) {
        const r = this.retries.get(userId) || { left: RETRIES, timer: null };
        if (r.timer) return;
        if (r.left <= 0) { this.retries.delete(userId); return; }
        r.left--;
        r.timer = setTimeout(() => {
            r.timer = null;
            if (this.waiting.has(userId)) this.notify(userId);
            else this.retries.delete(userId);
        }, this.retryMs);
        r.timer.unref?.();
        this.retries.set(userId, r);
    }
}
