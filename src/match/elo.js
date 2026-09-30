// Elo ratings per time-control category, as FIDE computes them (FIDE Rating Regulations effective
// from 1 March 2024): a pure mirror of the game's offline rating (src/game/elo.h / elo.cpp), so a
// player's online and offline numbers mean the same thing. Both are checked against the same
// vectors (test/fixtures/elo-vectors.json, written by tools/gen-elo-vectors.js; read by
// test/unit/match.elo.test.js and the game's tests/elo_tests.cpp).
//
//   expected score  PD from FIDE's table 8.1.2 (FIDE_PD_TABLE) for the rating difference D, D counting
//                   at most 400 points (8.3.1); the higher-rated player gets PD, the lower 1 - PD
//   change          K x (score - PD), rounded to the nearest point (halves away from zero), computed
//                   in whole hundredths so that the two languages agree to the last point
//   K               40 until the player has PROVISIONAL_GAMES (30) counted games in the category
//                   (below), the counted games of the unrated phase included; 20 afterwards; 10 once
//                   the player has reached 2400 (for good: the peak counts)
//   unrated phase   (8.2) a new record is unrated, with a working rating equal to INITIAL_RATING
//                   (used for pairing and as the opponent value of the other player). Each counted
//                   game adds the opponent's rating and the score; after UNRATED_GAMES (5) games
//                   Ru = Ra + dp(p), with Ra = (sum of the opponents' ratings + 2 x 1800) / (n + 2)
//                   and p = (score + 1) / (n + 2) rounded to hundredths (two hypothetical draws
//                   against 1800-rated opponents), dp from FIDE's table 8.1.1 (FIDE_DP_TABLE),
//                   rounded to the nearest point and capped at 2200. The peak becomes Ru.
//   zero score      FIDE disregards an unrated player's zero score, and their opponents' results
//                   against them (8.2.1); here, one game at a time: a game lost by an unrated player
//                   who has not scored yet in the category (no win, no draw) counts for neither
//                   player's rating (it counts in the games and the wins / draws / losses, not in
//                   the five games, the opponents' sum or the score of either side). Five losses to
//                   Stockfish at full strength would otherwise give a first rating of 2200, and an
//                   account that only loses would stay unrated for good while giving a first
//                   rating to everyone who beats it.
//   unrated opponent a rated player's game against an unrated opponent does not change the rated
//                   player's rating (8.3: only games against rated opponents count); it counts in
//                   the games and the wins / draws / losses, not in the counted games
//   counted games   the games that entered the rating (FIDE's rated games): those of the unrated
//                   phase that counted, then those against rated opponents. They set K, the
//                   provisional mark and a place on the leaderboard (PROVISIONAL_GAMES of them: a
//                   rating tested against rated players), not the games played.
//   floor           a rating never drops below 100. FIDE's list starts at 1400, which makes no sense
//                   here: players range from beginners to weak Stockfish presets rated 800.
//
// Departures from FIDE, needed by a game server:
//   * Games are rated one by one, each against the ratings before that game. FIDE rates monthly
//     periods, with ratings fixed within the period and K x games capped at 700 per period.
//   * A game between two unrated players counts for both, at the other's working rating. FIDE
//     ignores it, but then a new server (or two new names in a rated hot-seat game) could never
//     obtain a rating. The zero score still applies: such a game lost by a player who has not
//     scored yet counts for neither.
//   * FIDE's K = 40 for players under 18 does not apply (no ages here).
//
// A record is kept per player and category:
//   { rating, games, wins, draws, losses, peak, reachedSenior, rated, countedGames, unratedGames,
//     unratedOpponents, unratedHalfPoints }
// `reachedSenior` is the persisted form of the C++ `peak >= 2400` test (both are honoured; the
// working rating of an unrated record is no rating and never counts).
// `rated` is false during the unrated phase, which accumulates `unratedGames`, the sum of the
// opponents' ratings `unratedOpponents` and the score in half points `unratedHalfPoints` (the
// games counted: no zero score). `countedGames` equals `unratedGames` until the first rating.
// A record that has games but no `rated` field (stored before the unrated phase existed) is rated;
// a rated record without `countedGames` (stored before it existed) counts all its games.
//
// Categories: the official time controls come from cfg.categories ([{ id: '3+2', baseMs, incMs }]);
// any other time control is 'custom' and never rated.

