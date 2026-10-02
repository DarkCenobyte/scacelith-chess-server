// POST /api/v1/account/export: everything the server keeps about the signed-in player's account,
// as one JSON document to save (docs/API.md).
//
//   POST /api/v1/account/export { password, code?, recoveryCode? }
//     -> 200 the document, Content-Disposition: attachment; filename="scacelith-account-<username>.json"
//
// Re-authentication as for POST /account/delete (auth/accounts.js reauth through exportAccount:
// the password, plus an authenticator code or a recovery code when two-step verification is on):
// 403 invalid_password | mfa_code_required | invalid_code, 400 password_not_set (Google-only
// account), 429 too_many_attempts, 503 server_busy / 429 rate_limited (password hash queue).
// Rate: account_export (5 per hour per player, shared: an export is heavy; taken first), then the
// limits of every re-authentication (routes/account.js reauthRatesOf: reauth per address, shared,
// and reauth_user, AUTH_REAUTH_PER_USER per 10 minutes per player). An account_exported security
// event is recorded.
//
// The document (EXPORT_FORMAT, version 1; times are epoch milliseconds):
//   { format, version, exportedAt, server: { name, host }, notes: [plain English],
//     account: { id, username, email, emailVerified, pendingEmail, mfaEnabled, googleLinked,
//                googleEmail, hasPassword, acceptChallenges, createdAt, lastLoginAt },
//     ratings: [{ category, rating, games, wins, draws, losses, peak, provisional, rated,
//                 countedGames, updatedAt }],
//     ratingRefunds: [{ day, category, points }]   (points given back, per UTC day and category)
//     sessions: [{ id, createdAt, lastSeenAt, expiresAt, revokedAt, clientLabel, ip }]
//     securityEvents: [{ kind, at, ip, detail }]                 (newest first)
//     sanctions: [{ id, kind, reason, source, gameId, startsAt, endsAt, createdAt, liftedAt }]
//     conduct: [{ kind, at }]
//     reportsFiled: [{ gameId, reported, category, comment, createdAt, status: 'open'|'closed' }]
//     games: { total, list: [the summaries of GET /account/games, newest first] } }
//
// Never in it: the password hash, the MFA secret, the recovery codes, any token or token hash (the
// session list has none), the anti-cheat's data (integrity level and score, anomalies, analysis
// features, population statistics, the weight of a report), the reports made against the player,
// the identities of moderators (sanctions lose createdBy / liftedBy; a moderator_action event keeps
// only its action, and only for the actions in MODERATOR_ACTIONS), and other players' private data:
//  * nothing tells which opponent was sanctioned: a rating refund names neither the cheater nor
//    the game (its game id would name the opponent through games.list), the refunds are added up
//    per UTC day and category (what the game's Notice{RatingRestored} already tells) and their
//    rating_refund security events are left out; a filed report is only 'open' or 'closed' (not
//    'actioned' or 'dismissed') and names the reported player by the public name;
//  * no other person's IP address: an event keeps its IP only for the kinds in IP_KINDS, done by
//    someone signed in, or with the account's password and second factor, or with a link mailed
//    to its address; anyone can cause the other kinds (failed sign-ins, a reset requested or a
//    registration tried with the account's address) by typing its name or address.
// The detail of a security event keeps only the fields DETAIL_FIELDS lists for its kind (null for
// any other kind). The `notes` of the document say so to the player.

import { historySummary } from './account-games.js';
import { reauthRatesOf } from './account.js';

export const EXPORT_FORMAT = 'scacelith-account-export';
export const EXPORT_VERSION = 1;

const GAMES_PAGE = 500;
const ROWS_MAX = 100000;

/** Fields of a security event's detail the export keeps, per event kind (others: detail null). */
export const DETAIL_FIELDS = Object.freeze({
    login: ['method'],
    sso_login: ['provider'],
    sso_linked: ['provider'],
    sso_account_created: ['provider'],
    login_failed: ['failures'],
    login_lockout: ['retryAfterMs'],
    mfa_failed: ['attempts'],
    recovery_code_used: ['remaining'],
    reauth_failed: ['factor'],
    session_revoked: ['reason'],
    sessions_revoked_all: ['reason'],
    email_change_refused: ['reason'],
    sanction_auto: ['kind', 'gameId', 'until'],
});

/** Moderator actions an exported moderator_action event shows ({ action } only); others are left out. */
export const MODERATOR_ACTIONS = Object.freeze(['ban', 'unban', 'reset_mfa', 'verify_email', 'revoke_sessions']);

