// Journal compaction ('snapshot' records of src/store/journal.js, GameRoom.journalSnapshot,
// GameHost.compactJournal): a snapshot supersedes its game's earlier records, a shard's journal
// stays bounded while long games run, and a crash at any point of a compaction (before the
// snapshot is written, after it, in the middle of the deletions, with a torn tail, or a SIGKILL at
// a random moment) recovers every game exactly and never brings a committed game back. Also: with
// fsync on, deletions wait for the records that allow them and are ordered by directory fsyncs; a
// journal written before compaction existed is read as it is, then compacted; restarts in place.
import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import fsp from 'node:fs/promises';
import os from 'node:os';
import path from 'node:path';
import { spawn } from 'node:child_process';
import { openJournal, crc32c, JournalKind as JK } from '../../src/store/journal.js';
import { GameRoom, JournalKind, JournalError } from '../../src/game/room.js';
import { GameHost } from '../../src/game/host.js';
import { FakeChessGame, fakeMove, MemoryJournal, FakeStore, FakeAnticheat, FakePrimary, FakeEndpoint, silentLog } from '../../src/game/testing.js';
import { ChessGame, parseSquare } from '../../src/chess/index.js';
import { MSG, encodeMove } from '../../src/protocol/index.js';
import { Registry, metrics } from '../../src/metrics.js';
import { testConfig } from '../../src/config.js';

const JOURNAL_URL = new URL('../../src/store/journal.js', import.meta.url).href;
const { Created, Move, Ended, Committed, Snapshot } = JK;
const W = 0, B = 1;
const CFG = testConfig();
const T0 = 1_800_000_000_000;
const ROOM_OPTS = { config: CFG, createChessGame: () => new FakeChessGame() };
const quiet = { debug() {}, info() {}, warn() {}, error() {} };
const byNum = (a, b) => a - b;
const player = (id) => ({ userId: id, name: `user${id}`, rating: 1500, provisional: false });

function tmpDir(tag = 'jc') { return fs.mkdtempSync(path.join(os.tmpdir(), `scacelith-${tag}-`)); }
function segments(dir, shard = 0) {
    return fs.readdirSync(path.join(dir, `shard-${shard}`)).filter((n) => /^segment-\d+\.log$/.test(n)).sort();
}
function segName(seq) { return `segment-${String(seq).padStart(10, '0')}.log`; }
function segPath(dir, name, shard = 0) { return path.join(dir, `shard-${shard}`, name); }
function bytesOnDisk(dir) { return segments(dir).reduce((s, n) => s + fs.statSync(segPath(dir, n)).size, 0); }
function gauge(name) { return metrics.metrics.get(name).root.value; }
function seqOf(name) { return parseInt(/^segment-(\d+)\.log$/.exec(name)[1], 10); }

// One record in the format of the header of src/store/journal.js, written without it (a segment
// as any version of the server writes it).
function encodeRecord(kind, gameId, at, payload) {
    const p = Buffer.from(payload);
    const b = Buffer.alloc(25 + p.length);
    b.writeUInt32LE(17 + p.length, 0);
    b[8] = kind;
    b.writeUInt32LE(gameId % 2 ** 32, 9);
    b.writeUInt32LE(Math.floor(gameId / 2 ** 32), 13);
    b.writeDoubleLE(at, 17);
    p.copy(b, 25);
    b.writeUInt32LE(crc32c(b, 8), 4);
    return b;
}
function writeSegment(dir, seq, records, shard = 0) {
    fs.mkdirSync(path.join(dir, `shard-${shard}`), { recursive: true });
    fs.writeFileSync(segPath(dir, segName(seq), shard), Buffer.concat(records.map((r) => encodeRecord(...r))));
}

// Records the segment deletions and the directory fsyncs of a journal, in order.
function spyDeletions(j, events) {
    const unlink = fs.unlinkSync;
    fs.unlinkSync = (p, ...rest) => { events.push(`unlink ${path.basename(p)}`); return unlink(p, ...rest); };
    const proto = Object.getPrototypeOf(j);
    j.syncDir = function () { events.push('syncDir'); return proto.syncDir.call(this); };
    return () => { fs.unlinkSync = unlink; delete j.syncDir; };
}

// Everything of a room that its journal records carry, and its next deadline.
function fingerprint(room) {
    return {
        records: room.journalState().map((r) => [r.kind, r.at, Buffer.from(r.payload).toString('hex')]),
        next: room.isOver ? null : room.nextDeadline(),
    };
}

// Everything a replay must reproduce (the round-trip averages are not journaled).
function state(room, t) {
    const n = room.ply;
    const arr = (a) => Array.from(a.subarray(0, n));
    return {
        ply: n, moves: arr(room.moves), mflags: arr(room.mflags), mbits: arr(room.mbits), spent: arr(room.spent),
        clockAfter: arr(room.clockAfter), quotaAfter: arr(room.quotaAfter), gseqMove: arr(room.gseqMove), recvTime: arr(room.recvTime),
        gseq: room.gseq, drawOffer: room.drawOffer, drawOffersUsed: [...room.drawOffersUsed], drawDeclinedAt: [...room.drawDeclinedAt],
        desyncs: [...room.desyncs], connected: [...room.connected], disconnectedAt: [...room.disconnectedAt],
        clockMs: [...room.clock.ms], quota: [...room.clock.quota], turnStart: room.clock.turnStart,
        isOver: room.isOver, result: room.result, endGseq: room.endGseq, culprit: room.culprit, flags: room.flags,
        nextDeadline: room.nextDeadline(), digest: room.game.position.digest(),
        snapshotW: room.snapshot(W, t), snapshotB: room.snapshot(B, t),
    };
}

// A long custom game (3 h + 180 s) whose journal records are collected like the host appends them.
function journaledRoom() {
    const room = new GameRoom({
        id: 4242, category: 'custom', baseMs: 10_800_000, incMs: 180_000, rated: false,
        white: player(5), black: { ...player(6), provisional: true }, createdAt: T0, ...ROOM_OPTS,
    });
    const log = [room.createdRecord()];
    const run = (out) => { for (const r of out.journal) log.push({ kind: r.kind, at: r.at, payload: Buffer.from(r.payload) }); return out; };
    const mv = (t, extra = {}) => run(room.onMove(room.ply & 1, {
        seq: 1, ply: room.ply, move: fakeMove(room.ply), posHash: room.game.position.digest(), thinkMs: 0, drawOffer: false, ...extra,
    }, t));
    return { room, log, run, mv };
}

// The snapshot payload format documented in room.js, written independently.
function encodeSnapshot(records) {
    const parts = [Buffer.from([1, 0, records.length & 0xff, records.length >> 8])];
    for (const r of records) {
        const h = Buffer.alloc(13);
        h[0] = r.kind;
        h.writeUInt32LE(r.payload.length, 1);
        h.writeDoubleLE(r.at, 5);
        parts.push(h, Buffer.from(r.payload));
    }
    return Buffer.concat(parts);
}

// ---- GameRoom: the snapshot record ------------------------------------------------------------

