// ChessGame: game record with the game's automatic endings and claims (src/chess/game.cpp).

import { WHITE, BLACK, F_CHECK, F_MATE } from './tables.js';
import { Position } from './position.js';
import { GameStatus, EndReason } from './constants.js';

const NOT_OK = Object.freeze({ ok: false });

// English texts of the end reasons: assets/i18n/en.lang (reason.*) for 1..13, used in the PGN
// comment exactly like chess::Game::pgn; 20+ are the online-only reasons.
const REASON_TEXT = {
    [EndReason.Checkmate]: 'Checkmate',
    [EndReason.Resignation]: 'Resignation',
    [EndReason.Timeout]: 'Loss on time',
    [EndReason.IllegalMoves]: 'Second illegal move (forfeit)',
    [EndReason.Stalemate]: 'Stalemate',
    [EndReason.InsufficientMaterial]: 'Dead position (insufficient material)',
    [EndReason.TimeoutVsInsufficient]: 'Flag fall, but the opponent cannot checkmate',
    [EndReason.FivefoldRepetition]: 'Fivefold repetition',
    [EndReason.SeventyFiveMoves]: '75-move rule',
    [EndReason.ThreefoldClaim]: 'Threefold repetition (claimed)',
    [EndReason.FiftyMoveClaim]: '50-move rule (claimed)',
    [EndReason.Agreement]: 'Draw by agreement',
    [EndReason.IllegalMovesVsInsufficient]: 'Second illegal move, but the opponent cannot checkmate',
    [EndReason.Abandonment]: 'Abandoned (disconnected for too long)',
    [EndReason.AbandonmentVsInsufficient]: 'Abandoned, but the opponent cannot checkmate',
    [EndReason.Aborted]: 'Game aborted',
    [EndReason.NoShow]: 'Aborted: first move not played in time',
    [EndReason.Forfeit]: 'Forfeit (fair play violation)',
    [EndReason.ServerAborted]: 'Aborted by the server',
    [EndReason.BothDisconnected]: 'Aborted: both players disconnected',
};

/**
 * English description of an end reason ('' for None or unknown values).
 * @param {number} reason EndReason value
 * @returns {string}
 */
export function endReasonText(reason) {
    return REASON_TEXT[reason] ?? '';
}

// The PGN standard's Termination value. A game without a result ('*': still running, or aborted
// online, whatever the abort's reason) is "unterminated"; a flag fall is a "time forfeit" also
// when it ends in a draw (the opponent cannot mate); a second illegal move and a fair-play
// forfeit are a "rules infraction"; a game lost (or drawn) by a disconnection is "abandoned".
function terminationTag(status, reason) {
    if (status === GameStatus.Ongoing || status === GameStatus.Aborted) return 'unterminated';
    switch (reason) {
    case EndReason.Timeout:
    case EndReason.TimeoutVsInsufficient:
        return 'time forfeit';
    case EndReason.IllegalMoves:
    case EndReason.IllegalMovesVsInsufficient:
    case EndReason.Forfeit:
        return 'rules infraction';
    case EndReason.Abandonment:
    case EndReason.AbandonmentVsInsufficient:
        return 'abandoned';
    default:
        return 'normal';
    }
}

function pgnEscape(s) {
    let r = '';
    for (const c of String(s)) {
        if (c === '"' || c === '\\') r += '\\';
        r += (c === '\n' || c === '\r') ? ' ' : c;
    }
    return r;
}

// A word of a movetext comment: braces (a '}' would end the comment) and control characters dropped.
function commentText(w) {
    // eslint-disable-next-line no-control-regex
    return w.replace(/[{}\u0000-\u001f\u007f]/g, '').trim();
}

function todayUtc() {
    const d = new Date();
    const y = d.getUTCFullYear();
    if (!(y >= 1970 && y <= 9999)) return '????.??.??';
    return `${y}.${String(d.getUTCMonth() + 1).padStart(2, '0')}.${String(d.getUTCDate()).padStart(2, '0')}`;
}

