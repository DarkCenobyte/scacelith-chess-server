// Player reports: eligibility rules, reporter credibility, anti-brigading cap and review
// priority. Pure functions plus the request handler used by src/http/routes/reports.js.
//
// A report never changes a player's integrity level; it only raises the review priority that
// moderators see (bin/admin.js), in proportion to the reporter's credibility. Many reports from
// new or low-credibility accounts do not add up: the weight stored for a report is capped so
// that the reports received by one player in 24 hours sum to at most DAILY_WEIGHT_CAP, and
// low-credibility reports to at most LOW_CRED_DAILY_CAP.

import { DAY_MS, readIntegrity, parseMaybeJson } from './util.js';

export const REPORT_RULES = Object.freeze({
    categories: Object.freeze(['cheating', 'abuse', 'other']),
    maxAgeMs: 7 * DAY_MS,            // the game must have ended within the last 7 days
    commentMax: 500,
    usernameMax: 24,
    dailyWeightCap: 2.0,             // summed weight of the reports one player receives per 24 h
    lowCredibility: 0.5,             // a report weighing less than this is "low credibility"
    lowCredDailyCap: 0.5,            // ...and all of those together weigh at most this per 24 h
    fullAgeDays: 30,                 // account age giving full base credibility
    fullGames: 50,                   // games played giving full base credibility
});

const clamp = (x, lo, hi) => (x < lo ? lo : x > hi ? hi : x);

/**
 * Credibility weight of a reporter (0.02 .. 2).
 *   base  = 0.1 + 0.9 * sqrt(age * games), age = min(1, days / 30), games = min(1, games / 50):
 *           both are needed (fresh account farms and old idle accounts both stay low);
 *   trust = 2 * (actioned + 1) / (actioned + dismissed + 2), clamped to 0.25 .. 1.75: a Laplace
 *           estimate of the reporter's hit rate, 1.0 without history;
 *   a reporter flagged high_confidence counts half, a confirmed cheater a fifth.
 * @param {{ createdAt: number, gamesPlayed: number, actioned?: number, dismissed?: number, level?: string, now: number }} r
 * @returns {{ weight: number, base: number, trust: number }}
 */
export function reporterWeight({ createdAt, gamesPlayed, actioned = 0, dismissed = 0, level = 'none', now }) {
    const ageDays = Math.max(0, (now - (Number(createdAt) || now)) / DAY_MS);
    const age = clamp(ageDays / REPORT_RULES.fullAgeDays, 0, 1);
    const games = clamp((Number(gamesPlayed) || 0) / REPORT_RULES.fullGames, 0, 1);
    const base = 0.1 + 0.9 * Math.sqrt(age * games);
    const trust = clamp((2 * (actioned + 1)) / (actioned + dismissed + 2), 0.25, 1.75);
    let w = base * trust;
    if (level === 'confirmed') w *= 0.2;
    else if (level === 'high_confidence') w *= 0.5;
    return { weight: round3(clamp(w, 0.02, 2)), base: round3(base), trust: round3(trust) };
}

/**
 * Weight actually stored for a new report, given the reports the same player received in the
 * last 24 hours (their stored weights).
 * @param {number} raw
 * @param {{ weight: number, at: number }[]} received
 * @param {number} now
 */
export function cappedWeight(raw, received, now) {
    let today = 0, todayLow = 0;
    for (const r of received || []) {
        const at = Number(r.at ?? r.createdAt ?? 0);
        if (at < now - DAY_MS) continue;
        const w = Number(r.weight) || 0;
        today += w;
        if (w < REPORT_RULES.lowCredibility) todayLow += w;
    }
    let w = raw;
    if (raw < REPORT_RULES.lowCredibility) w = Math.min(w, Math.max(0, REPORT_RULES.lowCredDailyCap - todayLow));
    w = Math.min(w, Math.max(0, REPORT_RULES.dailyWeightCap - today));
    return round3(w);
}

/**
 * Review priority of a player for moderators (0..~130): integrity level, statistical score,
 * and the credibility-weighted reports (logarithmic: the 10th report adds less than the 1st).
 * @param {{ level?: string, score?: number, reportWeight?: number }} p  reportWeight: sum over 30 days
 */
export function reviewPriority({ level = 'none', score = 0, reportWeight = 0 }) {
    const base = { none: 0, suspected: 40, high_confidence: 70, confirmed: 5 }[level] ?? 0;
    const stat = Math.min(20, 4 * Math.max(0, Number(score) || 0));
    const rep = 15 * Math.log2(1 + Math.max(0, Number(reportWeight) || 0));
    return Math.round(base + stat + rep);
}

/** Sum of the weights of the reports received in the last `days` days. */
export function recentReportWeight(received, now, days = 30) {
    let s = 0;
    for (const r of received || []) if (Number(r.at ?? r.createdAt ?? 0) >= now - days * DAY_MS) s += Number(r.weight) || 0;
    return round3(s);
}

function round3(x) { return Math.round(x * 1000) / 1000; }

function bad(message) { return { status: 400, body: { error: 'invalid_request', message } }; }

/**
 * Validates a report body. Returns { value } or { error: response }.
 * @param {*} body
 */
