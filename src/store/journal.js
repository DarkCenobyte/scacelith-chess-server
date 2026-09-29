// Game journal (DESIGN.md 5.6): crash safety of the games in progress without a database write
// per move. One journal per shard, JOURNAL_DIR/shard-<n>/segment-<seq>.log.
//
// Record (little-endian):
//   u32 length   bytes after this 8-byte header (17 + payload length)
//   u32 crc32c   CRC-32C (Castagnoli) of those `length` bytes
//   u8  kind     1 created, 2 move, 3 event, 4 ended, 5 committed, 6 snapshot
//   u64 gameId   id53
//   f64 at       epoch ms
//   ... payload  opaque (the game module's encoding)
//
// append() encodes the record straight into an in-memory buffer (no allocation per record beyond
// the buffer's occasional growth). A flush hands the whole buffer to ONE write (+ fdatasync when
// fsync is on) and swaps in a spare buffer, so appends made while the write is in flight go to the
// next batch (group commit). Flushes happen JOURNAL_FLUSH_MS after the first pending record, or
// at once when flush() is awaited. Segments rotate once they reach SEGMENT_BYTES (checked before
// each batch, so a segment exceeds it by at most one batch; records never span segments).
//
// Bookkeeping (memory only; open() recomputes it by scanning the segments): per segment, the set
// of games it mentions and how many of them still need it (pins); per game, the segments that
// mention it, the first segment it needs (the one of its first record, or of its latest
// snapshot) and the one holding its 'committed' record. A game not committed needs every segment
// from its first one on. A segment is deleted when no game needs it any more (the records that
// release it, 'committed' or 'snapshot', durably written), and a segment holding a game's
// 'committed' record outlives every other segment that mentions that game, so a later recovery
// can never see a committed game's records without its 'committed' record.
//
// Compaction (JOURNAL_COMPACT_SEGMENTS) keeps a shard's journal at about compactSegments + 1
// segments whatever the length of its games; without it, a game of several hours would keep
// every segment written since its start. A 'snapshot' record holds the whole state of one game
// (the game module's encoding) and supersedes every earlier record of that game. When the
// journal starts segment N, the games not committed whose first needed segment is
// N - compactSegments or older are queued; the host takes a few of them at a time
// (compactionCandidates(max): at most compactPerFlush snapshots per batch) and appends their
// snapshot. Once the batch holding a snapshot is written (and fsynced), its segment becomes the
// game's first needed one, and the older segments go by the rule above. Crash safety: until then
// every older segment is still on disk (a torn snapshot is ignored like any torn record, and the
// game replays from its older records); after it, recovery starts the game from its latest
// snapshot and drops the records before it, wherever they are (a crash in the middle of the
// deletions leaves some of them behind).
//
// Durability of the deletions (fsync on, i.e. against a power loss and not only a process
// crash): a batch is fdatasynced before the bookkeeping that releases segments runs, so the
// 'committed' or 'snapshot' record that releases a segment is durable before that segment is
// unlinked. open() fdatasyncs every segment it read, then the directory, before it deletes
// anything: a record found at start-up may have been written by a process that died before its
// fdatasync and live only in the page cache. A segment holding a 'committed' record is unlinked
// only once the unlinks before it are durable (a directory fsync in between), so an older segment
// of a committed game can never survive a power loss without that record. That extra directory
// fsync is rare (about once per rotation). With fsync off, a batch holding a snapshot is still
// fdatasynced before its bookkeeping runs, with a directory fsync first when its segment was
// created since the last one, and open() makes the segments holding a snapshot durable before it
// deletes anything: a snapshot deletes older records of a running game, so a power loss must not
// be able to keep the deletions and lose the snapshot. fsync off then risks only the last
// records written (and a committed game coming back, which the database commit ignores), as it
// did before compaction existed. That costs one fdatasync per batch holding snapshots, at most
// one per game and rotation.
//
// Write failures: after a failed write, the next batch goes to a new segment; the batch's
// 'committed' records are appended again (the host has forgotten those games, and without the
// record their segments would stay until the next restart). Every other game of the batch lost
// records (its 'created' record, moves, events, its 'ended' record or a snapshot), and its later
// records would replay after a gap: it goes to the heal queue, which compactionCandidates() serves
// before the compaction queue, whatever the game's age and even when the journal does not track
// it yet (its first record was the lost one), still at most compactPerFlush snapshots per batch.
// The host appends a snapshot of each such game it still hosts and has not committed, which
// supersedes the lost records. failedWrites counts the failures, and hasUnwritten() tells whether
// a flush is still needed before the records appended so far are on disk.
//
// Recovery reads the segments in order and stops reading a segment at the first record whose
// length is impossible, which is truncated (torn write) or whose CRC does not match; the records
// before it are kept, and the next segment is read normally. New records always go to a new
// segment, never after a possibly torn tail. A shard's journal directory must be used by one
// process at a time.

