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
        let mut t = now_ms.saturating_sub(ID_EPOCH_MS).max(0);
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

    /// Marks every id up to the end of the millisecond of `id` (a game id already given, by any
    /// shard: found in the database or the journal) used, so every later id is greater. Values that
    /// are not game ids are ignored.
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

    /// Values computed with the former server's allocator (src/util/ids.js).
    const T0: i64 = 1_800_000_000_000;

    #[test]
    fn same_ids_as_the_former_allocator() {
        let mut a = GameIdAllocator::new(3);
        assert_eq!(
            [a.next(T0), a.next(T0), a.next(T0 - 5)],
            [134243942400192, 134243942400193, 134243942400194]
        );
        // The first id at the epoch itself gets sequence 1 (the allocator starts at time 0), and
        // a clock before the epoch counts as the epoch.
        let mut b = GameIdAllocator::new(0);
        assert_eq!([b.next(ID_EPOCH_MS), b.next(ID_EPOCH_MS - 1000)], [1, 2]);
        // 64 ids per millisecond, then the next millisecond is borrowed.
        let mut c = GameIdAllocator::new(63);
        let ids: Vec<GameId> = (0..65).map(|_| c.next(T0)).collect();
        assert_eq!(ids[62..], [134243942404094, 134243942404095, 134243942408128]);
        assert_eq!(shard_of(ids[64]), 63);
        assert_eq!(created_ms(ids[64]), T0 + 1);
        assert!(is_game_id(ID53_LIMIT - 1) && !is_game_id(ID53_LIMIT) && !is_game_id(0));
    }

    /// Ported from game.host.test.js (game ids are never given again after a restart with the
    /// clock behind).
    #[test]
    fn never_gives_an_id_again_after_a_restart_with_the_clock_behind() {
        let used = GameIdAllocator::new(9).next(T0); // another shard's id
        assert_eq!(used, 134243942400576);
        let mut ids = GameIdAllocator::new(3);
        ids.seed(used);
        let after: Vec<GameId> = [T0 - 120_000, T0, T0 + 1].into_iter().map(|t| ids.next(t)).collect();
        assert_eq!(after, [134243942404288, 134243942404289, 134243942404290]);
        for bad in [0, ID53_LIMIT, u64::MAX] {
            ids.seed(bad);
        }
        assert_eq!(ids.next(T0 + 2), 134243942408384);
        // An older id does not move the allocator back.
        ids.seed(used);
        assert!(ids.next(T0) > 134243942408384);
    }

    #[test]
    #[should_panic(expected = "shard must be 0..63")]
    fn refuses_a_shard_beyond_the_id_bits() {
        GameIdAllocator::new(MAX_SHARDS);
    }
}