test('journalSnapshot(): one record that replays to the same room, alone or followed by later records', () => {
    const j = journaledRoom();
    let t = T0;
    for (let i = 0; i < 30; i++) j.mv((t += 5000), { thinkMs: 4000 });
    j.run(j.room.onDrawOffer(B, (t += 100)));
    j.run(j.room.onDrawAnswer(W, false, (t += 100)));
    j.run(j.room.onDisconnect(W, (t += 100)));
    j.run(j.room.onMove(j.room.ply & 1, { seq: 1, ply: j.room.ply + 3, move: 1, posHash: 0, thinkMs: 0, drawOffer: false }, (t += 10)));
    assert.equal(j.room.desyncs[j.room.ply & 1], 1);
    const snap = j.room.journalSnapshot(t + 0.7);
    assert.equal(snap.kind, JournalKind.Snapshot);
    assert.equal(snap.at, t);
    assert.ok(snap.payload.length < 30 * 45 + 500, `about 45 bytes per ply (${snap.payload.length} bytes)`);
    assert.deepEqual(encodeSnapshot(j.room.journalState()), snap.payload, 'the documented format');
    assert.deepEqual(state(GameRoom.fromJournal([snap], ROOM_OPTS), t), state(j.room, t));

    // The game goes on after the snapshot.
    const cut = j.log.length;
    j.run(j.room.onReconnect(W, (t += 2000)));
    for (let i = 0; i < 6; i++) j.mv((t += 3000), { drawOffer: i === 2 });
    const later = j.log.slice(cut);
    // Whatever precedes the snapshot is ignored (a crash can leave some of it on disk, or none).
    const junk = { kind: Move, at: 0, payload: Buffer.alloc(32, 0xee) };
    for (const before of [[], j.log.slice(0, cut), j.log.slice(0, 3), [junk, junk]]) {
        assert.deepEqual(state(GameRoom.fromJournal([...before, snap, ...later], ROOM_OPTS), t), state(j.room, t));
    }
    // The latest snapshot wins.
    const snap2 = j.room.journalSnapshot(t);
    assert.deepEqual(state(GameRoom.fromJournal([...j.log.slice(0, cut), snap, ...later, snap2], ROOM_OPTS), t), state(j.room, t));
    // A restart from a snapshot is the same as a restart from the full journal.
    const full = GameRoom.fromJournal(j.log, ROOM_OPTS), compact = GameRoom.fromJournal([snap2], ROOM_OPTS);
    const of = full.recover(t + 60000), oc = compact.recover(t + 60000);
    assert.deepEqual(oc.journal.map((r) => [r.kind, r.at, r.payload]), of.journal.map((r) => [r.kind, r.at, r.payload]));
    assert.deepEqual(state(compact, t + 61000), state(full, t + 61000));
});

test('journalSnapshot() of a finished game waiting for its commit keeps its result and record', () => {
    const j = journaledRoom();
    let t = T0;
    for (let i = 0; i < 12; i++) j.mv((t += 1000));
    j.run(j.room.onResign(B, (t += 500)));
    const copy = GameRoom.fromJournal([...j.log, j.room.journalSnapshot(t + 10)], ROOM_OPTS);
    assert.equal(copy.isOver, true);
    assert.deepEqual(copy.result, j.room.result);
    assert.equal(copy.gseq, j.room.gseq);
    assert.deepEqual(copy.record(), j.room.record());
});

test('bad snapshot records are refused, strict or not', () => {
    const j = journaledRoom();
    j.mv(T0 + 1000);
    const snap = j.room.journalSnapshot(T0 + 2000);
    const bad = (payload) => ({ kind: Snapshot, at: 0, payload });
    const payloads = [
        Buffer.alloc(0), Buffer.from([2, 0, 0, 0]), Buffer.from([1, 0, 0, 0]),
        snap.payload.subarray(0, snap.payload.length - 1), Buffer.concat([snap.payload, Buffer.from([0])]),
    ];
    for (const p of payloads) {
        assert.throws(() => GameRoom.fromJournal([bad(p)], ROOM_OPTS), JournalError);
        assert.throws(() => GameRoom.fromJournal([j.log[0], bad(p)], { ...ROOM_OPTS, strict: false }), JournalError);
    }
    assert.throws(() => GameRoom.fromJournal([bad(encodeSnapshot([j.log[1]]))], ROOM_OPTS), /first record is not `created`/);
    assert.throws(() => GameRoom.fromJournal([bad(encodeSnapshot([j.log[0], snap]))], ROOM_OPTS), /snapshot inside a snapshot/);
});

test('GameHost: recovery from a snapshot in the middle of a game\'s records (in-memory journal, no compaction there)', () => {
    const journal = new MemoryJournal();
    const clock = { t: T0 };
    const mk = () => new GameHost({
        shard: 0, config: CFG, store: new FakeStore(), journal, anticheat: new FakeAnticheat(), primary: new FakePrimary(), log: silentLog,
        createChessGame: () => new FakeChessGame(), now: () => clock.t, metrics: new Registry(), autoStart: false,
    });
    const a = mk();
    const id = a.createGame({ white: player(1), black: player(2), rated: false, baseMs: 600000, incMs: 5000 });
    const play = (h) => {
        const room = h.room(id);
        clock.t += 1000;
        h.onClientMessage(id, room.playerOf(room.ply & 1).userId, { type: MSG.Move, seq: 1, game: id, ply: room.ply, move: fakeMove(room.ply), posHash: room.game.position.digest(), thinkMs: 0, drawOffer: false }, null);
    };
    for (let i = 0; i < 10; i++) play(a);
    assert.equal(a.compactJournal(clock.t), 0, 'a journal without compactionCandidates is never compacted');
    const snap = a.room(id).journalSnapshot(clock.t);
    journal.append(snap.kind, id, snap.payload, snap.at);
    journal.games.get(id)[2].payload = Buffer.alloc(32, 0xff);     // an old record past repair: superseded
    for (let i = 0; i < 3; i++) play(a);
    const t = clock.t;
    assert.deepEqual(state(GameRoom.fromJournal(journal.recover().get(id), ROOM_OPTS), t), state(a.room(id), t));
    const b = mk();
    assert.equal(b.recover(), 1);
    assert.deepEqual(b.room(id).moveList, a.room(id).moveList);
    assert.equal(b.counts.recovered, 1);
});

// ---- Journal: bookkeeping of snapshots ----------------------------------------------------------

test('a snapshot supersedes its game\'s earlier records and releases their segments; stale games are queued at open', async () => {
    const dir = tmpDir();
    const open = () => openJournal({ dir, log: quiet, flushMs: 1000, fsync: false, segmentBytes: 1, compactSegments: 2 });
    const snapshotsBefore = metrics.metrics.get('scacelith_journal_snapshots_total').root.value;
    // segmentBytes 1: every flushed batch goes to its own segment.
    let j = await open();
    j.append(Created, 1, 'c1', 1);
    j.append(Created, 2, 'c2', 1);
    await j.flush();                                   // segment 1: games 1, 2
    j.append(Move, 1, 'm1', 2);
    await j.flush();                                   // segment 2
    assert.deepEqual(j.compactionCandidates(), [], 'nothing is 2 segments old yet');
    j.append(Move, 1, 'm2', 3);
    await j.flush();                                   // segment 3: both games are 2 segments old
    assert.deepEqual([...j.compactionCandidates()].sort(byNum), [1, 2]);
    assert.deepEqual(j.compactionCandidates(), [], 'handed out once');
    j.append(Snapshot, 1, 's1', 4);                    // only game 1 is snapshotted
    j.append(Move, 1, 'm3', 5);
    await j.flush();                                   // segment 4
    assert.deepEqual(segments(dir), [segName(1), segName(4)], 'segments 2 and 3 only held game 1; segment 1 still holds game 2');
    assert.equal(j.stats().snapshots, 1);
    assert.equal(j.stats().diskBytes, bytesOnDisk(dir));
    assert.equal(gauge('scacelith_journal_segments'), 2);
    assert.equal(gauge('scacelith_journal_disk_bytes'), bytesOnDisk(dir));
    assert.equal(metrics.metrics.get('scacelith_journal_snapshots_total').root.value, snapshotsBefore + 1);
    await j.close();

    j = await open();
    const rec = j.recover();
    assert.deepEqual(rec.get(1).map((r) => [r.kind, r.payload.toString()]), [[Snapshot, 's1'], [Move, 'm3']]);
    assert.deepEqual(rec.get(2).map((r) => [r.kind, r.payload.toString()]), [[Created, 'c2']]);
    assert.equal(j.stats().recovery.snapshots, 1);
    assert.deepEqual([...j.compactionCandidates()], [2], 'game 2 is queued at open (segment 1 is 3 behind)');
    j.append(Snapshot, 2, 's2', 6);
    await j.flush();                                   // segment 5
    assert.deepEqual(segments(dir), [segName(4), segName(5)]);
    await j.close();
    j = await open();
    assert.deepEqual(j.recover().get(2).map((r) => r.payload.toString()), ['s2']);
    assert.deepEqual(j.compactionCandidates(), []);
    await j.close();
    fs.rmSync(dir, { recursive: true, force: true });
});

