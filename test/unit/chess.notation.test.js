// FEN parsing / normalisation / round trips, SAN, UCI (cases of the game's tests/chess_tests.cpp).
import { test } from 'node:test';
import assert from 'node:assert/strict';

import { Position, squareName, parseSquare, CastlingRight } from '../../src/chess/index.js';

const START = 'rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1';
const KIWIPETE = 'r3k2r/p1ppqpb1/bn2pnp1/3PN3/1p2P3/2N2Q1p/PPPBBPPP/R3K2R w KQkq - 0 1';

function fnv1a32(s) {
    let h = 0x811c9dc5;
    for (let i = 0; i < s.length; i++) {
        h ^= s.charCodeAt(i) & 0xff;
        h = Math.imul(h, 0x01000193) >>> 0;
    }
    return h >>> 0;
}

const sq = (s) => parseSquare(s);
const mv = (from, to, promo = 0) => sq(from) | (sq(to) << 6) | (promo << 12);

function pos(fen) {
    const p = Position.fromFEN(fen);
    assert.ok(p, `valid FEN ${fen}`);
    return p;
}

function sanOf(fen, uci) {
    const p = pos(fen);
    const m = p.parseUCI(uci);
    assert.notEqual(m, -1, `${uci} legal in ${fen}`);
    return p.san(m);
}

test('squares', () => {
    assert.equal(squareName(0), 'a1');
    assert.equal(squareName(28), 'e4');
    assert.equal(squareName(63), 'h8');
    assert.equal(squareName(64), '-');
    assert.equal(squareName(-1), '-');
    assert.equal(parseSquare('e4'), 28);
    assert.equal(parseSquare('E4'), 28);
    assert.equal(parseSquare('h8'), 63);
    assert.equal(parseSquare('i1'), -1);
    assert.equal(parseSquare('a9'), -1);
    assert.equal(parseSquare('e'), -1);
    assert.equal(parseSquare(12), -1);
});

test('FEN round trips, digest and repetition key', () => {
    const fens = [
        START, KIWIPETE,
        '8/2p5/3p4/KP5r/1R3p1k/8/4P1P1/8 w - - 0 1',
        'r3k2r/Pppp1ppp/1b3nbN/nP6/BBP1P3/q4N2/Pp1P2PP/R2Q1RK1 w kq - 0 1',
        'rnbq1k1r/pp1Pbppp/2p5/8/2B5/8/PPP1NnPP/RNBQK2R w KQ - 1 8',
        'rnbqkbnr/ppp1p1pp/8/3pPp2/8/8/PPPP1PPP/RNBQKBNR w KQkq f6 0 3',
        'rnbqkbnr/pppp1ppp/8/8/3Pp3/8/PPP1PPPP/RNBQKBNR b KQkq d3 0 3',
        '8/8/8/8/8/8/8/K6k w - - 99 150',
        '4k3/8/8/8/8/8/8/4K2R w K - 0 1',
        'r3k3/8/8/8/8/8/8/4K3 b q - 5 40',
    ];
    for (const f of fens) {
        const p = pos(f);
        assert.equal(p.fen(), f);
        assert.equal(p.digest(), fnv1a32(f.split(' ').slice(0, 4).join(' ')));
        const q = pos(p.fen());
        assert.equal(q.repetitionKey(), p.repetitionKey());
        assert.equal(q.clone().fen(), f);
    }
    const start = Position.start();
    assert.equal(start.fen(), START);
    assert.equal(start.side, 0);
    assert.equal(start.castling, 15);
    assert.equal(start.epSquare, -1);
    assert.equal(start.halfmove, 0);
    assert.equal(start.fullmove, 1);
    assert.equal(start.kingSquare(0), sq('e1'));
    assert.equal(start.kingSquare(1), sq('e8'));
    assert.equal(start.pieceAt(sq('d1')), 5);        // white queen
    assert.equal(start.pieceAt(sq('g8')), 8 | 2);    // black knight
    assert.equal(start.pieceAt(sq('e4')), 0);
    assert.match(start.repetitionKey(), /^[0-9a-f]{16}$/);
    // Optional move counters.
    assert.equal(pos('4k3/8/8/8/8/8/8/4K3 w - -').fen(), '4k3/8/8/8/8/8/8/4K3 w - - 0 1');
    assert.equal(pos('4k3/8/8/8/8/8/8/4K3 w - - 0 0').fen(), '4k3/8/8/8/8/8/8/4K3 w - - 0 1');
    // Transpositions reach the same key; the side to move matters.
    const a = Position.start(), b = Position.start();
    for (const m of ['g1f3', 'g8f6', 'b1c3']) a.play(a.parseUCI(m));
    for (const m of ['b1c3', 'g8f6', 'g1f3']) b.play(b.parseUCI(m));
    assert.equal(a.repetitionKey(), b.repetitionKey());
    assert.equal(a.digest(), b.digest());
    assert.notEqual(a.repetitionKey(), Position.start().repetitionKey());
});

