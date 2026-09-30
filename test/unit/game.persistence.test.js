// GameHost with the real modules around it: server chess rules (src/chess), SQLite store
// (src/store), on-disk journal (src/store/journal.js) and Elo (src/match/elo.js). A finished
// rated game is committed with both ratings and a RatingUpdate; a game in progress survives a
// restart (new journal and host on the same directory) with the same position and clocks.
import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { DatabaseSync } from 'node:sqlite';
import { GameHost, JOURNAL_GATE_TRIES } from '../../src/game/host.js';
import { GameRoom, JournalKind } from '../../src/game/room.js';
import { FakeAnticheat, FakePrimary, FakeEndpoint, FakeChessGame, FakeStore, fakeMove, silentLog } from '../../src/game/testing.js';
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
        // Rated records (10 games, K 40): a newcomer's first games are its unrated phase.
        const raw = new DatabaseSync(s.config.dbPath);
        const seed = raw.prepare(`INSERT INTO ratings (user_id, category, rating, games, wins, peak, rated, updated_at)
            VALUES (?, '3+2', 1500, 10, 5, 1500, 1, 0)`);
        for (const u of [alice, bob]) seed.run(u);
        raw.close();
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
        // The journal still holds the game's last records: the commit waits for their flush.
        assert.equal(s.journal.hasUnwritten(), true);
        const commit = s.host.pollCommits(s.clock.t);
        assert.equal(typeof commit.then, 'function');
        assert.equal(await commit, true);
        const rated = eb.msgs().find((m) => m.type === MSG.RatingUpdate);
        assert.ok(rated, 'RatingUpdate sent after the commit');
        assert.deepEqual([rated.black.before, rated.black.after, rated.white.after], [1500, 1520, 1480]);
        const row = s.store.games.byId(g1);
        assert.equal(row.status, GS.BlackWins);
        assert.equal(s.store.ratings.get(bob, '3+2').rating, 1520);
        assert.equal(s.store.ratings.get(alice, '3+2').games, 11);
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

// A host on a real journal whose store copies the journal directory when finishBatch is called:
// what a crash right after the database commit would find.
async function commitRig(dir, { log = silentLog } = {}) {
    const config = testConfig();
    const journal = await openJournal({ dir: path.join(dir, 'journal'), shard: 0, flushMs: 1000, fsync: false, log: silentLog });
    const store = new FakeStore();
    const copies = [];
    const finishBatch = store.games.finishBatch;
    store.games.finishBatch = (records) => {
        const to = path.join(dir, `crash-${copies.length}`);
        fs.cpSync(path.join(dir, 'journal'), to, { recursive: true });
        copies.push({ dir: to, ids: records.map((r) => r.id) });
        return finishBatch(records);
    };
    const clock = { t: T0 };
    const createChessGame = () => new FakeChessGame();
    const primary = new FakePrimary();
    const registry = new Registry();
    const host = new GameHost({
        shard: 0, config, store, journal, anticheat: new FakeAnticheat(), primary, log,
        createChessGame, now: () => clock.t, metrics: registry, autoStart: false,
    });
    const pl = (id) => ({ userId: id, name: `user${id}`, rating: 1500, provisional: false });
    const newGame = (w) => host.createGame({ white: pl(w), black: pl(w + 1), rated: true, baseMs: 180000, incMs: 2000 });
    const play = (id, n) => {
        for (let i = 0; i < n; i++) {
            const room = host.room(id);
            clock.t += 500;
            host.onClientMessage(id, room.playerOf(room.ply & 1).userId, {
                type: MSG.Move, seq: 1, game: id, ply: room.ply, move: fakeMove(room.ply), posHash: room.game.position.digest(), thinkMs: 0, drawOffer: false,
            }, null);
        }
    };
    const resign = (id) => host.onClientMessage(id, host.room(id).white.userId, { type: MSG.Resign, seq: 2, game: id }, null);
    // The games a restart from a crash copy brings back as running.
    const running = async (copyDir) => {
        const j = await openJournal({ dir: copyDir, shard: 0, flushMs: 1000, fsync: false, log: silentLog });
        try {
            const ids = [];
            for (const [id, recs] of j.recover()) if (!GameRoom.fromJournal(recs, { config, createChessGame }).isOver) ids.push(id);
            return ids;
        } finally {
            await j.close();
        }
    };
    return { journal, store, copies, clock, host, primary, registry, newGame, play, resign, running };
}

// Closes the rig's journal, even when a failed assertion left a write pending.
async function closeRig(r) {
    if (!r) return;
    delete r.journal.writeBatch;
    let timer;
    await Promise.race([r.journal.close().catch(() => {}), new Promise((res) => { timer = setTimeout(res, 2000); })]);
    clearTimeout(timer);
}

