// Hashed timer wheel: one timer per entry, O(1) schedule and cancel, no Node timer per entry.
//
// Slots are `slotMs` wide (10 ms for the game host); the wheel holds `slots` of them (a power of
// two). A deadline further away than one revolution goes to the slot it hashes to and is simply
// skipped (checked, not fired) when that slot comes round before it is due. advance(now) visits
// the slots from the last visited one up to now's slot and fires the entries whose deadline is
// <= now; now's slot is visited again on the next call, since entries in it may not be due yet.
//
// The wheel stores its links on the entry objects themselves (_twPrev, _twNext, _twSlot,
// _twDeadline): nothing is allocated per schedule. Firing order within one advance() is by slot,
// then arbitrary. A callback may schedule or cancel any entry, including itself; an entry
// scheduled at or before `now` from inside a callback fires on the next advance().

const IDLE = -1, DUE = -2;

/**
 * Timer wheel keyed by entry objects.
 */
export class TimerWheel {
    /**
     * @param {{slotMs?:number, slots?:number, startAt?:number}} opts slots must be a power of two
     */
    constructor({ slotMs = 10, slots = 4096, startAt = 0 } = {}) {
        if (!Number.isInteger(slots) || slots < 2 || (slots & (slots - 1)) !== 0) throw new RangeError('TimerWheel: slots must be a power of two');
        this.slotMs = slotMs;
        this.slots = slots;
        this.mask = slots - 1;
        this.heads = new Array(slots).fill(null);
        this.cursor = Math.floor(startAt / slotMs);
        this.count = 0;
        this._due = [];
        this._firing = false;
    }

    /** Number of scheduled entries. */
    get size() { return this.count; }

    /** Whether `entry` has a pending timer. */
    isScheduled(entry) { return entry._twSlot !== undefined && entry._twSlot !== IDLE; }

    /** Deadline of `entry` (Infinity when not scheduled). */
    deadlineOf(entry) { return this.isScheduled(entry) ? entry._twDeadline : Infinity; }

    /**
     * (Re)schedules `entry` at `deadline` (epoch ms). A non-finite deadline cancels it.
     * O(1).
     */
    schedule(entry, deadline) {
        if (this.isScheduled(entry)) this.cancel(entry);
        if (!(deadline < Infinity)) return;
        let t = Math.floor(deadline / this.slotMs);
        if (t < this.cursor) t = this.cursor;
        const idx = t & this.mask;
        const head = this.heads[idx];
        entry._twDeadline = deadline;
        entry._twSlot = idx;
        entry._twPrev = null;
        entry._twNext = head;
        if (head) head._twPrev = entry;
        this.heads[idx] = entry;
        this.count++;
    }

    /** Cancels the timer of `entry`; returns whether one was pending. O(1). */
    cancel(entry) {
        const s = entry._twSlot;
        if (s === undefined || s === IDLE) return false;
        if (s === DUE) {
            entry._twSlot = IDLE;
            this.count--;
            return true;
        }
        const prev = entry._twPrev, next = entry._twNext;
        if (prev) prev._twNext = next; else this.heads[s] = next;
        if (next) next._twPrev = prev;
        entry._twPrev = entry._twNext = null;
        entry._twSlot = IDLE;
        this.count--;
        return true;
    }

    /**
     * Fires every entry whose deadline is <= nowMs: fire(entry, nowMs). Returns the number fired.
     * Cost: O(slots visited + entries in them); after a pause longer than a revolution every slot
     * is visited once.
     */
    advance(nowMs, fire) {
        const nowT = Math.floor(nowMs / this.slotMs);
        let t = this.cursor;
        if (nowT < t) return 0;
        if (nowT - t >= this.slots) t = nowT - this.slots + 1;
        const due = this._firing ? [] : this._due;
        for (; t <= nowT; t++) {
            const idx = t & this.mask;
            let e = this.heads[idx];
            while (e) {
                const next = e._twNext;
                if (e._twDeadline <= nowMs) {
                    const prev = e._twPrev;
                    if (prev) prev._twNext = next; else this.heads[idx] = next;
                    if (next) next._twPrev = prev;
                    e._twPrev = e._twNext = null;
                    e._twSlot = DUE;
                    due.push(e);
                }
                e = next;
            }
        }
        this.cursor = nowT;
        const nested = this._firing;
        this._firing = true;
        let fired = 0;
        try {
            for (let i = 0; i < due.length; i++) {
                const e = due[i];
                due[i] = null;
                if (e._twSlot !== DUE) continue;   // cancelled or rescheduled by an earlier callback
                e._twSlot = IDLE;
                this.count--;
                fired++;
                fire(e, nowMs);
            }
        } finally {
            // An exception leaves the remaining due entries idle; the owner reschedules them.
            for (let i = 0; i < due.length; i++) {
                const e = due[i];
                if (e && e._twSlot === DUE) { e._twSlot = IDLE; this.count--; }
            }
            due.length = 0;
            this._firing = nested;
        }
        return fired;
    }
}
