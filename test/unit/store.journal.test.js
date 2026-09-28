import { test } from 'node:test';
import assert from 'node:assert/strict';
import crypto from 'node:crypto';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { spawn } from 'node:child_process';
import { setTimeout as sleep } from 'node:timers/promises';
import { openJournal, crc32c, parseSegment, JournalKind } from '../../src/store/journal.js';

const JOURNAL_URL = new URL('../../src/store/journal.js', import.meta.url).href;
const { Created, Move, Event, Ended, Committed } = JournalKind;
const quiet = { debug() {}, info() {}, warn() {}, error() {} };

function tmpDir() { return fs.mkdtempSync(path.join(os.tmpdir(), 'scacelith-journal-')); }
function segments(dir, shard = 0) {
    return fs.readdirSync(path.join(dir, `shard-${shard}`)).filter((n) => /^segment-\d+\.log$/.test(n)).sort();
}
function segPath(dir, name, shard = 0) { return path.join(dir, `shard-${shard}`, name); }

// Bitwise reference implementation.
function crcRef(buf) {
    let c = 0xffffffff;
    for (const b of buf) {
        c ^= b;
        for (let k = 0; k < 8; k++) c = c & 1 ? (c >>> 1) ^ 0x82f63b78 : c >>> 1;
    }
    return (c ^ 0xffffffff) >>> 0;
}

test('crc32c: standard check values, reference agreement, continuation', () => {
    assert.equal(crc32c(Buffer.from('123456789')), 0xe3069283);
    assert.equal(crc32c(Buffer.alloc(0)), 0);
    assert.equal(crc32c(Buffer.alloc(32, 0)), 0x8a9136aa);        // RFC 3720 B.4
    assert.equal(crc32c(Buffer.alloc(32, 0xff)), 0x62a8ab43);
    const inc = Buffer.from(Array.from({ length: 32 }, (_, i) => i));
    assert.equal(crc32c(inc), 0x46dd794e);
    for (const n of [1, 7, 8, 9, 63, 64, 65, 1000, 4097]) {
        const b = crypto.randomBytes(n);
        assert.equal(crc32c(b), crcRef(b), `length ${n}`);
        const cut = n >> 1;
        assert.equal(crc32c(b, cut, n, crc32c(b, 0, cut)), crc32c(b), 'continuation');
        assert.equal(crc32c(Buffer.concat([Buffer.from('xx'), b]), 2), crc32c(b), 'offset');
    }
});

test('append, flush, reopen: uncommitted games come back with their records in order', async () => {
    const dir = tmpDir();
    let j = await openJournal({ dir, shard: 0, log: quiet, flushMs: 1000, fsync: false });
    assert.equal(j.recover().size, 0);
    const big = 2 ** 53 - 1;
    const recs = [
        [Created, 11, Buffer.from('{"white":1}'), 1000.5],
        [Created, big, Buffer.from('{"white":2}'), 1001],
        [Move, 11, Buffer.from([1, 2]), 1002],
        [Move, big, Buffer.from([3, 4]), 1003],
        [Event, 11, Buffer.alloc(0), 1004],
        [Created, 12, Buffer.from('{}'), 1005],
        [Move, 11, Buffer.from([5]), 1006],
        [Ended, 12, Buffer.from([9]), 1007],
    ];
    for (const [k, g, p, at] of recs) j.append(k, g, p, at);
    j.append(Move, 12, 'text payload', 1008);
    j.committed(12);
    await j.flush();
    await j.close();

    j = await openJournal({ dir, shard: 0, log: quiet, flushMs: 1000, fsync: false });
    const rec = j.recover();
    assert.deepEqual([...rec.keys()].sort(), [11, big].sort());
    assert.deepEqual(rec.get(11).map((r) => [r.kind, r.at, [...r.payload]]),
        [[Created, 1000.5, [...Buffer.from('{"white":1}')]], [Move, 1002, [1, 2]], [Event, 1004, []], [Move, 1006, [5]]]);
    assert.deepEqual(rec.get(big).map((r) => r.kind), [Created, Move]);
    assert.ok(Buffer.isBuffer(rec.get(big)[1].payload));
    assert.equal(j.stats().recovery.records, 10);
    assert.deepEqual(j.stats().recovery.problems, []);
    assert.throws(() => j.append(0, 1, null), RangeError);
    assert.throws(() => j.append(Move, -1, null), RangeError);
    assert.throws(() => j.append(Move, 1.5, null), RangeError);
    await j.close();
    assert.throws(() => j.append(Move, 1, null), /closed/);
    fs.rmSync(dir, { recursive: true, force: true });
});

