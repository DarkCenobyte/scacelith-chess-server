// Game journal (DESIGN.md 5.6): crash safety of the games in progress without a database write
// per move. One journal per shard, JOURNAL_DIR/shard-<n>/segment-<seq>.log.
//
// Record (little-endian):
//   u32 length   bytes after this 8-byte header (17 + payload length)
//   u32 crc32c   CRC-32C (Castagnoli) of those `length` bytes
//   u8  kind     1 created, 2 move, 3 event, 4 ended, 5 committed
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
// of games it mentions and how many of them are not committed yet; per game, the segments that
// mention it and the one holding its 'committed' record. A segment is deleted when every game it
// mentions is committed (the 'committed' record durably written), and a segment holding a game's
// 'committed' record outlives that game's older segments, so a later recovery can never see a
// committed game's moves without its 'committed' record.
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
export const JournalKind = Object.freeze({ Created: 1, Move: 2, Event: 3, Ended: 4, Committed: 5 });

/** Default segment size before rotation. */
export const SEGMENT_BYTES = 16 * 1024 * 1024;
/** Largest payload of one record. */
export const MAX_PAYLOAD = 1024 * 1024;

const HEADER = 8;
const FIXED = 17;                  // kind + gameId + at
const TWO32 = 4294967296;
const INITIAL_BUFFER = 64 * 1024;
const MAX_SPARE = 4 * 1024 * 1024;
const EMPTY = Buffer.alloc(0);
const SEGMENT_RE = /^segment-(\d+)\.log$/;

const mFlushMs = metrics.histogram('scacelith_journal_flush_ms', 'Journal write (+ fsync) duration per flush',
    [0.25, 0.5, 1, 2, 5, 10, 25, 50, 100, 250]);