function checkColor(color) {
    if (color !== WHITE && color !== BLACK) throw new TypeError(`bad colour ${color}`);
}

const STANDARD_PREFIX = 'rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq -';

/**
 * A game: positions, moves, automatic endings (checkmate, stalemate, dead position, fivefold
 * repetition, 75-move rule), claims (threefold, fifty moves), resignation, agreement, flag fall.
 * Mirrors chess::Game. `position` is the current Position: read it, never play on it directly.
 */
export class ChessGame {
    /**
     * @param {string} [startFen] custom start position (normalised like Position.fromFEN).
     * @throws {Error} when startFen is not a valid FEN.
     */
    constructor(startFen) {
        let p;
        if (startFen === undefined || startFen === null) {
            p = Position.start();
        } else {
            p = Position.fromFEN(startFen);
            if (!p) throw new Error(`invalid FEN: ${startFen}`);
        }
        /** @type {Position} */
        this.position = p;
        /** @type {number[]} u16 moves played */
        this.moves = [];
        /** GameStatus value */
        this.status = GameStatus.Ongoing;
        /** EndReason value */
        this.reason = EndReason.None;
        this._start = p.clone();
        // Repetition keys of every position of the game (index = ply).
        this._klo = new Int32Array(64);
        this._khi = new Int32Array(64);
        this._n = 0;
        this._pushKey();
        this._sanList = null;
        this._sanPos = null;
        this._updateStatus(-1);
    }

    /**
     * Replays a move list from a start position (journal recovery).
     * @param {string|undefined|null} startFen
     * @param {Iterable<number>} moves u16 moves
     * @returns {ChessGame|null} null when the FEN is invalid or a move is not playable.
     */
    static fromMoves(startFen, moves) {
        let g;
        try {
            g = new ChessGame(startFen);
        } catch {
            return null;
        }
        for (const m of moves) if (!g.play(m).ok) return null;
        return g;
    }

    /** Normalised FEN of the start position. */
    get startFen() { return this._start.fen(); }
    /** Number of moves (plies) played. */
    get ply() { return this.moves.length; }
    get isOver() { return this.status !== GameStatus.Ongoing; }

    /**
     * Plays a move.
     * @param {number} move u16
     * @returns {{ok: false} | {ok: true, flags: number, status: number, reason: number}}
     *   ok false (nothing changed) when the game is over or the move is illegal; otherwise the
     *   MoveFlag bits of the move and the status after it.
     */
    play(move) {
        if (this.status !== GameStatus.Ongoing) return NOT_OK;
        const p = this.position;
        const im = p._validate(move);
        if (im < 0) return NOT_OK;
        const flags = p._play(im);
        this.moves.push(move);
        this._pushKey();
        this._updateStatus(flags);
        return { ok: true, flags, status: this.status, reason: this.reason };
    }

    /** @returns {number} occurrences of the current position (FIDE 9.2.3 identity). */
    repetitionCount() {
        const last = this._n - 1, klo = this._klo, khi = this._khi;
        const lo = klo[last], hi = khi[last];
        // Only positions since the last irreversible move (pawn move or capture) can repeat.
        const window = this.position.halfmove;
        let count = 0;
        for (let i = last; i >= 0 && last - i <= window; i -= 2) {
            if (klo[i] === lo && khi[i] === hi) ++count;
        }
        return count;
    }

    /** @returns {boolean} the game runs and the current position occurred at least 3 times. */
    canClaimThreefold() { return this.status === GameStatus.Ongoing && this.repetitionCount() >= 3; }

    /** @returns {boolean} the game runs and the halfmove clock is at least 100. */
    canClaimFiftyMove() { return this.status === GameStatus.Ongoing && this.position.halfmove >= 100; }

    /**
     * Applies the first valid claim (threefold, then fifty moves).
     * @returns {boolean} true when the game ended by the claim.
     */
    claimDraw() {
        if (this.canClaimThreefold()) return this._finish(GameStatus.Draw, EndReason.ThreefoldClaim);
        if (this.canClaimFiftyMove()) return this._finish(GameStatus.Draw, EndReason.FiftyMoveClaim);
        return false;
    }

