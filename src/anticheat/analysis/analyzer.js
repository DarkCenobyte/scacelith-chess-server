// Engine analysis of one finished game: per-move evaluations turned into per-player features.
//
// Positions analysed (the game always starts from the initial position):
//   * deep pass: every position from ply 2*OPENING_PLIES_PER_SIDE to the final one, depth
//     ANALYSIS_DEPTH_DEEP, MultiPV 3, one `ucinewgame` for the game and the positions in game
//     order (the hash carries over, as in any game analysis);
//   * shallow pass: the positions actually scored, depth ANALYSIS_DEPTH_FAST, MultiPV 1, with
//     the hash cleared before each search so the answer is really a shallow engine's choice and
//     does not inherit the deep pass.
// Single thread + fixed depth + controlled hash make the analysis reproducible: running it again
// with the same engine version and network (any build variant) gives the same numbers (evidence a
// moderator can re-check). Every record names its analysis profile (analysisProfile below), and
// only records of the same profile are ever compared or pooled.
//
// A move is scored unless:
//   * it is among the first OPENING_PLIES_PER_SIDE moves of its side (opening theory);
//   * the position before it is already decided: |best eval| > DECIDED_CP from the mover's view
//     (accuracy in a won or lost position says nothing);
//   * it is forced (MultiPV returns a single line: one legal move).
// For each scored move: win-probability loss and move accuracy (lichess' logistic model),
// centipawn loss capped at CP_LOSS_CAP, deep-best match (T1), shallow-best match, top-3 match,
// complexity (moves within GOOD_MARGIN_CP of the best among the top 3, gap best-second, whether
// the shallow and deep choices differ) and the clock time the server charged (spentMs).

import { moveToUci, numberList } from './moves.js';
import { winPercent, moveAccuracy, mean, harmonicMean, spearman, coefficientOfVariation, clamp } from './stats.js';

export const ANALYSIS = Object.freeze({
    version: 1,
    openingPliesPerSide: 8,
    decidedCp: 600,
    goodMarginCp: 50,
    cpLossCap: 1000,
    evalClampCp: 1000,
    mateCp: 10000,
    minTimedMoves: 8,       // time features need at least this many scored moves with a clock time
    multiPv: 3,
});

/**
 * Analysis profile of a record: what must be equal for two games' features to be comparable.
 * Another engine or version, network, depth, hash size or version of these rules moves the
 * numbers (on the same games, Stockfish 19 at 9/15 finds the human stand-ins' ACPL 26 % higher
 * than Stockfish 16 at 10/18, and scores 16 % fewer moves), so the population statistics are
 * kept per profile and a player is scored on the games of one profile (scoring.js). The CPU does
 * not matter: one thread at a fixed depth gives the same numbers with every build variant.
 * @param {{ engine: string, net?: string|null, depthFast: number, depthDeep: number, hashMb?: number|null }} p
 * @returns {string} e.g. 'Stockfish 19; nn-1a298aa575a0.nnue; depth 9/15; hash 32; analysis 1'
 */
export function analysisProfile({ engine, net = null, depthFast, depthDeep, hashMb = null }) {
    const parts = [String(engine || 'engine')];
    if (net) parts.push(String(net));
    parts.push(`depth ${depthFast}/${depthDeep}`);
    if (hashMb) parts.push(`hash ${hashMb}`);
    parts.push(`analysis ${ANALYSIS.version}`);
    // '|' separates the parts of the population statistics' keys.
    return parts.join('; ').replaceAll('|', '/');
}

/**
 * Centipawn value of an engine line from the side to move's point of view (mate in n: +/-
 * (MATE_CP - 10 n); mate 0 means the side to move is mated).
 * @param {{cp:number|null, mate:number|null}} line
 */
export function lineCp(line) {
    if (!line) return null;
    if (line.mate !== null && line.mate !== undefined) {
        if (line.mate === 0) return -ANALYSIS.mateCp;
        return line.mate > 0 ? ANALYSIS.mateCp - 10 * line.mate : -ANALYSIS.mateCp - 10 * line.mate;
    }
    return line.cp;
}

const clampEval = (cp) => clamp(cp, -ANALYSIS.evalClampCp, ANALYSIS.evalClampCp);

