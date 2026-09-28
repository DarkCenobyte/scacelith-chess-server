// Elo ratings per time-control category: a pure mirror of the game's offline rating
// (src/game/elo.h / elo.cpp), so a player's online and offline numbers mean the same thing.
//
//   expected score  E = 1 / (1 + 10^((Ro - Rp) / 400)), the difference counting at most 400
//                   points (FIDE rating regulations 8.3.1)
//   change          lround(K * (score - E)), score 1 / 0.5 / 0; rounding half away from zero
//                   like std::lround
//   K               40 while provisional (fewer than PROVISIONAL_GAMES games in the category),
//                   20 afterwards, 10 once the player has reached 2400 (for good: the peak
//                   counts, and the senior check comes first, exactly as elo.cpp)
//   floor           a rating never drops below 100
//
// A record is kept per player and category:
//   { rating, games, wins, draws, losses, peak, reachedSenior }
// `reachedSenior` is the persisted form of the C++ `peak >= 2400` test (both are honoured).
//
// Categories: the official time controls come from cfg.categories ([{ id: '3+2', baseMs, incMs }]);
// any other time control is 'custom' and never rated.

/** Rating from which K drops to 10 for good. */
export const SENIOR_RATING = 2400;
/** Largest rating difference taken into account (FIDE). */
export const MAX_RATING_GAP = 400;
/** Ratings never drop below this. */
export const RATING_FLOOR = 100;
/** Defaults of the game (elo.h), used when no configuration is given. */
export const DEFAULT_INITIAL_RATING = 1500;
export const DEFAULT_PROVISIONAL_GAMES = 30;
/** Id of every non-official time control. */
export const CUSTOM_CATEGORY = 'custom';

function initialRatingOf(cfg) {
    return cfg && Number.isFinite(cfg.initialRating) ? cfg.initialRating : DEFAULT_INITIAL_RATING;
}

function provisionalGamesOf(cfg) {
    return cfg && Number.isFinite(cfg.provisionalGames) ? cfg.provisionalGames : DEFAULT_PROVISIONAL_GAMES;
}

// std::lround: nearest integer, halves away from zero (Math.round rounds -2.5 to -2).
function lround(x) {
    return x < 0 ? -Math.round(-x) : Math.round(x);
}

/**
 * Expected score (0..1) of a player rated `rating` against `opponent`.
 * @param {number} rating
 * @param {number} opponent
 * @returns {number}
 */
export function expectedScore(rating, opponent) {
    let gap = opponent - rating;
    if (gap > MAX_RATING_GAP) gap = MAX_RATING_GAP;
    else if (gap < -MAX_RATING_GAP) gap = -MAX_RATING_GAP;
    return 1 / (1 + Math.pow(10, gap / 400));
}

/**
 * A fresh record: INITIAL_RATING, no game.
 * @param {object} [cfg] configuration (initialRating)
 * @returns {{rating:number, games:number, wins:number, draws:number, losses:number, peak:number, reachedSenior:boolean}}
 */
export function defaultRecord(cfg) {
    const r = initialRatingOf(cfg);
    return { rating: r, games: 0, wins: 0, draws: 0, losses: 0, peak: r, reachedSenior: r >= SENIOR_RATING };
}

/**
 * A complete record from a possibly partial one (missing fields take the defaults; the peak is
 * at least the rating). Returns a new object.
 * @param {object|null|undefined} rec
 * @param {object} [cfg]
 */
export function normalizeRecord(rec, cfg) {
    const d = defaultRecord(cfg);
    if (!rec) return d;
    const int = (v, dflt) => (Number.isFinite(v) ? Math.trunc(v) : dflt);
    const rating = int(rec.rating, d.rating);
    const peak = Math.max(int(rec.peak, rating), rating);
    return {
        rating,
        games: Math.max(0, int(rec.games, 0)),
        wins: Math.max(0, int(rec.wins, 0)),
        draws: Math.max(0, int(rec.draws, 0)),
        losses: Math.max(0, int(rec.losses, 0)),
        peak,
        reachedSenior: !!rec.reachedSenior || peak >= SENIOR_RATING,
    };
}

/**
 * Whether the rating is still provisional (fewer than PROVISIONAL_GAMES games in the category).
 * @param {{games:number}} record
 * @param {object} [cfg]
 */
export function isProvisional(record, cfg) {
    return (record ? record.games || 0 : 0) < provisionalGamesOf(cfg);
}