test('the committed record outlives the old segments a snapshotted game left records in (no resurrection)', async () => {
    const dir = tmpDir();
    const open = (d = dir) => openJournal({ dir: d, log: quiet, flushMs: 1000, fsync: false, segmentBytes: 1, compactSegments: 2 });
    const A = 10, Bg = 20;
    const j = await open();
    j.append(Created, A, 'A', 1);
    j.append(Created, Bg, 'B', 1);
    await j.flush();                                   // 1: A and B created
    j.append(Move, A, 'a1', 2);
    await j.flush();                                   // 2
    j.append(Move, A, 'a2', 3);
    await j.flush();                                   // 3
    assert.deepEqual([...j.compactionCandidates()].sort(byNum), [A, Bg]);
    j.append(Snapshot, A, 'A@3', 4);
    await j.flush();                                   // 4: A no longer needs 1-3
    assert.deepEqual(segments(dir), [segName(1), segName(4)]);
    j.append(Ended, A, '', 5);
    j.committed(A);
    await j.flush();                                   // 5: A committed
    assert.deepEqual(segments(dir), [segName(1), segName(5)],
        'segment 5 (A committed) outlives segment 1, which still holds A\'s created record');

    // A crash now: A stays committed, B comes back.
    const copy = tmpDir('jc-copy');
    fs.cpSync(path.join(dir, 'shard-0'), path.join(copy, 'shard-0'), { recursive: true });
    const c = await open(copy);
    assert.deepEqual([...c.recover().keys()], [Bg]);
    await c.close();
    fs.rmSync(copy, { recursive: true, force: true });

    // B's snapshot releases segment 1; then segment 5 has nothing left to protect.
    assert.deepEqual([...j.compactionCandidates()], [Bg], 'B was queued again at a later rotation');
    j.append(Snapshot, Bg, 'B@5', 6);
    await j.flush();                                   // 6
    assert.deepEqual(segments(dir), [segName(6)]);
    assert.equal(j.stats().diskBytes, bytesOnDisk(dir));
    await j.close();
    fs.rmSync(dir, { recursive: true, force: true });
});

test('compaction candidates: a few per batch, never a committed game or one with a pending snapshot, queued again after a failed write', async () => {
    const dir = tmpDir();
    const j = await openJournal({ dir, log: quiet, flushMs: 1000, fsync: false, segmentBytes: 1, compactSegments: 2, compactPerFlush: 3 });
    for (let g = 1; g <= 7; g++) j.append(Created, g, `c${g}`, g);
    await j.flush();                                   // 1
    j.append(Ended, 7, '', 8);
    j.committed(7);
    await j.flush();                                   // 2: game 7 committed
    j.append(Move, 1, 'm', 9);
    await j.flush();                                   // 3: the games of segment 1 are 2 segments old
    const c1 = [...j.compactionCandidates(2)];
    assert.equal(c1.length, 2, 'at most `max` per call');
    for (const g of c1) j.append(Snapshot, g, 'snap', 10);
    c1.push(...j.compactionCandidates());
    assert.equal(c1.length, 3, 'and at most compactPerFlush per batch, counting the snapshots already appended');
    j.append(Snapshot, c1[2], 'snap', 10);
    assert.deepEqual(j.compactionCandidates(), [], 'the batch already holds 3 snapshots');
    await j.flush();                                   // 4
    const c2 = [...j.compactionCandidates()];
    assert.equal(c2.length, 3);
    assert.deepEqual([...c1, ...c2].sort(byNum), [1, 2, 3, 4, 5, 6], 'never the committed game 7');

    // A failed write: the snapshots are not known to be durable, the games keep their segments.
    for (const g of c2) j.append(Snapshot, g, 'snap', 11);
    j.writeBatch = async () => { throw Object.assign(new Error('EIO: i/o error'), { code: 'EIO' }); };
    await assert.rejects(j.flush(), /EIO/);
    delete j.writeBatch;
    assert.ok(segments(dir).includes(segName(1)), 'segment 1 is still needed');
    assert.deepEqual([...j.compactionCandidates()].sort(byNum), [...c2].sort(byNum), 'queued again');

    // A game handed out but not snapshotted (the host no longer has it) is queued again at the
    // next rotation; the games snapshotted in segment 4 are not stale yet.
    const skipped = c2[0];
    for (const g of c2.slice(1)) j.append(Snapshot, g, 'snap', 12);
    assert.deepEqual(j.compactionCandidates(), [], 'handed out already');
    await j.flush();                                   // 5 (a new segment after the failure)
    assert.ok(segments(dir).includes(segName(1)), 'the skipped game still needs segment 1');
    assert.deepEqual([...j.compactionCandidates()], [skipped]);
    j.append(Snapshot, skipped, 'snap', 14);
    await j.flush();                                   // 6
    assert.ok(!segments(dir).includes(segName(1)));
    assert.deepEqual(segments(dir), [segName(4), segName(5), segName(6)]);
    const closing = j.close();
    assert.deepEqual(j.compactionCandidates(), [], 'nothing while closing');
    await closing;
    fs.rmSync(dir, { recursive: true, force: true });
});

test('a game that ends and is committed while its snapshot is written is never brought back, whatever the order of the records', async () => {
    for (const order of ['snapshot, then commit in the next batch', 'commit, then snapshot in the same batch']) {
        const dir = tmpDir();
        const open = (d = dir) => openJournal({ dir: d, log: quiet, flushMs: 1000, fsync: false, segmentBytes: 1, compactSegments: 1 });
        const recoveredFrom = async () => {               // a crash now (a copy of the directory reopened)
            const copy = tmpDir('jc-copy');
            fs.cpSync(path.join(dir, 'shard-0'), path.join(copy, 'shard-0'), { recursive: true });
            const c = await open(copy);
            const keys = [...c.recover().keys()].sort(byNum);
            await c.close();
            fs.rmSync(copy, { recursive: true, force: true });
            return keys;
        };
        const j = await open();
        j.append(Created, 1, 'c1', 1);
        j.append(Created, 2, 'c2', 1);
        await j.flush();                                  // 1
        j.append(Move, 1, 'm', 2);
        await j.flush();                                  // 2: both games are stale
        assert.deepEqual([...j.compactionCandidates()].sort(byNum), [1, 2], order);
        if (order.startsWith('snapshot')) {
            j.append(Snapshot, 1, 's1', 3);
            const writing = j.flush();                    // 3: the snapshot is being written...
            j.append(Ended, 1, '', 4);                    // ... while the game ends and is committed
            j.committed(1);
            await writing;
            assert.deepEqual(await recoveredFrom(), [1, 2], `${order}: game 1 from its snapshot`);
            await j.flush();                              // 4
        } else {
            j.append(Ended, 1, '', 4);
            j.committed(1);
            j.append(Snapshot, 1, 's1', 5);
            await j.flush();                              // 3
        }
        assert.deepEqual(await recoveredFrom(), [2], `${order}: game 1 committed`);
        j.append(Snapshot, 2, 's2', 6);
        await j.flush();
        assert.deepEqual(await recoveredFrom(), [2], order);
        assert.deepEqual(segments(dir), [segName(j.stats().seq)], `${order}: only the newest segment is left`);
        await j.close();
        fs.rmSync(dir, { recursive: true, force: true });
    }
});

