// Public read API (DESIGN.md 5.9, owner: store): player profiles, their recent games, game
// records and the leaderboards. Public data only: never an e-mail address, a session, a sanction,
// an anomaly or an integrity level. Accounts that were deleted (anonymized) have no profile; their
// games stay readable under the anonymized name.
//
//   GET /api/v1/players/:username            profile: ratings per category, game counts
//   GET /api/v1/players/:username/games      recent games, newest first (?before=<gameId>&limit<=50)
//   GET /api/v1/games/:id                    one game: players, result, UCI moves with times, PGN tags
//   GET /api/v1/games/:id/pgn                the same game as a PGN file (application/x-chess-pgn)
//   GET /api/v1/leaderboard?category=3+2     top established players of an official category
//
// Every route but the leaderboard takes an optional session (auth 'optional'; a token that is
// sent must be valid). When the session's player played the game, GET /games/:id adds `you`
// ('white' | 'black') and `reportable` (whether POST /reports would take a report of the opponent
// for this game now: anticheat/reports.js canReport, the route's own rules); every other answer
// is the public one, unchanged. The game routes and the two player routes share the
// 'public_read' limit: 60 requests per minute, all four together, per account when the request
// carries a session, else per client (an IPv4 address or an IPv6 /64). The leaderboard has no
// limit of its own (cached 10 s per category).
//
// The PGN (gamePgn): the game replayed with the server's chess module (ChessGame.fromMoves, SAN),
// tags in this order: Event (as the JSON's pgn.Event), Site (SERVER_PUBLIC_HOST), Date (UTC start),
// Round "-", White, Black, Result ("*" for an aborted game), UTCDate, UTCTime (start, HH:MM:SS),
// WhiteElo / BlackElo (ratings at the start, "-" when unknown), WhiteRatingDiff / BlackRatingDiff
// ("+8", "-8", "+0"; in every rated game, "+0" when the rules left a rating unchanged; never in
// a casual, custom or aborted game), TimeControl (seconds, "180+2"),
// Termination (PGN standard values: normal, time forfeit (a flag fall, also drawn), abandoned,
// rules infraction, unterminated (aborted)), PlyCount, ScacelithGameId (the decimal id). Each move
// carries {[%clk h:mm:ss.f] [%emt h:mm:ss.f]}: the mover's clock after the move and the time
// charged for it, in tenths of a second (truncated), each left out when the record has no value;
// after the last move the end reason in words, then the result; lines under 80 columns, '\n'
// line endings. The game's PGN reader (src/chess/pgn.cpp) reads [%clk] / [%emt] back into
// clockMs / elapsedMs: tools/gen-pgn-fixtures.js writes tests/data/server-pgn/*.pgn for its
// tests. Stored moves that do not replay to the stored ending answer 500 (logged).
//
// Paths are registered with the '/api/v1' prefix (deps.prefix overrides it, e.g. '' when the router
// adds the prefix itself). Handlers are synchronous (the store is) and return { status, body } (the
// PGN: { status, text, contentType, headers }).

import { enums } from '../../protocol/schema.js';
import { ChessGame } from '../../chess/index.js';
import { canReport } from '../../anticheat/reports.js';

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

/**
 * The summary of a stored game in the lists (GET /players/:username/games, GET /account/games):
 * id, category, rated, timeControl, white / black { name, rating, ratingAfter, ratingDiff }, color
 * (the side of `userId`; undefined without it), status, reason, result, termination, plies,
 * startedAt, endedAt.
 * @param {object} g  store game record (summary or full)
 * @param {number} [userId]
 * @returns {object}
 */