test('FEN rejected like chess::Position::setFEN', () => {
    const bad = [
        '', '   ', 'hello', 42, null, undefined,
        'rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP w KQkq - 0 1',             // 7 ranks
        'rnbqkbnr/pppppppp/9/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1',     // bad digit
        'rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR x KQkq - 0 1',     // bad side
        'rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkx - 0 1',     // bad castling
        'rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq e9 0 1',    // bad ep square
        '8/8/8/8/8/8/8/K7 w - - 0 1',                                   // no black king
        'k7/8/8/8/8/8/8/KK6 w - - 0 1',                                 // two white kings
        'P3k3/8/8/8/8/8/8/4K3 w - - 0 1',                               // pawn on the 8th rank
        '4k3/8/8/8/8/8/8/p3K3 w - - 0 1',                               // pawn on the 1st rank
        '4k3/8/8/8/8/P7/PPPPPPPP/4K3 w - - 0 1',                        // 9 pawns
        '4k3/8/8/8/8/N7/PPPPPPPP/RNBQKBNR w KQ - 0 1',                  // 17 pieces
        '4k2R/8/8/8/8/8/8/4K3 w - - 0 1',                               // side not to move in check
        '4k3/8/8/8/8/8/8/4K3 w - - x 1',                                // bad counter
        '4k3/8/8/8/8/8/8/4K3 w - - -1 1',
        '4k3/8/8/8/8/8/8/4K3 w - - 1000000000 1',                       // more than 9 digits
        '4k3/8/8/8/8/8/8/4K3 w - - 0 1 extra',                          // 7 fields
        '4k3/8/8/8/8/8/8/4K3 w -',                                      // 3 fields
    ];
    for (const f of bad) assert.equal(Position.fromFEN(f), null, JSON.stringify(f));
    assert.ok(pos('4k3/8/8/8/8/8/8/4K2R b - - 0 1'));                   // Black in check, to move: fine
    assert.ok(pos('4k3/8/8/8/8/8/8/4K3\tw\t-\t-\t0\t1'));               // tabs separate fields
});