import fs from 'node:fs';
import fsp from 'node:fs/promises';
import path from 'node:path';
import { logger } from '../log.js';
import { metrics } from '../metrics.js';

/** Record kinds. */
export const JournalKind = Object.freeze({ Created: 1, Move: 2, Event: 3, Ended: 4, Committed: 5, Snapshot: 6 });

/** Default segment size before rotation. */
export const SEGMENT_BYTES = 16 * 1024 * 1024;
/** Largest payload of one record. */
export const MAX_PAYLOAD = 1024 * 1024;
/** Default JOURNAL_COMPACT_SEGMENTS: a game is snapshotted once the journal is this many segments past the first one it needs. */
export const COMPACT_SEGMENTS = 4;
/** Default number of snapshots one flushed batch holds at most (the pace of the compaction). */
export const COMPACT_PER_FLUSH = 8;

const HEADER = 8;
const FIXED = 17;                  // kind + gameId + at
const TWO32 = 4294967296;
const INITIAL_BUFFER = 64 * 1024;
const MAX_SPARE = 4 * 1024 * 1024;
const EMPTY = Buffer.alloc(0);
const NO_GAMES = Object.freeze([]);
const SEGMENT_RE = /^segment-(\d+)\.log$/;

const mFlushMs = metrics.histogram('scacelith_journal_flush_ms', 'Journal write (+ fsync) duration per flush',
    [0.25, 0.5, 1, 2, 5, 10, 25, 50, 100, 250]);
const mBytes = metrics.counter('scacelith_journal_bytes_total', 'Bytes written to the game journal');
const mRecords = metrics.counter('scacelith_journal_records_total', 'Records appended to the game journal');
const mErrors = metrics.counter('scacelith_journal_errors_total', 'Failed journal writes');
const mSegmentsDeleted = metrics.counter('scacelith_journal_segments_deleted_total', 'Journal segments deleted once no game needed them');
const mSnapshots = metrics.counter('scacelith_journal_snapshots_total', 'Game snapshots written to the journal (compaction of long games)');
const mSegments = metrics.gauge('scacelith_journal_segments', 'Journal segments on disk', [], { perShard: true });
const mDiskBytes = metrics.gauge('scacelith_journal_disk_bytes', 'Size of the journal segments on disk', [], { perShard: true });

// ---- CRC-32C (Castagnoli, reflected polynomial 0x82F63B78), slicing-by-8 --------------------------

const CRC_TABLE = (() => {
    const t = new Int32Array(8 * 256);
    for (let n = 0; n < 256; n++) {
        let c = n;
        for (let k = 0; k < 8; k++) c = c & 1 ? (c >>> 1) ^ 0x82f63b78 : c >>> 1;
        t[n] = c;
    }
    for (let n = 0; n < 256; n++) {
        let c = t[n];
        for (let s = 1; s < 8; s++) {
            c = t[c & 0xff] ^ (c >>> 8);
            t[s * 256 + n] = c;
        }
    }
    return t;
})();

/**
 * CRC-32C of buf[start..end) (crc32c('123456789') === 0xE3069283).
 * @param {Uint8Array} buf
 * @param {number} [start=0]
 * @param {number} [end=buf.length]
 * @param {number} [crc=0] running value to continue from
 * @returns {number} unsigned 32-bit CRC
 */
