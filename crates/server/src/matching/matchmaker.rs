//! Matchmaker: one queue per (official category, rated flag), paired every `MATCH_TICK_MS` by
//! [`Matchmaker::tick`]. A pure state machine owned by the lobby actor: no timer, no I/O; time is
//! passed in, colours use an injected random source. DESIGN 5.4.
//!
//! Pairing rule:
//! * `window(player) = min(MATCH_WINDOW_START + floor(wait / MATCH_WINDOW_STEP_MS) *
//!   MATCH_WINDOW_STEP, MATCH_WINDOW_MAX)`, plus `MATCH_PROVISIONAL_BONUS` when provisional (the
//!   bonus is added after the cap);
//! * A and B may be paired only when `|rA - rB| <= window(A)` and `<= window(B)` (mutual rule),
//!   neither lists the other in its recent opponents, the pair is not held ([`Matchmaker::hold_pair`])
//!   and, in a rated queue, they have not played `MATCH_REPEAT_LIMIT` rated games together within
//!   `MATCH_REPEAT_WINDOW_MS`;
//! * each tick walks every queue (in creation order) from the longest-waiting player to the
//!   newest; each player still unpaired takes the valid partner with the closest rating (ties:
//!   the one who waited longer);
//! * colours: the player with the higher colour balance (whites minus blacks) gets Black; equal
//!   balances are drawn at random.
//!
//! Data structures (built for 100k+ waiting players): each queue keeps its players in a
//! doubly-linked FIFO ordered by `(joined_at, seq)` and buckets them by rating, one bucket per
//! rating point, each holding two FIFO lists (established and provisional players). A bitset of
//! the non-empty buckets lets a search jump over empty ratings 32 at a time, outwards from the
//! seeker's rating. Inside one bucket list every player has the same rating difference to the
//! seeker and windows only shrink from the head to the tail, so the first player that is not
//! excluded decides for the whole list. Exclusions are skipped at most [`SCAN_CAP`] times per
//! bucket list and search. A join whose `joined_at` is older than the last [`MAX_REORDER`] queued
//! players is raised to theirs (the insertion stays O(1)).
//!
//! Contract clarifications: [`Matchmaker::join`] refuses a user already queued
//! (`QueueNotAllowed`) and a category that is not official (`InvalidCategory`); the conduct and
//! "already in a game" checks belong to the caller. [`Matchmaker::tick`] does not count its rated
//! pairings toward the repeat limit: the lobby calls [`Matchmaker::record_pairing`] once a rated
//! game exists, whatever made it. A join's `color_balance` overrides the balance tracked here,
//! which only pairings update. Everything is in memory: a restart forgets counts, balances and
//! holds.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::sync::{Arc, LazyLock};

use super::elo::Categories;
use super::{MatchError, QueueState};
use crate::config::Config;
use crate::ids::{ConnId, UserId};
use crate::metrics::{self, CounterVec, Gauge, Histogram};

/// Highest rating a queue holds (`PlayerInfo.rating` is a u16).
pub const MAX_RATING: i64 = 65535;
/// Excluded entries skipped per bucket list and search before giving up on the list.
pub const SCAN_CAP: usize = 64;
/// Queued players a late-arriving join may be placed before.
pub const MAX_REORDER: usize = 64;
/// Colour balances kept in memory (the least recently updated are forgotten beyond it).
pub const BALANCE_CAP: usize = 500_000;
/// Period of the `QueueStatus{Searching}` refresh the lobby sends to every queued player.
pub const QUEUE_REFRESH_MS: i64 = 3000;
/// How long a pairing whose game could not be created is held ([`Matchmaker::hold_pair`]).
pub const PAIR_RETRY_DELAY_MS: i64 = 5000;

const INITIAL_CAPACITY: usize = 4096;
const NIL: u32 = u32::MAX;