// ---- Journal: durability of the deletions, failed rotations -------------------------------------

test('fsync on: a segment holding a committed record is deleted last, after a directory fsync that makes the earlier deletions durable', async () => {
    for (const fsync of [true, false]) {
        const dir = tmpDir();
        // segmentBytes 1: every flushed batch goes to its own segment.
        const j = await openJournal({ dir, log: quiet, flushMs: 1000, fsync, segmentBytes: 1 });
        const events = [];
        const restore = spyDeletions(j, events);
        try {
            j.append(Created, 1, 'g1', 1);
            j.append(Move, 1, 'm1', 2);
            await j.flush();                              // 1: game 1
            j.append(Created, 2, 'g2', 3);
            j.append(Ended, 1, '', 4);
            await j.flush();                              // 2: games 1 and 2
            j.committed(1);
            await j.flush();                              // 3: game 1 committed; segment 1 goes
            assert.deepEqual(segments(dir), [segName(2), segName(3)]);
            j.append(Ended, 2, '', 5);
            await j.flush();                              // 4
            events.length = 0;
            j.committed(2);
            await j.flush();                              // 5: game 2 committed
            assert.deepEqual(segments(dir), [segName(5)]);
            // Segment 3 holds game 1's committed record: it goes after segment 2 (game 1's older
            // records), and with fsync on only once that deletion is durable. The first fsync is
            // the rotation's (segment 5 created).
            const tail = [`unlink ${segName(2)}`, `unlink ${segName(4)}`, ...(fsync ? ['syncDir'] : []), `unlink ${segName(3)}`];
            assert.deepEqual(events, fsync ? ['syncDir', ...tail] : tail, `fsync ${fsync}`);
        } finally {
            restore();
        }
        await j.close();
        fs.rmSync(dir, { recursive: true, force: true });
    }
});

test('fsync on: open() makes the segments it read and the directory durable before it deletes anything on their strength', async () => {
    const dir = tmpDir();
    const write = () => {
        writeSegment(dir, 1, [[Created, 1, 1, 'c1'], [Move, 1, 2, 'm1']]);
        writeSegment(dir, 2, [[Snapshot, 1, 3, 's1'], [Created, 2, 4, 'c2']]);
        writeSegment(dir, 3, [[Ended, 2, 5, ''], [Committed, 2, 6, '']]);
    };
    write();
    // Spies on every FileHandle's datasync, on the journal's directory fsyncs and on the deletions.
    const fh = await fsp.open(segPath(dir, segName(1)), 'r');
    const FH = Object.getPrototypeOf(fh);
    await fh.close();
    const other = tmpDir();
    const probe = await openJournal({ dir: other, log: quiet, fsync: false });
    const proto = Object.getPrototypeOf(probe);
    await probe.close();
    fs.rmSync(other, { recursive: true, force: true });
    const events = [];
    const { datasync } = FH, { syncDir } = proto, unlink = fs.unlinkSync;
    FH.datasync = function (...a) { events.push('datasync'); return datasync.apply(this, a); };
    proto.syncDir = function () { events.push('syncDir'); return syncDir.call(this); };
    fs.unlinkSync = (p, ...rest) => { events.push(`unlink ${path.basename(p)}`); return unlink(p, ...rest); };
    try {
        for (const fsync of [true, false]) {
            events.length = 0;
            const j = await openJournal({ dir, log: quiet, flushMs: 1000, fsync });
            // Game 1's snapshot (segment 2) releases segment 1; segment 3 (game 2 committed) outlives
            // segment 2, which game 1 still needs.
            assert.deepEqual(events, fsync
                ? ['datasync', 'datasync', 'datasync', 'syncDir', `unlink ${segName(1)}`]
                : [`unlink ${segName(1)}`], `fsync ${fsync}`);
            assert.deepEqual([...j.recover().keys()], [1]);
            assert.deepEqual(j.recover().get(1).map((r) => [r.kind, r.payload.toString()]), [[Snapshot, 's1']]);
            assert.deepEqual(segments(dir), [segName(2), segName(3)]);
            await j.close();
            write();
        }
    } finally {
        FH.datasync = datasync;
        proto.syncDir = syncDir;
        fs.unlinkSync = unlink;
        fs.rmSync(dir, { recursive: true, force: true });
    }
});

test('a failed rotation does not leave the previous segment active: it is still deleted once no game needs it', async () => {
    const dir = tmpDir();
    const j = await openJournal({ dir, log: quiet, flushMs: 1000, fsync: false, segmentBytes: 1 });
    j.append(Created, 1, 'g1', 1);
    j.append(Created, 2, 'g2', 1);
    await j.flush();                                      // 1
    const open = fsp.open;
    fsp.open = async (file, flags, ...rest) => {
        if (flags === 'a') throw Object.assign(new Error('EMFILE: too many open files'), { code: 'EMFILE' });
        return open.call(fsp, file, flags, ...rest);
    };
    j.append(Move, 2, 'lost', 2);
    try {
        await assert.rejects(j.flush(), /EMFILE/);        // segment 1 closed, segment 2 not created
    } finally {
        fsp.open = open;
    }
    j.append(Ended, 1, '', 3);
    j.committed(1);
    await j.flush();                                      // 2
    j.append(Ended, 2, '', 4);
    j.committed(2);
    await j.flush();                                      // 3
    assert.deepEqual(segments(dir), [segName(3)]);
    assert.equal(j.stats().diskBytes, bytesOnDisk(dir));
    await j.close();
    fs.rmSync(dir, { recursive: true, force: true });
});

// ---- GameHost + journal: long games, crash points -------------------------------------------------