export function crc32c(buf, start = 0, end = buf.length, crc = 0) {
    const t = CRC_TABLE;
    let c = ~crc;
    let i = start;
    const end8 = end - 8;
    while (i <= end8) {
        c ^= buf[i] | (buf[i + 1] << 8) | (buf[i + 2] << 16) | (buf[i + 3] << 24);
        c = t[1792 + (c & 0xff)] ^ t[1536 + ((c >>> 8) & 0xff)] ^ t[1280 + ((c >>> 16) & 0xff)] ^ t[1024 + (c >>> 24)]
            ^ t[768 + buf[i + 4]] ^ t[512 + buf[i + 5]] ^ t[256 + buf[i + 6]] ^ t[buf[i + 7]];
        i += 8;
    }
    while (i < end) c = t[(c ^ buf[i++]) & 0xff] ^ (c >>> 8);
    return ~c >>> 0;
}

/**
 * Parses the records of one segment; stops at the first invalid one.
 * @param {Buffer} buf
 * @param {(kind:number, gameId:number, at:number, payload:Buffer) => void} onRecord
 * @returns {{ end: number, error: null | 'torn' | 'bad_length' | 'crc' }}  end: bytes of valid records
 */
export function parseSegment(buf, onRecord) {
    let o = 0;
    while (o < buf.length) {
        if (o + HEADER > buf.length) return { end: o, error: 'torn' };
        const len = buf.readUInt32LE(o);
        if (len < FIXED || len > FIXED + MAX_PAYLOAD) return { end: o, error: 'bad_length' };
        if (o + HEADER + len > buf.length) return { end: o, error: 'torn' };
        if (crc32c(buf, o + HEADER, o + HEADER + len) !== buf.readUInt32LE(o + 4)) return { end: o, error: 'crc' };
        const kind = buf[o + 8];
        const gameId = buf.readUInt32LE(o + 9) + buf.readUInt32LE(o + 13) * TWO32;
        const at = buf.readDoubleLE(o + 17);
        onRecord(kind, gameId, at, buf.subarray(o + HEADER + FIXED, o + HEADER + len));
        o += HEADER + len;
    }
    return { end: o, error: null };
}

function segmentName(seq) { return `segment-${String(seq).padStart(10, '0')}.log`; }

async function syncFile(file) {
    let fh;
    try {
        fh = await fsp.open(file, 'r');
        await fh.datasync();
    } catch { /* best effort (a read-only handle cannot be synced on Windows) */ } finally {
        if (fh) await fh.close().catch(() => {});
    }
}

async function fsyncDir(dir) {
    if (process.platform === 'win32') return;      // directories cannot be opened for fsync there
    let fh;
    try {
        fh = await fsp.open(dir, 'r');
        await fh.sync();
    } catch { /* best effort */ } finally {
        if (fh) await fh.close().catch(() => {});
    }
}

class Journal {
    constructor({ dir, shard = 0, flushMs = 50, fsync = true, segmentBytes = SEGMENT_BYTES,
        compactSegments = COMPACT_SEGMENTS, compactPerFlush = COMPACT_PER_FLUSH, log = logger.child('journal') }) {
        if (!dir) throw new TypeError('openJournal: dir is required');
        this.dir = path.join(dir, `shard-${shard}`);
        this.shard = shard;
        this.flushMs = Math.max(0, flushMs);
        this.fsync = !!fsync;
        this.segmentBytes = segmentBytes;
        this.compactSegments = Number.isFinite(compactSegments) ? Math.max(1, Math.floor(compactSegments)) : COMPACT_SEGMENTS;
        this.compactPerFlush = Number.isFinite(compactPerFlush) ? Math.max(1, Math.floor(compactPerFlush)) : COMPACT_PER_FLUSH;
        this.log = log;

        this.buf = Buffer.allocUnsafe(INITIAL_BUFFER);
        this.len = 0;
        this.spare = null;
        this.batchGames = new Set();
        this.batchCommits = [];
        this.batchSnaps = [];           // games with a snapshot in the current buffer
        this.snapsPending = new Set();  // games with a snapshot appended and not written yet
        this.compactQueue = new Set();  // games to snapshot, in queue order
        this.healQueue = new Set();     // games whose records a failed write lost: snapshotted first
        this.waiters = [];              // flush() calls covering the current buffer
        this.inflight = null;           // waiters of the batch being written
        this.writing = false;
        this.timer = null;
        this.pendingSince = 0;
        this.closed = false;
        this.closing = false;

        this.fh = null;                 // current segment (opened at the first write)
        this.seq = 0;                   // highest segment number used
        this.segSize = 0;
        this.forceRotate = false;
        this.segs = new Map();          // seq -> { seq, file, games: Set, pins, active, bytes }, ascending seq
        this.games = new Map();         // gameId -> { segs: Set<seq>, first, committed, commitSeg }
        this.diskBytes = 0;
        this.snapshots = 0;
        this.failedWrites = 0;          // batches whose write (or fsync) failed
        this.unlinks = 0;               // segments deleted so far
        this.syncedUnlinks = 0;         // ... of which a directory fsync made the deletion durable
        this.syncedSeq = 0;             // highest segment whose directory entry a directory fsync made durable
        this.recovered = new Map();
        this.recoverInfo = { segments: 0, records: 0, games: 0, snapshots: 0, problems: [] };
        this.onTimer = () => {
            this.timer = null;
            if (!this.writing && this.len > 0) this.startWrite();
        };
    }

