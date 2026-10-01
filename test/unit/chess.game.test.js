// ChessGame: automatic endings, claims, resignation, agreement, flag fall, online endings, PGN
// (the cases of the game's tests/chess_tests.cpp).
import { test } from 'node:test';
import assert from 'node:assert/strict';

import { ChessGame, GameStatus, EndReason, MoveFlag, WHITE, BLACK } from '../../src/chess/index.js';

/** Plays UCI moves; fails the test when one is refused. */
function line(g, ucis) {
    for (const u of ucis.split(' ')) {
        const m = g.position.parseUCI(u);
        assert.notEqual(m, -1, `${u} legal in ${g.position.fen()}`);
        const r = g.play(m);
        assert.equal(r.ok, true, `play ${u}`);
    }
    return g;
}

const CYCLE = 'g1f3 g8f6 f3g1 f6g8';

test('checkmate ends the game and is recorded', () => {
    const g = line(new ChessGame(), 'f2f3 e7e5 g2g4');
    const r = g.play(g.position.parseUCI('d8h4'));
    assert.deepEqual(r, { ok: true, flags: MoveFlag.Check | MoveFlag.Mate, status: GameStatus.BlackWins, reason: EndReason.Checkmate });
    assert.equal(g.status, GameStatus.BlackWins);
    assert.equal(g.reason, EndReason.Checkmate);
    assert.equal(g.isOver, true);
    assert.equal(g.resultString(), '0-1');
    assert.deepEqual(g.sanMoves(), ['f3', 'e5', 'g4', 'Qh4#']);
    assert.deepEqual(g.uciMoves(), ['f2f3', 'e7e5', 'g2g4', 'd8h4']);
    assert.equal(g.ply, 4);
    // Game over: nothing else is accepted.
    assert.deepEqual(g.play(g.position.parseUCI('a2a3')), { ok: false });
    assert.equal(g.moves.length, 4);
    assert.equal(g.resign(WHITE), false);
    assert.equal(g.agreeDraw(), false);
    assert.equal(g.flagFall(BLACK), false);
    assert.equal(g.claimDraw(), false);
    assert.equal(g.end(GameStatus.Aborted, EndReason.ServerAborted), false);
    assert.equal(g.status, GameStatus.BlackWins);
    // Illegal moves change nothing.
    const h = new ChessGame();
    assert.deepEqual(h.play(12 | (36 << 6)), { ok: false });    // e2e5
    assert.deepEqual(h.play(-1), { ok: false });
    assert.deepEqual(h.play('796'), { ok: false });
    assert.equal(h.moves.length, 0);
    assert.equal(h.status, GameStatus.Ongoing);
    assert.equal(h.reason, EndReason.None);
});

test('stalemate', () => {
    const g = new ChessGame('7k/8/6K1/8/8/8/8/5Q2 w - - 0 1');
    const r = g.play(g.position.parseUCI('f1f7'));
    assert.equal(r.flags, 0);
    assert.equal(r.status, GameStatus.Draw);
    assert.equal(r.reason, EndReason.Stalemate);
    assert.equal(g.resultString(), '1/2-1/2');
    // A start position that is already over.
    assert.equal(new ChessGame('7k/5Q2/6K1/8/8/8/8/8 b - - 0 1').reason, EndReason.Stalemate);
    const mated = new ChessGame('rnb1kbnr/pppp1ppp/8/4p3/6Pq/5P2/PPPPP2P/RNBQKBNR w KQkq - 1 3');
    assert.equal(mated.status, GameStatus.BlackWins);
    assert.equal(mated.reason, EndReason.Checkmate);
});

