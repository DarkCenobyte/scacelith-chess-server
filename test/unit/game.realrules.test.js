// GameRoom with the real server chess rules (src/chess) instead of the scripted FakeChessGame:
// checks that the room and the rules agree on digests, move flags and automatic endings.
import test from 'node:test';
import assert from 'node:assert/strict';
import { GameRoom } from '../../src/game/room.js';
import { ChessGame, parseSquare } from '../../src/chess/index.js';
import { decode, enums, encodeMove, MSG, MoveFlag } from '../../src/protocol/index.js';
import { testConfig } from '../../src/config.js';

const { GameStatus: GS, EndReason: ER, ErrorCode: EC } = enums;
const CFG = testConfig();
const T0 = 1_800_000_000_000;
const PROMO = { n: 2, b: 3, r: 4, q: 5 };

function uci(s) {
    return encodeMove(parseSquare(s.slice(0, 2)), parseSquare(s.slice(2, 4)), s[4] ? PROMO[s[4]] : 0);
}

function mkRoom(baseMs = 180000, incMs = 2000) {
    return new GameRoom({
        id: 987654321, category: '3+2', baseMs, incMs, rated: true,
        white: { userId: 1, name: 'alice', rating: 1500, provisional: false },
        black: { userId: 2, name: 'bob', rating: 1500, provisional: false },
        createdAt: T0, config: CFG, createChessGame: () => new ChessGame(),
    });
}

// Plays the moves alternately, one second apart; returns the last Outcome and the decoded MoveMade list.
function play(room, moves, t = T0) {
    let last = null;
    const made = [];
    for (const m of moves) {
        t += 1000;
        last = room.onMove(room.sideToMove, {
            seq: room.ply + 1, ply: room.ply, move: uci(m), posHash: room.game.position.digest(), thinkMs: 900, drawOffer: false,
        }, t);
        assert.equal(last.rejected, 0, `move ${m} refused`);
        for (const b of last.broadcast) { const d = decode(b); if (d.type === MSG.MoveMade) made.push(d); }
    }
    return { last, made, t };
}

const endOf = (o) => o.broadcast.map((b) => decode(b)).find((d) => d.type === MSG.GameEnd);

test('real rules: fool\'s mate ends the game by checkmate', () => {
    const room = mkRoom();
    const { last } = play(room, ['f2f3', 'e7e5', 'g2g4', 'd8h4']);
    assert.ok(last.ended);
    const end = endOf(last);
    assert.equal(end.status, GS.BlackWins);
    assert.equal(end.reason, ER.Checkmate);
});

test('real rules: castling, en passant and promotion flags reach MoveMade', () => {
    const room = mkRoom();
    const { made } = play(room, [
        'e2e4', 'a7a6', 'e4e5', 'd7d5', 'e5d6',          // en passant
        'g8f6', 'g1f3', 'b8c6', 'f1e2', 'a6a5', 'e1g1',  // castling
        'a5a4', 'd6c7', 'a4a3', 'c7d8q',                 // promotion with capture
    ]);
    const flags = (i) => made[i].flags;
    assert.ok(flags(4) & MoveFlag.EnPassant, 'en passant');
    assert.ok(flags(10) & MoveFlag.CastleKing, 'castle');
    assert.ok(flags(14) & MoveFlag.Promotion, 'promotion');
    assert.equal(room.ply, 15);
});

test('real rules: an illegal move in a synchronised position is a certain anomaly', () => {
    const room = mkRoom();
    play(room, ['e2e4', 'e7e5']);
    const o = room.onMove(0, { seq: 9, ply: 2, move: uci('e1e3'), posHash: room.game.position.digest(), thinkMs: 0, drawOffer: false }, T0 + 5000);
    assert.equal(o.rejected, EC.IllegalMove);
    assert.equal(o.anomaly.kind, 'illegal_move');
    assert.ok(o.anomaly.posMatched);
});

test('real rules: the player whose flag falls loses when the opponent can mate', () => {
    const room = mkRoom(10000, 0);
    const { t } = play(room, ['e2e4', 'e7e5']);
    // White's clock runs from Black's first move; nobody moves: White flags. Black can mate (full army).
    const o = room.onResync(0, t + 20000);
    const end = [...o.broadcast, ...o.reply].map((b) => decode(b)).find((d) => d.type === MSG.GameEnd);
    assert.ok(end, 'game ended on time');
    assert.equal(end.status, GS.BlackWins);
    assert.equal(end.reason, ER.Timeout);
});
