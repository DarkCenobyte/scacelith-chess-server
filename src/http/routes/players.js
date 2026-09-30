// Public read API (DESIGN.md 5.9, owner: store): player profiles, their recent games, game
// records and the leaderboards. Public data only: never an e-mail address, a session, a sanction,
// an anomaly or an integrity level. Accounts that were deleted (anonymized) have no profile; their
// games stay readable under the anonymized name.
//
//   GET /api/v1/players/:username            profile: ratings per category, game counts
//   GET /api/v1/players/:username/games      recent games, newest first (?before=<gameId>&limit<=50)
//   GET /api/v1/games/:id                    one game: players, result, UCI moves with times, PGN tags
//   GET /api/v1/leaderboard?category=3+2     top established players of an official category
//
// Paths are registered with the '/api/v1' prefix (deps.prefix overrides it, e.g. '' when the router
// adds the prefix itself). Handlers are synchronous (the store is) and return { status, body }.

import { enums } from '../../protocol/schema.js';

const USERNAME_RE = /^[A-Za-z0-9_.-]{2,24}$/;   // widest charset/length the server ever allows
const ID_RE = /^[1-9][0-9]{0,15}$/;
const LIST_MAX = 50;
const LIST_DEFAULT = 20;
const BOARD_MAX = 100;
const BOARD_CACHE_MS = 10000;
const PROMO = ['', '', 'n', 'b', 'r', 'q', '', ''];

const STATUS_NAME = Object.fromEntries(Object.entries(enums.GameStatus).map(([k, v]) => [v, k]));
const REASON_NAME = Object.fromEntries(Object.entries(enums.EndReason).map(([k, v]) => [v, k]));
const RESULT = { [enums.GameStatus.WhiteWins]: '1-0', [enums.GameStatus.BlackWins]: '0-1', [enums.GameStatus.Draw]: '1/2-1/2' };

function error(status, code, message) { return { status, body: { error: code, message } }; }

function param(query, name) {
    if (!query) return undefined;
    const v = typeof query.get === 'function' ? query.get(name) : query[name];
    if (Array.isArray(v)) return v[0];
    return v === null ? undefined : v;
}

function decode(s) {
    if (typeof s !== 'string') return null;
    if (!s.includes('%')) return s;
    try { return decodeURIComponent(s); } catch { return null; }
}

function squareName(sq) { return String.fromCharCode(97 + (sq & 7)) + String(1 + (sq >> 3)); }

/**
 * UCI text of a protocol u16 move (from | to << 6 | promo << 12), without chess rules.
 * @param {number} m
 * @returns {string}
 */
export function uciOf(m) {
    return squareName(m & 63) + squareName((m >> 6) & 63) + PROMO[(m >> 12) & 7];
}

function tcText(baseMs, incMs) { return `${Math.round(baseMs / 1000)}+${Math.round(incMs / 1000)}`; }

function pgnDate(t) {
    const d = new Date(t);
    return `${d.getUTCFullYear()}.${String(d.getUTCMonth() + 1).padStart(2, '0')}.${String(d.getUTCDate()).padStart(2, '0')}`;
}

function side(g, color) {
    const white = color === 'white';
    const c = g.ratingChanges ? g.ratingChanges[color] : null;
    return {
        name: white ? g.whiteName : g.blackName,
        rating: white ? g.whiteRating : g.blackRating,
        ratingAfter: c ? c.after : null,
        ratingDiff: c ? c.after - c.before : null,
    };
}

function summary(g, userId) {
    return {
        id: g.id,
        category: g.category,
        rated: g.rated,
        timeControl: tcText(g.baseMs, g.incMs),
        white: side(g, 'white'),
        black: side(g, 'black'),
        color: userId === undefined ? undefined : g.whiteId === userId ? 'white' : 'black',
        status: g.status,
        reason: g.reason,
        result: RESULT[g.status] ?? '*',
        termination: REASON_NAME[g.reason] ?? 'Unknown',
        plies: g.plyCount,
        startedAt: g.startedAt,
        endedAt: g.endedAt,
    };
}

/**
 * Registers the public read routes.
 * @param {object} router  router.get(path, handler, { auth, rate })
 * @param {object} deps
 * @param {object} deps.store
 * @param {object} deps.config
 * @param {object} [deps.log]
 * @param {string} [deps.prefix='/api/v1']
 */
