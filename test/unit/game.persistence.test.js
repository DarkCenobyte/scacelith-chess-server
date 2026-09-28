// GameHost with the real modules around it: server chess rules (src/chess), SQLite store
// (src/store), on-disk journal (src/store/journal.js) and Elo (src/match/elo.js). A finished
// rated game is committed with both ratings and a RatingUpdate; a game in progress survives a
// restart (new journal and host on the same directory) with the same position and clocks.
import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { GameHost } from '../../src/game/host.js';
import { FakeAnticheat, FakePrimary, FakeEndpoint, silentLog } from '../../src/game/testing.js';
import { ChessGame, parseSquare } from '../../src/chess/index.js';
import { openStore, migrate } from '../../src/store/index.js';
import { openJournal } from '../../src/store/journal.js';
import { applyGame } from '../../src/match/elo.js';
import { decode, enums, encodeMove, MSG } from '../../src/protocol/index.js';
import { Registry } from '../../src/metrics.js';
import { testConfig } from '../../src/config.js';

const { GameStatus: GS, EndReason: ER } = enums;
const T0 = 1_800_000_000_000;

function uci(s) { return encodeMove(parseSquare(s.slice(0, 2)), parseSquare(s.slice(2, 4)), 0); }

async function setup(dir) {
    const config = testConfig({ DB_PATH: path.join(dir, 'scacelith.db'), DATA_DIR: dir });
    const store = openStore(config, { applyGame, log: silentLog });
    migrate(store);
    const journal = await openJournal({ dir: path.join(dir, 'journal'), shard: 0, flushMs: 5, fsync: false, log: silentLog });
    const clock = { t: T0 };
    const primary = new FakePrimary();
    const host = new GameHost({
        shard: 0, config, store, journal, anticheat: new FakeAnticheat(), primary, log: silentLog,
        createChessGame: () => new ChessGame(), now: () => clock.t, metrics: new Registry(), autoStart: false,
    });
    return { config, store, journal, clock, host, primary };
}

function playMoves(host, clock, id, list, endpoints) {
    for (const m of list) {
        const room = host.room(id);
        const color = room.ply & 1;
        clock.t += 1000;
        host.onClientMessage(id, room.playerOf(color).userId, {
            type: MSG.Move, seq: room.ply + 1, game: id, ply: room.ply, move: uci(m),
            posHash: room.game.position.digest(), thinkMs: 900, drawOffer: false,
        }, endpoints[color]);
    }
}

test('real store + journal + rules: a rated game is committed with ratings, an open one survives a restart', async () => {
    const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'scacelith-gp-'));
    try {
        let s = await setup(dir);
        const alice = s.store.users.create({ username: 'alice', email: 'alice@example.org' });
        const bob = s.store.users.create({ username: 'bob', email: 'bob@example.org' });
        const carol = s.store.users.create({ username: 'carol', email: 'carol@example.org' });
        const p = (userId, name) => ({ userId, name, rating: 1500, provisional: true });

        // 1. Fool's mate in 3+2, rated.
        const g1 = s.host.createGame({ white: p(alice, 'alice'), black: p(bob, 'bob'), rated: true, baseMs: 180000, incMs: 2000 });
        const ew = new FakeEndpoint(1), eb = new FakeEndpoint(2);
        s.host.attach(g1, alice, ew);
        s.host.attach(g1, bob, eb);
        playMoves(s.host, s.clock, g1, ['f2f3', 'e7e5', 'g2g4', 'd8h4'], [ew, eb]);
        const end = eb.msgs().find((m) => m.type === MSG.GameEnd);
        assert.equal(end.status, GS.BlackWins);
        assert.equal(end.reason, ER.Checkmate);
        s.clock.t += 100;             // DB_COMMIT_MS later
        assert.equal(s.host.pollCommits(s.clock.t), true);
        const rated = eb.msgs().find((m) => m.type === MSG.RatingUpdate);
        assert.ok(rated, 'RatingUpdate sent after the commit');
        assert.deepEqual([rated.black.before, rated.black.after, rated.white.after], [1500, 1520, 1480]);
        const row = s.store.games.byId(g1);
        assert.equal(row.status, GS.BlackWins);
        assert.equal(s.store.ratings.get(bob, '3+2').rating, 1520);
        assert.equal(s.store.ratings.get(alice, '3+2').games, 1);
        assert.deepEqual(s.primary.of('game.ended').map((e) => e.gameId), [g1]);

        // 2. A game in progress, then the process goes away (journal flushed, nothing committed).
        const g2 = s.host.createGame({ white: p(carol, 'carol'), black: p(bob, 'bob'), rated: true, baseMs: 300000, incMs: 3000 });
        playMoves(s.host, s.clock, g2, ['e2e4', 'c7c5', 'g1f3'], [null, null]);
        const before = s.host.room(g2).snapshot(1, s.clock.t);
        const digest = s.host.room(g2).game.position.digest();
        await s.journal.flush();
        await s.journal.close();
        s.store.close();

        // 3. Restart on the same directory: g1 is committed (not replayed), g2 is restored.
        s = await setup(dir);
        s.clock.t = before.serverTime + 2000;
        assert.equal(await s.host.recover(), 1);
        const room = s.host.room(g2);
        assert.ok(room, 'the unfinished game is back');
        assert.equal(s.host.room(g1), null);
        assert.equal(room.ply, 3);
        assert.equal(room.game.position.digest(), digest);
        assert.deepEqual(s.primary.of('game.recovered').map((e) => e.gameId), [g2]);
        const after = room.snapshot(1, s.clock.t);
        assert.equal(after.whiteMs, before.whiteMs);
        assert.ok(after.blackMs <= before.blackMs, 'Black\'s clock did not gain time over the restart');
        // Black reconnects and keeps playing.
        const eb2 = new FakeEndpoint(3);
        assert.equal(s.host.attach(g2, bob, eb2), true);
        playMoves(s.host, s.clock, g2, ['d7d6'], [null, eb2]);
        assert.equal(s.host.room(g2).ply, 4);
        await s.journal.close();
        s.store.close();
    } finally {
        fs.rmSync(dir, { recursive: true, force: true });
    }
});