static PAIRS: LazyLock<CounterVec> = LazyLock::new(|| {
    metrics::counter_vec("scacelith_mm_pairs_total", "Pairs made by the matchmaker", &["rated"])
});
static WAIT: LazyLock<Histogram> = LazyLock::new(|| {
    metrics::histogram(
        "scacelith_mm_wait_ms",
        "Time spent in the queue before being paired",
        &[1000.0, 2500.0, 5000.0, 10000.0, 20000.0, 30000.0, 60000.0, 120000.0, 300000.0],
    )
});
static QUEUED: LazyLock<Gauge> =
    LazyLock::new(|| metrics::gauge("scacelith_mm_queued", "Players waiting in the matchmaking queues"));

/// The matchmaking settings of the configuration.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MatchSettings {
    /// MATCH_WINDOW_START
    pub window_start: i64,
    /// MATCH_WINDOW_STEP
    pub window_step: i64,
    /// MATCH_WINDOW_STEP_MS
    pub window_step_ms: i64,
    /// MATCH_WINDOW_MAX
    pub window_max: i64,
    /// MATCH_PROVISIONAL_BONUS
    pub provisional_bonus: i64,
    /// MATCH_REPEAT_LIMIT
    pub repeat_limit: u32,
    /// MATCH_REPEAT_WINDOW_MS
    pub repeat_window_ms: i64,
}

impl MatchSettings {
    /// The settings of a loaded configuration.
    pub fn from_config(config: &Config) -> MatchSettings {
        MatchSettings {
            window_start: config.match_window_start,
            window_step: config.match_window_step,
            window_step_ms: config.match_window_step_ms,
            window_max: config.match_window_max,
            provisional_bonus: config.match_provisional_bonus,
            repeat_limit: config.match_repeat_limit.clamp(1, i64::from(u32::MAX)) as u32,
            repeat_window_ms: config.match_repeat_window_ms,
        }
    }
}

impl Default for MatchSettings {
    /// The configuration defaults: window 100, +50 every 5 s up to 500, provisional +150, three
    /// rated games per pair and hour.
    fn default() -> Self {
        MatchSettings {
            window_start: 100,
            window_step: 50,
            window_step_ms: 5000,
            window_max: 500,
            provisional_bonus: 150,
            repeat_limit: 3,
            repeat_window_ms: 3_600_000,
        }
    }
}

/// Search window of a player who has waited `wait_ms` (DESIGN 5.4): the bonus of a provisional
/// player is added after the cap.
pub fn search_window(wait_ms: i64, provisional: bool, s: &MatchSettings) -> i64 {
    let step_ms = s.window_step_ms.max(1);
    let grown = if wait_ms > 0 { s.window_start + wait_ms / step_ms * s.window_step } else { s.window_start };
    let w = grown.min(s.window_max);
    if provisional { w + s.provisional_bonus } else { w }
}

/// A request to join a queue.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct JoinRequest {
    pub user_id: UserId,
    pub username: String,
    /// Official category id (`3+2`).
    pub category: String,
    pub rated: bool,
    /// Rating in the category (clamped to 0..=65535).
    pub rating: i64,
    pub provisional: bool,
    /// The live connection that asked (a pairing names it).
    pub conn_id: ConnId,
    /// Colour balance to use instead of the one tracked here.
    pub color_balance: Option<i64>,
    /// When the search started; the caller passes `now` (a rejoin keeps the original time).
    pub joined_at: i64,
    /// Users this player must not be paired with in this queue (both directions).
    pub recent_opponents: Vec<UserId>,
}

/// QueueStatus fields of a searching player.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueueStatus {
    pub category: Arc<str>,
    pub rated: bool,
    /// Always [`QueueState::Searching`] here.
    pub state: QueueState,
    pub wait_ms: u32,
    pub window: u16,
    /// Players in the same queue, the player included.
    pub queued: u32,
}

/// One side of a pairing: a copy of the queue entry plus its waiting time.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PairedPlayer {
    pub user_id: UserId,
    pub username: String,
    pub category: Arc<str>,
    pub rated: bool,
    pub rating: i64,
    pub provisional: bool,
    pub conn_id: ConnId,
    /// Balance used for the colour decision (before this pairing).
    pub color_balance: i64,
    pub joined_at: i64,
    pub wait_ms: i64,
}