/**
 * Security event kinds whose IP address the export keeps: what someone signed in to the account,
 * or holding its password and second factor, or a link mailed to its address, did. Any other kind
 * has ip null (failed sign-ins, reset and confirmation requests, a registration tried with the
 * account's address: their IP may be another person's).
 */
export const IP_KINDS = Object.freeze([
    'register', 'email_verified', 'login', 'sso_login', 'sso_account_created', 'recovery_code_used', 'password_reset',
    'password_changed', 'reauth_failed', 'mfa_setup_started', 'mfa_enabled', 'mfa_disabled', 'recovery_codes_regenerated',
    'session_revoked', 'sessions_revoked_all', 'email_change_requested', 'email_changed', 'email_change_refused', 'account_exported',
]);
const IP_KIND_SET = new Set(IP_KINDS);

const DAY_MS = 86400000;

/**
 * The plain English notes of the document.
 * @param {object} config
 * @returns {string[]}
 */
export function exportNotes(config) {
    return [
        `This file holds the data ${config.serverName} keeps about your account. Times are milliseconds since 1970-01-01 UTC.`,
        'Not included, to protect your account: your password (only a one-way hash of it is stored), your two-step verification secret, your recovery codes, and your sign-in and e-mail link tokens.',
        'Not included: the anti-cheat\'s records (integrity level, anomalies, the analysis of your games and the statistics it uses), the reports other players made about you, and the names of the moderators who acted on your account.',
        'Not included: other players\' private data. Your games show the public names and ratings of your opponents. The rating points given back to you after an opponent was found cheating are added up per day, without the games; a report you filed shows only whether it is still open.',
        'IP addresses are given only for what was done while signed in to your account, or with its password, or with a link sent to your address. Failed sign-ins and the requests anyone can make by typing your name or address (a password reset, a registration with your address) are listed without one: it may be another person\'s.',
        'games.list has a summary of each of your games; the moves of a game are at /api/v1/games/<id> and its PGN at /api/v1/games/<id>/pgn.',
        `Security events are kept for ${config.retentionSecurityDays} days, and the IP addresses stored with them and with your sessions for ${config.retentionIpDays} days. A session is deleted when it expires, or a day after it was signed out; conduct events (abandoned and aborted games) after 30 days.`,
    ];
}

function pick(obj, fields) {
    const out = {};
    for (const f of fields) if (obj[f] !== undefined) out[f] = obj[f];
    return out;
}

/**
 * A security event as exported, or null when it is left out.
 * @param {{ kind: string, at: number, ip?: string|null, detail?: any }} e
 */
export function exportedEvent(e) {
    // The auth module's events reach the store with their detail as JSON text (security/events.js),
    // which the store gives back as that text; the anti-cheat's as objects.
    let d = e.detail;
    if (typeof d === 'string') {
        try { d = JSON.parse(d); } catch { d = null; }
    }
    if (!d || typeof d !== 'object' || Array.isArray(d)) d = null;
    let detail = null;
    if (e.kind === 'rating_refund') return null;      // in ratingRefunds, added up per day (header)
    if (e.kind === 'moderator_action') {
        if (!d || !MODERATOR_ACTIONS.includes(d.action)) return null;
        detail = { action: d.action };
    } else if (d && DETAIL_FIELDS[e.kind]) {
        detail = pick(d, DETAIL_FIELDS[e.kind]);
    }
    return { kind: e.kind, at: e.at, ip: IP_KIND_SET.has(e.kind) ? e.ip ?? null : null, detail };
}

/**
 * Rating refunds added up per UTC day and category, newest day first: the points, without the
 * games they came from (each of whose opponents was found cheating).
 * @param {{ category: string, points: number, createdAt: number }[]} refunds
 * @returns {{ day: number, category: string, points: number }[]}  day: 00:00 UTC, epoch ms
 */
export function refundsPerDay(refunds) {
    const sums = new Map();
    for (const f of refunds) {
        const day = Math.floor(f.createdAt / DAY_MS) * DAY_MS;
        const key = `${day} ${f.category}`;
        const sum = sums.get(key);
        if (sum) sum.points += f.points;
        else sums.set(key, { day, category: f.category, points: f.points });
    }
    return [...sums.values()].sort((a, b) => b.day - a.day || (a.category < b.category ? -1 : a.category > b.category ? 1 : 0));
}

const tick = () => new Promise((resolve) => setImmediate(resolve));

/**
 * Builds the export document of `user` (an active account row).
 * @param {{ store: object, config: object, user: object, account: object, now: number }} o
 *   `account`: the account view (auth.accountView(user))
 * @returns {Promise<object>}
 */