test('dead positions (insufficient material)', () => {
    const cases = [
        ['4k3/8/8/8/8/8/3q4/4K3 w - - 0 1', 'e1d2'],       // K v K
        ['4k3/8/8/8/8/8/3n4/2B1K3 w - - 0 1', 'c1d2'],     // K+B v K
        ['4k3/8/8/8/8/8/3b4/1N2K3 w - - 0 1', 'b1d2'],     // K+N v K
        ['4k3/8/8/8/8/2b5/3n4/2B1K3 w - - 0 1', 'c1d2'],   // K+B v K+B, same colour
    ];
    for (const [fen, u] of cases) {
        const g = new ChessGame(fen);
        assert.equal(g.status, GameStatus.Ongoing);
        const r = g.play(g.position.parseUCI(u));
        assert.equal(r.flags & MoveFlag.Capture, MoveFlag.Capture);
        assert.equal(r.status, GameStatus.Draw, fen);
        assert.equal(r.reason, EndReason.InsufficientMaterial, fen);
    }
    assert.equal(new ChessGame('4k3/8/8/8/8/8/8/4K3 w - - 0 1').reason, EndReason.InsufficientMaterial);
    assert.equal(new ChessGame('4k3/8/8/8/8/8/2b5/2B1K3 w - - 0 1').status, GameStatus.Ongoing);  // opposite bishops
    assert.equal(new ChessGame('4k3/8/8/8/8/8/2n5/2N1K3 w - - 0 1').status, GameStatus.Ongoing);  // N v N
});

test('threefold claim, fivefold repetition', () => {
    const g = new ChessGame();
    assert.equal(g.repetitionCount(), 1);
    line(g, CYCLE);
    assert.equal(g.repetitionCount(), 2);
    assert.equal(g.canClaimThreefold(), false);
    line(g, CYCLE);
    assert.equal(g.repetitionCount(), 3);
    assert.equal(g.canClaimThreefold(), true);
    assert.equal(g.status, GameStatus.Ongoing);       // threefold must be claimed
    line(g, CYCLE);
    assert.equal(g.repetitionCount(), 4);
    assert.equal(g.status, GameStatus.Ongoing);
    line(g, 'g1f3 g8f6 f3g1');
    assert.equal(g.status, GameStatus.Ongoing);
    const r = g.play(g.position.parseUCI('f6g8'));
    assert.equal(r.status, GameStatus.Draw);
    assert.equal(r.reason, EndReason.FivefoldRepetition);

    // Claiming the threefold repetition.
    const c = line(new ChessGame(), CYCLE);
    assert.equal(c.claimDraw(), false);               // nothing to claim yet
    assert.equal(c.status, GameStatus.Ongoing);
    line(c, CYCLE);
    assert.equal(c.claimDraw(), true);
    assert.equal(c.status, GameStatus.Draw);
    assert.equal(c.reason, EndReason.ThreefoldClaim);
    assert.equal(c.canClaimThreefold(), false);       // game over

    // An en passant possibility makes positions different (FIDE 9.2.3.2).
    const e = line(new ChessGame(), 'e2e4 b8c6 e4e5 d7d5');
    line(e, 'g1f3 c6b8 f3g1 b8c6');
    assert.equal(e.repetitionCount(), 1);
    line(e, 'g1f3 c6b8 f3g1 b8c6');
    assert.equal(e.repetitionCount(), 2);

    // Castling rights make positions different.
    const k = line(new ChessGame(), 'g1f3 g8f6 h1g1 h8g8 g1h1 g8h8');
    assert.equal(k.repetitionCount(), 1);
    line(k, 'h1g1 h8g8 g1h1 g8h8');
    assert.equal(k.repetitionCount(), 2);

    // A double push without a possible en passant capture does not change the position identity.
    const d = line(new ChessGame(), 'e2e4 e7e5 g1f3 g8f6 f3g1 f6g8');
    assert.equal(d.repetitionCount(), 2);
    assert.equal(d.position.digest(), line(new ChessGame(), 'e2e4 e7e5').position.digest());
});