test('FEN normalisation: castling rights and en passant', () => {
    // Castling rights without king/rook on their squares are dropped.
    assert.equal(pos('4k3/8/8/8/8/8/8/4K3 w KQkq - 0 1').castling, 0);
    assert.equal(pos('r3k3/8/8/8/8/8/8/4K2R w KQkq - 0 1').fen(), 'r3k3/8/8/8/8/8/8/4K2R w Kq - 0 1');
    assert.equal(pos('r3k2r/8/8/8/8/8/8/R4K1R w KQkq - 0 1').fen(), 'r3k2r/8/8/8/8/8/8/R4K1R w kq - 0 1');
    assert.equal(pos('r3k2r/8/8/8/8/8/8/R3K2R w qkQK - 0 1').fen(), 'r3k2r/8/8/8/8/8/8/R3K2R w KQkq - 0 1');
    assert.equal(pos('r3k2r/8/8/8/8/8/8/R3K2R w KQkq - 0 1').castling,
        CastlingRight.WhiteKingSide | CastlingRight.WhiteQueenSide | CastlingRight.BlackKingSide | CastlingRight.BlackQueenSide);
    // En passant square kept only when a capture is legal.
    let p = pos('rnbqkbnr/ppp1pppp/8/3p4/8/8/PPPPPPPP/RNBQKBNR w KQkq d6 0 2');
    assert.equal(p.epSquare, -1);
    assert.equal(p.fen(), 'rnbqkbnr/ppp1pppp/8/3p4/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 2');
    p = pos('rnbqkbnr/ppp1pppp/8/3pP3/8/8/PPPP1PPP/RNBQKBNR w KQkq d6 0 2');
    assert.equal(p.epSquare, sq('d6'));
    // Wrong rank: silently dropped, like the game.
    assert.equal(pos('rnbqkbnr/ppp1pppp/8/3pP3/8/8/PPPP1PPP/RNBQKBNR w KQkq d3 0 2').epSquare, -1);
    // Pinned capturer (rank pin through both pawns): not stored.
    assert.equal(pos('4k3/8/8/KPp4r/8/8/8/8 w - c6 0 2').epSquare, -1);
    assert.equal(pos('4k3/8/8/1Pp5/8/8/8/K7 w - c6 0 2').epSquare, sq('c6'));
    // Diagonal pin of the capturer.
    assert.equal(pos('4k3/8/8/2pP4/8/8/8/4K1b1 w - c6 0 1').fen(), '4k3/8/8/2pP4/8/8/8/4K1b1 w - c6 0 1');
    assert.equal(pos('4k3/8/8/2pP4/8/b7/8/4K3 w - c6 0 1').epSquare, sq('c6'));
    assert.equal(pos('7k/8/8/8/3pP3/8/8/B6K b - e3 0 1').epSquare, -1);        // d4 pawn pinned on a1-h8
    // The capture removes the checking pawn: legal although in check.
    assert.equal(pos('8/8/8/8/k1pP4/8/8/4K3 b - d3 0 1').epSquare, sq('d3'));
    // Discovered check along the rank by removing both pawns: illegal.
    assert.equal(pos('8/8/8/8/k1pP3Q/8/8/4K3 b - d3 0 1').epSquare, -1);
    // After a double push the ep square appears only when capturable.
    p = Position.start();
    p.play(p.parseUCI('e2e4'));
    assert.equal(p.epSquare, -1);
    assert.equal(p.fen(), 'rnbqkbnr/pppppppp/8/8/4P3/8/PPPP1PPP/RNBQKBNR b KQkq - 0 1');
    for (const u of ['g8f6', 'e4e5', 'd7d5']) p.play(p.parseUCI(u));
    assert.equal(p.fen(), 'rnbqkb1r/ppp1pppp/5n2/3pP3/8/8/PPPP1PPP/RNBQKBNR w KQkq d6 0 3');
    p = pos('4k3/2p5/8/KP5r/8/8/8/8 b - - 0 1');
    p.play(p.parseUCI('c7c5'));
    assert.equal(p.epSquare, -1);
    assert.equal(p.parseUCI('b5c6'), -1);
});