export function register(router, { store, config, log, prefix = '/api/v1' }) {
    const categories = config.categories.map((c) => c.id);
    const provisionalGames = config.provisionalGames;
    const order = new Map(categories.map((c, i) => [c, i]));
    const rate = { key: 'public_read', limit: 60, windowMs: 60000 };
    const boardCache = new Map();

    function findPlayer(ctx) {
        const name = decode(ctx.params && ctx.params.username);
        if (!name || !USERNAME_RE.test(name)) return { res: error(400, 'invalid_username', 'Invalid username.') };
        const user = store.users.byUsername(name);
        if (!user || user.status !== 'active') return { res: error(404, 'not_found', 'No such player.') };
        return { user };
    }

    function profile(ctx) {
        const { user, res } = findPlayer(ctx);
        if (res) return res;
        const rows = store.ratings.forUser(user.id)
            .filter((r) => order.has(r.category))
            .sort((a, b) => order.get(a.category) - order.get(b.category));
        let wins = 0, draws = 0, losses = 0, rated = 0;
        const ratings = rows.map((r) => {
            wins += r.wins; draws += r.draws; losses += r.losses; rated += r.games;
            return {
                category: r.category, rating: r.rating, provisional: r.rated === false || r.games < provisionalGames, games: r.games,
                wins: r.wins, draws: r.draws, losses: r.losses, peak: r.peak,
            };
        });
        return {
            status: 200,
            body: {
                username: user.username,
                createdAt: user.createdAt,
                ratings,
                games: { total: store.games.countForUser(user.id), rated, wins, draws, losses },
            },
        };
    }

    function gamesOf(ctx) {
        const { user, res } = findPlayer(ctx);
        if (res) return res;
        const beforeRaw = param(ctx.query, 'before');
        const limitRaw = param(ctx.query, 'limit');
        let before;
        if (beforeRaw !== undefined && beforeRaw !== '') {
            if (!ID_RE.test(String(beforeRaw)) || !Number.isSafeInteger(Number(beforeRaw))) return error(400, 'invalid_cursor', 'before must be a game id.');
            before = Number(beforeRaw);
        }
        let limit = LIST_DEFAULT;
        if (limitRaw !== undefined && limitRaw !== '') {
            if (!/^\d{1,3}$/.test(String(limitRaw)) || +limitRaw < 1) return error(400, 'invalid_limit', `limit must be 1 to ${LIST_MAX}.`);
            limit = Math.min(LIST_MAX, +limitRaw);
        }
        const list = store.games.recentForUser(user.id, limit, before);
        return {
            status: 200,
            body: {
                username: user.username,
                games: list.map((g) => summary(g, user.id)),
                next: list.length === limit ? list[list.length - 1].id : null,
            },
        };
    }

    function game(ctx) {
        const raw = ctx.params && ctx.params.id;
        if (!ID_RE.test(String(raw)) || !Number.isSafeInteger(Number(raw))) return error(400, 'invalid_game_id', 'Invalid game id.');
        const g = store.games.byId(Number(raw));
        if (!g) return error(404, 'not_found', 'No such game.');
        const moves = new Array(g.moves.length);
        for (let i = 0; i < g.moves.length; i++) {
            moves[i] = {
                uci: uciOf(g.moves[i]),
                spentMs: i < g.spentMs.length ? g.spentMs[i] : null,
                clockMs: i < g.clockMs.length ? g.clockMs[i] : null,
            };
        }
        const s = summary(g);
        delete s.color;
        const white = s.white, black = s.black;
        return {
            status: 200,
            body: {
                ...s,
                baseMs: g.baseMs,
                incMs: g.incMs,
                statusName: STATUS_NAME[g.status] ?? 'Unknown',
                rematchOf: g.rematchOf,
                moves,
                pgn: {
                    Event: `${config.serverName} ${g.rated ? 'rated' : 'casual'} ${g.category}`,
                    Site: config.serverPublicHost,
                    Date: pgnDate(g.startedAt),
                    Round: '-',
                    White: white.name,
                    Black: black.name,
                    Result: s.result,
                    WhiteElo: white.rating ?? '-',
                    BlackElo: black.rating ?? '-',
                    TimeControl: tcText(g.baseMs, g.incMs),
                    Termination: s.termination,
                    PlyCount: g.plyCount,
                },
            },
        };
    }

    function leaderboard(ctx) {
        let category = param(ctx.query, 'category');
        // "3+2" in a query string decodes to "3 2" (form encoding): accept both spellings.
        if (typeof category === 'string') category = category.trim().replace(/ /g, '+');
        if (!category || !order.has(category)) {
            return error(400, 'invalid_category', `category must be one of: ${categories.join(', ')}.`);
        }
        let limit = BOARD_MAX;
        const limitRaw = param(ctx.query, 'limit');
        if (limitRaw !== undefined && limitRaw !== '') {
            if (!/^\d{1,3}$/.test(String(limitRaw)) || +limitRaw < 1) return error(400, 'invalid_limit', `limit must be 1 to ${BOARD_MAX}.`);
            limit = Math.min(BOARD_MAX, +limitRaw);
        }
        const now = Date.now();
        let cached = boardCache.get(category);
        if (!cached || now - cached.at > BOARD_CACHE_MS) {
            const rows = store.ratings.leaderboard(category, BOARD_MAX, provisionalGames);
            cached = {
                at: now,
                players: rows.map((r, i) => ({
                    rank: i + 1, username: r.username, rating: r.rating, games: r.games, wins: r.wins, draws: r.draws,
                    losses: r.losses, peak: r.peak,
                })),
            };
            boardCache.set(category, cached);
        }
        return {
            status: 200,
            body: { category, minGames: provisionalGames, updatedAt: cached.at, players: cached.players.slice(0, limit) },
        };
    }

    const wrap = (name, fn) => (ctx) => {
        try {
            return fn(ctx);
        } catch (e) {
            if (log) log.error(`${name} failed`, { err: e });
            if (e && e.code === 'busy') return { status: 503, body: { error: 'busy', message: 'Try again shortly.', retryAfter: 1 } };
            throw e;
        }
    };

    router.get(`${prefix}/players/:username`, wrap('profile', profile), { auth: 'none' });
    router.get(`${prefix}/players/:username/games`, wrap('games', gamesOf), { auth: 'none', rate });
    router.get(`${prefix}/games/:id`, wrap('game', game), { auth: 'none', rate });
    router.get(`${prefix}/leaderboard`, wrap('leaderboard', leaderboard), { auth: 'none' });
}