    async open() {
        await fsp.mkdir(this.dir, { recursive: true });
        const seqs = (await fsp.readdir(this.dir)).map((n) => SEGMENT_RE.exec(n)).filter(Boolean)
            .map((m) => parseInt(m[1], 10)).sort((a, b) => a - b);
        const perGame = new Map();
        const committed = new Set();
        const snapshotFiles = new Set();     // segments holding a snapshot (synced below with fsync off)
        for (const seq of seqs) {
            this.seq = Math.max(this.seq, seq);
            const file = path.join(this.dir, segmentName(seq));
            const buf = await this.readSegment(file);
            const seg = { seq, file, games: new Set(), pins: 0, active: false, bytes: buf.length };
            this.segs.set(seq, seg);
            this.diskBytes += buf.length;
            const res = parseSegment(buf, (kind, gameId, at, payload) => {
                this.recoverInfo.records++;
                const rec = { kind, at, payload: Buffer.from(payload) };
                const list = kind === JournalKind.Snapshot ? null : perGame.get(gameId);
                if (list) list.push(rec);
                else perGame.set(gameId, [rec]);      // a snapshot supersedes the game's earlier records
                this.track(gameId, seg);
                if (kind === JournalKind.Snapshot) {
                    this.recoverInfo.snapshots++;
                    this.markSnapshot(gameId, seq);
                    snapshotFiles.add(file);
                } else if (kind === JournalKind.Committed) {
                    committed.add(gameId);
                    this.markCommitted(gameId, seq);
                }
            });
            if (res.error) {
                const problem = { segment: seq, offset: res.end, error: res.error, bytesIgnored: buf.length - res.end, last: seq === seqs[seqs.length - 1] };
                this.recoverInfo.problems.push(problem);
                (problem.last && res.error === 'torn' ? this.log.info : this.log.warn).call(this.log, 'journal segment tail ignored', problem);
            }
        }
        for (const [gameId, list] of perGame) if (!committed.has(gameId)) this.recovered.set(gameId, list);
        this.recoverInfo.segments = seqs.length;
        this.recoverInfo.games = this.recovered.size;
        // What was read is durable (readSegment), and so are the directory's entries (a previous
        // process's deletions included) before anything is deleted on their strength. With fsync
        // off, the segments holding a snapshot are made durable all the same, since a snapshot
        // read here lets its game's older segments be deleted (like writeBatch does).
        if (this.fsync && seqs.length) await this.syncDir();
        else if (snapshotFiles.size) {
            for (const file of snapshotFiles) await syncFile(file);
            await this.syncDir();
        }
        await this.gc([...this.segs.keys()]);
        this.enqueueStale();
        this.updateGauges();
        return this;
    }

    // Reads a whole segment at open(); with fsync on, makes it durable first (it may hold records a
    // process wrote just before dying, which only the page cache has, and a 'snapshot' or
    // 'committed' record read here lets older segments be deleted).
    async readSegment(file) {
        const fh = await fsp.open(file, 'r');
        try {
            if (this.fsync) {
                try { await fh.datasync(); } catch { /* best effort (a read-only handle cannot be synced on Windows) */ }
            }
            return await fh.readFile();
        } finally {
            await fh.close();
        }
    }