const mBytes = metrics.counter('scacelith_journal_bytes_total', 'Bytes written to the game journal');
const mRecords = metrics.counter('scacelith_journal_records_total', 'Records appended to the game journal');
const mErrors = metrics.counter('scacelith_journal_errors_total', 'Failed journal writes');
const mSegmentsDeleted = metrics.counter('scacelith_journal_segments_deleted_total', 'Journal segments deleted once all their games were committed');

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
    constructor({ dir, shard = 0, flushMs = 50, fsync = true, segmentBytes = SEGMENT_BYTES, log = logger.child('journal') }) {
        if (!dir) throw new TypeError('openJournal: dir is required');
        this.dir = path.join(dir, `shard-${shard}`);
        this.shard = shard;
        this.flushMs = Math.max(0, flushMs);
        this.fsync = !!fsync;
        this.segmentBytes = segmentBytes;
        this.log = log;

        this.buf = Buffer.allocUnsafe(INITIAL_BUFFER);
        this.len = 0;
        this.spare = null;
        this.batchGames = new Set();
        this.batchCommits = [];
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
        this.segs = new Map();          // seq -> { seq, file, games: Set, uncommitted, active }
        this.games = new Map();         // gameId -> { segs: Set<seq>, committed, commitSeg }
        this.recovered = new Map();
        this.recoverInfo = { segments: 0, records: 0, games: 0, problems: [] };
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
        for (const seq of seqs) {
            this.seq = Math.max(this.seq, seq);
            const file = path.join(this.dir, segmentName(seq));
            const buf = await fsp.readFile(file);
            const seg = { seq, file, games: new Set(), uncommitted: 0, active: false };
            this.segs.set(seq, seg);
            const res = parseSegment(buf, (kind, gameId, at, payload) => {
                this.recoverInfo.records++;
                let list = perGame.get(gameId);
                if (!list) { list = []; perGame.set(gameId, list); }
                list.push({ kind, at, payload: Buffer.from(payload) });
                this.track(gameId, seg);
                if (kind === JournalKind.Committed) {
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
        this.gc([...this.segs.keys()]);
        return this;
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

    /** Marks a game as committed to the database: its records may be forgotten. */
    committed(gameId) {
        this.append(JournalKind.Committed, gameId, EMPTY, Date.now());
    }

    /**
     * Games found at open() without a 'committed' record, with their records in order.
     * @returns {Map<number, {kind:number, at:number, payload:Buffer}[]>}
     */
    recover() {
        return this.recovered;
    }

    /** Numbers for metrics and tests. */
    stats() {
        return {
            segments: this.segs.size, seq: this.seq, segmentBytes: this.segSize, pendingBytes: this.len,
            trackedGames: this.games.size, writing: this.writing, recovery: this.recoverInfo,
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
        const waiters = this.waiters;
        this.buf = this.spare || Buffer.allocUnsafe(INITIAL_BUFFER);
        this.spare = null;
        this.len = 0;
        this.batchGames = new Set();
        this.batchCommits = [];
        this.waiters = [];
        this.inflight = waiters;
        this.writing = true;
        const t0 = performance.now();
        this.writeBatch(buf, len).then((seg) => {
            mFlushMs.observe(performance.now() - t0);
            mBytes.inc(len);
            for (const g of games) this.track(g, seg);
            const touched = new Set();
            for (const g of commits) for (const s of this.markCommitted(g, seg.seq)) touched.add(s);
            if (touched.size) this.gc([...touched]);
            if (buf.length <= MAX_SPARE) this.spare = buf;
            this.finishWrite(null);
        }, (err) => {
            mErrors.inc();
            this.forceRotate = true;        // later records must not follow a possibly torn write
            this.log.error('journal write failed', { err, bytes: len });
            this.finishWrite(err);
        });
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

    async writeBatch(buf, len) {
        if (!this.fh || this.forceRotate || this.segSize >= this.segmentBytes) await this.rotate();
        let off = 0;
        while (off < len) {
            const { bytesWritten } = await this.fh.write(buf, off, len - off, null);
            off += bytesWritten;
        }
        this.segSize += len;
        if (this.fsync) await this.fh.datasync();
        return this.segs.get(this.seq);
    }

    async rotate() {
        this.forceRotate = false;
        const prev = this.fh ? this.segs.get(this.seq) : null;
        if (this.fh) {
            const fh = this.fh;
            this.fh = null;
            await fh.close();
        }
        const seq = this.seq + 1;
        const file = path.join(this.dir, segmentName(seq));
        this.fh = await fsp.open(file, 'a');
        if (this.fsync) await fsyncDir(this.dir);
        this.seq = seq;
        this.segSize = 0;
        this.segs.set(seq, { seq, file, games: new Set(), uncommitted: 0, active: true });
        if (prev) {
            prev.active = false;
            this.gc([prev.seq]);
        }
    }

    track(gameId, seg) {
        let g = this.games.get(gameId);
        if (!g) {
            g = { segs: new Set(), committed: false, commitSeg: -1 };
            this.games.set(gameId, g);
        }
        if (!g.segs.has(seg.seq)) {
            g.segs.add(seg.seq);
            seg.games.add(gameId);
            if (!g.committed) seg.uncommitted++;
        }
    }

    // Returns the segments whose uncommitted count dropped.
    markCommitted(gameId, seq) {
        const g = this.games.get(gameId);
        if (!g) return [];
        g.commitSeg = Math.max(g.commitSeg, seq);
        if (g.committed) return [];
        g.committed = true;
        for (const s of g.segs) this.segs.get(s).uncommitted--;
        return [...g.segs];
    }

    deletable(seg) {
        if (seg.active || seg.uncommitted > 0) return false;
        for (const gameId of seg.games) {
            const g = this.games.get(gameId);
            if (g && g.commitSeg === seg.seq && g.segs.size > 1) return false;
        }
        return true;
    }

    gc(candidates) {
        const queue = [...candidates];
        while (queue.length) {
            const seg = this.segs.get(queue.pop());
            if (!seg || !this.deletable(seg)) continue;
            try {
                fs.unlinkSync(seg.file);
            } catch (e) {
                if (e.code !== 'ENOENT') { this.log.warn('journal segment not deleted', { file: seg.file, err: e }); continue; }
            }
            mSegmentsDeleted.inc();
            this.segs.delete(seg.seq);
            for (const gameId of seg.games) {
                const g = this.games.get(gameId);
                if (!g) continue;
                g.segs.delete(seg.seq);
                if (g.segs.size === 0) this.games.delete(gameId);
                else if (g.segs.size === 1 && g.segs.has(g.commitSeg)) queue.push(g.commitSeg);
            }
        }
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
 * @param {object} [opts.log]
 * @returns {Promise<Journal>}
 */
export async function openJournal(opts) {
    return new Journal(opts).open();
}