test('SAN: disambiguation, promotions, captures, checks, mates, castling', () => {
    assert.equal(sanOf(START, 'g1f3'), 'Nf3');
    assert.equal(sanOf(START, 'e2e4'), 'e4');
    const knights = 'rnbqkb1r/ppp1pppp/5n2/3p4/8/8/PPPPPPPP/RNBQKBNR b KQkq - 0 1';
    assert.equal(sanOf(knights, 'b8d7'), 'Nbd7');
    assert.equal(sanOf(knights, 'f6d7'), 'Nfd7');
    assert.equal(sanOf(knights, 'b8c6'), 'Nc6');
    const rooks = '4k3/8/8/R7/8/8/8/R3K3 w - - 0 1';
    assert.equal(sanOf(rooks, 'a1a3'), 'R1a3');
    assert.equal(sanOf(rooks, 'a5a3'), 'R5a3');
    assert.equal(sanOf(rooks, 'a1d1'), 'Rd1');
    assert.equal(sanOf(rooks, 'a5a8'), 'Ra8+');
    const queens = '1k6/8/8/8/4Q2Q/8/K7/7Q w - - 0 1';
    assert.equal(sanOf(queens, 'h4e1'), 'Qh4e1');
    assert.equal(sanOf(queens, 'e4e1'), 'Qee1');
    assert.equal(sanOf(queens, 'h1e1'), 'Q1e1');
    const knightsFile = '4k3/8/8/8/8/N7/8/N3K3 w - - 0 1';
    assert.equal(sanOf(knightsFile, 'a1c2'), 'N1c2');
    assert.equal(sanOf(knightsFile, 'a3c2'), 'N3c2');
    const promo = 'k2r4/4P3/8/8/8/8/8/4K3 w - - 0 1';
    assert.equal(sanOf(promo, 'e7d8q'), 'exd8=Q+');
    assert.equal(sanOf(promo, 'e7d8r'), 'exd8=R+');
    assert.equal(sanOf(promo, 'e7d8b'), 'exd8=B');
    assert.equal(sanOf(promo, 'e7d8n'), 'exd8=N');
    assert.equal(sanOf(promo, 'e7e8q'), 'e8=Q');
    assert.equal(sanOf('8/2P1k3/8/8/8/8/8/K7 w - - 0 1', 'c7c8n'), 'c8=N+');
    assert.equal(sanOf('4rkr1/4p1p1/8/8/8/8/8/4K2R w K - 0 1', 'e1g1'), 'O-O#');
    assert.equal(sanOf('5k2/8/8/8/8/8/8/4K2R w K - 0 1', 'e1g1'), 'O-O+');
    assert.equal(sanOf('3k4/8/8/8/8/8/8/R3K3 w Q - 0 1', 'e1c1'), 'O-O-O+');
    assert.equal(sanOf('r3k3/8/8/8/8/8/8/R3K3 w Qq - 0 1', 'e1c1'), 'O-O-O');
    assert.equal(sanOf('r3k3/8/8/8/8/8/8/4K3 b q - 0 1', 'e8c8'), 'O-O-O');
    assert.equal(sanOf('rnbqkbnr/ppp1p1pp/8/3pPp2/8/8/PPPP1PPP/RNBQKBNR w KQkq f6 0 3', 'e5f6'), 'exf6');
    assert.equal(sanOf('rnbqkbnr/ppp2ppp/3p4/4p3/4P3/8/PPPP1PPP/RNBQKBNR w KQkq - 0 3', 'f1b5'), 'Bb5+');
    assert.equal(sanOf('4k3/8/8/2p1p3/3P4/8/8/4K3 w - - 0 1', 'd4c5'), 'dxc5');
    assert.equal(sanOf('4k3/8/8/2p1p3/3P4/8/8/4K3 w - - 0 1', 'd4e5'), 'dxe5');
    assert.equal(sanOf('rnbqkbnr/pppp1ppp/8/4p3/6P1/5P2/PPPPP2P/RNBQKBNR b KQkq - 0 2', 'd8h4'), 'Qh4#');
    assert.equal(sanOf('6k1/5ppp/8/8/8/8/8/R3R1K1 w - - 0 1', 'e1e8'), 'Re8#');
    assert.equal(sanOf('6k1/5ppp/8/8/8/8/8/R3R1K1 w - - 0 1', 'a1d1'), 'Rad1');
    assert.equal(sanOf('6k1/5ppp/8/8/8/8/8/R3R1K1 w - - 0 1', 'e1d1'), 'Red1');
    assert.equal(sanOf('6k1/4pppp/8/8/8/8/8/R3R1K1 w - - 0 1', 'a1a8'), 'Ra8#');
    // A pinned piece does not count for disambiguation (only legal moves do).
    assert.equal(sanOf('4k3/8/8/8/1b6/8/3N4/4K1N1 w - - 0 1', 'g1f3'), 'Nf3');
    // Illegal -> empty, and the position is unchanged by san().
    const p = Position.start();
    assert.equal(p.san(mv('e2', 'e5')), '');
    assert.equal(p.san(mv('e2', 'e4', 5)), '');
    assert.equal(p.fen(), START);
});