test('75-move rule, fifty-move claim', () => {
    let g = new ChessGame('7k/8/6K1/8/8/8/8/R7 w - - 149 100');
    let r = g.play(g.position.parseUCI('a1a2'));
    assert.equal(r.status, GameStatus.Draw);
    assert.equal(r.reason, EndReason.SeventyFiveMoves);
    // Checkmate on the 75th move takes precedence.
    g = new ChessGame('7k/8/6K1/8/8/8/8/R7 w - - 149 100');
    r = g.play(g.position.parseUCI('a1a8'));
    assert.equal(r.status, GameStatus.WhiteWins);
    assert.equal(r.reason, EndReason.Checkmate);
    // Already over at the start.
    assert.equal(new ChessGame('7k/8/6K1/8/8/8/8/R7 w - - 150 100').reason, EndReason.SeventyFiveMoves);
    // Fifty-move claim.
    g = new ChessGame('7k/8/6K1/8/8/8/8/R7 w - - 98 60');
    assert.equal(g.canClaimFiftyMove(), false);
    line(g, 'a1a2');
    assert.equal(g.canClaimFiftyMove(), false);
    line(g, 'h8g8');
    assert.equal(g.canClaimFiftyMove(), true);
    assert.equal(g.status, GameStatus.Ongoing);
    assert.equal(g.claimDraw(), true);
    assert.equal(g.status, GameStatus.Draw);
    assert.equal(g.reason, EndReason.FiftyMoveClaim);
    // A pawn move resets the counter.
    g = new ChessGame('7k/8/6K1/8/8/8/P7/R7 w - - 99 60');
    line(g, 'a2a3');
    assert.equal(g.position.halfmove, 0);
    assert.equal(g.canClaimFiftyMove(), false);
});

test('flag fall: loss on time, or draw when the opponent cannot mate', () => {
    let g = new ChessGame();
    assert.equal(g.flagFall(WHITE), true);
    assert.equal(g.status, GameStatus.BlackWins);
    assert.equal(g.reason, EndReason.Timeout);
    g = new ChessGame('4k3/8/8/8/8/8/8/R3K3 w - - 0 1');
    g.flagFall(WHITE);                                // Black has a bare king
    assert.equal(g.status, GameStatus.Draw);
    assert.equal(g.reason, EndReason.TimeoutVsInsufficient);
    g = new ChessGame('4k3/8/8/8/8/8/8/R3K3 b - - 0 1');
    g.flagFall(BLACK);
    assert.equal(g.status, GameStatus.WhiteWins);
    g = new ChessGame('4k3/8/8/8/8/8/8/4KN2 b - - 0 1');
    assert.equal(g.isOver, true);                     // K+N v K is already dead
    assert.equal(g.flagFall(BLACK), false);
    assert.equal(g.reason, EndReason.InsufficientMaterial);
    g = new ChessGame('4k3/8/8/8/8/8/1p6/4K3 w - - 0 1');
    g.flagFall(BLACK);                                // White has a bare king
    assert.equal(g.status, GameStatus.Draw);
    assert.equal(g.reason, EndReason.TimeoutVsInsufficient);
    g = new ChessGame('4k3/7p/8/8/8/8/8/4KN2 b - - 0 1');
    g.flagFall(BLACK);                                // K+N v K+P: a mate is possible
    assert.equal(g.status, GameStatus.WhiteWins);
    assert.equal(g.reason, EndReason.Timeout);
    g = new ChessGame('4k3/8/8/8/8/8/8/2B1K3 b - - 0 1');
    assert.equal(g.isOver, true);                     // K+B v K
    g = new ChessGame('4k3/8/8/8/5n2/8/8/2B1K3 w - - 0 1');
    g.flagFall(BLACK);                                // lone bishop v knight: helpmate exists
    assert.equal(g.status, GameStatus.WhiteWins);
    g = new ChessGame('4k3/8/8/8/8/8/8/2B1K3 w - - 0 1');
    assert.throws(() => g.flagFall(2), TypeError);
});

