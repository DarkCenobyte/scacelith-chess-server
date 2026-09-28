// Structured logging: one JSON object per line on stdout (or readable text with
// LOG_FORMAT=pretty). Every module takes a child logger: `const log = logger.child('auth')`.
//
// Privacy rules, enforced here so that no call site can forget them:
//   - fields whose name looks like a credential (password, token, secret, code, otp, cookie,
//     authorization, recovery, mfa...) are replaced by "[redacted]", at any depth;
//   - strings that look like session tokens (sct_...) or bearer headers are masked;
//   - client IP addresses go through ipForLog() (LOG_IP: truncated / full / hashed).
// Log what helps diagnosis and security (user id, game id, event kind, counts, durations), never
// what would let someone log in or read a private message.

import crypto from 'node:crypto';

const LEVELS = { debug: 10, info: 20, warn: 30, error: 40, security: 35 };
const SENSITIVE = /pass(word)?|token|secret|^code$|otp|cookie|authorization|recovery|mfa_?secret|totp|verifier|nonce_?secret|private|credential|smtp_?pass/i;
const TOKEN_LIKE = /\b(sct|swt|mfa|sso)_[A-Za-z0-9_-]{8,}/g;

let state = { level: LEVELS.info, format: 'json', ipMode: 'truncated', ipKey: null, ipKeyDay: -1, secret: null, out: process.stdout, base: {} };

export function configureLogging({ level = 'info', format = 'json', ipMode = 'truncated', secret = null, base = {}, out } = {}) {
    state = { ...state, level: LEVELS[level] ?? LEVELS.info, format, ipMode, secret, base, out: out || process.stdout, ipKey: null, ipKeyDay: -1 };
}

function scrub(v, depth = 0) {
    if (v == null) return v;
    if (typeof v === 'string') return v.length > 2000 ? v.slice(0, 2000) + '…' : v.replace(TOKEN_LIKE, '$1_[redacted]');
    if (typeof v !== 'object') return v;
    if (depth > 4) return '[depth]';
    if (Buffer.isBuffer(v) || ArrayBuffer.isView(v)) return `[${v.length ?? v.byteLength} bytes]`;
    if (v instanceof Error) return { name: v.name, message: scrub(v.message), code: v.code, stack: v.stack ? scrub(v.stack.split('\n').slice(0, 6).join('\n')) : undefined };
    if (Array.isArray(v)) return v.slice(0, 50).map((x) => scrub(x, depth + 1));
    const o = {};
    for (const [k, x] of Object.entries(v)) o[k] = SENSITIVE.test(k) ? '[redacted]' : scrub(x, depth + 1);
    return o;
}

// The client address as it may appear in logs.
export function ipForLog(ip) {
    if (!ip) return undefined;
    if (state.ipMode === 'full') return ip;
    if (state.ipMode === 'hashed') {
        const day = Math.floor(Date.now() / 86400000);
        if (day !== state.ipKeyDay) {
            state.ipKey = crypto.createHmac('sha256', state.secret || 'scacelith').update('log-ip:' + day).digest();
            state.ipKeyDay = day;
        }
        return 'ip:' + crypto.createHmac('sha256', state.ipKey).update(ip).digest('base64url').slice(0, 12);
    }
    return truncateIp(ip);
}

// IPv4 /24, IPv6 /48 (IPv4-mapped IPv6 addresses are treated as IPv4).
export function truncateIp(ip) {
    let a = String(ip);
    if (a.startsWith('::ffff:') && a.includes('.')) a = a.slice(7);
    if (a.includes('.')) { const p = a.split('.'); return p.length === 4 ? `${p[0]}.${p[1]}.${p[2]}.0/24` : a; }
    const parts = expandIPv6(a);
    return parts ? parts.slice(0, 3).join(':') + '::/48' : a;
}

export function expandIPv6(a) {
    const s = a.split('%')[0];
    const halves = s.split('::');
    if (halves.length > 2) return null;
    const head = halves[0] ? halves[0].split(':') : [];
    const tail = halves.length === 2 && halves[1] ? halves[1].split(':') : [];
    const fill = halves.length === 2 ? 8 - head.length - tail.length : 0;
    const all = [...head, ...Array(Math.max(0, fill)).fill('0'), ...tail];
    if (all.length !== 8) return null;
    return all.map((x) => (parseInt(x, 16) || 0).toString(16));
}

function emit(level, component, msg, fields) {
    if (LEVELS[level] < state.level) return;
    const rec = { t: new Date().toISOString(), level, c: component, msg, ...state.base, ...(fields ? scrub(fields) : null) };
    let line;
    if (state.format === 'pretty') {
        const extra = fields ? ' ' + JSON.stringify(scrub(fields)) : '';
        line = `${rec.t} ${level.toUpperCase().padEnd(8)} [${component}] ${msg}${extra}\n`;
    } else {
        line = JSON.stringify(rec) + '\n';
    }
    state.out.write(line);
}

export class Logger {
    constructor(component) { this.component = component; }
    child(name) { return new Logger(this.component ? `${this.component}.${name}` : name); }
    debug(msg, f) { emit('debug', this.component, msg, f); }
    info(msg, f) { emit('info', this.component, msg, f); }
    warn(msg, f) { emit('warn', this.component, msg, f); }
    error(msg, f) { emit('error', this.component, msg, f); }
    // Security-relevant event (failed login, rate limit, anomaly, sanction...). Always logged at
    // info level or above; persisted separately by the modules that need an audit trail.
    security(event, f) { emit('security', this.component, event, f); }
    get debugEnabled() { return LEVELS.debug >= state.level; }
}

export const logger = new Logger('');