    /**
     * Buffers one record; it is written at the next flush.
     * @param {number} kind  JournalKind
     * @param {number} gameId  id53
     * @param {Uint8Array|string|null} payload
     * @param {number} [at]  epoch ms
     */
    append(kind, gameId, payload, at = Date.now()) {
        if (this.closed) throw new Error('journal closed');
        if (!Number.isInteger(kind) || kind < 1 || kind > 255) throw new RangeError('journal: bad record kind');
        if (!Number.isSafeInteger(gameId) || gameId < 0) throw new RangeError('journal: bad game id');
        const p = payload === null || payload === undefined ? EMPTY : typeof payload === 'string' ? Buffer.from(payload) : payload;
        if (p.length > MAX_PAYLOAD) throw new RangeError('journal: payload too large');
        const bodyLen = FIXED + p.length;
        const need = this.len + HEADER + bodyLen;
        if (need > this.buf.length) {
            const nb = Buffer.allocUnsafe(Math.max(need, this.buf.length * 2));
            this.buf.copy(nb, 0, 0, this.len);
            this.buf = nb;
        }
        const b = this.buf;
        const o = this.len;
        b.writeUInt32LE(bodyLen, o);
        b[o + 8] = kind;
        b.writeUInt32LE(gameId >>> 0, o + 9);
        b.writeUInt32LE((gameId / TWO32) >>> 0, o + 13);
        b.writeDoubleLE(at, o + 17);
        if (p.length) b.set(p, o + HEADER + FIXED);
        b.writeUInt32LE(crc32c(b, o + HEADER, o + HEADER + bodyLen), o + 4);
        if (o === 0) this.pendingSince = performance.now();
        this.len = need;
        this.batchGames.add(gameId);
        if (kind === JournalKind.Committed) this.batchCommits.push(gameId);
        else if (kind === JournalKind.Snapshot) {
            this.batchSnaps.push(gameId);
            this.snapsPending.add(gameId);
        }
        mRecords.inc();
        if (this.timer === null && !this.writing) this.timer = setTimeout(this.onTimer, this.flushMs);
    }

    /**
     * Writes everything appended so far; resolves once it is written (and fsynced when enabled).
     * @returns {Promise<void>}
     */
    flush() {
        if (this.len > 0) {
            const p = new Promise((resolve, reject) => this.waiters.push({ resolve, reject }));
            if (!this.writing) this.startWrite();
            return p;
        }
        if (this.writing) return new Promise((resolve, reject) => this.inflight.push({ resolve, reject }));
        return Promise.resolve();
    }

    /**
     * Whether records appended so far are not written yet (buffered, or in the batch being
     * written): a flush() is needed before relying on them being on disk.
     * @returns {boolean}
     */
    hasUnwritten() { return this.len > 0 || this.writing; }

    /** Marks a game as committed to the database: its records may be forgotten. */
    committed(gameId) {
        this.append(JournalKind.Committed, gameId, EMPTY, Date.now());
    }

    /**
     * Games whose snapshot the journal wants now (see the header of this file): first the games
     * whose records a failed write lost (heal queue), then those the compaction queued. At most
     * `max`, and at most compactPerFlush snapshots per batch counting those already appended to
     * it. O(1) when nothing is due. The caller appends a snapshot record (JournalKind.Snapshot)
     * for each game it still hosts and has not committed, from a state that includes every record
     * it appended for that game; a game it skips is queued again at a later rotation (a stale
     * one) or not at all (it no longer needs one).
     * @param {number} [max=Infinity]
     * @returns {readonly number[]} game ids
     */
    compactionCandidates(max = Infinity) {
        if ((this.compactQueue.size === 0 && this.healQueue.size === 0) || this.closing || this.closed) return NO_GAMES;
        const room = Math.min(max, this.compactPerFlush - this.batchSnaps.length);
        if (!(room >= 1)) return NO_GAMES;
        const out = [];
        // A game of a failed write: whatever its age, and possibly not tracked yet (its first
        // record was lost); a snapshot already pending supersedes the lost records too.
        for (const gameId of this.healQueue) {
            if (out.length >= room) return out;
            this.healQueue.delete(gameId);
            this.compactQueue.delete(gameId);
            const g = this.games.get(gameId);
            if ((!g || !g.committed) && !this.snapsPending.has(gameId)) out.push(gameId);
        }
        const limit = this.seq - this.compactSegments;
        for (const gameId of this.compactQueue) {
            if (out.length >= room) break;
            this.compactQueue.delete(gameId);
            const g = this.games.get(gameId);
            if (g && !g.committed && g.first <= limit && !this.snapsPending.has(gameId)) out.push(gameId);
        }
        return out;
    }