test('resignation, agreement and online endings', () => {
    let g = new ChessGame();
    assert.equal(g.resign(WHITE), true);
    assert.equal(g.status, GameStatus.BlackWins);
    assert.equal(g.reason, EndReason.Resignation);
    assert.equal(g.agreeDraw(), false);               // no effect after the end
    assert.equal(g.status, GameStatus.BlackWins);
    g = new ChessGame();
    g.resign(BLACK);
    assert.equal(g.status, GameStatus.WhiteWins);
    g = new ChessGame();
    assert.equal(g.agreeDraw(), true);
    assert.equal(g.status, GameStatus.Draw);
    assert.equal(g.reason, EndReason.Agreement);
    g = new ChessGame();
    assert.equal(g.end(GameStatus.Aborted, EndReason.NoShow), true);
    assert.equal(g.status, GameStatus.Aborted);
    assert.equal(g.reason, EndReason.NoShow);
    assert.equal(g.resultString(), '*');
    assert.deepEqual(g.play(796), { ok: false });
    g = new ChessGame();
    assert.equal(g.end(GameStatus.WhiteWins, EndReason.Forfeit), true);
    assert.throws(() => new ChessGame().end(0, EndReason.None), RangeError);
    assert.throws(() => new ChessGame().end(GameStatus.Draw, -1), RangeError);
    assert.throws(() => new ChessGame().resign('white'), TypeError);
    assert.throws(() => new ChessGame('not a fen'));
});

test('journal replay with fromMoves', () => {
    const g = line(new ChessGame(), 'e2e4 e7e5 g1f3 b8c6 f1b5 a7a6 b5c6 d7c6 e1g1');
    const r = ChessGame.fromMoves(undefined, g.moves);
    assert.equal(r.position.fen(), g.position.fen());
    assert.deepEqual(r.moves, g.moves);
    assert.equal(ChessGame.fromMoves(undefined, [...g.moves, 12 | (36 << 6)]), null);
    assert.equal(ChessGame.fromMoves('bad fen', []), null);
    const custom = ChessGame.fromMoves('4k3/8/8/8/8/8/8/R3K3 w Q - 0 1', [4 | (2 << 6)]);
    assert.equal(custom.position.fen(), '4k3/8/8/8/8/8/8/2KR4 b - - 1 1');
    assert.equal(custom.startFen, '4k3/8/8/8/8/8/8/R3K3 w Q - 0 1');
});

test('PGN export', () => {
    const tags = { event: 'Rated 3+2', site: 'Scacelith online', date: '2026.09.28', round: '-', white: 'Alice "A"', black: 'Bob\\B', timeControl: '180+2' };
    const g = line(new ChessGame(), 'f2f3 e7e5 g2g4 d8h4');
    assert.equal(g.pgn(tags), [
        '[Event "Rated 3+2"]',
        '[Site "Scacelith online"]',
        '[Date "2026.09.28"]',
        '[Round "-"]',
        '[White "Alice \\"A\\""]',
        '[Black "Bob\\\\B"]',
        '[Result "0-1"]',
        '[TimeControl "180+2"]',
        '[Termination "normal"]',
        '',
        '1. f3 e5 2. g4 Qh4# {Checkmate} 0-1',
        '',
    ].join('\n'));
    const custom = line(new ChessGame('4k3/8/8/8/8/8/8/R3K3 b Q - 3 20'), 'e8d7');
    custom.end(GameStatus.WhiteWins, EndReason.Abandonment);
    const pgn = custom.pgn({ date: '2026.09.28', white: 'W', black: 'B', extra: [['WhiteElo', '1500']] });
    assert.match(pgn, /\[SetUp "1"\]\n\[FEN "4k3\/8\/8\/8\/8\/8\/8\/R3K3 b Q - 3 20"\]\n/);
    assert.match(pgn, /\[Termination "abandoned"\]\n\[WhiteElo "1500"\]\n/);
    assert.match(pgn, /\n20\.\.\. Kd7 \{Abandoned \(disconnected for too long\)\} 1-0\n$/);
    assert.match(new ChessGame().pgn(), /\[Date "\d{4}\.\d{2}\.\d{2}"\][\s\S]*\[Termination "unterminated"\]\n\n\*\n$/);
    // Long games wrap at 80 columns.
    const long = new ChessGame();
    for (let i = 0; i < 120 && !long.isOver; i++) {
        const legal = long.position.legalMoves().sort((a, b) => a - b);
        long.play(legal[(i * 7919) % legal.length]);
    }
    const lines = long.pgn(tags).split('\n');
    assert.ok(lines.length > 15);
    for (const l of lines) assert.ok(l.length <= 79, l);
});