    /**
     * @param {number} loser colour that resigns
     * @returns {boolean} true when the game ended now.
     */
    resign(loser) {
        checkColor(loser);
        return this._finish(loser === WHITE ? GameStatus.BlackWins : GameStatus.WhiteWins, EndReason.Resignation);
    }

    /** @returns {boolean} true when the game ended now (draw by agreement). */
    agreeDraw() { return this._finish(GameStatus.Draw, EndReason.Agreement); }

    /**
     * Flag fall of `flagged`: loss on time, or a draw when the opponent cannot checkmate.
     * @param {number} flagged
     * @returns {boolean} true when the game ended now.
     */
    flagFall(flagged) {
        checkColor(flagged);
        const winner = flagged ^ 1;
        if (!this.position.canColorMate(winner)) return this._finish(GameStatus.Draw, EndReason.TimeoutVsInsufficient);
        return this._finish(winner === WHITE ? GameStatus.WhiteWins : GameStatus.BlackWins, EndReason.Timeout);
    }

    /**
     * Ends the game with any result (online reasons: abandonment, abort, forfeit...).
     * @param {number} status GameStatus.WhiteWins | BlackWins | Draw | Aborted
     * @param {number} reason EndReason value
     * @returns {boolean} true when the game ended now (false when it was already over).
     */
    end(status, reason) {
        if (!(status >= 1 && status <= 4) || (status | 0) !== status) throw new RangeError(`bad status ${status}`);
        if (!(reason >= 0 && reason <= 255) || (reason | 0) !== reason) throw new RangeError(`bad reason ${reason}`);
        return this._finish(status, reason);
    }

    /** @returns {string} "1-0", "0-1", "1/2-1/2" or "*" (ongoing or aborted). */
    resultString() {
        switch (this.status) {
        case GameStatus.WhiteWins: return '1-0';
        case GameStatus.BlackWins: return '0-1';
        case GameStatus.Draw: return '1/2-1/2';
        default: return '*';
        }
    }

    /** @returns {string[]} SAN of every move (computed lazily, cached). */
    sanMoves() {
        if (this._sanList === null) {
            this._sanList = [];
            this._sanPos = this._start.clone();
        }
        const list = this._sanList, pos = this._sanPos;
        while (list.length < this.moves.length) {
            const m = this.moves[list.length];
            list.push(pos.san(m));
            pos.play(m);
        }
        return list.slice();
    }

    /** @returns {string[]} UCI of every move. */
    uciMoves() {
        return this.moves.map((m) => this.position.uci(m));
    }