impl PairedPlayer {
    /// The request that puts this player back in the queue with the same waiting time and
    /// colour balance (after a pairing whose game could not be created).
    pub fn rejoin_request(&self) -> JoinRequest {
        JoinRequest {
            user_id: self.user_id,
            username: self.username.clone(),
            category: self.category.to_string(),
            rated: self.rated,
            rating: self.rating,
            provisional: self.provisional,
            conn_id: self.conn_id,
            color_balance: Some(self.color_balance),
            joined_at: self.joined_at,
            recent_opponents: Vec::new(),
        }
    }
}

/// Two players paired by [`Matchmaker::tick`], with their colours.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pairing {
    pub category: Arc<str>,
    pub rated: bool,
    pub white: PairedPlayer,
    pub black: PairedPlayer,
}

/// Size of one queue, for tests and diagnostics.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueueSize {
    pub category: Arc<str>,
    pub rated: bool,
    pub size: usize,
}

// A waiting player, stored in the entry arena and linked into its queue's FIFO and bucket list.
#[derive(Debug)]
struct Entry {
    user_id: UserId,
    username: String,
    rating: usize,
    provisional: bool,
    conn_id: ConnId,
    color_balance: i64,
    joined_at: i64,
    seq: u64,
    recent: Option<HashSet<UserId>>,
    queue: usize,
    prev: u32,
    next: u32,
    lprev: u32,
    lnext: u32,
}

// One rating point of a queue: FIFO lists of established (e*) and provisional (p*) players.
#[derive(Clone, Copy, Debug)]
struct Bucket {
    eh: u32,
    et: u32,
    ph: u32,
    pt: u32,
    n: u32,
}

const EMPTY_BUCKET: Bucket = Bucket { eh: NIL, et: NIL, ph: NIL, pt: NIL, n: 0 };

#[derive(Debug)]
struct Queue {
    category: Arc<str>,
    rated: bool,
    size: usize,
    head: u32,
    tail: u32,
    bits: Vec<u32>,
    buckets: Vec<Bucket>,
}

impl Queue {
    fn new(category: Arc<str>, rated: bool) -> Queue {
        Queue {
            category,
            rated,
            size: 0,
            head: NIL,
            tail: NIL,
            bits: vec![0; INITIAL_CAPACITY / 32],
            buckets: vec![EMPTY_BUCKET; INITIAL_CAPACITY],
        }
    }

    fn cap(&self) -> usize {
        self.buckets.len()
    }

    fn grow(&mut self, rating: usize) {
        let mut cap = self.cap();
        while cap <= rating {
            cap *= 2;
        }
        self.bits.resize(cap / 32, 0);
        self.buckets.resize(cap, EMPTY_BUCKET);
    }

    // Lowest non-empty rating in [from, limit].
    fn next_set(&self, from: usize, limit: usize) -> Option<usize> {
        if from > limit {
            return None;
        }
        let mut w = from >> 5;
        let last = limit >> 5;
        let mut m = self.bits[w] & (u32::MAX << (from & 31));
        while m == 0 {
            w += 1;
            if w > last {
                return None;
            }
            m = self.bits[w];
        }
        let r = (w << 5) + m.trailing_zeros() as usize;
        (r <= limit).then_some(r)
    }

    // Highest non-empty rating in [limit, from].
    fn prev_set(&self, from: usize, limit: usize) -> Option<usize> {
        if from < limit {
            return None;
        }
        let mut w = from >> 5;
        let first = limit >> 5;
        let mut m = self.bits[w] & (u32::MAX >> (31 - (from & 31)));
        while m == 0 {
            if w == first {
                return None;
            }
            w -= 1;
            m = self.bits[w];
        }
        let r = (w << 5) + 31 - m.leading_zeros() as usize;
        (r >= limit).then_some(r)
    }
}

// The entries of every queue, in one arena (indices are stable while an entry lives).
#[derive(Debug, Default)]
struct Arena {
    slots: Vec<Option<Entry>>,
    free: Vec<u32>,
}