    /**
     * Games found at open() without a 'committed' record, with their records in order; a game with
     * a snapshot starts with its latest one (the earlier records are dropped).
     * @returns {Map<number, {kind:number, at:number, payload:Buffer}[]>}
     */
    recover() {
        return this.recovered;
    }

    /** Numbers for metrics and tests. */
    stats() {
        return {
            segments: this.segs.size, seq: this.seq, segmentBytes: this.segSize, pendingBytes: this.len,
            diskBytes: this.diskBytes, snapshots: this.snapshots, compactQueue: this.compactQueue.size,
            healQueue: this.healQueue.size, trackedGames: this.games.size, writing: this.writing, recovery: this.recoverInfo,
        };
    }

    /** Flushes, then closes the segment. */
    async close() {
        if (this.closed) return;
        this.closing = true;
        try {
            await this.flush();
        } finally {
            this.closed = true;
            if (this.timer) { clearTimeout(this.timer); this.timer = null; }
            if (this.fh) { const fh = this.fh; this.fh = null; await fh.close(); }
        }
    }

    // ---- internals -----------------------------------------------------------------------------

    startWrite() {
        if (this.timer) { clearTimeout(this.timer); this.timer = null; }
        const buf = this.buf;
        const len = this.len;
        const games = this.batchGames;
        const commits = this.batchCommits;
        const snaps = this.batchSnaps;
        const waiters = this.waiters;
        this.buf = this.spare || Buffer.allocUnsafe(INITIAL_BUFFER);
        this.spare = null;
        this.len = 0;
        this.batchGames = new Set();
        this.batchCommits = [];
        this.batchSnaps = [];
        this.waiters = [];
        this.inflight = waiters;
        this.writing = true;
        const t0 = performance.now();
        this.writeBatch(buf, len, snaps.length > 0).then((seg) => this.written(seg, buf, len, games, commits, snaps, t0), (err) => {
            mErrors.inc();
            this.failedWrites++;
            this.forceRotate = true;        // later records must not follow a possibly torn write
            // Not known to be durable: the games keep their segments.
            for (const g of snaps) this.snapsPending.delete(g);
            // The 'committed' records are appended again: the host has forgotten these games, and
            // without the record the journal would keep their segments until the next restart.
            if (!this.closing && !this.closed) {
                for (const g of commits) {
                    try { this.append(JournalKind.Committed, g, EMPTY, Date.now()); } catch { /* closed meanwhile */ }
                }
            }
            // The other games lost records (their snapshots included): a new snapshot heals them.
            const done = commits.length ? new Set(commits) : null;
            for (const g of games) {
                const tracked = this.games.get(g);
                if (!(done && done.has(g)) && !(tracked && tracked.committed)) this.healQueue.add(g);
            }
            this.log.error('journal write failed', { err, bytes: len });
            this.finishWrite(err);
        });
    }

    // The batch is written (and fsynced): bookkeeping, deletions, then its flush() calls resolve.
    // The next batch starts after the deletions, so they never run concurrently with a rotation's.
    async written(seg, buf, len, games, commits, snaps, t0) {
        mFlushMs.observe(performance.now() - t0);
        mBytes.inc(len);
        for (const g of games) this.track(g, seg);
        const touched = new Set();
        // The batch is durable: its snapshots release their games' older segments.
        for (const g of snaps) {
            this.snapsPending.delete(g);
            for (const s of this.markSnapshot(g, seg.seq)) touched.add(s);
        }
        if (snaps.length) { this.snapshots += snaps.length; mSnapshots.inc(snaps.length); }
        for (const g of commits) for (const s of this.markCommitted(g, seg.seq)) touched.add(s);
        if (touched.size) {
            try { await this.gc([...touched]); } catch (err) { this.log.error('journal segments not deleted', { err }); }
        }
        this.updateGauges();
        if (buf.length <= MAX_SPARE) this.spare = buf;
        this.finishWrite(null);
    }

