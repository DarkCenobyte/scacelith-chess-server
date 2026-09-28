// Presence (primary): the one live connection of each account, and the connection counts that
// enforce MAX_CONNECTIONS (whole server) and MAX_CONNECTIONS_PER_IP (IPv4 address or IPv6 /64).
//
// Connection counts are acquired before the WebSocket upgrade completes ('conn.ipAcquire') and
// released when the socket closes ('conn.ipRelease'); they are kept per shard as well, so that a
// crashed shard's connections can be forgotten at once (dropShard).
//
// All operations are O(1).

import { ipGroupKey } from '../net/ip.js';

export class Presence {
    /**
     * @param {{ maxConnections?: number, maxPerIp?: number, now?: () => number }} [o]
     */
    constructor({ maxConnections = 200000, maxPerIp = 16, now = Date.now } = {}) {
        this.maxConnections = maxConnections;
        this.maxPerIp = maxPerIp;
        this.now = now;
        /** @type {Map<number, {userId:number, username:string, shard:number, connId:number, ip:string, since:number}>} */
        this.users = new Map();
        /** @type {Map<string, number>} lower-case username -> userId */
        this.byName = new Map();
        /** @type {Map<string, number>} ip group -> open connections */
        this.ipCounts = new Map();
        /** @type {Map<number, Map<string, number>>} shard -> ip group -> open connections */
        this.shardIps = new Map();
        /** @type {Map<number, number>} shard -> open connections */
        this.shardCounts = new Map();
        this.connections = 0;
    }

    /**
     * Counts a new connection from `ip` on `shard`, unless a limit is reached.
     * @param {string} ip
     * @param {number} shard
     * @returns {{ ok: boolean, reason?: 'per_ip'|'global' }}
     */
    ipAcquire(ip, shard) {
        if (this.connections >= this.maxConnections) return { ok: false, reason: 'global' };
        const key = ipGroupKey(ip);
        const n = this.ipCounts.get(key) || 0;
        if (n >= this.maxPerIp) return { ok: false, reason: 'per_ip' };
        this.ipCounts.set(key, n + 1);
        let m = this.shardIps.get(shard);
        if (!m) { m = new Map(); this.shardIps.set(shard, m); }
        m.set(key, (m.get(key) || 0) + 1);
        this.shardCounts.set(shard, (this.shardCounts.get(shard) || 0) + 1);
        this.connections++;
        return { ok: true };
    }

    /**
     * Releases a connection counted by ipAcquire.
     * @param {string} ip
     * @param {number} shard
     */
    ipRelease(ip, shard) {
        const key = ipGroupKey(ip);
        const m = this.shardIps.get(shard);
        const s = m?.get(key) || 0;
        if (!s) return false;                        // unknown (e.g. already dropped with its shard)
        if (s === 1) m.delete(key); else m.set(key, s - 1);
        const n = this.ipCounts.get(key) || 0;
        if (n <= 1) this.ipCounts.delete(key); else this.ipCounts.set(key, n - 1);
        this.shardCounts.set(shard, Math.max(0, (this.shardCounts.get(shard) || 0) - 1));
        this.connections--;
        return true;
    }

    /** Open connections counted for an address's group. */
    ipCount(ip) { return this.ipCounts.get(ipGroupKey(ip)) || 0; }

    /**
     * Makes (shard, connId) the live connection of the user.
     * @returns {{ previous: null | {shard:number, connId:number} }} the connection it replaces
     */
    claim({ userId, username = '', shard, connId, ip = '' }) {
        const prev = this.users.get(userId) || null;
        if (prev && prev.username) this.byName.delete(prev.username.toLowerCase());
        this.users.set(userId, { userId, username, shard, connId, ip, since: this.now() });
        if (username) this.byName.set(username.toLowerCase(), userId);
        if (prev && prev.shard === shard && prev.connId === connId) return { previous: null };
        return { previous: prev ? { shard: prev.shard, connId: prev.connId } : null };
    }

    /**
     * Forgets the user's connection if it is still (shard?, connId). Returns true when it was.
     */
    release(userId, connId, shard) {
        const cur = this.users.get(userId);
        if (!cur || cur.connId !== connId || (shard !== undefined && cur.shard !== shard)) return false;
        this.users.delete(userId);
        if (cur.username && this.byName.get(cur.username.toLowerCase()) === userId) this.byName.delete(cur.username.toLowerCase());
        return true;
    }

    /** The user's live connection. */
    get(userId) { return this.users.get(userId); }

    /** userId of an online user by name (case-insensitive), or 0. */
    userIdByName(name) { return this.byName.get(String(name).toLowerCase()) || 0; }

    /** Online users. */
    get size() { return this.users.size; }

    /**
     * Forgets everything of a shard that went away (its process exited).
     * @param {number} shard
     * @returns {number[]} users whose live connection was on that shard
     */
    dropShard(shard) {
        const gone = [];
        for (const [userId, p] of this.users) {
            if (p.shard !== shard) continue;
            this.users.delete(userId);
            if (p.username && this.byName.get(p.username.toLowerCase()) === userId) this.byName.delete(p.username.toLowerCase());
            gone.push(userId);
        }
        const m = this.shardIps.get(shard);
        if (m) {
            for (const [key, s] of m) {
                const n = (this.ipCounts.get(key) || 0) - s;
                if (n <= 0) this.ipCounts.delete(key); else this.ipCounts.set(key, n);
                this.connections -= s;
            }
            this.shardIps.delete(shard);
        }
        this.shardCounts.delete(shard);
        if (this.connections < 0) this.connections = 0;
        return gone;
    }

    /** Connections counted per shard. */
    shardConnections(shard) { return this.shardCounts.get(shard) || 0; }
}