export async function buildAccountExport({ store, config, user, account, now }) {
    const id = user.id;
    const google = (store.sso.forUser(id) || []).find((l) => l.provider === 'google') || null;

    const list = [];
    for (let before = null; ;) {
        const page = store.games.listForUser(id, { before, limit: GAMES_PAGE });
        for (const g of page) list.push(historySummary(g, id));
        if (page.length < GAMES_PAGE) break;
        before = page[page.length - 1].id;
        await tick();       // a long history: let the other requests of this worker in between
    }

    return {
        format: EXPORT_FORMAT,
        version: EXPORT_VERSION,
        exportedAt: now,
        server: { name: config.serverName, host: config.serverPublicHost },
        notes: exportNotes(config),
        account: {
            id, username: account.username, email: account.email ?? null, emailVerified: !!account.emailVerified,
            pendingEmail: account.pendingEmail ?? null, mfaEnabled: !!account.mfaEnabled, googleLinked: !!account.googleLinked,
            googleEmail: google ? google.email ?? null : null, hasPassword: !!account.hasPassword,
            acceptChallenges: account.acceptChallenges, createdAt: account.createdAt ?? null, lastLoginAt: account.lastLoginAt ?? null,
        },
        ratings: (store.ratings.forUser(id) || []).map((r) => ({
            category: r.category, rating: r.rating, games: r.games, wins: r.wins, draws: r.draws, losses: r.losses, peak: r.peak,
            provisional: !!r.provisional, rated: r.rated ?? null, countedGames: r.countedGames ?? null, updatedAt: r.updatedAt ?? null,
        })),
        ratingRefunds: refundsPerDay(store.refunds.list({ victimId: id, limit: ROWS_MAX }) || []),
        sessions: (store.sessions.allForUser(id) || []).map((r) => ({
            id: r.id, createdAt: r.createdAt, lastSeenAt: r.lastSeenAt ?? r.createdAt, expiresAt: r.expiresAt, revokedAt: r.revokedAt ?? null,
            clientLabel: r.clientLabel ?? null, ip: r.ip ?? null,
        })),
        securityEvents: (store.security.forUser(id, ROWS_MAX) || []).map(exportedEvent).filter(Boolean),
        sanctions: (store.sanctions.list(id) || []).map((s) => ({
            id: s.id, kind: s.kind, reason: s.reason ?? null, source: s.source ?? null, gameId: s.gameId || null, startsAt: s.startsAt ?? null,
            endsAt: s.endsAt ?? null, createdAt: s.createdAt ?? null, liftedAt: s.liftedAt ?? null,
        })),
        conduct: (store.conduct.forUser(id, ROWS_MAX) || []).map((c) => ({ kind: c.kind, at: c.at })),
        reportsFiled: (store.reports.forReporter(id, ROWS_MAX) || []).map((r) => ({
            gameId: r.gameId || null, reported: r.reportedName ?? null, category: r.category, comment: r.comment ?? null,
            createdAt: r.createdAt, status: (r.status || r.outcome || 'open') === 'open' ? 'open' : 'closed',
        })),
        games: { total: store.games.countForUser(id), list },
    };
}

/** The file name of a player's export ("scacelith-account-<username>.json"). */
export function exportFileName(username) {
    return `scacelith-account-${String(username).replace(/[^A-Za-z0-9_.-]/g, '_')}.json`;
}

const PASSWORD = { type: 'string', min: 1, max: 1024 };
const CODE = { type: 'string', min: 1, max: 32 };

/**
 * Registers POST /api/v1/account/export.
 * @param {import('../router.js').Router} router
 * @param {{ config: object, store: object, auth: object, log?: object, now?: () => number }} deps
 */
export function register(router, { config, store, auth, log, now = Date.now }) {
    const exportRate = { key: 'account_export', limit: 5, windowMs: 3600000, by: 'user', shared: true };

    router.post('/account/export', async (ctx) => {
        let doc;
        try {
            doc = await auth.exportAccount(ctx.user, { ...ctx.body, ip: ctx.ip },
                (user) => buildAccountExport({ store, config, user, account: auth.accountView(user), now: now() }));
        } catch (e) {
            if (e && e.code === 'busy' && !e.expose) {
                if (log) log.warn('account export: store busy', { err: e });
                return { status: 503, body: { error: 'busy', message: 'Try again shortly.', retryAfter: 1 }, headers: { 'Retry-After': '1' } };
            }
            throw e;
        }
        return { body: doc, headers: { 'Content-Disposition': `attachment; filename="${exportFileName(doc.account.username)}"` } };
    }, {
        auth: 'required', rate: [exportRate, ...reauthRatesOf(config)], timeoutMs: 60000,
        body: { password: PASSWORD, code: { ...CODE, optional: true }, recoveryCode: { ...CODE, optional: true } },
    });
}