test('long games over many segments: the journal stays bounded while they run, and every crash point of a compaction recovers each game exactly', async (t) => {
    const dir = tmpDir();
    const scratch = tmpDir('jc-copies');
    const JOPTS = { shard: 0, log: quiet, flushMs: 1000, fsync: false, segmentBytes: 16384, compactSegments: 2, compactPerFlush: 2 };
    const journal = await openJournal({ dir, ...JOPTS });
    const store = new FakeStore();
    const clock = { t: T0 };
    const host = new GameHost({
        shard: 0, config: CFG, store, journal, anticheat: new FakeAnticheat(), primary: new FakePrimary(), log: silentLog,
        createChessGame: () => new FakeChessGame(), now: () => clock.t, metrics: new Registry(), autoStart: false,
    });
    let users = 0, created = 0, copies = 0;
    const newGame = (baseMs, incMs) => host.createGame({ white: player(++users), black: player(++users), rated: false, baseMs, incMs });
    const send = (id, color, msg) => {
        const room = host.room(id);
        host.onClientMessage(id, room.playerOf(color).userId, { seq: 1, game: id, ...msg }, null);
    };
    const play = (id) => {
        const room = host.room(id);
        send(id, room.ply & 1, { type: MSG.Move, ply: room.ply, move: fakeMove(room.ply), posHash: room.game.position.digest(), thinkMs: 500, drawOffer: false });
    };
    const fingerprint = (room) => ({
        records: room.journalState().map((r) => [r.kind, r.at, Buffer.from(r.payload).toString('hex')]),
        next: room.isOver ? null : room.nextDeadline(),
    });
    // What recovery must rebuild: every game not committed (running, or ended and waiting).
    const expectedNow = () => {
        const m = new Map();
        for (const e of host.rooms.values()) if (!e.committed) m.set(e.room.id, fingerprint(e.room));
        return m;
    };
    const copyJournal = (src = dir) => {
        const dst = path.join(scratch, `c${copies++}`);
        fs.cpSync(path.join(src, 'shard-0'), path.join(dst, 'shard-0'), { recursive: true });
        return dst;
    };
    let verified = 0;
    const verify = async (d, expected, label) => {
        const j = await openJournal({ dir: d, ...JOPTS });
        try {
            const rec = j.recover();
            assert.deepEqual([...rec.keys()].sort(byNum), [...expected.keys()].sort(byNum), `${label}: the games not committed, and only them`);
            for (const [id, want] of expected) assert.deepEqual(fingerprint(GameRoom.fromJournal(rec.get(id), ROOM_OPTS)), want, `${label}: game ${id}`);
        } finally {
            await j.close();
            fs.rmSync(d, { recursive: true, force: true });
        }
        verified++;
    };

    // Crash points of one compaction: the batch holds only snapshots (everything else was flushed
    // before), so recovery must rebuild the live state at every point.
    const proto = Object.getPrototypeOf(journal);
    let deepProbes = 0, deletionPoints = 0;
    const deepProbe = async (step) => {
        await journal.flush();
        if (host.compactJournal(clock.t) === 0) return;
        deepProbes++;
        const expected = expectedNow();
        const batchLen = journal.stats().pendingBytes;
        const points = [{ label: `step ${step}, snapshots appended, not written`, dir: copyJournal() }];
        let written = null, inHook = false;
        journal.markSnapshot = function (...args) {      // the batch is written, nothing released yet
            if (!written) written = copyJournal();
            return proto.markSnapshot.apply(this, args);
        };
        const unlink = fs.unlinkSync;
        fs.unlinkSync = (p, ...rest) => {
            if (!inHook) {
                inHook = true;
                try { points.push({ label: `step ${step}, before deleting ${path.basename(p)}`, dir: copyJournal() }); } finally { inHook = false; }
                deletionPoints++;
            }
            return unlink(p, ...rest);
        };
        try { await journal.flush(); } finally {
            fs.unlinkSync = unlink;
            delete journal.markSnapshot;
        }
        assert.ok(written, 'the snapshot batch was written');
        points.push({ label: `step ${step}, after the deletions`, dir: copyJournal() });
        // Torn snapshot batch (power loss before the fsync completed): cut inside it.
        const last = segments(written).at(-1);
        const size = fs.statSync(segPath(written, last)).size;
        const start = size - batchLen;
        for (const cut of [start + 3, start + (batchLen >> 1), size - 1]) {
            const d = copyJournal(written);
            fs.truncateSync(segPath(d, last), cut);
            points.push({ label: `step ${step}, snapshot batch torn at ${cut - start}/${batchLen}`, dir: d });
        }
        points.push({ label: `step ${step}, snapshots written, nothing released`, dir: written });
        for (const p of points) await verify(p.dir, expected, p.label);
    };

    const long = [newGame(10_800_000, 180_000), newGame(10_800_000, 180_000), newGame(10_800_000, 180_000)];
    const firstLong = [...long];
    const short = new Map();                           // id -> plies before its resignation
    let stuck = 0;
    let maxSegs = 0;
    const STEPS = 900;
    try {
        for (let step = 1; step <= STEPS; step++) {
            clock.t += 1000;
            if (step % 2 === 0) for (const id of long) play(id);
            for (const [id, plies] of short) {
                const room = host.room(id);
                if (room.ply < plies) play(id);
                else { send(id, room.ply & 1, { type: MSG.Resign }); short.delete(id); }
            }
            while (short.size < 4) {
                const id = newGame(180_000, 2000);
                short.set(id, 8 + ((created * 7) % 33));
                // One finished game whose database commit keeps failing: it stays in the journal.
                if (++created === 3) { store.badIds.add(id); stuck = id; }
            }
            // Events that only the checkpoint of a snapshot carries (offers, presence, desyncs).
            if (step % 200 === 50) {
                send(long[0], W, { type: MSG.DrawOffer });
                send(long[0], B, { type: MSG.DrawAnswer, accept: false });
            }
            if (step % 150 === 70) host.detach(long[1], host.room(long[1]).black.userId);
            if (step % 150 === 80) host.attach(long[1], host.room(long[1]).black.userId, new FakeEndpoint(step));
            if (step % 100 === 33) {
                const room = host.room(long[2]);
                send(long[2], room.ply & 1, { type: MSG.Move, ply: room.ply, move: fakeMove(room.ply), posHash: 12345, thinkMs: 0, drawOffer: false });
            }
            if (step === 300) long.push(newGame(10_800_000, 180_000));           // a younger long game
            if (step === 600) { send(long[2], host.room(long[2]).ply & 1, { type: MSG.Resign }); long.splice(2, 1); }
            host.runTimers(clock.t);
            host.pollCommits(clock.t);

            if (journal.stats().compactQueue > 0 && step % 2 === 0) {
                await deepProbe(step);
            } else {
                host.compactJournal(clock.t);          // snapshots in the same batch as the moves
                await journal.flush();
                if (step % 3 === 0) await verify(copyJournal(), expectedNow(), `step ${step}`);
            }
            const st = journal.stats();
            maxSegs = Math.max(maxSegs, st.segments);
            assert.equal(st.segments, segments(dir).length);
            assert.equal(st.diskBytes, bytesOnDisk(dir), `step ${step}: disk bytes`);
        }

        const st = journal.stats();
        t.diagnostic(`${st.seq} segments written, at most ${maxSegs} on disk; ${st.snapshots} snapshots; crash points: ${deepProbes} compactions, ${deletionPoints} deletions`);
        assert.ok(deepProbes >= 10 && deletionPoints >= 10, `crash points exercised (${deepProbes} compactions, ${deletionPoints} deletions)`);
        assert.ok(st.snapshots >= deepProbes && host.counts.snapshots === st.snapshots);
        assert.ok(st.seq >= 4 * maxSegs, `${st.seq} segments written, at most ${maxSegs} on disk`);
        assert.ok(maxSegs <= JOPTS.compactSegments + 4, `at most ${maxSegs} segments on disk`);
        assert.ok(!segments(dir).includes(segName(1)), 'the long games no longer pin their first segment');
        assert.ok(host.room(stuck).isOver && host.pending.has(stuck), 'the game whose commit fails is still pending');
        for (const id of firstLong.slice(0, 2)) assert.ok(host.room(id).ply > 400, 'long games spanning the whole run');

        // A clean restart on the directory: the games not committed come back, from their snapshots.
        play(long[0]);
        await journal.flush();
        assert.equal(gauge('scacelith_journal_segments'), journal.stats().segments);
        assert.equal(gauge('scacelith_journal_disk_bytes'), bytesOnDisk(dir));
        const expected = expectedNow();
        const plies = new Map([...expected.keys()].map((id) => [id, host.room(id).ply]));
        await journal.close();
        const j2 = await openJournal({ dir, ...JOPTS });
        const rec = j2.recover();
        assert.deepEqual([...rec.keys()].sort(byNum), [...expected.keys()].sort(byNum));
        for (const id of firstLong.slice(0, 2)) assert.equal(rec.get(id)[0].kind, Snapshot, 'a long game starts from its snapshot');
        for (const [id, want] of expected) assert.deepEqual(fingerprint(GameRoom.fromJournal(rec.get(id), ROOM_OPTS)), want);
        const store2 = new FakeStore();
        const host2 = new GameHost({
            shard: 0, config: CFG, store: store2, journal: j2, anticheat: new FakeAnticheat(), primary: new FakePrimary(), log: silentLog,
            createChessGame: () => new FakeChessGame(), now: () => clock.t, metrics: new Registry(), autoStart: false,
        });
        assert.equal(host2.recover(), expected.size);
        for (const [id, n] of plies) assert.equal(host2.room(id).ply, n);
        clock.t += 1000;
        host2.pollCommits(clock.t);
        assert.ok(store2.committedIds.includes(stuck), 'the stuck game is committed after the restart');
        await j2.close();
    } finally {
        fs.rmSync(dir, { recursive: true, force: true });
        fs.rmSync(scratch, { recursive: true, force: true });
    }
    assert.ok(verified > 100, `${verified} journal states verified`);
    t.diagnostic(`${verified} journal states verified`);
});