test('group commit: flush resolves after the write, appends during a write join the next batch, timer flush', async () => {
    const dir = tmpDir();
    const j = await openJournal({ dir, shard: 1, log: quiet, flushMs: 20, fsync: true });
    for (let i = 0; i < 100; i++) j.append(Move, 5, Buffer.alloc(20, i), i);
    const first = j.flush();
    assert.equal(j.stats().writing, true);
    j.append(Move, 5, Buffer.alloc(20, 200), 200);      // lands in the next batch
    assert.ok(j.stats().pendingBytes > 0);
    await first;
    const file = segPath(dir, segments(dir, 1)[0], 1);
    const size1 = fs.statSync(file).size;
    assert.equal(size1, 100 * (8 + 17 + 20));
    await j.flush();
    assert.equal(fs.statSync(file).size, size1 + 45);
    assert.equal(j.stats().pendingBytes, 0);
    await j.flush();                                      // nothing pending: resolves at once

    // Without flush(), the timer writes within flushMs.
    j.append(Move, 5, Buffer.alloc(20, 1), 300);
    await sleep(80);
    assert.equal(fs.statSync(file).size, size1 + 90);

    // flush() during a write with an empty buffer waits for that write.
    j.append(Move, 5, Buffer.alloc(20, 2), 400);
    const p1 = j.flush();
    const p2 = j.flush();
    await Promise.all([p1, p2]);
    assert.equal(fs.statSync(file).size, size1 + 135);
    // close() flushes what is pending.
    j.append(Move, 5, Buffer.alloc(20, 3), 500);
    await j.close();
    assert.equal(fs.statSync(file).size, size1 + 180);
    fs.rmSync(dir, { recursive: true, force: true });
});

test('a torn tail is ignored; new records go to a new segment', async () => {
    const dir = tmpDir();
    let j = await openJournal({ dir, log: quiet, flushMs: 1000, fsync: false });
    for (let i = 0; i < 10; i++) j.append(Move, 7, Buffer.from([i, i, i]), i);
    await j.close();
    const [seg] = segments(dir);
    const file = segPath(dir, seg);
    fs.truncateSync(file, fs.statSync(file).size - 3);   // last record cut in the middle

    j = await openJournal({ dir, log: quiet, flushMs: 1000, fsync: false });
    let r = j.recover().get(7);
    assert.equal(r.length, 9);
    assert.deepEqual([...r[8].payload], [8, 8, 8]);
    assert.equal(j.stats().recovery.problems[0].error, 'torn');
    j.append(Move, 7, Buffer.from([42]), 42);
    await j.close();
    assert.equal(segments(dir).length, 2, 'the torn segment is never appended to');

    // Garbage after valid records (e.g. zero-filled blocks after a power loss) is ignored too.
    fs.appendFileSync(segPath(dir, segments(dir)[1]), Buffer.alloc(100, 0));
    j = await openJournal({ dir, log: quiet, flushMs: 1000, fsync: false });
    r = j.recover().get(7);
    assert.deepEqual(r.map((x) => x.at), [0, 1, 2, 3, 4, 5, 6, 7, 8, 42]);
    assert.equal(j.stats().recovery.problems.find((p) => p.segment === 2).error, 'bad_length');
    await j.close();
    fs.rmSync(dir, { recursive: true, force: true });
});

