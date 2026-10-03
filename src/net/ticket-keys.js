// TLS session-ticket keys shared by every worker, with forward secrecy.
//
// The primary draws a random 32-byte key at its start (TicketKeys.random) and gives its current
// state { key, day } to each shard that starts, a restarted one included ('tls.ticketKeys' IPC
// request). Every process then moves its state forward one UTC day at a time (the key of day d + 1
// is HMAC-SHA256 of the key of day d) and overwrites the previous key, so every worker applies the
// same ticket keys on the same day (a client resumes its TLS session on whichever worker the
// kernel hands it to), while nothing left in memory or on disk recomputes the keys of a past day:
// recorded TLS 1.2 sessions stay private when SERVER_SECRET leaks or the memory is read later.
// Nothing is written to disk either, so after a full restart every client does one full handshake.
// The copies that cross the IPC are wiped as far as this code holds them: a shard overwrites the
// key it received once copied (TicketKeys.take); the primary's reply and the serialized message
// are left to the garbage collector (best effort, the key of the day a worker started).

import crypto from 'node:crypto';

const DAY_MS = 86400000;
const KEY_BYTES = 32;

export class TicketKeys {
    /**
     * @param {{ key: Uint8Array, day: number }} state the key of `day` (days since the epoch, UTC)
     */
    constructor({ key, day } = {}) {
        if (!(key instanceof Uint8Array) || key.length !== KEY_BYTES || !Number.isInteger(day)) {
            throw new TypeError(`ticket keys: a ${KEY_BYTES}-byte key and its day are required`);
        }
        this._key = Buffer.from(key);
        this._day = day;
    }

    /** A new random state for the day of `now` (the primary's, at its start). */
    static random(now = Date.now()) {
        return new TicketKeys({ key: crypto.randomBytes(KEY_BYTES), day: Math.floor(now / DAY_MS) });
    }

    /** The state a shard received from the primary: copied, then the received key overwritten. */
    static take(state) {
        const keys = new TicketKeys(state);
        state.key.fill(0);
        return keys;
    }

    /**
     * Moves forward to the day of `now`, overwriting each previous key; never back. Returns the
     * current day.
     */
    advance(now = Date.now()) {
        const day = Math.floor(now / DAY_MS);
        while (this._day < day) {
            const next = crypto.createHmac('sha256', this._key).update('scacelith-tls-tickets next day').digest();
            this._key.fill(0);
            this._key = next;
            this._day++;
        }
        return this._day;
    }

    /** The current state (a copy), for a shard that starts. */
    state(now = Date.now()) {
        this.advance(now);
        return { key: Buffer.from(this._key), day: this._day };
    }

    /** The 48 bytes of ticket keys of the current day (tls.Server setTicketKeys). */
    ticketKeys(now = Date.now()) {
        this.advance(now);
        return Buffer.from(crypto.hkdfSync('sha256', this._key, '', 'scacelith-tls-tickets', 48));
    }
}