impl Arena {
    fn alloc(&mut self, e: Entry) -> u32 {
        if let Some(i) = self.free.pop() {
            self.slots[i as usize] = Some(e);
            i
        } else {
            self.slots.push(Some(e));
            (self.slots.len() - 1) as u32
        }
    }

    fn release(&mut self, i: u32) -> Entry {
        let e = self.slots[i as usize].take().expect("released entries are live");
        self.free.push(i);
        e
    }

    fn get(&self, i: u32) -> &Entry {
        self.slots[i as usize].as_ref().expect("linked entries are live")
    }

    fn get_mut(&mut self, i: u32) -> &mut Entry {
        self.slots[i as usize].as_mut().expect("linked entries are live")
    }
}

fn pair_key(a: UserId, b: UserId) -> u64 {
    let (lo, hi) = if a < b { (a, b) } else { (b, a) };
    (u64::from(lo) << 32) | u64::from(hi)
}

/// A source of uniform draws in [0, 1) (the colour draws of equal balances).
pub type RandomSource = Box<dyn FnMut() -> f64 + Send>;

/// A random source seeded from the operating system (SplitMix64). Colour draws need no
/// cryptographic strength.
pub fn os_random() -> RandomSource {
    let mut state =
        getrandom::u64().unwrap_or_else(|_| crate::clock::wall_ms() as u64 ^ 0x9e37_79b9_7f4a_7c15);
    Box::new(move || {
        state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = state;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^= z >> 31;
        (z >> 11) as f64 / (1u64 << 53) as f64
    })
}

// Colour balances (whites minus blacks), forgetting the least recently updated beyond `cap`
// (down to 90% of it). Zero balances are not stored.
#[derive(Debug)]
struct Balances {
    values: HashMap<UserId, (i64, u64)>,
    order: BTreeMap<u64, UserId>,
    stamp: u64,
    cap: usize,
}

impl Balances {
    fn new(cap: usize) -> Balances {
        Balances { values: HashMap::new(), order: BTreeMap::new(), stamp: 0, cap }
    }

    fn get(&self, user: UserId) -> i64 {
        self.values.get(&user).map_or(0, |v| v.0)
    }

    fn set(&mut self, user: UserId, v: i64) {
        if let Some((_, old)) = self.values.remove(&user) {
            self.order.remove(&old);
        }
        if v == 0 {
            return;
        }
        self.stamp += 1;
        self.values.insert(user, (v, self.stamp));
        self.order.insert(self.stamp, user);
        if self.values.len() > self.cap {
            let drop = self.values.len() - self.cap * 9 / 10;
            for _ in 0..drop {
                if let Some((_, u)) = self.order.pop_first() {
                    self.values.remove(&u);
                }
            }
        }
    }
}

/// The matchmaking queues of the lobby.
pub struct Matchmaker {
    settings: MatchSettings,
    categories: Categories,
    random: RandomSource,
    queues: Vec<Queue>,
    queue_index: HashMap<(Arc<str>, bool), usize>,
    arena: Arena,
    by_user: HashMap<UserId, u32>,
    balances: Balances,
    pair_counts: HashMap<u64, u32>,
    holds: HashMap<u64, i64>,
    log: VecDeque<(u64, i64)>,
    seq: u64,
}

impl std::fmt::Debug for Matchmaker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Matchmaker")
            .field("queued", &self.by_user.len())
            .field("queues", &self.queues.len())
            .finish()
    }
}

impl Matchmaker {
    /// A matchmaker over the official `categories`, drawing colours with `random`.
    pub fn new(settings: MatchSettings, categories: Categories, random: RandomSource) -> Matchmaker {
        Matchmaker {
            settings,
            categories,
            random,
            queues: Vec::new(),
            queue_index: HashMap::new(),
            arena: Arena::default(),
            by_user: HashMap::new(),
            balances: Balances::new(BALANCE_CAP),
            pair_counts: HashMap::new(),
            holds: HashMap::new(),
            log: VecDeque::new(),
            seq: 0,
        }
    }