/** Rating from which K drops to 10 for good. */
export const SENIOR_RATING = 2400;
/** Largest rating difference taken into account (FIDE 8.3.1). */
export const MAX_RATING_GAP = 400;
/** Ratings never drop below this. */
export const RATING_FLOOR = 100;
/** Counted games of the unrated phase before the first rating (FIDE 8.2; a zero score does not count). */
export const UNRATED_GAMES = 5;
/** Rating of the two hypothetical opponents drawn with in the first rating (FIDE 8.2). */
export const HYPOTHETICAL_OPPONENT = 1800;
/** Highest first rating (FIDE 8.2). */
export const MAX_INITIAL_RATING = 2200;
/** Defaults of the game (elo.h), used when no configuration is given. */
export const DEFAULT_INITIAL_RATING = 1500;
export const DEFAULT_PROVISIONAL_GAMES = 30;
/** Id of every non-official time control. */
export const CUSTOM_CATEGORY = 'custom';

/**
 * FIDE table 8.1.2 as published: [highest D of the row, PD of the higher-rated player in
 * hundredths]. D above 735 gives 1.00. The table is the normal distribution with a standard
 * deviation of 2000 / 7 rounded to hundredths, except at six differences where FIDE's rows keep
 * the neighbouring value (54, 343, 344, 358, 392 and 620; match.elo.test.js): FIDE applies the
 * table, so the table is what counts. The 400-point rule caps D before the lookup; the rows past
 * 400 are the published table's, and FIDE_DP_TABLE is the middle of each row (rounded down).
 */
export const FIDE_PD_TABLE = Object.freeze([
    [3, 50], [10, 51], [17, 52], [25, 53], [32, 54], [39, 55], [46, 56], [53, 57], [61, 58], [68, 59],
    [76, 60], [83, 61], [91, 62], [98, 63], [106, 64], [113, 65], [121, 66], [129, 67], [137, 68], [145, 69],
    [153, 70], [162, 71], [170, 72], [179, 73], [188, 74], [197, 75], [206, 76], [215, 77], [225, 78], [235, 79],
    [245, 80], [256, 81], [267, 82], [278, 83], [290, 84], [302, 85], [315, 86], [328, 87], [344, 88], [357, 89],
    [374, 90], [391, 91], [411, 92], [432, 93], [456, 94], [484, 95], [517, 96], [559, 97], [619, 98], [735, 99],
].map(Object.freeze));

/**
 * FIDE table 8.1.1: dp for p = 1.00, 0.99 ... 0.50 (index 0 is p = 1.00, index 50 is p = 0.50).
 * Below 0.50, dp(p) = -dp(1 - p).
 */
export const FIDE_DP_TABLE = Object.freeze([
    800, 677, 589, 538, 501, 470, 444, 422, 401, 383, 366, 351, 336, 322, 309, 296, 284, 273, 262, 251,
    240, 230, 220, 211, 202, 193, 184, 175, 166, 158, 149, 141, 133, 125, 117, 110, 102, 95, 87, 80,
    72, 65, 57, 50, 43, 36, 29, 21, 14, 7, 0,
]);

function initialRatingOf(cfg) {
    return cfg && Number.isFinite(cfg.initialRating) ? cfg.initialRating : DEFAULT_INITIAL_RATING;
}

function provisionalGamesOf(cfg) {
    return cfg && Number.isFinite(cfg.provisionalGames) ? cfg.provisionalGames : DEFAULT_PROVISIONAL_GAMES;
}

// The counted games of a possibly partial record: without the field, all the games of a rated
// record (they were all rated then), those of the unrated phase of an unrated one.
function countedGamesOf(record) {
    if (Number.isFinite(record.countedGames)) return record.countedGames;
    return isUnrated(record) ? record.unratedGames || 0 : record.games || 0;
}

// Integer division rounded to the nearest integer, halves away from zero (std::lround of a/b).
function divRound(a, b) {
    return a < 0 ? -Math.floor((-2 * a + b) / (2 * b)) : Math.floor((2 * a + b) / (2 * b));
}

/**
 * PD of FIDE table 8.1.2, in hundredths, for a rating difference `d` (its absolute value; no cap).
 * @param {number} d
 * @returns {number} 50..100
 */
export function scoringProbability(d) {
    const x = Math.abs(Math.trunc(d));
    for (const [hi, pd] of FIDE_PD_TABLE) if (x <= hi) return pd;
    return 100;
}

/**
 * dp of FIDE table 8.1.1 for a percentage score p (in hundredths, 0..100).
 * @param {number} p
 * @returns {number} -800..800
 */