test('the database never has a finished game before the journal has its ended record: a crash in between cannot bring it back running', async () => {
    const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'scacelith-gp-'));
    let r;
    try {
        r = await commitRig(dir);
        const a = r.newGame(1), b = r.newGame(3);
        r.play(a, 6);
        r.play(b, 6);
        await r.journal.flush();
        // A ends: its ended record is still in the journal's buffer when the commit is due.
        r.resign(a);
        assert.equal(r.journal.hasUnwritten(), true);
        r.clock.t += 100;
        const p = r.host.pollCommits(r.clock.t);
        assert.equal(typeof p.then, 'function');
        assert.equal(r.store.batches.length, 0, 'the commit waits for the journal');
        assert.equal(await p, true);
        assert.deepEqual(r.store.committedIds, [a]);
        // B ends while the batch holding its ended record is being written.
        let release;
        const gate = new Promise((res) => { release = res; });
        const write = r.journal.writeBatch;
        r.journal.writeBatch = async function (...args) { await gate; return write.apply(this, args); };
        r.resign(b);
        const flushing = r.journal.flush();
        assert.deepEqual([r.journal.stats().pendingBytes, r.journal.hasUnwritten()], [0, true], 'the write is in flight');
        r.clock.t += 100;
        const q = r.host.pollCommits(r.clock.t);
        await new Promise((res) => setImmediate(res));
        assert.equal(r.store.batches.length, 1, 'the commit waits for the write in flight');
        release();
        await flushing;
        assert.equal(await q, true);
        delete r.journal.writeBatch;
        assert.deepEqual(r.store.committedIds, [a, b]);
        // A crash right after each database commit: no committed game comes back running.
        const inDb = new Set();
        for (const c of r.copies) {
            for (const id of c.ids) inDb.add(id);
            assert.deepEqual((await r.running(c.dir)).filter((id) => inDb.has(id)), [], `crash after committing ${c.ids}`);
        }
        // With nothing left to write, the commit is immediate (synchronous store).
        const c = r.newGame(5);
        r.play(c, 2);
        r.resign(c);
        await r.journal.flush();
        r.clock.t += 100;
        assert.equal(r.host.pollCommits(r.clock.t), true);
    } finally {
        await closeRig(r);
        fs.rmSync(dir, { recursive: true, force: true });
    }
});

test('a journal write that fails before the commit: the games are journaled again, and committed once that is written', async () => {
    const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'scacelith-gp-'));
    const enospc = () => Object.assign(new Error('ENOSPC: no space left on device'), { code: 'ENOSPC' });
    let r;
    try {
        r = await commitRig(dir);
        // 1. The flush the commit waits for fails.
        const a = r.newGame(1);
        r.play(a, 6);
        await r.journal.flush();
        r.resign(a);
        r.journal.writeBatch = async () => { delete r.journal.writeBatch; throw enospc(); };
        r.clock.t += 100;
        assert.equal(await r.host.pollCommits(r.clock.t), false);
        assert.equal(r.store.batches.length, 0, 'not committed');
        assert.ok(r.journal.snapsPending.has(a), 'the game is journaled again (a snapshot)');
        assert.equal(r.host.pollCommits(r.clock.t + 50), null, 'backing off');
        r.clock.t += r.host.backoffMs;
        assert.equal(await r.host.pollCommits(r.clock.t), true);
        assert.deepEqual(r.store.committedIds, [a]);
        assert.deepEqual(await r.running(r.copies[0].dir), [], 'the crash copy has the game over (its snapshot)');

        // 2. The write in flight fails, the next one (which the commit waits for) succeeds.
        const b = r.newGame(3), c = r.newGame(5);
        r.play(b, 6);
        r.play(c, 2);
        await r.journal.flush();
        r.resign(b);
        let fail;
        const gate = new Promise((res) => { fail = res; });
        r.journal.writeBatch = async () => { delete r.journal.writeBatch; await gate; throw enospc(); };
        const lost = r.journal.flush().catch((e) => e.code);
        r.play(c, 1);                                   // the next batch
        r.clock.t += 100;
        const q = r.host.pollCommits(r.clock.t);
        fail();
        assert.equal(await lost, 'ENOSPC');
        assert.equal(await q, false, 'a write failed meanwhile: not committed');
        assert.deepEqual(r.store.committedIds, [a]);
        r.clock.t += r.host.backoffMs;
        assert.equal(await r.host.pollCommits(r.clock.t), true);
        assert.deepEqual(r.store.committedIds, [a, b]);
        assert.deepEqual((await r.running(r.copies.at(-1).dir)).sort(), [c], 'only the running game comes back running');

        // 3. The write holding the ended record failed before the commit was due, and nothing is
        // left to write when it is: the game is journaled again first.
        const d = r.newGame(7);
        r.play(d, 6);
        await r.journal.flush();
        r.resign(d);
        r.journal.writeBatch = async () => { delete r.journal.writeBatch; throw enospc(); };
        assert.equal(await r.journal.flush().catch((e) => e.code), 'ENOSPC');
        assert.equal(r.journal.hasUnwritten(), false);
        r.clock.t += 100;
        const s = r.host.pollCommits(r.clock.t);
        assert.equal(typeof s.then, 'function', 'the commit waits for the snapshot');
        assert.equal(await s, true);
        assert.deepEqual(r.store.committedIds, [a, b, d]);
        assert.deepEqual((await r.running(r.copies.at(-1).dir)).sort(), [c], 'the crash copy has the game over');
    } finally {
        await closeRig(r);
        fs.rmSync(dir, { recursive: true, force: true });
    }
});