    /// A matchmaker configured from `config`, with an OS-seeded random source.
    pub fn from_config(config: &Config) -> Matchmaker {
        Matchmaker::new(MatchSettings::from_config(config), Categories::from_config(config), os_random())
    }

    /// Players waiting in all queues.
    pub fn len(&self) -> usize {
        self.by_user.len()
    }

    /// Whether no player waits.
    pub fn is_empty(&self) -> bool {
        self.by_user.is_empty()
    }

    fn window(&self, e: &Entry, now: i64) -> i64 {
        search_window(now - e.joined_at, e.provisional, &self.settings)
    }

    fn before(&self, a: u32, b: u32) -> bool {
        let (x, y) = (self.arena.get(a), self.arena.get(b));
        x.joined_at < y.joined_at || (x.joined_at == y.joined_at && x.seq < y.seq)
    }

    /// Adds a player to the queue of (category, rated). Refuses a user already queued in any
    /// queue (`QueueNotAllowed`, also for the invalid user id 0) and a category that is not
    /// official (`InvalidCategory`).
    pub fn join(&mut self, req: JoinRequest) -> Result<(), MatchError> {
        if req.user_id == 0 || self.by_user.contains_key(&req.user_id) {
            return Err(MatchError::QueueNotAllowed);
        }
        let category = match self.categories.parse(&req.category) {
            Some(c) => c.id.clone(),
            None => return Err(MatchError::InvalidCategory),
        };
        let rating = req.rating.clamp(0, MAX_RATING) as usize;
        let color_balance = req.color_balance.unwrap_or_else(|| self.balances.get(req.user_id));
        let recent =
            (!req.recent_opponents.is_empty()).then(|| req.recent_opponents.iter().copied().collect());
        let qi = self.queue_of(category, req.rated);
        self.seq += 1;
        let idx = self.arena.alloc(Entry {
            user_id: req.user_id,
            username: req.username,
            rating,
            provisional: req.provisional,
            conn_id: req.conn_id,
            color_balance,
            joined_at: req.joined_at,
            seq: self.seq,
            recent,
            queue: qi,
            prev: NIL,
            next: NIL,
            lprev: NIL,
            lnext: NIL,
        });
        self.insert(qi, idx);
        self.by_user.insert(req.user_id, idx);
        QUEUED.set(self.by_user.len() as f64);
        Ok(())
    }

    fn queue_of(&mut self, category: String, rated: bool) -> usize {
        let category: Arc<str> = Arc::from(category);
        if let Some(&i) = self.queue_index.get(&(category.clone(), rated)) {
            return i;
        }
        self.queues.push(Queue::new(category.clone(), rated));
        self.queue_index.insert((category, rated), self.queues.len() - 1);
        self.queues.len() - 1
    }

    fn insert(&mut self, qi: usize, e: u32) {
        let rating = self.arena.get(e).rating;
        if rating >= self.queues[qi].cap() {
            self.queues[qi].grow(rating);
        }
        // Queue FIFO by joined_at (appending is the normal case); a joined_at older than
        // MAX_REORDER queued players is raised to theirs.
        let mut p = self.queues[qi].tail;
        let mut steps = 0;
        while p != NIL && self.before(e, p) {
            steps += 1;
            if steps > MAX_REORDER {
                let joined = self.arena.get(p).joined_at;
                self.arena.get_mut(e).joined_at = joined;
                break;
            }
            p = self.arena.get(p).prev;
        }
        let next = if p == NIL { self.queues[qi].head } else { self.arena.get(p).next };
        {
            let entry = self.arena.get_mut(e);
            entry.prev = p;
            entry.next = next;
        }
        if next != NIL {
            self.arena.get_mut(next).prev = e;
        } else {
            self.queues[qi].tail = e;
        }
        if p != NIL {
            self.arena.get_mut(p).next = e;
        } else {
            self.queues[qi].head = e;
        }
        // Bucket list, walking back without a cap.
        let prov = self.arena.get(e).provisional;
        let bucket = self.queues[qi].buckets[rating];
        let mut q = if prov { bucket.pt } else { bucket.et };
        while q != NIL && self.before(e, q) {
            q = self.arena.get(q).lprev;
        }
        let lnext = if q == NIL { if prov { bucket.ph } else { bucket.eh } } else { self.arena.get(q).lnext };
        {
            let entry = self.arena.get_mut(e);
            entry.lprev = q;
            entry.lnext = lnext;
        }
        let queue = &mut self.queues[qi];
        let b = &mut queue.buckets[rating];
        if lnext != NIL {
            self.arena.get_mut(lnext).lprev = e;
        } else if prov {
            b.pt = e;
        } else {
            b.et = e;
        }
        if q != NIL {
            self.arena.get_mut(q).lnext = e;
        } else if prov {
            b.ph = e;
        } else {
            b.eh = e;
        }
        b.n += 1;
        if b.n == 1 {
            queue.bits[rating >> 5] |= 1 << (rating & 31);
        }
        queue.size += 1;
    }