/**
 * Development coefficient for the next game: 10 once 2400 has been reached (checked first, as
 * in elo.cpp), else 40 while provisional, else 20.
 * @param {{rating:number, games:number, peak?:number, reachedSenior?:boolean}} record
 * @param {object} [cfg]
 * @returns {10|20|40}
 */
export function kFactor(record, cfg) {
    if (record.reachedSenior || (record.peak || 0) >= SENIOR_RATING || record.rating >= SENIOR_RATING) return 10;
    return record.games < provisionalGamesOf(cfg) ? 40 : 20;
}

/**
 * Rating change a result would bring, without applying it.
 * @param {object} record
 * @param {number} opponent opponent's rating before the game
 * @param {number} score 1, 0.5 or 0 (clamped to 0..1)
 * @param {object} [cfg]
 */
export function ratingDelta(record, opponent, score, cfg) {
    const s = Math.min(1, Math.max(0, score));
    return lround(kFactor(record, cfg) * (s - expectedScore(record.rating, opponent)));
}

// One side of a game (elo::applyResult): returns the change and the updated record (new object).
function applySide(rec, opponent, score, cfg) {
    const k = kFactor(rec, cfg);
    const expected = expectedScore(rec.rating, opponent);
    const after = Math.max(RATING_FLOOR, rec.rating + ratingDelta(rec, opponent, score, cfg));
    const record = { ...rec, rating: after, games: rec.games + 1 };
    if (score > 0.75) record.wins++;
    else if (score < 0.25) record.losses++;
    else record.draws++;
    record.peak = Math.max(rec.peak, after);
    record.reachedSenior = rec.reachedSenior || record.peak >= SENIOR_RATING;
    return {
        before: rec.rating, after, delta: after - rec.rating, k, expected,
        games: record.games, provisional: isProvisional(record, cfg), record,
    };
}

/**
 * Rates one game. Both changes are computed from the ratings before the game. The input records
 * are not modified (partial records are completed with the defaults).
 * @param {object} white White's record in the game's category
 * @param {object} black Black's record
 * @param {number} score from White's side: 1, 0.5 or 0
 * @param {object} [cfg] configuration (initialRating, provisionalGames)
 * @returns {{white: {before:number, after:number, delta:number, k:number, expected:number, games:number, provisional:boolean, record:object},
 *            black: {before:number, after:number, delta:number, k:number, expected:number, games:number, provisional:boolean, record:object}}}
 *   `before`, `after`, `games` and `provisional` are the protocol's RatingChange fields;
 *   `record` is the updated record to store.
 */
export function applyGame(white, black, score, cfg) {
    if (typeof score !== 'number' || !(score >= 0 && score <= 1)) throw new RangeError(`elo: score must be 0..1, got ${score}`);
    const w = normalizeRecord(white, cfg);
    const b = normalizeRecord(black, cfg);
    return {
        white: applySide(w, b.rating, score, cfg),
        black: applySide(b, w.rating, 1 - score, cfg),
    };
}

// cfg -> Map('baseMs:incMs' -> id) and Map(id -> category), built once per configuration object.
const categoryIndex = new WeakMap();
function indexOf(cfg) {
    let idx = categoryIndex.get(cfg);
    if (!idx) {
        idx = { byTc: new Map(), byId: new Map() };
        for (const c of (cfg && cfg.categories) || []) {
            idx.byTc.set(`${c.baseMs}:${c.incMs}`, c.id);
            idx.byId.set(c.id, c);
        }
        if (cfg && typeof cfg === 'object') categoryIndex.set(cfg, idx);
    }
    return idx;
}

/**
 * Category of a time control: the official id ('3+2') or 'custom'.
 * @param {number} baseMs
 * @param {number} incMs
 * @param {object} cfg configuration (categories)
 * @returns {string}
 */
export function categoryOf(baseMs, incMs, cfg) {
    return indexOf(cfg).byTc.get(`${baseMs}:${incMs}`) || CUSTOM_CATEGORY;
}

/**
 * The official category with this id ({ id, baseMs, incMs }), or null (unknown id, 'custom').
 * @param {string} id
 * @param {object} cfg
 * @returns {{id:string, baseMs:number, incMs:number}|null}
 */
export function parseCategory(id, cfg) {
    if (typeof id !== 'string') return null;
    return indexOf(cfg).byId.get(id) || null;
}

/**
 * Whether the id names an official (rated) category.
 * @param {string} id
 * @param {object} cfg
 */
export function isOfficialCategory(id, cfg) {
    return parseCategory(id, cfg) !== null;
}