test('PGN export: tags after Result, per-ply comments, Termination of the online endings', () => {
    const g = line(new ChessGame(), 'e2e4 e7e5 g1f3');
    g.resign(BLACK);
    const pgn = g.pgn({
        event: 'E', site: 'S', date: '2026.09.28', white: 'W', black: 'B', timeControl: '180+2',
        afterResult: [['UTCDate', '2026.09.28'], ['UTCTime', '12:00:00']], extra: [['PlyCount', '3']],
        comments: [['[%clk 0:03:00.0]', '[%emt 0:00:00.0]'], 'nice {move}\n', null, ['ignored: no such ply']],
    });
    assert.equal(pgn, [
        '[Event "E"]', '[Site "S"]', '[Date "2026.09.28"]', '[Round "-"]', '[White "W"]', '[Black "B"]', '[Result "1-0"]',
        '[UTCDate "2026.09.28"]', '[UTCTime "12:00:00"]', '[TimeControl "180+2"]', '[Termination "normal"]', '[PlyCount "3"]', '',
        '1. e4 {[%clk 0:03:00.0] [%emt 0:00:00.0]} 1... e5 {nice move} 2. Nf3', '{Resignation} 1-0', '',
    ].join('\n'));
    // An empty comment is no comment (and the Black move keeps its plain form).
    assert.match(line(new ChessGame(), 'e2e4 e7e5').pgn({ comments: ['', [], ' '] }), /\n1\. e4 e5 \*\n$/);
    // Termination of the online endings (PGN standard values).
    const term = (status, reason) => {
        const x = new ChessGame();
        x.end(status, reason);
        return /\[Termination "([^"]+)"\]/.exec(x.pgn())[1];
    };
    assert.equal(term(GameStatus.Aborted, EndReason.Aborted), 'unterminated');
    assert.equal(term(GameStatus.Aborted, EndReason.NoShow), 'unterminated');
    assert.equal(term(GameStatus.Aborted, EndReason.ServerAborted), 'unterminated');
    assert.equal(term(GameStatus.Aborted, EndReason.BothDisconnected), 'unterminated');
    assert.equal(term(GameStatus.WhiteWins, EndReason.Abandonment), 'abandoned');
    assert.equal(term(GameStatus.Draw, EndReason.AbandonmentVsInsufficient), 'abandoned');
    assert.equal(term(GameStatus.BlackWins, EndReason.Forfeit), 'rules infraction');
    assert.equal(term(GameStatus.WhiteWins, EndReason.IllegalMoves), 'rules infraction');
    assert.equal(term(GameStatus.Draw, EndReason.TimeoutVsInsufficient), 'time forfeit');
    assert.equal(term(GameStatus.BlackWins, EndReason.Timeout), 'time forfeit');
    assert.equal(term(GameStatus.Draw, EndReason.Agreement), 'normal');
    const aborted = new ChessGame();
    aborted.end(GameStatus.Aborted, EndReason.NoShow);
    assert.match(aborted.pgn(), /\[Result "\*"\][\s\S]*\n\{Aborted: first move not played in time\} \*\n$/);
    // Long games with clocks still wrap under 80 columns, a [%command] never broken.
    const long = new ChessGame();
    const comments = [];
    for (let i = 0; i < 160 && !long.isOver; i++) {
        const legal = long.position.legalMoves().sort((a, b) => a - b);
        long.play(legal[(i * 7919) % legal.length]);
        comments.push([`[%clk 1:${String(59 - (i % 60)).padStart(2, '0')}:00.${i % 10}]`, '[%emt 0:00:01.5]']);
    }
    const text = long.pgn({ comments });
    for (const l of text.split('\n')) {
        assert.ok(l.length <= 79, l);
        assert.ok(!/\[%(clk|emt)$/.test(l) && !/^[0-9:.]+\]/.test(l), `a command split: ${l}`);
    }
    assert.equal((text.match(/\[%clk /g) || []).length, long.ply);
});