export function ratingDifference(p) {
    const q = Math.min(100, Math.max(0, Math.trunc(p)));
    return q >= 50 ? FIDE_DP_TABLE[100 - q] : -FIDE_DP_TABLE[q];
}

// Expected score in hundredths: the table's PD after the 400-point rule.
function expected100(rating, opponent) {
    const d = Math.min(MAX_RATING_GAP, Math.abs(rating - opponent));
    const pd = scoringProbability(d);
    return rating >= opponent ? pd : 100 - pd;
}

/**
 * Expected score (0..1) of a player rated `rating` against `opponent` (FIDE table 8.1.2).
 * @param {number} rating
 * @param {number} opponent
 * @returns {number}
 */
export function expectedScore(rating, opponent) {
    return expected100(rating, opponent) / 100;
}

// A score (clamped to 0..1) in half points: 2 win, 1 draw, 0 loss.
function halfPoints(score) {
    return Math.round(Math.min(1, Math.max(0, score)) * 2);
}

/**
 * First rating of an unrated record after its unrated phase (FIDE 8.2): Ra + dp(p) with the two
 * hypothetical draws against 1800, rounded, capped at MAX_INITIAL_RATING, floored at RATING_FLOOR.
 * @param {{unratedGames:number, unratedOpponents:number, unratedHalfPoints:number}} rec
 * @returns {number}
 */
export function initialRating(rec) {
    const n = rec.unratedGames + 2;
    // p = (score + 1) / (n + 2) = (half points + 2) / (2 (n + 2)), in hundredths rounded half up.
    const p = divRound(100 * (rec.unratedHalfPoints + 2), 2 * n);
    const ru = divRound(rec.unratedOpponents + 2 * HYPOTHETICAL_OPPONENT + ratingDifference(p) * n, n);
    return Math.min(MAX_INITIAL_RATING, Math.max(RATING_FLOOR, ru));
}

/**
 * A fresh record: unrated, the working rating INITIAL_RATING, no game.
 * @param {object} [cfg] configuration (initialRating)
 */
export function defaultRecord(cfg) {
    const r = initialRatingOf(cfg);
    return {
        rating: r, games: 0, wins: 0, draws: 0, losses: 0, peak: r, reachedSenior: false,
        rated: false, countedGames: 0, unratedGames: 0, unratedOpponents: 0, unratedHalfPoints: 0,
    };
}

/**
 * A complete record from a possibly partial one (missing fields take the defaults; the peak is
 * at least the rating; without a `rated` field, a record with games is rated; the counted games
 * are at most the games, and those of the unrated phase while unrated). Returns a new object.
 * @param {object|null|undefined} rec
 * @param {object} [cfg]
 */
export function normalizeRecord(rec, cfg) {
    const d = defaultRecord(cfg);
    if (!rec) return d;
    const int = (v, dflt) => (Number.isFinite(v) ? Math.trunc(v) : dflt);
    const rating = int(rec.rating, d.rating);
    const peak = Math.max(int(rec.peak, rating), rating);
    const games = Math.max(0, int(rec.games, 0));
    const rated = typeof rec.rated === 'boolean' ? rec.rated : games > 0;
    const unratedGames = rated ? 0 : Math.max(0, int(rec.unratedGames, 0));
    return {
        rating,
        games,
        wins: Math.max(0, int(rec.wins, 0)),
        draws: Math.max(0, int(rec.draws, 0)),
        losses: Math.max(0, int(rec.losses, 0)),
        peak,
        reachedSenior: !!rec.reachedSenior || (rated && peak >= SENIOR_RATING),
        rated,
        countedGames: rated ? Math.min(games, Math.max(0, int(rec.countedGames, games))) : unratedGames,
        unratedGames,
        unratedOpponents: rated ? 0 : Math.max(0, int(rec.unratedOpponents, 0)),
        unratedHalfPoints: rated ? 0 : Math.max(0, int(rec.unratedHalfPoints, 0)),
    };
}

/**
 * Whether the record has no rating yet (its unrated phase).
 * @param {{rated?:boolean, games?:number}} record
 */
export function isUnrated(record) {
    if (!record) return true;
    return typeof record.rated === 'boolean' ? !record.rated : !(record.games > 0);
}

/**
 * Whether the rating is shown as provisional ("1500?"): unrated, or fewer than PROVISIONAL_GAMES
 * counted games in the category (K = 40; not on the leaderboard).
 * @param {{games:number, rated?:boolean, countedGames?:number}} record
 * @param {object} [cfg]
 */
