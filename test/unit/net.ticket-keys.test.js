import assert from 'node:assert/strict';
import crypto from 'node:crypto';
import { describe, it } from 'node:test';
import { testConfig } from '../../src/config.js';
import { TicketKeys } from '../../src/net/ticket-keys.js';

const DAY_MS = 86400000;
const at = (day, hours = 0) => day * DAY_MS + hours * 3600000;

describe('TLS session-ticket keys', () => {
    it('are random, not derived from SERVER_SECRET', () => {
        const secret = testConfig().serverSecret;
        const now = Date.now(), day = Math.floor(now / DAY_MS);
        const a = TicketKeys.random(now), b = TicketKeys.random(now);
        assert.equal(a.ticketKeys(now).length, 48);
        assert.notDeepEqual(a.ticketKeys(now), b.ticketKeys(now), 'two starts with the same SERVER_SECRET');
        // The former derivation (HKDF of SERVER_SECRET and the day) gives none of them.
        const derived = Buffer.from(crypto.hkdfSync('sha256', secret, 'scacelith-tls-tickets', `day:${day}`, 48));
        assert.notDeepEqual(a.ticketKeys(now), derived);
        assert.equal(a.state(now).day, day);
    });

    it('move forward one way each UTC day and never back', () => {
        const k = new TicketKeys({ key: Buffer.alloc(32, 9), day: 100 });
        const day100 = k.ticketKeys(at(100, 23));
        assert.deepEqual(k.ticketKeys(at(100, 1)), day100, 'the same keys all day');
        const old = k._key;
        const day101 = k.ticketKeys(at(101));
        assert.notDeepEqual(day101, day100);
        assert.deepEqual(old, Buffer.alloc(32), 'the key of the previous day is overwritten');
        assert.equal(k.advance(at(100, 12)), 101, 'a clock back in time does not go back');
        assert.deepEqual(k.ticketKeys(at(100, 12)), day101);
        assert.equal(k.state(at(100)).day, 101);
        // A state given out on day 100 reaches the same keys on day 103 as one kept moving.
        const kept = new TicketKeys({ key: Buffer.alloc(32, 9), day: 100 });
        assert.deepEqual(kept.ticketKeys(at(103)), k.ticketKeys(at(103)));
        assert.notDeepEqual(k.ticketKeys(at(103)), day101);
    });

    it('give out copies of their state, and refuse a malformed one', () => {
        const k = new TicketKeys({ key: Buffer.alloc(32, 1), day: 5 });
        const s = k.state(at(5));
        s.key.fill(7);
        assert.deepEqual(k.state(at(5)).key, Buffer.alloc(32, 1));
        const given = Buffer.alloc(32, 3);
        const w = new TicketKeys({ key: given, day: 5 });
        w.advance(at(6));
        assert.deepEqual(given, Buffer.alloc(32, 3), 'the caller\'s buffer is not touched');
        for (const bad of [undefined, null, {}, { key: Buffer.alloc(31), day: 5 }, { key: Buffer.alloc(32), day: 1.5 }, { key: 'x'.repeat(32), day: 5 }]) {
            assert.throws(() => new TicketKeys(bad), TypeError);
        }
    });
});