test('a CRC mismatch stops the segment at the corrupt record', async () => {
    const dir = tmpDir();
    let j = await openJournal({ dir, log: quiet, flushMs: 1000, fsync: false });
    for (let i = 0; i < 10; i++) j.append(Move, 8, Buffer.alloc(10, i), i);
    await j.close();
    const file = segPath(dir, segments(dir)[0]);
    const buf = fs.readFileSync(file);
    const recLen = 8 + 17 + 10;
    buf[6 * recLen + 30] ^= 0x40;                        // one bit of record 6's payload
    fs.writeFileSync(file, buf);
    const seen = [];
    const res = parseSegment(buf, (kind, gameId, at) => seen.push(at));
    assert.deepEqual(res, { end: 6 * recLen, error: 'crc' });
    assert.deepEqual(seen, [0, 1, 2, 3, 4, 5]);
    j = await openJournal({ dir, log: quiet, flushMs: 1000, fsync: false });
    assert.deepEqual(j.recover().get(8).map((r) => r.at), [0, 1, 2, 3, 4, 5]);
    const problem = j.stats().recovery.problems[0];
    assert.equal(problem.error, 'crc');
    assert.equal(problem.bytesIgnored, 4 * recLen);
    await j.close();
    fs.rmSync(dir, { recursive: true, force: true });
});

test('segments rotate at the size limit and are read back in order', async () => {
    const dir = tmpDir();
    let j = await openJournal({ dir, shard: 2, log: quiet, flushMs: 1000, fsync: false, segmentBytes: 4096 });
    for (let i = 0; i < 1000; i++) {
        j.append(Move, 100 + (i % 3), Buffer.alloc(40, i & 0xff), i);
        if (i % 50 === 49) await j.flush();
    }
    await j.close();
    const segs = segments(dir, 2);
    assert.ok(segs.length >= 10, `${segs.length} segments`);
    for (const s of segs.slice(0, -1)) {
        const size = fs.statSync(segPath(dir, s, 2)).size;
        assert.ok(size >= 4096 && size < 4096 + 50 * 65, 'a segment exceeds the limit by less than one batch');
    }
    j = await openJournal({ dir, shard: 2, log: quiet, flushMs: 1000, fsync: false, segmentBytes: 4096 });
    const rec = j.recover();
    for (let g = 0; g < 3; g++) {
        const ats = rec.get(100 + g).map((r) => r.at);
        assert.deepEqual(ats, Array.from({ length: ats.length }, (_, k) => g + 3 * k));
    }
    await j.close();
    fs.rmSync(dir, { recursive: true, force: true });
});

test('segments are deleted once all their games are committed, the commit record outliving older segments', async () => {
    const dir = tmpDir();
    // segmentBytes 1: every flushed batch goes to its own segment.
    const open = () => openJournal({ dir, log: quiet, flushMs: 1000, fsync: false, segmentBytes: 1 });
    let j = await open();
    j.append(Created, 1, 'g1', 1);
    j.append(Move, 1, 'm1', 2);
    await j.flush();                                  // segment 1: game 1
    j.append(Created, 2, 'g2', 3);
    j.append(Move, 1, 'm2', 4);
    j.append(Ended, 1, '', 5);
    await j.flush();                                  // segment 2: games 1, 2
    j.committed(1);
    await j.flush();                                  // segment 3: commit of game 1
    assert.deepEqual(segments(dir), ['segment-0000000002.log', 'segment-0000000003.log'],
        'segment 1 only held the committed game; segment 3 must outlive segment 2');

    // A crash now: game 1 must not come back (its commit record still exists), game 2 must.
    await j.close();
    j = await open();
    assert.deepEqual([...j.recover().keys()], [2]);
    assert.deepEqual(j.recover().get(2).map((r) => r.kind), [Created]);
    assert.deepEqual(segments(dir), ['segment-0000000002.log', 'segment-0000000003.log']);

    j.append(Move, 2, 'm3', 6);
    j.append(Ended, 2, '', 7);
    await j.flush();                                  // segment 4
    assert.equal(segments(dir).length, 3);
    j.committed(2);
    await j.flush();                                  // segment 5
    assert.deepEqual(segments(dir), ['segment-0000000005.log'], 'only the active segment remains');
    await j.close();
    j = await open();
    assert.equal(j.recover().size, 0);
    assert.deepEqual(segments(dir), [], 'fully committed segments are deleted at open');
    j.append(Created, 3, 'g3', 8);
    await j.close();
    assert.deepEqual(segments(dir), ['segment-0000000006.log'], 'numbering continues');
    fs.rmSync(dir, { recursive: true, force: true });
});