/**
 * Runs the engine over a game.
 * @param {object} engine   UciEngine-like: newGame(), clearHash(), analyse(moves, {depth, multiPv}), name
 * @param {string[]} uci    the game's moves in UCI
 * @param {{ depthFast: number, depthDeep: number, fromPly?: number, onProgress?: Function }} o
 * @returns {Promise<{ deep: Map<number, object>, fast: Map<number, object> }>}
 */
export async function analysePositions(engine, uci, { depthFast, depthDeep, fromPly = 2 * ANALYSIS.openingPliesPerSide }) {
    const deep = new Map(), fast = new Map();
    const n = uci.length;
    if (n <= fromPly) return { deep, fast };
    await engine.newGame();
    for (let p = fromPly; p <= n; p++) {
        deep.set(p, await engine.analyse(uci.slice(0, p), { depth: depthDeep, multiPv: ANALYSIS.multiPv }));
    }
    for (let p = fromPly; p < n; p++) {
        const d = deep.get(p);
        if (!d || d.lines.length <= 1) continue;                               // forced (or no data)
        if (Math.abs(lineCp(d.lines[0])) > ANALYSIS.decidedCp) continue;       // decided
        await engine.clearHash();
        fast.set(p, await engine.analyse(uci.slice(0, p), { depth: depthFast, multiPv: 1 }));
    }
    return { deep, fast };
}

function emptySide() {
    return { n: 0, accuracy: null, acpl: null, wpl: null, t1Deep: null, t1Fast: null, top3: null,
        nComplex: 0, t1Complex: null, timeCorr: null, timeCv: null, nTimed: 0, meanSpentMs: null,
        skipped: { opening: 0, forced: 0, decided: 0, missing: 0 }, moves: [] };
}

/**
 * Turns position analyses into per-player features (pure; see the file header for the rules).
 * @param {{ moves: number[], spentMs: number[] }} game   normalised record
 * @param {{ deep: Map, fast: Map }} pos
 * @returns {{ white: object, black: object }}
 */
export function computeFeatures(game, pos) {
    const sides = [emptySide(), emptySide()];
    const acc = [[], []], cpl = [[], []], wpl = [[], []];
    const t1d = [0, 0], t1f = [0, 0], top3 = [0, 0], cx = [0, 0], cxHit = [0, 0];
    const times = [[], []], cplx = [[], []];
    const moves = game.moves;
    for (let p = 0; p < moves.length; p++) {
        const s = p & 1;
        const side = sides[s];
        if (p < 2 * ANALYSIS.openingPliesPerSide) { side.skipped.opening++; continue; }
        const before = pos.deep.get(p);
        if (!before || !before.lines.length) { side.skipped.missing++; continue; }
        if (before.lines.length === 1) { side.skipped.forced++; continue; }
        const bestCp = lineCp(before.lines[0]);
        if (Math.abs(bestCp) > ANALYSIS.decidedCp) { side.skipped.decided++; continue; }
        const played = moveToUci(moves[p]);
        const playedLine = before.lines.find((l) => l.move === played);
        let playedCp;
        if (playedLine) playedCp = lineCp(playedLine);
        else {
            const after = pos.deep.get(p + 1);
            playedCp = after && after.lines.length ? -lineCp(after.lines[0]) : null;
        }
        const fast = pos.fast.get(p);
        if (playedCp === null || !fast) { side.skipped.missing++; continue; }

        const loss = clamp(clampEval(bestCp) - clampEval(playedCp), 0, ANALYSIS.cpLossCap);
        const wBefore = winPercent(bestCp), wAfter = winPercent(playedCp);
        const a = moveAccuracy(wBefore, wAfter);
        const deepBest = before.lines[0].move || before.bestmove;
        const isT1 = played === deepBest;
        const isFast = played === fast.bestmove;
        const inTop3 = !!playedLine;
        let nGood = 0;
        for (const l of before.lines) if (bestCp - lineCp(l) <= ANALYSIS.goodMarginCp) nGood++;
        const gap = Math.min(ANALYSIS.cpLossCap, bestCp - lineCp(before.lines[1]));
        const complex = nGood >= 2;
        const tricky = fast.bestmove !== deepBest;
        // Continuous complexity used for the time correlation: several good moves, a shallow
        // search that is misled, and a small gap between the two best moves all make a decision
        // harder for a human.
        const complexity = (nGood - 1) + (tricky ? 1 : 0) + (1 - Math.min(gap, 200) / 200);

        acc[s].push(a); cpl[s].push(loss); wpl[s].push(Math.max(0, wBefore - wAfter));
        if (isT1) t1d[s]++;
        if (isFast) t1f[s]++;
        if (inTop3) top3[s]++;
        if (complex) { cx[s]++; if (isT1) cxHit[s]++; }
        const spent = game.spentMs[p];
        if (Number.isFinite(spent)) { times[s].push(spent); cplx[s].push(complexity); }
        side.moves.push([p, Math.round(loss), (isT1 ? 1 : 0) | (isFast ? 2 : 0) | (inTop3 ? 4 : 0) | (complex ? 8 : 0) | (tricky ? 16 : 0),
            nGood, Number.isFinite(spent) ? spent : -1]);
    }
    for (let s = 0; s < 2; s++) {
        const side = sides[s];
        const n = acc[s].length;
        side.n = n;
        if (!n) continue;
        side.accuracy = round((mean(acc[s]) + harmonicMean(acc[s])) / 2, 2);
        side.acpl = round(mean(cpl[s]), 1);
        side.wpl = round(mean(wpl[s]), 2);
        side.t1Deep = round(t1d[s] / n, 4);
        side.t1Fast = round(t1f[s] / n, 4);
        side.top3 = round(top3[s] / n, 4);
        side.nComplex = cx[s];
        side.t1Complex = cx[s] ? round(cxHit[s] / cx[s], 4) : null;
        side.nTimed = times[s].length;
        if (times[s].length >= ANALYSIS.minTimedMoves) {
            const r = spearman(times[s], cplx[s]);
            side.timeCorr = r === null ? null : round(r, 4);
            const cv = coefficientOfVariation(times[s]);
            side.timeCv = cv === null ? null : round(cv, 4);
        }
        side.meanSpentMs = times[s].length ? Math.round(mean(times[s])) : null;
    }
    return { white: sides[0], black: sides[1] };
}

