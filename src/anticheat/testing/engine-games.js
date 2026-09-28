// Game generators driven by a real UCI engine, used to validate the analysis end to end (tests
// and calibration only). They produce game records in the store's shape (u16 moves, spentMs).
//
//   assisted side  plays the engine's best move at `assistDepth`, relayed with a think time
//                  unrelated to the position (uniform 2-5 s): an engine user;
//   plausible side picks uniformly among the engine's top-N MultiPV moves at a low depth that
//                  lose at most `plausibleCp` against the best (a weak human stand-in), and
//                  thinks longer when there are several candidate moves (lognormal noise).

import { uciToMove } from '../analysis/moves.js';
import { lineCp } from '../analysis/analyzer.js';
import { gauss } from './synthetic.js';

/**
 * Chooses the next move of a side. Returns { move, spentMs } or null when the side has no move.
 * @param {object} engine   UciEngine
 * @param {string[]} uci    moves so far
 * @param {{ kind: 'assisted'|'plausible', r: () => number, assistDepth?: number, depth?: number, topN?: number, plausibleCp?: number }} style
 */
export async function chooseMove(engine, uci, style) {
    const r = style.r;
    if (style.kind === 'assisted') {
        const res = await engine.analyse(uci, { depth: style.assistDepth ?? 12, multiPv: 1 });
        if (!res.bestmove) return null;
        return { move: res.bestmove, spentMs: Math.round(2000 + 3000 * r()) };
    }
    const res = await engine.analyse(uci, { depth: style.depth ?? 6, multiPv: style.topN ?? 4 });
    if (!res.bestmove || !res.lines.length) return null;
    const best = lineCp(res.lines[0]);
    const cands = res.lines.filter((l) => l.move && best - lineCp(l) <= (style.plausibleCp ?? 150));
    const pick = cands.length ? cands[Math.floor(r() * cands.length)] : res.lines[0];
    const good = res.lines.filter((l) => best - lineCp(l) <= 50).length;
    // Humans think longer with several good candidates; lognormal spread (CV about 0.8).
    const spentMs = Math.round(3000 * (0.6 + 0.7 * good) * Math.exp(0.7 * gauss(r)));
    return { move: pick.move, spentMs };
}

/**
 * Plays a game from a random opening (openingPlies uniform picks among the top 5 at depth 4).
 * @param {object} engine
 * @param {{ r: () => number, white: object, black: object, maxPlies?: number, openingPlies?: number }} o
 * @returns {Promise<{ moves: number[], spentMs: number[], uci: string[] }>}
 */
export async function playGame(engine, { r, white, black, maxPlies = 70, openingPlies = 8 }) {
    await engine.newGame();
    const uci = [], spent = [];
    for (let p = 0; p < maxPlies; p++) {
        let m;
        if (p < openingPlies) {
            const res = await engine.analyse(uci, { depth: 4, multiPv: 5 });
            if (!res.bestmove) break;
            const opts = res.lines.filter((l) => l.move);
            m = { move: opts[Math.floor(r() * opts.length)].move, spentMs: p < 2 ? 0 : Math.round(1500 * Math.exp(0.5 * gauss(r))) };
        } else {
            m = await chooseMove(engine, uci, { ...(p % 2 === 0 ? white : black), r });
            if (!m) break;
        }
        uci.push(m.move);
        spent.push(m.spentMs);
    }
    return { moves: uci.map(uciToMove), spentMs: spent, uci };
}