test('SAN disambiguation matches the legal move list over random games', () => {
    // san() validates only the other pieces of the same code; the reference filters every legal move.
    const starts = [
        START, KIWIPETE,
        'q3k2q/8/8/8/8/8/1QQQ4/Q3K2Q w - - 0 1',
        'rn2k1nr/8/8/8/8/8/8/RN2K1NR w KQkq - 0 1',
        'b3k2b/8/8/8/8/8/8/B3K2B w - - 0 1',
        'k7/2PPPPPP/8/8/8/8/2pppppp/K7 w - - 0 1',
    ];
    let seed = 0x5a17;
    const rnd = (n) => {
        seed = (Math.imul(seed, 1103515245) + 12345) >>> 0;
        return (seed >>> 8) % n;
    };
    let checked = 0;
    for (const fen of starts) {
        for (let g = 0; g < 4; g++) {
            const p = pos(fen);
            for (let ply = 0; ply < 150; ply++) {
                const legal = p.legalMoves();
                if (!legal.length) break;
                for (const m of legal) {
                    const from = m & 63, to = (m >> 6) & 63, piece = p.pieceAt(from);
                    if ((piece & 7) === 1 || ((piece & 7) === 6 && Math.abs((to & 7) - (from & 7)) === 2)) continue;
                    let ambiguous = false, sameFile = false, sameRank = false;
                    for (const o of legal) {
                        const of = o & 63;
                        if (((o >> 6) & 63) !== to || of === from || p.pieceAt(of) !== piece) continue;
                        ambiguous = true;
                        if ((of & 7) === (from & 7)) sameFile = true;
                        if ((of >> 3) === (from >> 3)) sameRank = true;
                    }
                    const name = squareName(from);
                    const dis = !ambiguous ? '' : !sameFile ? name[0] : !sameRank ? name[1] : name;
                    const want = ' PNBRQK'[piece & 7] + dis + (p.pieceAt(to) ? 'x' : '') + squareName(to);
                    assert.equal(p.san(m).replace(/[+#]$/, ''), want, `${p.fen()} ${p.uci(m)}`);
                    checked++;
                }
                p.play(legal[rnd(legal.length)]);
            }
        }
    }
    assert.ok(checked > 10000, `${checked} piece moves checked`);
});

test('UCI: output and strict parsing', () => {
    const p = Position.start();
    assert.equal(p.uci(mv('e2', 'e4')), 'e2e4');
    assert.equal(p.uci(mv('e7', 'e8', 2)), 'e7e8n');
    assert.equal(p.uci(-5), '0000');
    assert.equal(p.parseUCI('e2e4'), mv('e2', 'e4'));
    assert.equal(p.parseUCI('E2E4'), mv('e2', 'e4'));
    assert.equal(p.parseUCI('e2e5'), -1);
    assert.equal(p.parseUCI('e2e4q'), -1);
    assert.equal(p.parseUCI('e2'), -1);
    assert.equal(p.parseUCI('z9e4'), -1);
    assert.equal(p.parseUCI('e2e4 '), -1);
    assert.equal(p.parseUCI(1234), -1);
    const promo = pos('k2r4/4P3/8/8/8/8/8/4K3 w - - 0 1');
    assert.equal(promo.parseUCI('e7e8'), -1);           // promotion needs a piece
    assert.equal(promo.parseUCI('e7e8k'), -1);
    assert.equal(promo.parseUCI('e7e8p'), -1);
    assert.equal(promo.parseUCI('e7e8q'), mv('e7', 'e8', 5));
    assert.equal(promo.parseUCI('e7e8Q'), mv('e7', 'e8', 5));
    assert.equal(promo.parseUCI('e7d8n'), mv('e7', 'd8', 2));
    const castle = pos('r3k2r/8/8/8/8/8/8/R3K2R w KQkq - 0 1');
    assert.equal(castle.parseUCI('e1g1'), mv('e1', 'g1'));
    assert.equal(castle.parseUCI('e1c1'), mv('e1', 'c1'));
    assert.equal(castle.parseUCI('e1h1'), -1);           // king-takes-rook notation is not used
});

test('legal move queries', () => {
    const p = Position.start();
    assert.equal(p.legalMoves().length, 20);
    assert.ok(p.isLegal(mv('e2', 'e4')));
    assert.ok(!p.isLegal(mv('e2', 'e5')));
    assert.ok(!p.isLegal(mv('e2', 'e4', 5)));            // promo bits on a normal move
    assert.ok(!p.isLegal(mv('e2', 'e4') | 0x8000));      // bit 15
    assert.ok(!p.isLegal(mv('e7', 'e5')));               // not the side to move
    assert.ok(!p.inCheck());
    assert.ok(p.isAttacked(sq('f3'), 0));
    assert.ok(!p.isAttacked(sq('e4'), 0));
    assert.throws(() => p.play(mv('e2', 'e5')));
    assert.equal(p.fen(), START);
    // Castling: rights, path, check.
    const c = pos('r3k2r/8/8/8/8/8/8/R3K2R w KQkq - 0 1');
    assert.equal(c.clone().play(mv('e1', 'g1')), 4);
    assert.equal(c.clone().play(mv('e1', 'c1')), 8);
    const through = pos('r3k2r/8/8/8/8/8/5r2/R3K2R w KQkq - 0 1');   // f1 attacked
    assert.ok(!through.isLegal(mv('e1', 'g1')));
    assert.ok(through.isLegal(mv('e1', 'c1')));
    const b1 = pos('r3k2r/8/8/8/8/8/1r6/R3K2R w KQkq - 0 1');        // b1 attacked: O-O-O still legal
    assert.ok(b1.isLegal(mv('e1', 'c1')));
    const inCheck = pos('r3k2r/8/8/8/8/8/4r3/R3K2R w KQkq - 0 1');
    assert.ok(inCheck.inCheck());
    assert.ok(!inCheck.isLegal(mv('e1', 'g1')));
    assert.ok(!inCheck.isLegal(mv('e1', 'c1')));
    const blocked = pos('r3k2r/8/8/8/8/8/8/RN2K1NR w KQkq - 0 1');
    assert.ok(!blocked.isLegal(mv('e1', 'g1')));
    assert.ok(!blocked.isLegal(mv('e1', 'c1')));
    // Promotions need a piece; every promotion piece is its own move.
    const promo = pos('8/4P3/8/8/8/8/k7/4K3 w - - 0 1');
    assert.ok(!promo.isLegal(mv('e7', 'e8')));
    for (const t of [2, 3, 4, 5]) assert.ok(promo.isLegal(mv('e7', 'e8', t)));
    for (const t of [1, 6, 7]) assert.ok(!promo.isLegal(mv('e7', 'e8', t)));
    assert.equal(promo.legalMoves().filter((m) => (m & 63) === sq('e7')).length, 4);
    // Flags returned by play().
    const e = pos('rnbqkbnr/ppp1p1pp/8/3pPp2/8/8/PPPP1PPP/RNBQKBNR w KQkq f6 0 3');
    assert.equal(e.clone().play(mv('e5', 'f6')), 1 | 2);
    assert.equal(Position.start().play(mv('e2', 'e4')), 16);
    assert.equal(pos('k2r4/4P3/8/8/8/8/8/4K3 w - - 0 1').play(mv('e7', 'd8', 5)), 1 | 32 | 64);
    const mate = pos('rnbqkbnr/pppp1ppp/8/4p3/6P1/5P2/PPPPP2P/RNBQKBNR b KQkq - 0 2');
    assert.equal(mate.play(mv('d8', 'h4')), 64 | 128);
    assert.ok(mate.isCheckmate());
    assert.ok(pos('7k/5Q2/6K1/8/8/8/8/8 b - - 0 1').isStalemate());
    assert.ok(!pos('7k/5Q2/6K1/8/8/8/8/8 b - - 0 1').hasLegalMove());
});

test('insufficient material and canColorMate', () => {
    const dead = (f) => pos(f).hasInsufficientMaterial();
    assert.ok(dead('4k3/8/8/8/8/8/8/4K3 w - - 0 1'));
    assert.ok(dead('4k3/8/8/8/8/8/8/2B1K3 w - - 0 1'));
    assert.ok(dead('4k3/8/8/8/8/8/8/1N2K3 w - - 0 1'));
    assert.ok(dead('4k3/8/8/8/8/2b5/8/2B1K3 w - - 0 1'));      // both bishops on dark squares
    assert.ok(dead('4k3/8/8/8/8/8/8/B1B1K3 w - - 0 1'));       // a1, c1 both dark
    assert.ok(!dead('4k3/8/8/8/8/8/2b5/2B1K3 w - - 0 1'));     // opposite colours
    assert.ok(!dead('4k3/8/8/8/8/8/2n5/2N1K3 w - - 0 1'));     // knight v knight
    assert.ok(!dead('4k3/8/8/8/8/8/8/1NN1K3 w - - 0 1'));      // two knights
    assert.ok(!dead('4k3/7p/8/8/8/8/8/2B1K3 w - - 0 1'));      // bishop v pawn
    assert.ok(!dead('4k3/8/8/8/8/8/2n5/2B1K3 w - - 0 1'));     // bishop v knight
    assert.ok(!dead('4k3/8/8/8/8/8/8/2B1KB2 w - - 0 1'));      // c1 dark, f1 light
    let p = pos('4k3/8/8/8/8/8/8/4K2N w - - 0 1');
    assert.ok(!p.canColorMate(0));                              // lone knight v bare king
    assert.ok(!p.canColorMate(1));                              // bare king
    p = pos('4k3/7p/8/8/8/8/8/4K2N w - - 0 1');
    assert.ok(p.canColorMate(0));                               // the pawn can block: helpmate exists
    assert.ok(p.canColorMate(1));
    p = pos('4k3/8/8/8/8/8/8/R3K3 w - - 0 1');
    assert.ok(p.canColorMate(0));
    assert.ok(!p.canColorMate(1));
    assert.ok(pos('4k3/8/8/8/8/8/8/1NN1K3 w - - 0 1').canColorMate(0));
    p = pos('4k3/8/8/8/8/8/2b5/2B1K3 w - - 0 1');
    assert.ok(p.canColorMate(0));
    assert.ok(p.canColorMate(1));
});