    /**
     * PGN export, like chess::Game::pgn: Seven Tag Roster, SetUp/FEN for a custom start,
     * TimeControl, Termination, then the movetext wrapped at 80 columns (lines of at most 79
     * characters) with the end reason as a comment.
     * Additions for the server's game records (GET /api/v1/games/:id/pgn): `afterResult` tags
     * written right after Result, and a comment per ply. A comment is a string (broken between its
     * words when a line is full) or an array of words each kept whole, e.g. ['[%clk 0:02:58.3]',
     * '[%emt 0:00:01.7]'] gives {[%clk 0:02:58.3] [%emt 0:00:01.7]}; a Black move right after a
     * comment repeats its number ("12... Nf6"), as the PGN export format asks.
     * @param {{event?: string, site?: string, date?: string, round?: string, white?: string,
     *   black?: string, timeControl?: string, extra?: Array<[string, string]>,
     *   afterResult?: Array<[string, string]>, comments?: Array<string|string[]|null|undefined>}} [tags]
     *   date "YYYY.MM.DD" (default: today, UTC); extra: tags written after Termination;
     *   comments[i]: the comment after ply i (none when missing or empty).
     * @returns {string}
     */
    pgn(tags = {}) {
        const result = this.resultString();
        let out = '';
        const tag = (name, value) => { out += `[${name} "${pgnEscape(value)}"]\n`; };
        tag('Event', tags.event ?? 'Casual game');
        tag('Site', tags.site ?? 'Scacelith');
        tag('Date', tags.date ? tags.date : todayUtc());
        tag('Round', tags.round ?? '-');
        tag('White', tags.white ?? '?');
        tag('Black', tags.black ?? '?');
        tag('Result', result);
        if (Array.isArray(tags.afterResult)) for (const [k, v] of tags.afterResult) tag(k, v);
        const start = this._start;
        if (start._fenPrefix() !== STANDARD_PREFIX || start.halfmove !== 0 || start.fullmove !== 1) {
            tag('SetUp', '1');
            tag('FEN', start.fen());
        }
        tag('TimeControl', tags.timeControl ? tags.timeControl : '?');
        tag('Termination', terminationTag(this.status, this.reason));
        if (Array.isArray(tags.extra)) for (const [k, v] of tags.extra) tag(k, v);
        out += '\n';

        let line = '';
        const emit = (tok) => {
            if (line.length && line.length + 1 + tok.length > 79) {
                out += line + '\n';
                line = '';
            }
            if (line.length) line += ' ';
            line += tok;
        };
        // A comment: its words, '{' before the first and '}' after the last (no break inside a word).
        const comment = (c) => {
            const words = typeof c === 'string' ? c.split(' ') : Array.isArray(c) ? c.map(String) : [];
            const kept = words.map(commentText).filter((w) => w.length > 0);
            if (!kept.length) return false;
            kept[0] = `{${kept[0]}`;
            kept[kept.length - 1] += '}';
            for (const w of kept) emit(w);
            return true;
        };
        const comments = Array.isArray(tags.comments) ? tags.comments : null;
        const san = this.sanMoves();
        let moveNo = start.fullmove;
        let side = start.side;
        let afterComment = false;
        for (let i = 0; i < san.length; i++) {
            if (side === WHITE) emit(`${moveNo}.`);
            else if (i === 0 || afterComment) emit(`${moveNo}...`);
            emit(san[i]);
            afterComment = comments !== null && comments[i] !== undefined && comments[i] !== null && comment(comments[i]);
            if (side === BLACK) ++moveNo;
            side ^= 1;
        }
        if (this.status !== GameStatus.Ongoing) {
            const text = endReasonText(this.reason);
            if (text) for (const w of `{${text}}`.split(' ')) emit(w);
        }
        emit(result);
        out += line + '\n';
        return out;
    }

    // ---- internals ----------------------------------------------------------------------------

    _pushKey() {
        if (this._n === this._klo.length) {
            const lo = new Int32Array(this._n * 2), hi = new Int32Array(this._n * 2);
            lo.set(this._klo);
            hi.set(this._khi);
            this._klo = lo;
            this._khi = hi;
        }
        this._klo[this._n] = this.position._lo;
        this._khi[this._n] = this.position._hi;
        this._n++;
    }

    _finish(status, reason) {
        if (this.status !== GameStatus.Ongoing) return false;
        this.status = status;
        this.reason = reason;
        return true;
    }

    /** chess::Game::updateStatus. flags: MoveFlag bits of the move just played, -1 at the start. */
    _updateStatus(flags) {
        const p = this.position;
        const check = flags >= 0 ? (flags & F_CHECK) !== 0 : p.inCheck();
        const noMove = (check && flags >= 0) ? (flags & F_MATE) !== 0 : !p.hasLegalMove();
        if (noMove) {
            if (check) this._finish(p.side === WHITE ? GameStatus.BlackWins : GameStatus.WhiteWins, EndReason.Checkmate);
            else this._finish(GameStatus.Draw, EndReason.Stalemate);
            return;  // checkmate takes precedence over the 75-move rule (9.6.2)
        }
        if (p.hasInsufficientMaterial()) {
            this._finish(GameStatus.Draw, EndReason.InsufficientMaterial);
            return;
        }
        if (this.repetitionCount() >= 5) {
            this._finish(GameStatus.Draw, EndReason.FivefoldRepetition);
            return;
        }
        if (p.halfmove >= 150) this._finish(GameStatus.Draw, EndReason.SeventyFiveMoves);
    }
}