    finishWrite(err) {
        const waiters = this.inflight;
        this.inflight = null;
        this.writing = false;
        for (const w of waiters) {
            if (err) w.reject(err); else w.resolve();
        }
        if (this.len > 0) {
            if (this.waiters.length || this.closing) this.startWrite();
            else if (this.timer === null) {
                const wait = Math.max(0, this.pendingSince + this.flushMs - performance.now());
                this.timer = setTimeout(this.onTimer, wait);
            }
        }
    }

    // Writes one batch (and fdatasyncs it when fsync is on). A batch holding a snapshot is made
    // durable even with fsync off, its segment's directory entry included, because the snapshot
    // releases older segments (see the header of this file).
    async writeBatch(buf, len, hasSnapshot = false) {
        if (!this.fh || this.forceRotate || this.segSize >= this.segmentBytes) await this.rotate();
        const seg = this.segs.get(this.seq);
        let off = 0;
        while (off < len) {
            const { bytesWritten } = await this.fh.write(buf, off, len - off, null);
            off += bytesWritten;
            seg.bytes += bytesWritten;
            this.diskBytes += bytesWritten;
        }
        this.segSize += len;
        if (this.fsync || hasSnapshot) await this.fh.datasync();
        if (hasSnapshot && seg.seq > this.syncedSeq) await this.syncDir();
        return seg;
    }

    async rotate() {
        this.forceRotate = false;
        // The newest segment: the active one, or the one a failed rotation left without a handle
        // (it must become deletable all the same).
        const prev = this.segs.get(this.seq) || null;
        if (this.fh) {
            const fh = this.fh;
            this.fh = null;
            await fh.close();
        }
        const seq = this.seq + 1;
        const file = path.join(this.dir, segmentName(seq));
        this.fh = await fsp.open(file, 'a');
        if (this.fsync) {
            await this.syncDir();
            this.syncedSeq = seq;           // the new segment's directory entry is durable
        }
        this.seq = seq;
        this.segSize = 0;
        this.segs.set(seq, { seq, file, games: new Set(), pins: 0, active: true, bytes: 0 });
        if (prev) {
            prev.active = false;
            await this.gc([prev.seq]);
        }
        this.enqueueStale();
        this.updateGauges();
    }

    track(gameId, seg) {
        let g = this.games.get(gameId);
        if (!g) {
            g = { segs: new Set(), first: seg.seq, committed: false, commitSeg: -1 };
            this.games.set(gameId, g);
        }
        if (!g.segs.has(seg.seq)) {
            g.segs.add(seg.seq);
            seg.games.add(gameId);
            if (!g.committed && seg.seq >= g.first) seg.pins++;
        }
    }

    // A snapshot of the game is durable in segment `seq`: the game no longer needs the segments
    // before it. Returns the segments it released.
    markSnapshot(gameId, seq) {
        const g = this.games.get(gameId);
        if (!g || g.committed || seq <= g.first) return NO_GAMES;
        const released = [];
        for (const s of g.segs) {
            if (s >= g.first && s < seq) {
                this.segs.get(s).pins--;
                released.push(s);
            }
        }
        g.first = seq;
        return released;
    }

    // The game's 'committed' record is durable in segment `seq`. Returns the segments it mentions.
    markCommitted(gameId, seq) {
        const g = this.games.get(gameId);
        if (!g) return NO_GAMES;
        g.commitSeg = Math.max(g.commitSeg, seq);
        if (g.committed) return NO_GAMES;
        g.committed = true;
        for (const s of g.segs) if (s >= g.first) this.segs.get(s).pins--;
        return [...g.segs];
    }