test('real rules: a game restarted from its snapshot keeps its position (en passant, castling) and goes on', async () => {
    const dir = tmpDir();
    const clock = { t: T0 };
    const JOPTS = { shard: 0, log: quiet, flushMs: 1000, fsync: false, segmentBytes: 1, compactSegments: 1 };
    const mkHost = (journal) => new GameHost({
        shard: 0, config: CFG, store: new FakeStore(), journal, anticheat: new FakeAnticheat(), primary: new FakePrimary(), log: silentLog,
        createChessGame: () => new ChessGame(), now: () => clock.t, metrics: new Registry(), autoStart: false,
    });
    const uci = (s) => encodeMove(parseSquare(s.slice(0, 2)), parseSquare(s.slice(2, 4)), 0);
    const play = (host, id, list) => {
        for (const m of list) {
            const room = host.room(id);
            clock.t += 1000;
            host.onClientMessage(id, room.playerOf(room.ply & 1).userId, {
                type: MSG.Move, seq: 1, game: id, ply: room.ply, move: uci(m), posHash: room.game.position.digest(), thinkMs: 900, drawOffer: false,
            }, null);
        }
    };
    try {
        let journal = await openJournal({ dir, ...JOPTS });
        let host = mkHost(journal);
        const id = host.createGame({ white: player(1), black: player(2), rated: false, baseMs: 10_800_000, incMs: 180_000 });
        const opening = ['e2e4', 'a7a6', 'e4e5', 'd7d5', 'e5d6', 'g8f6', 'g1f3', 'b8c6', 'f1e2', 'a6a5', 'e1g1'];
        for (const m of opening) { play(host, id, [m]); await journal.flush(); }      // one segment per move
        assert.ok(host.compactJournal(clock.t) === 1);
        await journal.flush();
        assert.equal(segments(dir).length, 1, 'only the snapshot segment is left');
        const digest = host.room(id).game.position.digest();
        const before = host.room(id).snapshot(W, clock.t);
        await journal.close();

        journal = await openJournal({ dir, ...JOPTS });
        assert.equal(journal.recover().get(id)[0].kind, Snapshot);
        host = mkHost(journal);
        assert.equal(host.recover(), 1);
        const room = host.room(id);
        assert.equal(room.ply, opening.length);
        assert.equal(room.game.position.digest(), digest);
        assert.deepEqual(room.snapshot(W, clock.t).moves, before.moves);
        play(host, id, ['a5a4']);
        assert.equal(host.room(id).ply, opening.length + 1, 'the game goes on');
        await journal.close();
    } finally {
        fs.rmSync(dir, { recursive: true, force: true });
    }
});

test('the host\'s own 10 ms interval compacts the journal (no explicit call)', async () => {
    const dir = tmpDir();
    const journal = await openJournal({ dir, shard: 0, log: quiet, flushMs: 5, fsync: false, segmentBytes: 1, compactSegments: 1 });
    const host = new GameHost({
        shard: 0, config: CFG, store: new FakeStore(), journal, anticheat: new FakeAnticheat(), primary: new FakePrimary(), log: silentLog,
        createChessGame: () => new FakeChessGame(),
    });
    try {
        const ids = [1, 2, 3].map((k) => host.createGame({ white: player(10 * k), black: player(10 * k + 1), rated: false, baseMs: 10_800_000, incMs: 180_000 }));
        for (let i = 0; i < 4; i++) {
            for (const id of ids) {
                const room = host.room(id);
                host.onClientMessage(id, room.playerOf(room.ply & 1).userId, { type: MSG.Move, seq: 1, game: id, ply: room.ply, move: fakeMove(room.ply), posHash: room.game.position.digest(), thinkMs: 0, drawOffer: false }, null);
            }
            await journal.flush();                        // one segment per batch
        }
        const deadline = Date.now() + 5000;
        while ((journal.stats().compactQueue > 0 || journal.stats().snapshots < ids.length) && Date.now() < deadline) {
            await new Promise((r) => setTimeout(r, 20));
        }
        await journal.flush();
        assert.ok(journal.stats().snapshots >= ids.length, 'the games were snapshotted by the interval');
        assert.equal(host.stats().snapshots, journal.stats().snapshots);
        assert.ok(segments(dir).length <= 2, `${segments(dir).length} segments left`);
        const copy = tmpDir('jc-copy');
        fs.cpSync(path.join(dir, 'shard-0'), path.join(copy, 'shard-0'), { recursive: true });
        const c = await openJournal({ dir: copy, shard: 0, log: quiet, flushMs: 1000, fsync: false });
        for (const id of ids) assert.equal(c.recover().get(id)[0].kind, Snapshot, `game ${id} starts from a snapshot`);
        await c.close();
        fs.rmSync(copy, { recursive: true, force: true });
    } finally {
        await host.shutdown();
        await journal.close();
        fs.rmSync(dir, { recursive: true, force: true });
    }
});

// Long and short games driven through a GameHost (FakeChessGame rules).
function gamePlayer(host, clock) {
    let users = 0;
    const api = {
        newGame: (baseMs, incMs) => host().createGame({ white: player(++users + 1000), black: player(++users + 1000), rated: false, baseMs, incMs }),
        send: (id, color, msg) => host().onClientMessage(id, host().room(id).playerOf(color).userId, { seq: 1, game: id, ...msg }, null),
        play: (id) => {
            const room = host().room(id);
            api.send(id, room.ply & 1, { type: MSG.Move, ply: room.ply, move: fakeMove(room.ply), posHash: room.game.position.digest(), thinkMs: 0, drawOffer: false });
        },
        resign: (id) => api.send(id, host().room(id).ply & 1, { type: MSG.Resign }),
        reattach: () => {                                 // the players of the recovered games come back
            for (const e of host().rooms.values()) {
                if (e.room.isOver) continue;
                for (const p of [e.room.white, e.room.black]) host().attach(e.room.id, p.userId, new FakeEndpoint(++users));
            }
        },
        clock,
    };
    return api;
}

