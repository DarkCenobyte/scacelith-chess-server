// Player reports: eligibility rules, reporter credibility, anti-brigading cap and review
// priority. Pure functions plus the request handler used by src/http/routes/reports.js.
//
// A report never changes a player's integrity level; it only raises the review priority that
// moderators see (bin/admin.js), in proportion to the reporter's credibility, and asks for the
// engine analysis of the reported game ahead of the ordinary games (a low-credibility report only
// at the priority of a suspicion signal). Many reports from
// new or low-credibility accounts do not add up: the weight stored for a report is capped so
// that the reports received by one player in 24 hours sum to at most DAILY_WEIGHT_CAP, and
// low-credibility reports to at most LOW_CRED_DAILY_CAP.
//
// The eligibility rules (reportableOpponent, the REPORTS_PER_DAY quota, the duplicate check) are
// shared with GET /api/v1/games/:id, whose `reportable` (canReport) tells the game's players
// whether a report would be taken now. The reporter's account age is read from the account when
// the session's user object does not carry it (the HTTP server's does not; before, every report
// sent through the API was weighed as from an account created that instant).

import { DAY_MS, readIntegrity } from './util.js';

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
    return cappedWeightOfSums(raw, today, todayLow);
}

// cappedWeight from the sums of the weights received in the last 24 hours: all of them (today),
// and those below REPORT_RULES.lowCredibility (todayLow).
function cappedWeightOfSums(raw, today, todayLow) {
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

// When the reporter's account was created: the session's user object (src/http/server.js) does
// not carry it, the account does.
function accountCreatedAt(store, user) {
    if (Number(user.createdAt) > 0) return Number(user.createdAt);
    try { return Number(store.users.byId(user.id)?.createdAt) || undefined; } catch { return undefined; }
}

/**
 * The opponent `userId` may report for `game` (a stored game record): the user played it, against
 * another account, and it ended within the last REPORT_RULES.maxAgeMs (not in the future). Only
 * the reporter's own games qualify, so no lookup of arbitrary usernames happens (nothing can be
 * learnt about accounts the reporter did not play).
 * @param {object|null} game  store.games.byId record (camelCase or column names)
 * @param {number} userId
 * @param {number} now
 * @returns {{ opponentId: number, opponentName: string|undefined } | null}
 */
export function reportableOpponent(game, userId, now) {
    if (!game) return null;
    const whiteId = Number(game.whiteId ?? game.white_id), blackId = Number(game.blackId ?? game.black_id);
    const me = Number(userId);
    let opponentId, opponentName;
    if (me === whiteId) { opponentId = blackId; opponentName = game.blackName ?? game.black_name; }
    else if (me === blackId) { opponentId = whiteId; opponentName = game.whiteName ?? game.white_name; }
    else return null;
    if (!opponentId || opponentId === me) return null;
    const endedAt = Number(game.endedAt ?? game.ended_at ?? 0);
    if (!endedAt || endedAt < now - REPORT_RULES.maxAgeMs || endedAt > now + 60000) return null;
    return { opponentId, opponentName };
}

/** Whether the reporter already filed REPORTS_PER_DAY reports in the last 24 hours. */
function reportQuotaReached(store, config, userId, now) {
    return store.reports.countByReporterSince(userId, now - DAY_MS) >= config.reportsPerDay;
}

/**
 * Whether POST /api/v1/reports would take a new report from `userId` against the opponent of
 * `game` now (GET /api/v1/games/:id `reportable`): the game qualifies (reportableOpponent), the
 * reporter is under REPORTS_PER_DAY, and has not reported that opponent for that game yet (a
 * second report gets the same 202 but changes nothing). Any store failure answers false.
 * @param {object} store
 * @param {{ reportsPerDay: number }} config
 * @param {number} userId
 * @param {object|null} game  store.games.byId record
 * @param {number} now
 * @returns {boolean}
 */
export function canReport(store, config, userId, game, now) {
    const o = reportableOpponent(game, userId, now);
    if (!o) return false;
    try {
        return !reportQuotaReached(store, config, userId, now) && !store.reports.exists(userId, o.opponentId, Number(game.id));
    } catch { return false; }
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

    if (reportQuotaReached(store, config, user.id, now)) {
        return { status: 429, body: { error: 'report_limit', message: `At most ${config.reportsPerDay} reports per day.`, retryAfter: 3600 }, headers: { 'Retry-After': '3600' } };
    }

    const opponent = reportableOpponent(store.games.byId(gameId), user.id, now);
    if (!opponent) return NOT_ALLOWED;
    const { opponentId, opponentName } = opponent;
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
    const raw = reporterWeight({ createdAt: accountCreatedAt(store, user), gamesPlayed: gamesPlayed(store, user.id), actioned, dismissed, level, now });
    // The sums over every report of the last 24 hours when the store has them (forReported returns
    // only the newest reports: past them, the cap would start over).
    let weight;
    if (typeof store.reports.weightSince === 'function') {
        let today = 0, todayLow = 0;
        try { ({ total: today, low: todayLow } = store.reports.weightSince(opponentId, now - DAY_MS, REPORT_RULES.lowCredibility)); } catch { /* none counted */ }
        weight = cappedWeightOfSums(raw.weight, today, todayLow);
    } else {
        let received = [];
        try { received = store.reports.forReported(opponentId) || []; } catch { received = []; }
        weight = cappedWeight(raw.weight, received, now);
    }
    let id;
    try {
        id = store.reports.create({ reporterId: user.id, reportedId: opponentId, gameId, category, comment, weight, at: now });
    } catch (e) {
        // The same report filed at the same moment through another shard (separate connections:
        // both passed exists()); the UNIQUE index kept one, and a duplicate gets the same answer.
        if (e?.code === 'duplicate') return ACCEPTED;
        throw e;
    }
    log?.security?.('report.filed', { reportId: id, reporterId: user.id, reportedId: opponentId, gameId, category, weight, rawWeight: raw.weight });
    // The reported game is analysed ahead of the ordinary ones, even when the queue policy left
    // it out (ANALYSIS_QUEUE_MAX, ANALYSIS_SAMPLE_RATE): at 'report' priority when the report is
    // credible (its stored weight reaches lowCredibility), otherwise at 'signal' priority, beside
    // the statistical suspicion signals and not ahead of them, so that new or brigading accounts
    // cannot push the games of statistically suspected players back. An abuse report is not
    // about how the game was played. This only produces evidence for moderators: it never
    // changes a level by itself.
    if (category !== 'abuse' && typeof store.analysis?.request === 'function') {
        const reason = weight >= REPORT_RULES.lowCredibility ? 'report' : 'signal';
        try { store.analysis.request(gameId, reason, now); } catch (e) { log?.warn?.('analysis request of a reported game failed', { err: e, gameId }); }
    }
    return ACCEPTED;
}