export function isProvisional(record, cfg) {
    return isUnrated(record) || countedGamesOf(record) < provisionalGamesOf(cfg);
}

/**
 * Development coefficient of a rated player's next game: 10 once 2400 has been reached (checked
 * first, as in elo.cpp), else 40 before PROVISIONAL_GAMES counted games, else 20.
 * @param {{rating:number, games:number, countedGames?:number, peak?:number, reachedSenior?:boolean}} record
 * @param {object} [cfg]
 * @returns {10|20|40}
 */
export function kFactor(record, cfg) {
    if (record.reachedSenior || (record.peak || 0) >= SENIOR_RATING || record.rating >= SENIOR_RATING) return 10;
    return countedGamesOf(record) < provisionalGamesOf(cfg) ? 40 : 20;
}

/**
 * Rating change of the K formula for a rated record against a rated opponent, before the floor:
 * K x (score - PD), rounded.
 * @param {object} record
 * @param {number} opponent opponent's rating before the game
 * @param {number} score 1, 0.5 or 0 (rounded to the nearest half point)
 * @param {object} [cfg]
 */
export function ratingDelta(record, opponent, score, cfg) {
    return divRound(kFactor(record, cfg) * (50 * halfPoints(score) - expected100(record.rating, opponent)), 100);
}

// Whether a record scoring `half` half points in a game makes it a zero score (FIDE 8.2.1): an
// unrated record that has not scored yet in the category (no win, no draw) loses.
function zeroScore(rec, half) {
    return !rec.rated && half === 0 && rec.wins + rec.draws === 0;
}

// One side of a game (elo::applyResult against a record): returns the change and the updated
// record (a new object). `opp` is the opponent's record before the game.
function applySide(rec, opp, score, cfg) {
    const half = halfPoints(score);
    const record = { ...rec, games: rec.games + 1 };
    if (half === 2) record.wins++;
    else if (half === 0) record.losses++;
    else record.draws++;
    let k = 0;
    if (!rec.rated) {
        // The zero-score rule: a zero score is disregarded, and so are the opponent's results
        // against it (a rated opponent's rating does not move against an unrated player anyway).
        if (!zeroScore(rec, half) && !zeroScore(opp, 2 - half)) {
            record.countedGames++;
            record.unratedGames++;
            record.unratedOpponents += opp.rating;
            record.unratedHalfPoints += half;
            if (record.unratedGames >= UNRATED_GAMES) {
                record.rating = record.peak = initialRating(record);
                record.rated = true;
                record.unratedGames = record.unratedOpponents = record.unratedHalfPoints = 0;
            }
        }
    } else if (opp.rated) {
        k = kFactor(rec, cfg);
        record.countedGames++;
        record.rating = Math.max(RATING_FLOOR, rec.rating + ratingDelta(rec, opp.rating, score, cfg));
        record.peak = Math.max(rec.peak, record.rating);
    }
    record.reachedSenior = rec.reachedSenior || (record.rated && record.peak >= SENIOR_RATING);
    return {
        before: rec.rating, after: record.rating, delta: record.rating - rec.rating, k,
        expected: expectedScore(rec.rating, opp.rating), games: record.games, provisional: isProvisional(record, cfg), record,
    };
}

/**
 * Rates one game. Both changes are computed from the records before the game. The input records
 * are not modified (partial records are completed with the defaults).
 * @param {object} white White's record in the game's category
 * @param {object} black Black's record
 * @param {number} score from White's side: 1, 0.5 or 0
 * @param {object} [cfg] configuration (initialRating, provisionalGames)
 * @returns {{white: {before:number, after:number, delta:number, k:number, expected:number, games:number, provisional:boolean, record:object},
 *            black: {before:number, after:number, delta:number, k:number, expected:number, games:number, provisional:boolean, record:object}}}
 *   `before`, `after`, `games` and `provisional` are the protocol's RatingChange fields; `k` is the
 *   coefficient applied (0 when the game did not change the rating by the K formula: the unrated
 *   phase, or a rated player against an unrated one); `record` is the updated record to store.
 */
export function applyGame(white, black, score, cfg) {
    if (typeof score !== 'number' || !(score >= 0 && score <= 1)) throw new RangeError(`elo: score must be 0..1, got ${score}`);
    const w = normalizeRecord(white, cfg);
    const b = normalizeRecord(black, cfg);
    return {
        white: applySide(w, b, score, cfg),
        black: applySide(b, w, 1 - score, cfg),
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