export function validateReport(body) {
    if (!body || typeof body !== 'object' || Array.isArray(body)) return { error: bad('A JSON object is expected.') };
    let gameId = body.gameId;
    if (typeof gameId === 'string' && /^\d{1,16}$/.test(gameId)) gameId = Number(gameId);
    if (!Number.isSafeInteger(gameId) || gameId <= 0) return { error: bad('gameId must be a game id.') };
    const reported = body.reported;
    if (typeof reported !== 'string' || !reported.trim() || reported.length > REPORT_RULES.usernameMax) return { error: bad('reported must be a username.') };
    if (!REPORT_RULES.categories.includes(body.category)) return { error: bad(`category must be one of ${REPORT_RULES.categories.join(', ')}.`) };
    let comment = body.comment ?? '';
    if (typeof comment !== 'string') return { error: bad('comment must be text.') };
    // eslint-disable-next-line no-control-regex
    comment = comment.replace(/[\u0000-\u0008\u000b-\u001f\u007f]/g, '').trim();
    if ([...comment].length > REPORT_RULES.commentMax) return { error: bad(`comment is limited to ${REPORT_RULES.commentMax} characters.`) };
    return { value: { gameId, reported: reported.trim(), category: body.category, comment } };
}

const ACCEPTED = Object.freeze({ status: 202, body: Object.freeze({ status: 'received' }) });
const NOT_ALLOWED = Object.freeze({ status: 403, body: Object.freeze({ error: 'report_not_allowed', message: 'You can report the opponent of one of your games that ended in the last 7 days.' }) });

function outcomesOf(store, reporterId) {
    let actioned = 0, dismissed = 0;
    if (typeof store.reports.forReporter !== 'function') return { actioned, dismissed };
    try {
        for (const r of store.reports.forReporter(reporterId) || []) {
            const o = r.outcome ?? r.resolution ?? null;
            if (o === 'actioned') actioned++;
            else if (o === 'dismissed') dismissed++;
        }
    } catch { /* neutral track record */ }
    return { actioned, dismissed };
}

function gamesPlayed(store, userId) {
    try {
        let n = 0;
        for (const r of store.ratings.forUser(userId) || []) n += Number(r.games) || 0;
        return n;
    } catch { return 0; }
}

/**
 * Handles POST /api/v1/reports. The answer to an accepted report is always the same 202 (also
 * for a duplicate), and nothing in it depends on the reported account.
 * @param {{ body: *, user: object, store: object, config: object, log?: object }} ctx
 * @param {{ now?: () => number }} [deps]
 */
export function handleReport(ctx, deps = {}) {
    const store = ctx.store ?? deps.store;
    const config = ctx.config ?? deps.config;
    const log = ctx.log ?? deps.log ?? null;
    const now = (deps.now || Date.now)();
    const user = ctx.user;
    if (!user || !user.id) return { status: 401, body: { error: 'unauthorized', message: 'Log in first.' } };
    const v = validateReport(ctx.body);
    if (v.error) return v.error;
    const { gameId, reported, category, comment } = v.value;

    if (store.reports.countByReporterSince(user.id, now - DAY_MS) >= config.reportsPerDay) {
        return { status: 429, body: { error: 'report_limit', message: `At most ${config.reportsPerDay} reports per day.`, retryAfter: 3600 }, headers: { 'Retry-After': '3600' } };
    }

    // Eligibility: only from the reporter's own game, so no lookup of arbitrary usernames happens
    // (nothing can be learnt about accounts the reporter did not play).
    const game = store.games.byId(gameId);
    if (!game) return NOT_ALLOWED;
    const whiteId = Number(game.whiteId ?? game.white_id), blackId = Number(game.blackId ?? game.black_id);
    const me = Number(user.id);
    let opponentId, opponentName;
    if (me === whiteId) { opponentId = blackId; opponentName = game.blackName ?? game.black_name; }
    else if (me === blackId) { opponentId = whiteId; opponentName = game.whiteName ?? game.white_name; }
    else return NOT_ALLOWED;
    if (!opponentId || opponentId === me) return NOT_ALLOWED;
    const endedAt = Number(game.endedAt ?? game.ended_at ?? 0);
    if (!endedAt || endedAt < now - REPORT_RULES.maxAgeMs || endedAt > now + 60000) return NOT_ALLOWED;
    const want = reported.toLowerCase();
    let match = typeof opponentName === 'string' && opponentName.toLowerCase() === want;
    if (!match) {
        // The opponent may have been renamed since the game.
        try { match = String(store.users.byId(opponentId)?.username || '').toLowerCase() === want; } catch { match = false; }
    }
    if (!match) return NOT_ALLOWED;

    if (store.reports.exists(user.id, opponentId, gameId)) return ACCEPTED;

    const { actioned, dismissed } = outcomesOf(store, user.id);
    const level = readIntegrity(store, user.id).level;
    const raw = reporterWeight({ createdAt: user.createdAt, gamesPlayed: gamesPlayed(store, user.id), actioned, dismissed, level, now });
    let received = [];
    try { received = store.reports.forReported(opponentId) || []; } catch { received = []; }
    const weight = cappedWeight(raw.weight, received, now);
    const id = store.reports.create({ reporterId: user.id, reportedId: opponentId, gameId, category, comment, weight, at: now });
    log?.security?.('report.filed', { reportId: id, reporterId: user.id, reportedId: opponentId, gameId, category, weight, rawWeight: raw.weight });
    return ACCEPTED;
}

/** Evidence-friendly summary of the reports a player received. */
export function summariseReports(received, now) {
    const rows = (received || []).map((r) => ({ ...r, detail: parseMaybeJson(r.detail, r.detail) }));
    const open = rows.filter((r) => !(r.outcome ?? r.resolution));
    return { total: rows.length, open: open.length, weight30d: recentReportWeight(rows, now, 30) };
}