// ---- a journal that fails: the limits of the commit gate --------------------------------------

const eio = () => Object.assign(new Error('EIO: i/o error, write'), { code: 'EIO' });

function metricValue(registry, name) {
    const m = registry.metrics.get(name);
    if (!m) return undefined;
    for (const c of m.children.values()) return c.value;
    return 0;
}

// A logger that keeps what it is given (level, message, fields).
function recordingLog() {
    const events = [];
    const log = {
        debug() {}, security() {}, debugEnabled: false,
        info(msg, f) { events.push(['info', msg, f]); },
        warn(msg, f) { events.push(['warn', msg, f]); },
        error(msg, f) { events.push(['error', msg, f]); },
        child() { return log; },
    };
    return { log, events, count: (level, re) => events.filter(([l, m]) => l === level && re.test(m)).length };
}

// The games a restart from a copy of the journal (taken now) would take back, finished or not.
async function recoveredIds(dir, tag) {
    const to = path.join(dir, `copy-${tag}`);
    fs.cpSync(path.join(dir, 'journal'), to, { recursive: true });
    const j = await openJournal({ dir: to, shard: 0, flushMs: 1000, fsync: false, log: silentLog });
    try { return [...j.recover().keys()]; } finally { await j.close(); }
}

test('a failed flush journals again only the games of the batch; the other games waiting for their commit get their snapshot before their own', async () => {
    const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'scacelith-gp-'));
    let r;
    try {
        r = await commitRig(dir);
        r.host.commitBatchMax = 1;
        const ids = [r.newGame(1), r.newGame(3), r.newGame(5)];
        for (const id of ids) r.play(id, 6);
        await r.journal.flush();
        for (const id of ids) r.resign(id);            // three ended records in the buffer
        const snaps = [];
        const append = r.journal.append;
        r.journal.append = function (kind, gameId, ...rest) {
            if (kind === JournalKind.Snapshot) snaps.push(gameId);
            return append.call(this, kind, gameId, ...rest);
        };
        // The write holding the three ended records fails.
        r.journal.writeBatch = async () => { delete r.journal.writeBatch; throw eio(); };
        r.clock.t += 100;
        assert.equal(await r.host.pollCommits(r.clock.t), false);
        assert.deepEqual(snaps, [ids[0]], 'only the batch is journaled again at once');
        for (let i = 0; i < ids.length; i++) {
            r.clock.t += Math.max(100, r.host.backoffMs);
            assert.equal(await r.host.pollCommits(r.clock.t), true);
        }
        assert.deepEqual(r.store.committedIds, ids);
        assert.deepEqual(snaps, ids, 'each game is journaled again right before its own commit');
        // A crash right after each database commit: no committed game comes back running.
        const inDb = new Set();
        for (const c of r.copies) {
            for (const id of c.ids) inDb.add(id);
            assert.deepEqual((await r.running(c.dir)).filter((id) => inDb.has(id)), [], `crash after committing ${c.ids}`);
        }
    } finally {
        await closeRig(r);
        fs.rmSync(dir, { recursive: true, force: true });
    }
});