test('a journal written before compaction existed (many segments, no snapshot record) recovers exactly, then is compacted', async () => {
    // What a GameHost appends, in order, as the server journaled it before this change.
    const tape = [];
    class TapeJournal extends MemoryJournal {
        append(kind, gameId, payload, at) { super.append(kind, gameId, payload, at); tape.push([kind, gameId, at, Buffer.from(payload)]); }
        committed(gameId) { super.committed(gameId); tape.push([Committed, gameId, 0, Buffer.alloc(0)]); }
    }
    const clock = { t: T0 };
    const mkHost = (journal, store) => new GameHost({
        shard: 0, config: CFG, store, journal, anticheat: new FakeAnticheat(), primary: new FakePrimary(), log: silentLog,
        createChessGame: () => new FakeChessGame(), now: () => clock.t, metrics: new Registry(), autoStart: false,
    });
    const store1 = new FakeStore();
    let host = mkHost(new TapeJournal(), store1);
    const g = gamePlayer(() => host, clock);
    const long = [g.newGame(10_800_000, 180_000), g.newGame(10_800_000, 180_000), g.newGame(10_800_000, 180_000)];
    const short = new Map();
    let created = 0, stuck = 0;
    for (let step = 1; step <= 400; step++) {
        clock.t += 1000;
        for (const id of long) g.play(id);
        for (const [id, plies] of short) {
            if (host.room(id).ply < plies) g.play(id);
            else { g.resign(id); short.delete(id); }
        }
        while (short.size < 3) {
            const id = g.newGame(180_000, 2000);
            short.set(id, 4 + (created % 9));
            if (++created === 5) { store1.badIds.add(id); stuck = id; }   // its commit keeps failing
        }
        host.runTimers(clock.t);
        host.pollCommits(clock.t);
    }
    const expected = new Map();
    for (const e of host.rooms.values()) if (!e.committed) expected.set(e.room.id, fingerprint(e.room));
    assert.ok(expected.has(stuck) && store1.committedIds.length > 50);

    // 60 records per segment, the way the old server spread them (nothing compacted).
    const dir = tmpDir();
    const PER = 60, nSegs = Math.ceil(tape.length / PER);
    for (let s = 0; s < nSegs; s++) writeSegment(dir, s + 1, tape.slice(s * PER, (s + 1) * PER));
    const JOPTS = { shard: 0, log: quiet, flushMs: 1000, fsync: false, segmentBytes: 8192, compactSegments: 2 };
    try {
        let j = await openJournal({ dir, ...JOPTS });
        let rec = j.recover();
        assert.equal(j.stats().recovery.segments, nSegs);
        assert.equal(j.stats().recovery.snapshots, 0);
        assert.deepEqual([...rec.keys()].sort(byNum), [...expected.keys()].sort(byNum), 'the games not committed, and only them');
        for (const [id, want] of expected) {
            const raw = tape.filter((r) => r[1] === id).map(([kind, , at, p]) => [kind, at, p.toString('hex')]);
            assert.deepEqual(rec.get(id).map((r) => [r.kind, r.at, r.payload.toString('hex')]), raw, `game ${id}: its records as written`);
            assert.deepEqual(fingerprint(GameRoom.fromJournal(rec.get(id), ROOM_OPTS)), want, `game ${id}`);
        }
        assert.ok(j.stats().compactQueue >= long.length, 'the old games are queued at open');

        // The new server takes the games back and compacts them, a few at a time.
        host = mkHost(j, new FakeStore());
        assert.equal(host.recover(), expected.size);
        let rounds = 0;
        while (segments(dir).some((n) => seqOf(n) <= nSegs) && rounds < 300) {
            rounds++;
            clock.t += 100;                               // within the recovered games' grace
            const n = host.compactJournal(clock.t);
            assert.ok(n <= 2, 'at most 2 snapshots per interval tick');
            g.play(long[rounds % long.length]);
            await j.flush();
        }
        assert.ok(segments(dir).every((n) => seqOf(n) > nSegs), `every old segment deleted (${rounds} rounds)`);
        assert.ok(segments(dir).length <= JOPTS.compactSegments + 2, `${segments(dir).length} segments left`);
        assert.equal(j.stats().diskBytes, bytesOnDisk(dir));
        const now = new Map();
        for (const e of host.rooms.values()) if (!e.committed) now.set(e.room.id, fingerprint(e.room));
        await j.close();

        j = await openJournal({ dir, ...JOPTS });
        rec = j.recover();
        assert.deepEqual([...rec.keys()].sort(byNum), [...now.keys()].sort(byNum));
        for (const id of [...long, stuck]) assert.equal(rec.get(id)[0].kind, Snapshot, `game ${id} starts from its snapshot`);
        for (const [id, want] of now) assert.deepEqual(fingerprint(GameRoom.fromJournal(rec.get(id), ROOM_OPTS)), want, `game ${id}`);
        await j.close();
    } finally {
        fs.rmSync(dir, { recursive: true, force: true });
    }
});

test('restart loop: the journal is reopened in place again and again (clean stops, torn tails) while long games run and get compacted', async (t) => {
    const dir = tmpDir();
    const JOPTS = { shard: 0, log: quiet, flushMs: 1000, fsync: false, segmentBytes: 8192, compactSegments: 2, compactPerFlush: 4 };
    const clock = { t: T0 };
    let host = null, expected = new Map(), maxSegs = 0, fromSnapshot = 0, snapshots = 0, long = [];
    const committed = new Set();
    const g = gamePlayer(() => host, clock);
    const LIVES = 8, STEPS = 120;
    try {
        for (let life = 0; life < LIVES; life++) {
            const journal = await openJournal({ dir, ...JOPTS });
            const rec = journal.recover();
            assert.deepEqual([...rec.keys()].sort(byNum), [...expected.keys()].sort(byNum), `life ${life}: the games not committed, and only them`);
            for (const id of committed) assert.ok(!rec.has(id), `life ${life}: committed game ${id} not resurrected`);
            for (const [id, want] of expected) {
                if (rec.get(id)[0].kind === Snapshot) fromSnapshot++;
                assert.deepEqual(fingerprint(GameRoom.fromJournal(rec.get(id), ROOM_OPTS)), want, `life ${life}: game ${id}`);
            }
            const store = new FakeStore();
            host = new GameHost({
                shard: 0, config: CFG, store, journal, anticheat: new FakeAnticheat(), primary: new FakePrimary(), log: silentLog,
                createChessGame: () => new FakeChessGame(), now: () => clock.t, metrics: new Registry(), autoStart: false,
            });
            assert.equal(host.recover(), expected.size);
            g.reattach();
            if (life === 0) long = [g.newGame(10_800_000, 180_000), g.newGame(10_800_000, 180_000), g.newGame(10_800_000, 180_000)];
            const short = new Map();
            for (const e of host.rooms.values()) if (!e.room.isOver && !long.includes(e.room.id)) short.set(e.room.id, e.room.ply + 2);
            for (let step = 1; step <= STEPS; step++) {
                clock.t += 1000;
                for (const id of long) g.play(id);
                for (const [id, plies] of short) {
                    if (host.room(id).ply < plies) g.play(id);
                    else { g.resign(id); short.delete(id); }
                }
                while (short.size < 3) short.set(g.newGame(180_000, 2000), 4 + (step % 9));
                host.runTimers(clock.t);
                host.pollCommits(clock.t);
                host.compactJournal(clock.t);
                await journal.flush();
                maxSegs = Math.max(maxSegs, segments(dir).length);
            }
            for (const id of store.committedIds) committed.add(id);
            snapshots += journal.stats().snapshots;
            expected = new Map();
            for (const e of host.rooms.values()) if (!e.committed) expected.set(e.room.id, fingerprint(e.room));
            if (life % 2 === 1) {
                // A crash in the middle of the next write: its only record is torn.
                const before = journal.stats();
                const size0 = fs.existsSync(segPath(dir, segName(before.seq))) ? fs.statSync(segPath(dir, segName(before.seq))).size : 0;
                g.play(long[0]);
                await journal.flush();
                const after = journal.stats();
                const file = segPath(dir, segName(after.seq));
                const start = after.seq === before.seq ? size0 : 0;
                fs.truncateSync(file, start + ((fs.statSync(file).size - start) >> 1));
            }
            await journal.close();
        }
    } finally {
        fs.rmSync(dir, { recursive: true, force: true });
    }
    t.diagnostic(`${LIVES} lives, ${committed.size} games committed, ${snapshots} snapshots, ${fromSnapshot} recoveries from a snapshot, at most ${maxSegs} segments`);
    assert.ok(host.room(long[0]).ply >= LIVES * STEPS / 2, 'the long games ran through every life');
    assert.ok(fromSnapshot >= LIVES, 'recoveries started from snapshots');
    assert.ok(maxSegs <= JOPTS.compactSegments + 4, `at most ${maxSegs} segments on disk`);
});

