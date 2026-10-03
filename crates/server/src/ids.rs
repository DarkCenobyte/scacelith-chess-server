//! Identifiers shared by every module.
//!
//! A game id packs `(ms since 2026-01-01) << 12 | shard << 6 | seq`: 41 bits of time, 6 bits of
//! host shard and 6 bits of sequence (64 ids per millisecond and shard, borrowing the next
//! millisecond when exhausted). Ids stay below 2^53 so JSON numbers and the protocol's `id53`
//! carry them exactly. The shard bits route a game to the host actor that owns it, without any
//! lookup table.

/// Account id (SQLite rowid; the protocol carries it as `u32`).
pub type UserId = u32;

/// Game id (see the module documentation for the layout). `0` means "no game".
pub type GameId = u64;

/// Process-wide connection id of a realtime connection (never 0).
pub type ConnId = u32;

/// 2026-01-01T00:00:00Z in Unix milliseconds.
pub const ID_EPOCH_MS: i64 = 1_767_225_600_000;

/// Number of host shards a game id can address.
pub const MAX_SHARDS: u32 = 64;

/// Largest value an `id53` can hold, exclusive.
pub const ID53_LIMIT: u64 = 1 << 53;

/// Whether `id` is a possible game id (non-zero and below 2^53).
pub fn is_game_id(id: u64) -> bool {
    id > 0 && id < ID53_LIMIT
}

/// The host shard that owns a game.
pub fn shard_of(id: GameId) -> u32 {
    ((id >> 6) & 63) as u32
}

/// The creation time of a game, in Unix milliseconds.
pub fn created_ms(id: GameId) -> i64 {
    (id >> 12) as i64 + ID_EPOCH_MS
}

/// Allocates increasing game ids for one host shard.
#[derive(Debug, Clone)]
pub struct GameIdAllocator {
    shard: u64,
    last_ms: i64,
    seq: u64,
}

impl GameIdAllocator {
    /// # Panics
    /// When `shard` is not below [`MAX_SHARDS`].
    pub fn new(shard: u32) -> GameIdAllocator {
        assert!(shard < MAX_SHARDS, "shard must be 0..63");
        GameIdAllocator { shard: shard as u64, last_ms: 0, seq: 0 }
    }

    /// The next id at wall time `now_ms`. Ids keep increasing when the clock goes backwards.
    pub fn next(&mut self, now_ms: i64) -> GameId {
        let mut t = (now_ms - ID_EPOCH_MS).max(0);
        if t < self.last_ms {
            t = self.last_ms;
        }
        if t == self.last_ms {
            self.seq += 1;
            if self.seq >= 64 {
                t = self.last_ms + 1;
                self.seq = 0;
            }
        } else {
            self.seq = 0;
        }
        self.last_ms = t;
        ((t as u64) << 12) | (self.shard << 6) | self.seq
    }

    /// Makes every later id greater than `id` (an id found in the database or the journal).
    pub fn seed(&mut self, id: GameId) {
        if !is_game_id(id) {
            return;
        }
        let t = (id >> 12) as i64;
        if t >= self.last_ms {
            self.last_ms = t;
            self.seq = 63;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_and_routing() {
        let mut a = GameIdAllocator::new(5);
        let id = a.next(ID_EPOCH_MS + 1000);
        assert_eq!(shard_of(id), 5);
        assert_eq!(created_ms(id), ID_EPOCH_MS + 1000);
        assert!(is_game_id(id));
        assert_eq!(id & 63, 0);
    }

    #[test]
    fn borrows_the_next_millisecond_and_never_goes_back() {
        let mut a = GameIdAllocator::new(0);
        let mut last = 0;
        for _ in 0..200 {
            let id = a.next(ID_EPOCH_MS + 50);
            assert!(id > last);
            last = id;
        }
        assert!(a.next(ID_EPOCH_MS) > last);
    }

    #[test]
    fn seed_moves_past_known_ids() {
        let mut a = GameIdAllocator::new(1);
        let mut other = GameIdAllocator::new(2);
        let known = other.next(ID_EPOCH_MS + 10_000);
        a.seed(known);
        assert!(a.next(ID_EPOCH_MS + 5) > known);
        a.seed(0);
    }
}