test('a journal whose writes keep failing: after a few failed flushes finished games are committed without it (game.ended, RatingUpdate), until a flush writes again', async () => {
    const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'scacelith-gp-'));
    const rec = recordingLog();
    const unjournaled = /without waiting for the journal/;
    let r;
    try {
        r = await commitRig(dir, { log: rec.log });
        const a = r.newGame(1);
        const ew = new FakeEndpoint(1), eb = new FakeEndpoint(2);
        r.host.attach(a, 1, ew);
        r.host.attach(a, 2, eb);
        r.play(a, 6);
        await r.journal.flush();
        // From now on every journal write fails (a full disk, a read-only volume, EMFILE...).
        r.journal.writeBatch = async () => { throw eio(); };
        r.resign(a);
        for (let i = 1; i < JOURNAL_GATE_TRIES; i++) {
            r.clock.t += Math.max(100, r.host.backoffMs);
            assert.equal(await r.host.pollCommits(r.clock.t), false, `attempt ${i} waits for the journal`);
        }
        assert.deepEqual([r.store.committedIds, r.primary.of('game.ended').length], [[], 0]);
        r.clock.t += r.host.backoffMs;
        assert.equal(await r.host.pollCommits(r.clock.t), true, 'the last attempt commits without the journal');
        assert.deepEqual(r.store.committedIds, [a]);
        assert.deepEqual(r.primary.of('game.ended').map((e) => [e.gameId, e.rated]), [[a, true]]);
        for (const ep of [ew, eb]) assert.ok(ep.msgs().some((m) => m.type === MSG.RatingUpdate && m.game === a), 'a RatingUpdate for each player');
        assert.equal(metricValue(r.registry, 'scacelith_game_commit_unjournaled_total'), 1);

        // While it lasts, a commit does not wait for the journal; one error for the episode.
        const b = r.newGame(3);
        r.play(b, 2);
        r.resign(b);
        r.clock.t += 100;
        assert.equal(r.host.pollCommits(r.clock.t), true, 'committed at once');
        assert.deepEqual(r.store.committedIds, [a, b]);
        assert.equal(metricValue(r.registry, 'scacelith_game_commit_unjournaled_total'), 2);
        assert.equal(rec.count('error', unjournaled), 1);
        await r.host.journalProbe;                        // its flush failed too
        assert.equal(r.host.unjournaled, true);

        // The journal writes again: the flush started by the next commit ends the episode, and
        // later commits wait for the journal again.
        delete r.journal.writeBatch;
        const c = r.newGame(5);
        r.play(c, 2);
        r.resign(c);
        r.clock.t += 100;
        assert.equal(r.host.pollCommits(r.clock.t), true);
        await r.host.journalProbe;
        assert.equal(r.host.unjournaled, false);
        assert.equal(rec.count('info', /journal writes succeed again/), 1);
        const d = r.newGame(7);
        r.play(d, 2);
        r.resign(d);
        r.clock.t += 100;
        const p = r.host.pollCommits(r.clock.t);
        assert.equal(typeof p.then, 'function', 'the commit waits for the journal again');
        assert.equal(await p, true);
        assert.deepEqual(r.store.committedIds, [a, b, c, d]);
        assert.equal(metricValue(r.registry, 'scacelith_game_commit_unjournaled_total'), 3);
        assert.equal(rec.count('error', unjournaled), 1);
        // Their `committed` records reached the journal: a restart takes none of them back.
        await r.journal.flush();
        assert.deepEqual(await recoveredIds(dir, 'end'), []);

        // A new episode is logged again.
        const e = r.newGame(9);
        r.play(e, 2);
        r.resign(e);
        r.journal.writeBatch = async () => { throw eio(); };
        for (let i = 0; i < JOURNAL_GATE_TRIES; i++) {
            r.clock.t += Math.max(100, r.host.backoffMs);
            await r.host.pollCommits(r.clock.t);
        }
        assert.deepEqual(r.store.committedIds, [a, b, c, d, e]);
        assert.equal(rec.count('error', unjournaled), 2);
    } finally {
        await closeRig(r);
        fs.rmSync(dir, { recursive: true, force: true });
    }
});

test('shutdown with a journal whose writes fail: the finished games are committed without it, and the failed final flush is logged', async () => {
    for (const variant of ['idle', 'commit in flight, asynchronous store']) {
        const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'scacelith-gp-'));
        const rec = recordingLog();
        let r;
        try {
            r = await commitRig(dir, { log: rec.log });
            r.store.async = variant !== 'idle';
            const a = r.newGame(1), b = r.newGame(3);
            const ew = new FakeEndpoint(1);
            r.host.attach(a, 1, ew);
            r.play(a, 6);
            r.play(b, 2);
            await r.journal.flush();
            r.journal.writeBatch = async () => { throw eio(); };
            r.resign(a);
            let inFlight = null;
            if (variant !== 'idle') {
                r.clock.t += 100;
                inFlight = r.host.pollCommits(r.clock.t);
                assert.equal(typeof inFlight.then, 'function');
            }
            await r.host.shutdown();                      // does not throw
            if (inFlight) assert.equal(await inFlight, true, variant);
            assert.deepEqual(r.store.committedIds, [a], variant);
            assert.deepEqual(r.primary.of('game.ended').map((e) => e.gameId), [a], variant);
            assert.ok(ew.msgs().some((m) => m.type === MSG.RatingUpdate), variant);
            assert.equal(metricValue(r.registry, 'scacelith_game_commit_unjournaled_total'), 1, variant);
            assert.equal(rec.count('error', /journal flush failed at shutdown/), 1, variant);
            assert.equal(r.host.pending.size, 0, variant);
            assert.equal(r.host.room(b).isOver, false, 'a running game is left to the journal');
        } finally {
            await closeRig(r);
            fs.rmSync(dir, { recursive: true, force: true });
        }
    }
});