    // Queues for a snapshot the games not committed that still need a segment compactSegments or
    // more behind the newest one (at open() and at every rotation; the segments are in ascending
    // order, and only the old ones are visited).
    enqueueStale() {
        const limit = this.seq - this.compactSegments;
        for (const seg of this.segs.values()) {
            if (seg.seq > limit) break;
            if (seg.pins === 0) continue;
            for (const gameId of seg.games) {
                const g = this.games.get(gameId);
                if (g && !g.committed && g.first <= limit && !this.snapsPending.has(gameId)) this.compactQueue.add(gameId);
            }
        }
    }

    updateGauges() {
        mSegments.set(this.segs.size);
        mDiskBytes.set(this.diskBytes);
    }

    deletable(seg) {
        if (seg.active || seg.pins > 0) return false;
        for (const gameId of seg.games) {
            const g = this.games.get(gameId);
            if (g && g.commitSeg === seg.seq && g.segs.size > 1) return false;
        }
        return true;
    }

    // Whether the segment holds the 'committed' record of a game it mentions.
    holdsCommit(seg) {
        for (const gameId of seg.games) {
            const g = this.games.get(gameId);
            if (g && g.commitSeg === seg.seq) return true;
        }
        return false;
    }

    // Deletes the candidate segments that no game needs any more, then the segments this makes
    // deletable in turn (one holding a game's 'committed' record, once the game's other segments
    // are gone). With fsync on, the segments holding a 'committed' record are deleted after the
    // others, and only once every earlier deletion is durable (see the header of this file).
    async gc(candidates) {
        let queue = candidates;
        while (queue.length) {
            const next = [];
            const held = new Set();
            for (const s of queue) {
                const seg = this.segs.get(s);
                if (!seg || !this.deletable(seg)) continue;
                if (this.fsync && this.holdsCommit(seg)) held.add(seg);
                else this.remove(seg, next);
            }
            if (held.size) {
                if (this.unlinks > this.syncedUnlinks) await this.syncDir();
                // All of them deletable at the same moment: none waits for another's deletion.
                const ready = [...held].filter((seg) => this.segs.get(seg.seq) === seg && this.deletable(seg));
                for (const seg of ready) this.remove(seg, next);
            }
            queue = next;
        }
    }

    // Unlinks one segment and forgets it; `next` receives the segments holding a 'committed'
    // record that may be deletable now.
    remove(seg, next) {
        try {
            fs.unlinkSync(seg.file);
        } catch (e) {
            if (e.code !== 'ENOENT') { this.log.warn('journal segment not deleted', { file: seg.file, err: e }); return; }
        }
        this.unlinks++;
        mSegmentsDeleted.inc();
        this.segs.delete(seg.seq);
        this.diskBytes -= seg.bytes;
        for (const gameId of seg.games) {
            const g = this.games.get(gameId);
            if (!g) continue;
            g.segs.delete(seg.seq);
            if (g.segs.size === 0) this.games.delete(gameId);
            else if (g.segs.size === 1 && g.segs.has(g.commitSeg)) next.push(g.commitSeg);
        }
    }

    // Directory fsync: the segments created and deleted so far are durable.
    async syncDir() {
        const n = this.unlinks, seq = this.seq;
        await fsyncDir(this.dir);
        if (n > this.syncedUnlinks) this.syncedUnlinks = n;
        if (seq > this.syncedSeq) this.syncedSeq = seq;
    }
}

/**
 * Opens (creating it if needed) the journal of one shard and scans its segments; the games to
 * recover are then available from recover().
 * @param {object} opts
 * @param {string} opts.dir  JOURNAL_DIR
 * @param {number} [opts.shard=0]
 * @param {number} [opts.flushMs=50]  JOURNAL_FLUSH_MS
 * @param {boolean} [opts.fsync=true]  JOURNAL_FSYNC
 * @param {number} [opts.segmentBytes]  rotation size (default 16 MB)
 * @param {number} [opts.compactSegments=4]  JOURNAL_COMPACT_SEGMENTS
 * @param {number} [opts.compactPerFlush=8]  snapshots per flushed batch at most
 * @param {object} [opts.log]
 * @returns {Promise<Journal>}
 */
export async function openJournal(opts) {
    return new Journal(opts).open();
}