    fn unlink(&mut self, e: u32) {
        let (qi, rating, prov, prev, next, lprev, lnext) = {
            let x = self.arena.get(e);
            (x.queue, x.rating, x.provisional, x.prev, x.next, x.lprev, x.lnext)
        };
        if prev != NIL {
            self.arena.get_mut(prev).next = next;
        } else {
            self.queues[qi].head = next;
        }
        if next != NIL {
            self.arena.get_mut(next).prev = prev;
        } else {
            self.queues[qi].tail = prev;
        }
        let queue = &mut self.queues[qi];
        let b = &mut queue.buckets[rating];
        if lprev != NIL {
            self.arena.get_mut(lprev).lnext = lnext;
        } else if prov {
            b.ph = lnext;
        } else {
            b.eh = lnext;
        }
        if lnext != NIL {
            self.arena.get_mut(lnext).lprev = lprev;
        } else if prov {
            b.pt = lprev;
        } else {
            b.et = lprev;
        }
        b.n -= 1;
        if b.n == 0 {
            queue.bits[rating >> 5] &= !(1 << (rating & 31));
        }
        queue.size -= 1;
        let x = self.arena.get_mut(e);
        x.prev = NIL;
        x.next = NIL;
        x.lprev = NIL;
        x.lnext = NIL;
    }

    /// Removes a player from its queue; returns whether the player was queued.
    pub fn leave(&mut self, user_id: UserId) -> bool {
        let Some(e) = self.by_user.remove(&user_id) else { return false };
        self.unlink(e);
        self.arena.release(e);
        QUEUED.set(self.by_user.len() as f64);
        true
    }

    /// Whether the user is queued.
    pub fn has(&self, user_id: UserId) -> bool {
        self.by_user.contains_key(&user_id)
    }

    /// The QueueStatus of a searching player, `None` when the player is not queued.
    pub fn status_of(&self, user_id: UserId, now: i64) -> Option<QueueStatus> {
        let &e = self.by_user.get(&user_id)?;
        Some(self.status(e, now))
    }

    fn status(&self, e: u32, now: i64) -> QueueStatus {
        let entry = self.arena.get(e);
        let queue = &self.queues[entry.queue];
        QueueStatus {
            category: queue.category.clone(),
            rated: queue.rated,
            state: QueueState::Searching,
            wait_ms: (now - entry.joined_at).clamp(0, i64::from(u32::MAX)) as u32,
            window: self.window(entry, now).clamp(0, 0xFFFF) as u16,
            queued: queue.size.min(u32::MAX as usize) as u32,
        }
    }

    /// The QueueStatus of every queued player (the lobby's refresh every [`QUEUE_REFRESH_MS`]).
    pub fn statuses(&self, now: i64) -> Vec<(UserId, QueueStatus)> {
        self.by_user.iter().map(|(&u, &e)| (u, self.status(e, now))).collect()
    }