function round(x, d) {
    const f = 10 ** d;
    return Math.round(x * f) / f;
}

/**
 * Normalises a game record from the store (typed arrays, BLOBs, snake_case fallbacks).
 * @param {object} rec
 */
export function normaliseGame(rec) {
    const g = rec || {};
    return {
        id: g.id ?? g.gameId ?? g.game_id,
        category: g.category ?? 'custom',
        rated: !!g.rated,
        baseMs: g.baseMs ?? g.base_ms ?? 0,
        incMs: g.incMs ?? g.inc_ms ?? 0,
        whiteId: g.whiteId ?? g.white_id,
        blackId: g.blackId ?? g.black_id,
        whiteRating: g.whiteRating ?? g.white_rating ?? null,
        blackRating: g.blackRating ?? g.black_rating ?? null,
        endedAt: g.endedAt ?? g.ended_at ?? null,
        moves: numberList(g.moves, 2),
        spentMs: numberList(g.spentMs ?? g.spent_ms, 4),
    };
}

/**
 * Analyses a finished game and returns the features record stored by store.analysis.complete.
 * @param {object} engine   UciEngine-like (name, and when known net and hashMb: the profile)
 * @param {object} record   game record (store.games.byId or the finishBatch record)
 * @param {{ depthFast: number, depthDeep: number, extra?: { white?: object, black?: object } }} o
 *        extra: per-side context added to the features (rating games count at analysis time...)
 */
export async function analyseGame(engine, record, { depthFast, depthDeep, extra = {} }) {
    const game = normaliseGame(record);
    const uci = game.moves.map(moveToUci);
    const started = Date.now();
    const pos = await analysePositions(engine, uci, { depthFast, depthDeep });
    const f = computeFeatures(game, pos);
    // The engine learnt its name and network when it started (UciEngine.start).
    const name = engine.name || 'engine', net = engine.net ?? null, hashMb = engine.hashMb ?? null;
    return {
        v: ANALYSIS.version,
        gameId: game.id,
        category: game.category,
        baseMs: game.baseMs,
        incMs: game.incMs,
        plies: game.moves.length,
        endedAt: game.endedAt,
        engine: name, net, hashMb,
        depthFast, depthDeep,
        profile: analysisProfile({ engine: name, net, depthFast, depthDeep, hashMb }),
        analysedAt: Date.now(),
        durationMs: Date.now() - started,
        white: { userId: game.whiteId, rating: game.whiteRating, ...(extra.white || {}), ...f.white },
        black: { userId: game.blackId, rating: game.blackRating, ...(extra.black || {}), ...f.black },
    };
}