test('commit bookkeeping across rotation with many games', async () => {
    const dir = tmpDir();
    const j = await openJournal({ dir, log: quiet, flushMs: 1000, fsync: false, segmentBytes: 2048 });
    const live = new Set();
    for (let round = 0; round < 60; round++) {
        for (let g = round; g < round + 5; g++) { j.append(Move, 1000 + g, Buffer.alloc(30, g), round); live.add(1000 + g); }
        const done = 1000 + round;                    // game `round` ends and is committed
        j.append(Ended, done, '', round);
        j.committed(done);
        live.delete(done);
        await j.flush();
    }
    await j.close();
    const j2 = await openJournal({ dir, log: quiet, flushMs: 1000, fsync: false, segmentBytes: 2048 });
    assert.deepEqual([...j2.recover().keys()].sort((a, b) => a - b), [...live].sort((a, b) => a - b));
    assert.ok(segments(dir).length <= 4, `old segments deleted (${segments(dir).length} left)`);
    await j2.close();
    fs.rmSync(dir, { recursive: true, force: true });
});

// Crash simulation: a child process journals continuously and is killed with SIGKILL. Every record
// acknowledged by a resolved flush() must be recovered, and what is recovered must be an exact
// prefix of what was appended (no gap, no corrupt record), across segment rotations.
async function crashRun(fsync) {
    const dir = tmpDir();
    const code = `
        const { openJournal } = await import(${JSON.stringify(JOURNAL_URL)});
        const j = await openJournal({ dir: ${JSON.stringify(dir)}, shard: 3, flushMs: 2, fsync: ${fsync}, segmentBytes: 64 * 1024 });
        let i = 0;
        const payload = Buffer.alloc(24);
        function tick() {
            for (let k = 0; k < 25; k++) {
                payload.writeUInt32LE(i, 0);
                payload.fill(i & 0xff, 4);
                j.append(2, 5000 + (i % 5), payload, i);
                i++;
            }
            const upto = i - 1;
            j.flush().then(() => process.stdout.write('ack ' + upto + '\\n'));
            setImmediate(tick);
        }
        tick();`;
    const child = spawn(process.execPath, ['--input-type=module', '-e', code], { stdio: ['ignore', 'pipe', 'inherit'] });
    let lastAck = -1;
    let out = '';
    const exited = new Promise((resolve) => child.once('exit', resolve));
    await new Promise((resolve, reject) => {
        const timer = setTimeout(() => reject(new Error('child produced no acks')), 20000);
        child.stdout.on('data', (d) => {
            out += d;
            const lines = out.split('\n');
            out = lines.pop();
            for (const l of lines) if (l.startsWith('ack ')) lastAck = Math.max(lastAck, +l.slice(4));
            if (lastAck >= 6000) {
                clearTimeout(timer);
                child.kill('SIGKILL');
                resolve();
            }
        });
        child.once('exit', () => { clearTimeout(timer); resolve(); });
    });
    await exited;
    const j = await openJournal({ dir, shard: 3, log: quiet, flushMs: 1000, fsync: false, segmentBytes: 64 * 1024 });
    const seen = [];
    for (const [gameId, recs] of j.recover()) {
        let prev = -1;
        for (const r of recs) {
            const i = r.payload.readUInt32LE(0);
            assert.equal(gameId, 5000 + (i % 5));
            assert.ok(i > prev, 'records of a game in order');
            prev = i;
            assert.ok(r.payload.subarray(4).every((b) => b === (i & 0xff)), 'payload intact');
            assert.equal(r.at, i);
            seen.push(i);
        }
    }
    seen.sort((a, b) => a - b);
    assert.ok(seen.length > lastAck, `every acknowledged record recovered (${seen.length} > ${lastAck})`);
    for (let k = 0; k < seen.length; k++) assert.equal(seen[k], k, 'recovered records form an exact prefix');
    const segs = segments(dir, 3).length;
    await j.close();
    fs.rmSync(dir, { recursive: true, force: true });
    return { lastAck, recovered: seen.length, segs };
}

test('crash simulation (SIGKILL mid-writes), fsync off', async () => {
    for (let run = 0; run < 3; run++) {
        const r = await crashRun(false);
        assert.ok(r.segs > 1, 'the run rotated segments');
    }
});

test('crash simulation (SIGKILL mid-writes), fsync on', async () => {
    await crashRun(true);
});
