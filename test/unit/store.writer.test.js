// Store writer thread (src/store/writer.js): GameHost commits finished games through it into the
// real SQLite store; the results (ratings), StoreError codes and gameId come back from the thread.
import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { GameHost } from '../../src/game/host.js';
import { FakeAnticheat, FakePrimary, FakeEndpoint, silentLog } from '../../src/game/testing.js';
import { ChessGame, parseSquare } from '../../src/chess/index.js';
import { openStore, migrate } from '../../src/store/index.js';
import { startStoreWriter } from '../../src/store/writer.js';
import { applyGame } from '../../src/match/elo.js';
import { enums, encodeMove, MSG } from '../../src/protocol/index.js';
import { Registry } from '../../src/metrics.js';
import { testConfig } from '../../src/config.js';
import { DatabaseSync } from 'node:sqlite';

const { GameStatus: GS } = enums;
const T0 = 1_800_000_000_000;
const uci = (s) => encodeMove(parseSquare(s.slice(0, 2)), parseSquare(s.slice(2, 4)), 0);

test('writer thread: a rated game is committed with its ratings; errors keep their code and gameId', async () => {
    const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'scacelith-writer-'));
    const config = testConfig({ DB_PATH: path.join(dir, 'scacelith.db'), DATA_DIR: dir });
    const store = openStore(config, { applyGame, log: silentLog });
    migrate(store);
    const writer = startStoreWriter({ config, logging: false });
    try {
        const alice = store.users.create({ username: 'alice', email: 'alice@example.org' });
        const bob = store.users.create({ username: 'bob', email: 'bob@example.org' });
        // Rated records (10 games, K 40): a newcomer's first games are its unrated phase.
        const raw = new DatabaseSync(config.dbPath);
        const seed = raw.prepare(`INSERT INTO ratings (user_id, category, rating, games, wins, peak, rated, updated_at)
            VALUES (?, '3+2', 1500, 10, 5, 1500, 1, 0)`);
        for (const u of [alice, bob]) seed.run(u);
        raw.close();
        const clock = { t: T0 };
        const host = new GameHost({
            shard: 0, config, store: { games: { finishBatch: (r) => writer.finishBatch(r) } }, anticheat: new FakeAnticheat(),
            primary: new FakePrimary(), log: silentLog, createChessGame: () => new ChessGame(), now: () => clock.t,
            metrics: new Registry(), autoStart: false,
        });
        const p = (userId, name) => ({ userId, name, rating: 1500, provisional: true });
        const id = host.createGame({ white: p(alice, 'alice'), black: p(bob, 'bob'), rated: true, baseMs: 180000, incMs: 2000 });
        const ew = new FakeEndpoint(1), eb = new FakeEndpoint(2);
        host.attach(id, alice, ew);
        host.attach(id, bob, eb);
        for (const m of ['f2f3', 'e7e5', 'g2g4', 'd8h4']) {
            const room = host.room(id);
            clock.t += 1000;
            host.onClientMessage(id, room.playerOf(room.ply & 1).userId, {
                type: MSG.Move, seq: room.ply + 1, game: id, ply: room.ply, move: uci(m),
                posHash: room.game.position.digest(), thinkMs: 900, drawOffer: false,
            }, room.ply & 1 ? eb : ew);
        }
        const record = host.room(id).record();
        clock.t += 100;
        const pending = host.pollCommits(clock.t);
        assert.equal(typeof pending.then, 'function', 'the commit runs on the thread');
        assert.equal(await pending, true);
        const rated = eb.msgs().find((m) => m.type === MSG.RatingUpdate);
        assert.deepEqual([rated.black.before, rated.black.after, rated.white.after], [1500, 1520, 1480]);
        // Written by the thread's own connection, visible to the shard's.
        assert.equal(store.games.byId(id).status, GS.BlackWins);
        assert.equal(store.ratings.get(bob, '3+2').rating, 1520);

        // A committed game sent again is a duplicate, not a second rating change.
        const again = await writer.finishBatch([record]);
        assert.equal(again[0].duplicate, true);
        assert.equal(store.ratings.get(bob, '3+2').rating, 1520);
        await assert.rejects(writer.finishBatch([{ id: 424242, status: 99 }]), (e) => e.code === 'invalid_record' && e.gameId === 424242);
    } finally {
        await writer.close();
        await assert.rejects(writer.finishBatch([]), /closed/);
        store.close();
        fs.rmSync(dir, { recursive: true, force: true });
    }
});

test('close(): the requests the thread has not answered when the close times out are rejected, not left pending', async () => {
    const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'scacelith-writer-'));
    const config = testConfig({ DB_PATH: path.join(dir, 'scacelith.db'), DATA_DIR: dir });
    const store = openStore(config, { applyGame, log: silentLog });
    migrate(store);
    store.close();
    // Another connection holds the write lock: each commit waits busy_timeout (5 s) for it, so the
    // second one is still waiting when the close times out (7 s).
    const raw = new DatabaseSync(config.dbPath);
    raw.exec('BEGIN IMMEDIATE');
    const writer = startStoreWriter({ config, logging: false });
    try {
        const outcome = (p) => p.then(() => 'resolved', (e) => e.message);
        const first = outcome(writer.finishBatch([{ id: 9001, status: 99 }]));
        let second = 'pending';
        outcome(writer.finishBatch([{ id: 9002, status: 99 }])).then((o) => { second = o; });
        await writer.close();
        await new Promise((resolve) => setImmediate(resolve));
        assert.equal(second, 'store writer closed before answering (outcome unknown)');
        assert.match(await first, /closed before answering|locked/);
    } finally {
        raw.exec('ROLLBACK');
        raw.close();
        fs.rmSync(dir, { recursive: true, force: true });
    }
});

test('close(): a request answered after one busy_timeout wait for the write lock settles with its own outcome', async () => {
    const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'scacelith-writer-'));
    const config = testConfig({ DB_PATH: path.join(dir, 'scacelith.db'), DATA_DIR: dir });
    const store = openStore(config, { applyGame, log: silentLog });
    migrate(store);
    store.close();
    const writer = startStoreWriter({ config, logging: false });
    await writer.finishBatch([]);                        // the thread is up before the lock is taken
    const raw = new DatabaseSync(config.dbPath);
    raw.exec('BEGIN IMMEDIATE');
    let locked = true, timer = null;
    const unlock = () => { if (locked) { locked = false; raw.exec('ROLLBACK'); } };
    try {
        const outcome = (p) => p.then(() => 'resolved', (e) => e.message);
        // The first commit waits busy_timeout (5 s) for the lock and fails; the lock is released
        // 300 ms later, so the second one is answered about 5.3 s after the close started.
        const first = outcome(writer.finishBatch([{ id: 9001, status: 99 }]))
            .then((o) => { timer = setTimeout(unlock, 300); return o; });
        const second = outcome(writer.finishBatch([{ id: 9002, status: 99 }]));
        await writer.close();
        assert.match(await first, /locked/);
        assert.match(await second, /game 9002: invalid/);
    } finally {
        clearTimeout(timer);
        unlock();
        raw.close();
        fs.rmSync(dir, { recursive: true, force: true });
    }
});