export function gameSummary(g, userId) {
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

/** The Event of a game: "<SERVER_NAME> rated 3+2", "<SERVER_NAME> casual custom". */
function eventName(g, config) { return `${config.serverName} ${g.rated ? 'rated' : 'casual'} ${g.category}`; }

const pad2 = (n) => String(n).padStart(2, '0');

function pgnTime(t) {
    const d = new Date(t);
    return `${pad2(d.getUTCHours())}:${pad2(d.getUTCMinutes())}:${pad2(d.getUTCSeconds())}`;
}

/**
 * A clock value of a [%clk] / [%emt] command: h:mm:ss.f, tenths of a second, truncated.
 * @param {number} ms
 * @returns {string}
 */
export function pgnClock(ms) {
    const tenths = Math.floor(Math.max(0, Number(ms) || 0) / 100);
    const s = Math.floor(tenths / 10);
    return `${Math.floor(s / 3600)}:${pad2(Math.floor(s / 60) % 60)}:${pad2(s % 60)}.${tenths % 10}`;
}

function ratingDiffText(c) {
    const d = Math.round(c.after - c.before);
    return d < 0 ? String(d) : `+${d}`;
}

/** The content type of a PGN download. */
export const PGN_CONTENT_TYPE = 'application/x-chess-pgn; charset=utf-8';

/**
 * The PGN text of a stored game (header of this file): one game, '\n' line endings.
 * @param {object} g  full store game record (store.games.byId: moves, spentMs, clockMs)
 * @param {{ serverName: string, serverPublicHost: string }} config
 * @returns {string|null} null when the stored moves are not legal from the start position, or
 *   lead to another ending than the stored one
 */
export function gamePgn(g, config) {
    const cg = ChessGame.fromMoves(undefined, g.moves);
    if (!cg) return null;
    // Resignation, flag fall, abandonment, abort... are not in the moves: the stored ending is
    // applied unless the moves ended the game by themselves, then they must agree.
    if (!cg.isOver) {
        try { cg.end(g.status, g.reason); } catch { return null; }
    } else if (cg.status !== g.status || cg.reason !== g.reason) {
        return null;
    }
    const afterResult = [
        ['UTCDate', pgnDate(g.startedAt)],
        ['UTCTime', pgnTime(g.startedAt)],
        ['WhiteElo', g.whiteRating === null || g.whiteRating === undefined ? '-' : String(g.whiteRating)],
        ['BlackElo', g.blackRating === null || g.blackRating === undefined ? '-' : String(g.blackRating)],
    ];
    if (g.ratingChanges) {
        afterResult.push(['WhiteRatingDiff', ratingDiffText(g.ratingChanges.white)], ['BlackRatingDiff', ratingDiffText(g.ratingChanges.black)]);
    }
    const comments = new Array(cg.ply);
    for (let i = 0; i < cg.ply; i++) {
        const words = [];
        if (g.clockMs && i < g.clockMs.length) words.push(`[%clk ${pgnClock(g.clockMs[i])}]`);
        if (g.spentMs && i < g.spentMs.length) words.push(`[%emt ${pgnClock(g.spentMs[i])}]`);
        comments[i] = words;
    }
    return cg.pgn({
        event: eventName(g, config),
        site: config.serverPublicHost,
        date: pgnDate(g.startedAt),
        round: '-',
        white: g.whiteName,
        black: g.blackName,
        timeControl: tcText(g.baseMs, g.incMs),
        afterResult,
        extra: [['PlyCount', String(cg.ply)], ['ScacelithGameId', String(g.id)]],
        comments,
    });
}

/**
 * Registers the public read routes.
 * @param {object} router  router.get(path, handler, { auth, rate })
 * @param {object} deps
 * @param {object} deps.store
 * @param {object} deps.config
 * @param {object} [deps.log]
 * @param {string} [deps.prefix='/api/v1']
 * @param {() => number} [deps.now]  clock of `reportable` when the request context has no `now`
 */
export function register(router, { store, config, log, prefix = '/api/v1', now = Date.now }) {
    const categories = config.categories.map((c) => c.id);
    const provisionalGames = config.provisionalGames;
    const order = new Map(categories.map((c, i) => [c, i]));
    const rate = { key: 'public_read', limit: 60, windowMs: 60000, by: 'user' };
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
                category: r.category, rating: r.rating, provisional: r.provisional, games: r.games,
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
                games: list.map((g) => gameSummary(g, user.id)),
                next: list.length === limit ? list[list.length - 1].id : null,
            },
        };
    }

    function findGame(ctx) {
        const raw = ctx.params && ctx.params.id;
        if (!ID_RE.test(String(raw)) || !Number.isSafeInteger(Number(raw))) return { res: error(400, 'invalid_game_id', 'Invalid game id.') };
        const g = store.games.byId(Number(raw));
        if (!g) return { res: error(404, 'not_found', 'No such game.') };
        return { g };
    }

    // The session's player, when the request carries a valid session (auth 'optional').
    const viewerId = (ctx) => (ctx.user ? Number(ctx.user.userId ?? ctx.user.id) : null);

    function game(ctx) {
        const { g, res } = findGame(ctx);
        if (res) return res;
        const moves = new Array(g.moves.length);
        for (let i = 0; i < g.moves.length; i++) {
            moves[i] = {
                uci: uciOf(g.moves[i]),
                spentMs: i < g.spentMs.length ? g.spentMs[i] : null,
                clockMs: i < g.clockMs.length ? g.clockMs[i] : null,
            };
        }
        const s = gameSummary(g);
        delete s.color;
        const white = s.white, black = s.black;
        const body = {
            ...s,
            baseMs: g.baseMs,
            incMs: g.incMs,
            statusName: STATUS_NAME[g.status] ?? 'Unknown',
            rematchOf: g.rematchOf,
            moves,
            pgn: {
                Event: eventName(g, config),
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
        };
        const me = viewerId(ctx);
        if (me && (g.whiteId === me || g.blackId === me)) {
            body.you = g.whiteId === me ? 'white' : 'black';
            body.reportable = canReport(store, config, me, g, typeof ctx.now === 'number' ? ctx.now : now());
        }
        return { status: 200, body };
    }

    function gamePgnFile(ctx) {
        const { g, res } = findGame(ctx);
        if (res) return res;
        const text = gamePgn(g, config);
        if (text === null) {
            if (log) log.error('stored game cannot be replayed', { gameId: g.id, plies: g.moves.length, status: g.status, reason: g.reason });
            return error(500, 'internal_error', 'The moves stored for this game cannot be replayed.');
        }
        return {
            status: 200,
            text,
            contentType: PGN_CONTENT_TYPE,
            headers: { 'Content-Disposition': `attachment; filename="scacelith-${g.id}.pgn"` },
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

    router.get(`${prefix}/players/:username`, wrap('profile', profile), { auth: 'optional', rate });
    router.get(`${prefix}/players/:username/games`, wrap('games', gamesOf), { auth: 'optional', rate });
    router.get(`${prefix}/games/:id`, wrap('game', game), { auth: 'optional', rate });
    router.get(`${prefix}/games/:id/pgn`, wrap('pgn', gamePgnFile), { auth: 'optional', rate });
    router.get(`${prefix}/leaderboard`, wrap('leaderboard', leaderboard), { auth: 'none' });
}
