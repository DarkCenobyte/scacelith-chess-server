// Game identifiers: 53-bit integers, exact in a JavaScript number and in a C++ uint64_t.
//
//   id = (ms since 2026-01-01T00:00:00Z) * 2^12 + shard * 2^6 + seq
//        41 bits of time (69 years)       6 bits      6 bits (64 ids per ms and shard)
//
// The shard that hosts a game is part of its id, so any process can route a message to the
// game's host without a lookup table, including after a restart (the journal replays the game
// on the same shard). A multi-instance deployment gives each instance its own shard range
// (SHARD_BASE). Ids are unique as long as a shard number is used by one process at a time.

export const ID_EPOCH_MS = Date.UTC(2026, 0, 1);
const SHARD_MUL = 64;          // 2^6
const TIME_MUL = 4096;         // 2^12

export class GameIdAllocator {
    constructor(shard) {
        if (!Number.isInteger(shard) || shard < 0 || shard > 63) throw new RangeError('shard must be 0..63');
        this.shard = shard;
        this.lastMs = 0;
        this.seq = 0;
    }
    next(nowMs = Date.now()) {
        let t = Math.max(0, Math.floor(nowMs) - ID_EPOCH_MS);
        if (t < this.lastMs) t = this.lastMs;          // clock went backwards: stay monotonic
        if (t === this.lastMs) {
            if (++this.seq >= 64) { t = this.lastMs + 1; this.seq = 0; }  // borrow the next ms
        } else this.seq = 0;
        this.lastMs = t;
        return t * TIME_MUL + this.shard * SHARD_MUL + this.seq;
    }
}

export function shardOfGameId(id) { return Math.floor(id / SHARD_MUL) % 64; }
export function isGameId(id) { return Number.isSafeInteger(id) && id > 0; }
