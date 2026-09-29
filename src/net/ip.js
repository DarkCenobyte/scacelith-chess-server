// Client address helpers: normalisation, per-subnet grouping for connection limits, trusted proxy
// matching and X-Forwarded-For resolution.
//
// Addresses are handled as the strings Node gives (`socket.remoteAddress`). IPv4-mapped IPv6
// addresses (`::ffff:1.2.3.4`, what a dual-stack listener reports for IPv4 clients) are turned into
// plain IPv4 so that one client has one key whatever the listener.

import net from 'node:net';
import { expandIPv6 } from '../log.js';

/**
 * Canonical form of an address: IPv4-mapped IPv6 becomes IPv4, zone ids are dropped, IPv6 is
 * lower-cased. Returns '' for anything that is not an IP address.
 * @param {string} ip
 * @returns {string}
 */
export function normalizeIp(ip) {
    if (!ip) return '';
    let a = String(ip).trim();
    if (a.startsWith('[') && a.endsWith(']')) a = a.slice(1, -1);
    const pct = a.indexOf('%');
    if (pct >= 0) a = a.slice(0, pct);
    const lower = a.toLowerCase();
    if (lower.startsWith('::ffff:') && lower.includes('.')) a = lower.slice(7);
    const v = net.isIP(a);
    if (v === 4) return a;
    if (v === 6) return a.toLowerCase();
    return '';
}

/**
 * Key used to count connections per client: the IPv4 address itself, or the /64 prefix of an
 * IPv6 address (one subscriber usually owns a whole /64, so counting single IPv6 addresses would
 * let one host open a practically unlimited number of connections). With `v6Prefix` 48, IPv6 is
 * grouped by /48 instead, the usual size of one customer's allocation: the TLS admission gate
 * uses it, so that one customer cannot pass for many address groups.
 * @param {string} ip
 * @param {48|64} [v6Prefix]
 * @returns {string}
 */
export function ipGroupKey(ip, v6Prefix = 64) {
    const a = normalizeIp(ip);
    if (!a) return 'unknown';
    if (net.isIP(a) === 4) return a;
    const parts = expandIPv6(a);
    if (!parts) return a;
    return v6Prefix === 48 ? parts.slice(0, 3).join(':') + '::/48' : parts.slice(0, 4).join(':') + '::/64';
}

/**
 * Builds a matcher for a list of addresses and CIDR subnets ("10.0.0.0/8", "::1", "fd00::/8").
 * Invalid entries throw, so a typo in TRUSTED_PROXIES is found at start-up.
 * @param {string[]} list
 * @returns {(ip: string) => boolean}
 */
export function ipMatcher(list) {
    const bl = new net.BlockList();
    let n = 0;
    for (const raw of list || []) {
        const entry = String(raw).trim();
        if (!entry) continue;
        const slash = entry.indexOf('/');
        const addr = normalizeIp(slash >= 0 ? entry.slice(0, slash) : entry);
        const type = net.isIP(addr) === 6 ? 'ipv6' : 'ipv4';
        if (!addr) throw new Error(`not an IP address or subnet: "${entry}"`);
        if (slash >= 0) {
            const prefix = Number(entry.slice(slash + 1));
            const max = type === 'ipv6' ? 128 : 32;
            if (!Number.isInteger(prefix) || prefix < 0 || prefix > max) throw new Error(`bad prefix length: "${entry}"`);
            bl.addSubnet(addr, prefix, type);
        } else {
            bl.addAddress(addr, type);
        }
        n++;
    }
    if (!n) return () => false;
    return (ip) => {
        const a = normalizeIp(ip);
        if (!a) return false;
        return bl.check(a, net.isIP(a) === 6 ? 'ipv6' : 'ipv4');
    };
}

/**
 * The client address of a request that arrived through `peer`. X-Forwarded-For is only read when
 * the peer is a trusted proxy; the list is then walked from the right (the closest hop), skipping
 * trusted proxies, and the first untrusted address is the client. A malformed entry stops the
 * walk (everything to its left may be forged by the client).
 * @param {string} peer socket.remoteAddress
 * @param {string|undefined} xff X-Forwarded-For header value (comma-joined when repeated)
 * @param {(ip: string) => boolean} isTrusted
 * @returns {string}
 */
export function resolveClientIp(peer, xff, isTrusted) {
    const p = normalizeIp(peer);
    if (!xff || !isTrusted || !isTrusted(p)) return p;
    const hops = String(xff).split(',');
    let last = p;
    for (let i = hops.length - 1; i >= 0; i--) {
        const a = normalizeIp(hops[i]);
        if (!a) return last;
        if (!isTrusted(a)) return a;
        last = a;
    }
    return last;
}