// Crash simulation: a child process journals 5 long games (compacted as the journal asks) and
// short games (created, then committed) and is killed with SIGKILL. For every long game, recovery
// must give its latest durable snapshot (or its first record) followed by every later record, with
// no gap, up to at least the last acknowledged one; no short game whose commit was acknowledged
// may come back; and the journal must have stayed small.
async function compactingCrashRun(fsync) {
    const dir = tmpDir();
    const SHORT0 = 100000;
    const code = `
        const { openJournal } = await import(${JSON.stringify(JOURNAL_URL)});
        const j = await openJournal({ dir: ${JSON.stringify(dir)}, shard: 4, flushMs: 2, fsync: ${fsync},
            segmentBytes: 16 * 1024, compactSegments: 2, compactPerFlush: 4 });
        const last = new Map();
        let i = 0, tick = 0;
        const payload = Buffer.alloc(24);
        function step() {
            tick++;
            for (let k = 0; k < 25; k++) {
                const g = 7000 + (i % 5);
                payload.writeUInt32LE(i, 0);
                payload.fill(i & 0xff, 4);
                j.append(2, g, payload, i);
                last.set(g, i);
                i++;
            }
            j.append(1, ${SHORT0} + tick, 'short', -1);
            let committed = 0;
            if (tick > 2) { j.append(4, ${SHORT0} + tick - 2, '', -1); j.committed(${SHORT0} + tick - 2); committed = ${SHORT0} + tick - 2; }
            for (const g of j.compactionCandidates()) {
                if (!last.has(g)) continue;             // a short game: committed soon, not snapshotted
                const s = Buffer.alloc(8);
                s.writeUInt32LE(last.get(g), 0);
                s.writeUInt32LE(0xabcdef, 4);
                j.append(6, g, s, -2);
            }
            const upto = i - 1, segs = j.stats().segments;
            j.flush().then(() => process.stdout.write('ack ' + upto + ' ' + committed + ' ' + segs + '\\n'));
            setImmediate(step);
        }
        step();`;
    const child = spawn(process.execPath, ['--input-type=module', '-e', code], { stdio: ['ignore', 'pipe', 'inherit'] });
    let lastAck = -1, lastCommit = 0, maxSegs = 0, out = '';
    const exited = new Promise((resolve) => child.once('exit', resolve));
    await new Promise((resolve, reject) => {
        const timer = setTimeout(() => reject(new Error('child produced no acks')), 30000);
        child.stdout.on('data', (d) => {
            out += d;
            const lines = out.split('\n');
            out = lines.pop();
            for (const l of lines) {
                const [tag, upto, committed, segs] = l.split(' ');
                if (tag !== 'ack') continue;
                lastAck = Math.max(lastAck, +upto);
                lastCommit = Math.max(lastCommit, +committed);
                maxSegs = Math.max(maxSegs, +segs);
            }
            if (lastAck >= 15000) {
                clearTimeout(timer);
                child.kill('SIGKILL');
                resolve();
            }
        });
        child.once('exit', () => { clearTimeout(timer); resolve(); });
    });
    await exited;
    const j = await openJournal({ dir, shard: 4, log: quiet, flushMs: 1000, fsync: false, segmentBytes: 16 * 1024, compactSegments: 2 });
    const rec = j.recover();
    let snapshots = 0;
    for (let k = 0; k < 5; k++) {
        const g = 7000 + k;
        const list = rec.get(g);
        assert.ok(list && list.length, `game ${g} recovered`);
        let next = k;                                   // the index its next record must have
        let from = 0;
        if (list[0].kind === Snapshot) {
            assert.equal(list[0].payload.readUInt32LE(4), 0xabcdef, 'snapshot intact');
            next = list[0].payload.readUInt32LE(0) + 5;
            from = 1;
            snapshots++;
        }
        for (const r of list.slice(from)) {
            assert.equal(r.kind, Move);
            const i = r.payload.readUInt32LE(0);
            assert.equal(i, next, `game ${g}: records after its snapshot, in order, no gap`);
            assert.ok(r.payload.subarray(4).every((b) => b === (i & 0xff)), 'payload intact');
            next += 5;
        }
        assert.ok(next - 5 >= lastAck - 4, `game ${g}: every acknowledged record recovered (${next - 5} >= ${lastAck - 4})`);
    }
    for (const id of rec.keys()) {
        if (id >= SHORT0) assert.ok(id > lastCommit, `short game ${id} committed (acknowledged up to ${lastCommit}) is not resurrected`);
    }
    assert.ok(snapshots >= 3, `the long games were compacted (${snapshots} start from a snapshot)`);
    const segs = segments(dir, 4).length;
    await j.close();
    fs.rmSync(dir, { recursive: true, force: true });
    return { lastAck, maxSegs, segs, snapshots };
}

test('crash simulation with compaction (SIGKILL mid-writes and mid-deletions), fsync off', async (t) => {
    for (let run = 0; run < 3; run++) {
        const r = await compactingCrashRun(false);
        t.diagnostic(`run ${run}: ${r.lastAck + 1} records acknowledged, at most ${r.maxSegs} segments, ${r.segs} left, ${r.snapshots} games from a snapshot`);
        // 15,000 records of 49 bytes are about 45 segments of 16 KB; a handful stay on disk.
        assert.ok(r.maxSegs <= 8 && r.segs <= 8, `journal bounded (at most ${r.maxSegs} segments, ${r.segs} left)`);
    }
});

test('crash simulation with compaction, fsync on', async (t) => {
    const r = await compactingCrashRun(true);
    t.diagnostic(`${r.lastAck + 1} records acknowledged, at most ${r.maxSegs} segments, ${r.segs} left, ${r.snapshots} games from a snapshot`);
    assert.ok(r.maxSegs <= 8 && r.segs <= 8, `journal bounded (at most ${r.maxSegs} segments, ${r.segs} left)`);
});