    /// One pairing round over every queue. The paired players leave their queues.
    pub fn tick(&mut self, now: i64) -> Vec<Pairing> {
        self.expire_repeats(now);
        if !self.holds.is_empty() {
            self.holds.retain(|_, until| *until > now);
        }
        let mut pairs = Vec::new();
        for qi in 0..self.queues.len() {
            let mut a = self.queues[qi].head;
            while a != NIL && self.queues[qi].size >= 2 {
                let Some(b) = self.search(qi, a, now) else {
                    a = self.arena.get(a).next;
                    continue;
                };
                self.unlink(b); // b may be a.next: unlink it before reading a.next
                let next = self.arena.get(a).next;
                self.unlink(a);
                let ea = self.arena.release(a);
                let eb = self.arena.release(b);
                self.by_user.remove(&ea.user_id);
                self.by_user.remove(&eb.user_id);
                pairs.push(self.pair(qi, ea, eb, now));
                a = next;
            }
        }
        if !pairs.is_empty() {
            QUEUED.set(self.by_user.len() as f64);
        }
        pairs
    }

    // Closest valid partner of `a` (ties: longest wait).
    fn search(&self, qi: usize, a: u32, now: i64) -> Option<u32> {
        let q = &self.queues[qi];
        let entry = self.arena.get(a);
        let ra = entry.rating;
        let wa = self.window(entry, now).max(0) as usize;
        let lo = ra.saturating_sub(wa);
        let hi = (ra + wa).min(q.cap() - 1);
        let mut up = q.next_set(ra, hi);
        let mut down = if ra > lo { q.prev_set(ra - 1, lo) } else { None };
        while up.is_some() || down.is_some() {
            let du = up.map_or(usize::MAX, |u| u - ra);
            let dd = down.map_or(usize::MAX, |d| ra - d);
            let d = du.min(dd);
            let mut best = None;
            if let Some(u) = up.filter(|_| du == d) {
                best = self.scan_bucket(qi, u, a, d as i64, now);
                up = if u < hi { q.next_set(u + 1, hi) } else { None };
            }
            if let Some(dn) = down.filter(|_| dd == d) {
                if let Some(c) = self.scan_bucket(qi, dn, a, d as i64, now)
                    && best.is_none_or(|b| self.before(c, b))
                {
                    best = Some(c);
                }
                down = if dn > lo { q.prev_set(dn - 1, lo) } else { None };
            }
            if best.is_some() {
                return best;
            }
        }
        None
    }

    // Oldest valid partner of `a` in one bucket (rating difference d, inside a's window).
    fn scan_bucket(&self, qi: usize, rating: usize, a: u32, d: i64, now: i64) -> Option<u32> {
        let bucket = self.queues[qi].buckets[rating];
        let mut best: Option<u32> = None;
        for head in [bucket.eh, bucket.ph] {
            let mut b = head;
            let mut scanned = 0;
            while b != NIL && scanned < SCAN_CAP {
                if b != a {
                    if !self.excluded(qi, a, b, now) {
                        // The first entry that is not excluded decides for the whole list.
                        if d <= self.window(self.arena.get(b), now) && best.is_none_or(|x| self.before(b, x))
                        {
                            best = Some(b);
                        }
                        break;
                    }
                    scanned += 1;
                }
                b = self.arena.get(b).lnext;
            }
        }
        best
    }

    fn excluded(&self, qi: usize, a: u32, b: u32, now: i64) -> bool {
        let (ea, eb) = (self.arena.get(a), self.arena.get(b));
        if ea.recent.as_ref().is_some_and(|r| r.contains(&eb.user_id)) {
            return true;
        }
        if eb.recent.as_ref().is_some_and(|r| r.contains(&ea.user_id)) {
            return true;
        }
        let key = pair_key(ea.user_id, eb.user_id);
        if !self.holds.is_empty() && self.holds.get(&key).is_some_and(|&until| until > now) {
            return true;
        }
        self.queues[qi].rated
            && !self.pair_counts.is_empty()
            && self.pair_counts.get(&key).is_some_and(|&c| c >= self.settings.repeat_limit)
    }

