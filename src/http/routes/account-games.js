// GET /api/v1/account/games: the signed-in player's own game history (docs/API.md), newest first,
// with filters and the number of games matching them.
//
//   GET /api/v1/account/games?before=<gameId>&limit=20&category=3%2B2&rated=true&result=win
//     -> 200 { games: [summary], next: <gameId> | null, total }
//
// Query (each optional; an empty value counts as absent, unknown parameters are ignored):
//   before    a game id: only games older than it (exclusive cursor: pass the previous `next`)
//   limit     1..50 (default 20; a larger value is capped at 50, like /players/:username/games)
//   category  an official category id (RATED_CATEGORIES, "3+2"; a '+' sent unencoded decodes to a
//             space and is accepted) or "custom"
//   rated     "true" | "false"
//   result    "win" | "loss" | "draw", from the player's side; aborted games only appear
//             without a result filter
// Errors: 400 invalid_cursor | invalid_limit | invalid_filter (with `field`), 401 without a valid
// session, 429 rate_limited (account_games: 60 per minute per player).
//
// A summary is the object of GET /players/:username/games (players.js gameSummary: id, category,
// rated, timeControl, white / black { name, rating, ratingAfter, ratingDiff }, color, status,
// reason, result, termination, plies, startedAt, endedAt) plus baseMs, incMs and `outcome`
// ('win' | 'loss' | 'draw' | 'aborted', from the player's side): historySummary, which the data
// export reuses. `next` is the id of the page's last game when older games match the filter
// (null on the last page); `total` counts every game matching the filter, all pages together.
// The store reads the player's rows through the per-colour indexes (store.games.listForUser /
// countForUser). Deleted accounts never get here: their sessions are gone.

import { enums } from '../../protocol/schema.js';
import { ID_RE, gameSummary } from './players.js';

const { GameStatus } = enums;
const LIST_MAX = 50;
const LIST_DEFAULT = 20;
const RESULTS = ['win', 'loss', 'draw'];

/**
 * The game's outcome from the side of `userId`.
 * @param {{ status: number, whiteId: number }} g
 * @param {number} userId
 * @returns {'win'|'loss'|'draw'|'aborted'}
 */
export function outcomeFor(g, userId) {
    switch (g.status) {
    case GameStatus.Draw: return 'draw';
    case GameStatus.WhiteWins: return g.whiteId === userId ? 'win' : 'loss';
    case GameStatus.BlackWins: return g.whiteId === userId ? 'loss' : 'win';
    default: return 'aborted';
    }
}

/**
 * The summary of a game in the player's history (and the data export): gameSummary plus baseMs,
 * incMs and outcome.
 * @param {object} g  store game record
 * @param {number} userId  the player whose history it is
 * @returns {object}
 */
export function historySummary(g, userId) {
    return { ...gameSummary(g, userId), baseMs: g.baseMs, incMs: g.incMs, outcome: outcomeFor(g, userId) };
}

function param(query, name) {
    if (!query) return undefined;
    const v = typeof query.get === 'function' ? query.get(name) : query[name];
    if (Array.isArray(v)) return v[0];
    return v === null || v === '' ? undefined : v;
}

function invalid(code, message, field) { return { status: 400, body: { error: code, message, field } }; }

/**
 * Parses the query of GET /account/games.
 * @param {object|URLSearchParams} query
 * @param {Set<string>} categories  the accepted category ids ('custom' included)
 * @returns {{ before: number|null, limit: number, filter: { category?: string, rated?: boolean,
 *   result?: string } } | { error: object }}
 */
export function parseHistoryQuery(query, categories) {
    let before = null;
    const beforeRaw = param(query, 'before');
    if (beforeRaw !== undefined) {
        if (!ID_RE.test(String(beforeRaw)) || !Number.isSafeInteger(Number(beforeRaw))) return { error: invalid('invalid_cursor', 'before must be a game id.', 'before') };
        before = Number(beforeRaw);
    }
    let limit = LIST_DEFAULT;
    const limitRaw = param(query, 'limit');
    if (limitRaw !== undefined) {
        if (!/^\d{1,3}$/.test(String(limitRaw)) || +limitRaw < 1) return { error: invalid('invalid_limit', `limit must be 1 to ${LIST_MAX}.`, 'limit') };
        limit = Math.min(LIST_MAX, +limitRaw);
    }
    const filter = {};
    let category = param(query, 'category');
    if (category !== undefined) {
        category = String(category).trim().replace(/ /g, '+');
        if (!categories.has(category)) return { error: invalid('invalid_filter', `category must be one of: ${[...categories].join(', ')}.`, 'category') };
        filter.category = category;
    }
    const rated = param(query, 'rated');
    if (rated !== undefined) {
        if (rated !== 'true' && rated !== 'false') return { error: invalid('invalid_filter', 'rated must be true or false.', 'rated') };
        filter.rated = rated === 'true';
    }
    const result = param(query, 'result');
    if (result !== undefined) {
        if (!RESULTS.includes(result)) return { error: invalid('invalid_filter', `result must be one of: ${RESULTS.join(', ')}.`, 'result') };
        filter.result = result;
    }
    return { before, limit, filter };
}

/**
 * Registers GET /api/v1/account/games.
 * @param {import('../router.js').Router} router
 * @param {{ store: object, config: object, log?: object }} deps
 */
export function register(router, { store, config, log }) {
    const categories = new Set([...config.categories.map((c) => c.id), 'custom']);

    function history(ctx) {
        const q = parseHistoryQuery(ctx.query, categories);
        if (q.error) return q.error;
        const userId = Number(ctx.user.userId ?? ctx.user.id);
        // One game more than the page tells whether another page follows.
        const list = store.games.listForUser(userId, { before: q.before, limit: q.limit + 1, ...q.filter });
        const page = list.length > q.limit ? list.slice(0, q.limit) : list;
        return {
            status: 200,
            body: {
                games: page.map((g) => historySummary(g, userId)),
                next: list.length > q.limit ? page[page.length - 1].id : null,
                total: store.games.countForUser(userId, q.filter),
            },
        };
    }

    router.get('/account/games', (ctx) => {
        try {
            return history(ctx);
        } catch (e) {
            if (log) log.error('account games failed', { err: e });
            if (e && e.code === 'busy') return { status: 503, body: { error: 'busy', message: 'Try again shortly.', retryAfter: 1 } };
            throw e;
        }
    }, { auth: 'required', rate: { key: 'account_games', limit: 60, windowMs: 60000, by: 'user' } });
}