    fn pair(&mut self, qi: usize, a: Entry, b: Entry, now: i64) -> Pairing {
        let a_white = if a.color_balance > b.color_balance {
            false
        } else if b.color_balance > a.color_balance {
            true
        } else {
            (self.random)() < 0.5
        };
        let (white, black) = if a_white { (a, b) } else { (b, a) };
        self.balances.set(white.user_id, white.color_balance + 1);
        self.balances.set(black.user_id, black.color_balance - 1);
        let (category, rated) = (self.queues[qi].category.clone(), self.queues[qi].rated);
        PAIRS.with(&[if rated { "true" } else { "false" }]).inc();
        WAIT.observe((now - white.joined_at) as f64);
        WAIT.observe((now - black.joined_at) as f64);
        let view = |e: Entry| PairedPlayer {
            user_id: e.user_id,
            username: e.username,
            category: category.clone(),
            rated,
            rating: e.rating as i64,
            provisional: e.provisional,
            conn_id: e.conn_id,
            color_balance: e.color_balance,
            joined_at: e.joined_at,
            wait_ms: (now - e.joined_at).max(0),
        };
        Pairing { category: category.clone(), rated, white: view(white), black: view(black) }
    }

    /// Colour balance (whites minus blacks) the matchmaker keeps for a user.
    pub fn color_balance_of(&self, user_id: UserId) -> i64 {
        self.balances.get(user_id)
    }

    /// Records the colours of one game in the balances. The lobby calls it with the colours
    /// reversed to give back a pairing whose game could not be created.
    pub fn record_colors(&mut self, white: UserId, black: UserId) {
        let w = self.balances.get(white);
        self.balances.set(white, w + 1);
        let b = self.balances.get(black);
        self.balances.set(black, b - 1);
    }

    /// Counts one rated game between two users for the repeat limit (the lobby, once a rated
    /// game exists: queue, challenge, private code or rematch).
    pub fn record_pairing(&mut self, a: UserId, b: UserId, now: i64) {
        let key = pair_key(a, b);
        *self.pair_counts.entry(key).or_insert(0) += 1;
        self.log.push_back((key, now));
    }

    /// Keeps two users from being paired together before `until`; their pairings with others
    /// are not affected.
    pub fn hold_pair(&mut self, a: UserId, b: UserId, until: i64) {
        self.holds.insert(pair_key(a, b), until);
    }

    /// Pairs currently held (ended holds are dropped by the next tick).
    pub fn held_pairs(&self) -> usize {
        self.holds.len()
    }

    /// Rated games of two users inside the repeat window.
    pub fn repeat_count(&mut self, a: UserId, b: UserId, now: i64) -> u32 {
        self.expire_repeats(now);
        self.pair_counts.get(&pair_key(a, b)).copied().unwrap_or(0)
    }

    /// Whether two users played `MATCH_REPEAT_LIMIT` rated games together inside the repeat
    /// window: the rated queue no longer pairs them, and the lobby refuses their rated
    /// challenges, private games and rematches.
    pub fn repeat_limited(&mut self, a: UserId, b: UserId, now: i64) -> bool {
        self.repeat_count(a, b, now) >= self.settings.repeat_limit
    }

    fn expire_repeats(&mut self, now: i64) {
        let cutoff = now - self.settings.repeat_window_ms;
        while let Some(&(key, at)) = self.log.front() {
            if at > cutoff {
                break;
            }
            self.log.pop_front();
            match self.pair_counts.get_mut(&key) {
                Some(c) if *c > 1 => *c -= 1,
                _ => {
                    self.pair_counts.remove(&key);
                }
            }
        }
    }

    /// Queue sizes in creation order (tests and diagnostics).
    pub fn queue_sizes(&self) -> Vec<QueueSize> {
        self.queues
            .iter()
            .map(|q| QueueSize { category: q.category.clone(), rated: q.rated, size: q.size })
            .collect()
    }
}

#[cfg(test)]
mod tests;
