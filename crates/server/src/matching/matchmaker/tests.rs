//! Port of test/unit/match.matchmaker.test.js, plus the Rust-only parts of the API.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::Instant;

use indexmap::{IndexMap, IndexSet};

use super::*;
use crate::config::Config;

// Window 100, +50 every 5 s up to 500, provisional +150, three rated games per pair and hour.
fn settings() -> MatchSettings {
    MatchSettings::from_config(&Config::for_tests())
}

fn mm_with(random: RandomSource) -> Matchmaker {
    Matchmaker::new(settings(), Categories::from_config(&Config::for_tests()), random)
}

fn mm() -> Matchmaker {
    mm_with(fixed(0.25))
}

fn fixed(v: f64) -> RandomSource {
    Box::new(move || v)
}

fn player(user_id: UserId, rating: i64) -> JoinRequest {
    JoinRequest {
        user_id,
        username: format!("p{user_id}"),
        category: "3+2".to_string(),
        rated: true,
        rating,
        provisional: false,
        conn_id: user_id,
        color_balance: None,
        joined_at: 0,
        recent_opponents: Vec::new(),
    }
}

fn at(user_id: UserId, rating: i64, joined_at: i64) -> JoinRequest {
    JoinRequest { joined_at, ..player(user_id, rating) }
}

fn ids(p: &Pairing) -> [UserId; 2] {
    let (a, b) = (p.white.user_id, p.black.user_id);
    if a < b { [a, b] } else { [b, a] }
}

fn all_ids(pairs: &[Pairing]) -> Vec<[UserId; 2]> {
    pairs.iter().map(ids).collect()
}

fn lcg(seed: u32) -> impl FnMut() -> f64 {
    let mut s = seed;
    move || {
        s = s.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        f64::from(s) / 4_294_967_296.0
    }
}

#[test]
fn join_validation_leave_has() {
    let mut m = mm();
    assert_eq!(m.join(player(1, 1500)), Ok(()));
    assert!(m.has(1));
    assert_eq!(m.join(player(1, 1500)), Err(MatchError::QueueNotAllowed));
    assert_eq!(
        m.join(JoinRequest { category: "5+0".into(), ..player(1, 1500) }),
        Err(MatchError::QueueNotAllowed)
    );
    for category in ["4+0", "custom", ""] {
        assert_eq!(
            m.join(JoinRequest { category: category.into(), ..player(2, 1500) }),
            Err(MatchError::InvalidCategory)
        );
    }
    assert_eq!(m.join(player(0, 1500)), Err(MatchError::QueueNotAllowed), "user id 0 is never valid");
    assert!(m.leave(1));
    assert!(!m.leave(1));
    assert!(!m.has(1));
    assert_eq!(m.join(JoinRequest { category: "5+0".into(), ..player(1, 1500) }), Ok(()));
    assert_eq!(m.len(), 1);
}

#[test]
fn window_growth_cap_and_provisional_bonus() {
    let s = settings();
    assert_eq!(search_window(0, false, &s), 100);
    assert_eq!(search_window(4999, false, &s), 100);
    assert_eq!(search_window(5000, false, &s), 150);
    assert_eq!(search_window(20000, false, &s), 300);
    assert_eq!(search_window(40000, false, &s), 500);
    assert_eq!(search_window(600000, false, &s), 500);
    assert_eq!(search_window(0, true, &s), 250);
    assert_eq!(search_window(600000, true, &s), 650);
    assert_eq!(search_window(-5000, false, &s), 100, "a negative wait keeps the start window");

    let mut m = mm();
    m.join(at(10, 1500, 1000)).unwrap();
    m.join(JoinRequest { provisional: true, category: "5+0".into(), ..at(11, 1500, 1000) }).unwrap();
    assert_eq!(
        m.status_of(10, 1000),
        Some(QueueStatus {
            category: Arc::from("3+2"),
            rated: true,
            state: QueueState::Searching,
            wait_ms: 0,
            window: 100,
            queued: 1
        })
    );
    assert_eq!(m.status_of(10, 16000).unwrap().window, 250);
    assert_eq!(m.status_of(10, 16000).unwrap().wait_ms, 15000);
    assert_eq!(m.status_of(10, 1_000_000).unwrap().window, 500);
    assert_eq!(m.status_of(10, 0).unwrap().wait_ms, 0, "a join in the future waits 0");
    assert_eq!(m.status_of(11, 1000).unwrap().window, 250);
    assert_eq!(m.status_of(11, 1_000_000).unwrap().window, 650);
    assert_eq!(m.status_of(12, 1000), None);
}

#[test]
fn statuses_cover_every_queued_player() {
    let mut m = mm();
    m.join(at(1, 1500, 0)).unwrap();
    m.join(JoinRequest { rated: false, ..at(2, 1500, 1000) }).unwrap();
    let mut all = m.statuses(6000);
    all.sort_by_key(|(u, _)| *u);
    assert_eq!(all.len(), 2);
    assert_eq!((all[0].0, all[0].1.wait_ms, all[0].1.window, all[0].1.rated), (1, 6000, 150, true));
    assert_eq!((all[1].0, all[1].1.wait_ms, all[1].1.window, all[1].1.rated), (2, 5000, 150, false));
    assert_eq!(QUEUE_REFRESH_MS, 3000);
}

#[test]
fn a_pair_needs_each_player_inside_the_others_window() {
    let mut m = mm();
    m.join(at(1, 1500, 0)).unwrap(); // waits: window 500 after 40 s
    m.join(at(2, 1800, 60000)).unwrap(); // new: window 100
    assert!(m.tick(60000).is_empty());
    assert_eq!(m.status_of(1, 60000).unwrap().window, 500);
    assert!(m.tick(79999).is_empty()); // player 2: window 250 < 300
    let pairs = m.tick(80000); // player 2: window 300
    assert_eq!(all_ids(&pairs), [[1, 2]]);
    assert!(m.is_empty());
    assert_eq!(&*pairs[0].category, "3+2");
    assert!(pairs[0].rated);
}

#[test]
fn the_provisional_bonus_widens_both_directions_of_the_rule() {
    let mut m = mm();
    m.join(JoinRequest { provisional: true, ..player(1, 1500) }).unwrap(); // window 250
    m.join(player(2, 1700)).unwrap(); // window 100
    assert!(m.tick(0).is_empty());
    m.join(JoinRequest { provisional: true, ..player(3, 1700) }).unwrap(); // 200 inside both
    assert_eq!(all_ids(&m.tick(0)), [[1, 3]]);
}

#[test]
fn closest_rating_inside_the_window_ties_to_the_longer_wait() {
    let mut m = mm();
    m.join(at(1, 1500, 0)).unwrap();
    m.join(at(2, 1580, 10)).unwrap();
    m.join(at(3, 1520, 20)).unwrap();
    m.join(at(4, 1450, 30)).unwrap();
    assert_eq!(all_ids(&m.tick(100)), [[1, 3]]); // 2 and 4 are 130 apart
    assert!(m.has(2) && m.has(4));

    let mut n = mm();
    n.join(at(1, 1500, 0)).unwrap(); // seeker (oldest)
    n.join(at(2, 1480, 10)).unwrap(); // 20 below, older
    n.join(at(3, 1520, 20)).unwrap(); // 20 above, newer
    assert_eq!(all_ids(&n.tick(100)), [[1, 2]]);

    let mut o = mm();
    o.join(at(1, 1500, 0)).unwrap();
    o.join(at(3, 1520, 10)).unwrap(); // above, older this time
    o.join(at(2, 1480, 20)).unwrap();
    assert_eq!(all_ids(&o.tick(100)), [[1, 3]]);
}

#[test]
fn fifo_fairness_the_longest_waiting_player_chooses_first() {
    let mut m = mm();
    m.join(at(1, 1500, 0)).unwrap();
    m.join(at(2, 1620, 1000)).unwrap(); // 120 from player 1: never with 1 early
    m.join(at(3, 1570, 2000)).unwrap(); // 70 from 1, 50 from 2
    assert_eq!(all_ids(&m.tick(2000)), [[1, 3]]);
    assert!(m.has(2));

    // Many players at one rating: pairs come out in joining order.
    let mut n = mm();
    for i in 0..10 {
        n.join(at(100 + i, 1500, i64::from(i))).unwrap();
    }
    assert_eq!(all_ids(&n.tick(100)), [[100, 101], [102, 103], [104, 105], [106, 107], [108, 109]]);
}

#[test]
fn an_out_of_order_joined_at_is_queued_by_its_time() {
    let mut m = mm();
    m.join(at(1, 1500, 5000)).unwrap();
    m.join(at(2, 1540, 1000)).unwrap(); // waited longer: seeks first
    m.join(at(3, 1530, 6000)).unwrap();
    assert_eq!(all_ids(&m.tick(6000)), [[2, 3]]);
}

#[test]
fn a_join_older_than_max_reorder_players_is_raised_to_theirs() {
    let mut m = mm();
    for i in 0..=MAX_REORDER as u32 {
        m.join(at(10 + i, 1000 + 200 * i64::from(i), 1000)).unwrap();
    }
    m.join(at(1, 9000, 0)).unwrap();
    assert_eq!(m.status_of(1, 2000).unwrap().wait_ms, 1000, "joined_at raised to 1000");
}

#[test]
fn queues_are_separate_per_category_and_rated_flag() {
    let mut m = mm();
    m.join(player(1, 1500)).unwrap();
    m.join(JoinRequest { rated: false, ..player(2, 1500) }).unwrap();
    m.join(JoinRequest { category: "5+0".into(), ..player(3, 1500) }).unwrap();
    assert!(m.tick(0).is_empty());
    assert_eq!(m.status_of(1, 0).unwrap().queued, 1);
    m.join(JoinRequest { rated: false, ..player(4, 1510) }).unwrap();
    let pairs = m.tick(0);
    assert_eq!(all_ids(&pairs), [[2, 4]]);
    assert!(!pairs[0].rated);
    assert_eq!(m.len(), 2);
    let sizes: Vec<_> =
        m.queue_sizes().into_iter().map(|q| (q.category.to_string(), q.rated, q.size)).collect();
    assert_eq!(
        sizes,
        [("3+2".to_string(), true, 1), ("3+2".to_string(), false, 0), ("5+0".to_string(), true, 1)]
    );
}

#[test]
fn leave_removes_the_player_from_its_rating_bucket() {
    let mut m = mm();
    m.join(player(1, 1500)).unwrap();
    m.join(player(2, 1500)).unwrap();
    m.join(player(3, 1500)).unwrap();
    m.leave(2);
    m.leave(1);
    assert!(m.tick(0).is_empty());
    m.join(player(2, 1500)).unwrap();
    assert_eq!(all_ids(&m.tick(0)), [[2, 3]]);
    assert!(m.is_empty());
    // Rejoin after a pairing.
    assert_eq!(m.join(player(2, 1500)), Ok(()));
}

#[test]
fn repeat_limit_for_rated_pairings_with_expiry() {
    let mut m = mm();
    let s = settings();
    let h = s.repeat_window_ms;
    for i in 0..i64::from(s.repeat_limit) {
        m.join(at(1, 1500, i * 1000)).unwrap();
        m.join(at(2, 1500, i * 1000)).unwrap();
        let pairs = m.tick(i * 1000);
        assert_eq!(pairs.len(), 1);
        // A pairing counts once its game exists: the lobby records it then.
        assert_eq!(i64::from(m.repeat_count(1, 2, i * 1000)), i);
        assert!(!m.repeat_limited(1, 2, i * 1000));
        m.record_pairing(pairs[0].white.user_id, pairs[0].black.user_id, i * 1000);
    }
    assert_eq!(m.repeat_count(1, 2, 3000), 3);
    assert!(m.repeat_limited(2, 1, 3000), "the lobby refuses their rated challenges and rematches");
    m.join(at(1, 1500, 3000)).unwrap();
    m.join(at(2, 1500, 3000)).unwrap();
    assert!(m.tick(3000).is_empty());
    // A third player can take either of them.
    m.join(at(3, 1600, 3000)).unwrap();
    assert_eq!(all_ids(&m.tick(3000)), [[1, 3]]);
    m.leave(2);
    // Casual games are not limited.
    m.join(JoinRequest { rated: false, ..at(1, 1500, 3000) }).unwrap();
    m.join(JoinRequest { rated: false, ..at(2, 1500, 3000) }).unwrap();
    assert_eq!(m.tick(3000).len(), 1);
    // The first pairing leaves the window after an hour.
    m.join(at(1, 1500, h)).unwrap();
    m.join(at(2, 1500, h)).unwrap();
    assert!(m.tick(h - 1).is_empty());
    assert_eq!(m.tick(h).len(), 1);
    assert!(!m.repeat_limited(1, 2, h));

    let mut n = mm();
    for _ in 0..3 {
        n.record_pairing(7, 8, 0);
    }
    n.join(player(7, 1500)).unwrap();
    n.join(player(8, 1500)).unwrap();
    assert!(n.tick(0).is_empty());
}

#[test]
fn a_held_pair_is_not_made_before_its_time_in_any_queue_other_pairs_are() {
    let mut m = mm();
    m.hold_pair(2, 1, 5000);
    for rated in [true, false] {
        m.join(JoinRequest { rated, ..player(1, 1500) }).unwrap();
        m.join(JoinRequest { rated, ..player(2, 1500) }).unwrap();
        assert!(m.tick(4999).is_empty(), "rated {rated}");
        m.join(JoinRequest { rated, ..player(3, 1500) }).unwrap();
        assert_eq!(all_ids(&m.tick(4999)), [[1, 3]]);
        m.leave(2);
    }
    m.join(player(1, 1500)).unwrap();
    m.join(player(2, 1500)).unwrap();
    assert_eq!(all_ids(&m.tick(5000)), [[1, 2]]);
    assert_eq!(m.held_pairs(), 0, "an ended hold is dropped");
    assert_eq!(PAIR_RETRY_DELAY_MS, 5000);
}

#[test]
fn recent_opponents_exclusions_apply_both_ways() {
    let mut m = mm();
    m.join(JoinRequest { recent_opponents: vec![2], ..player(1, 1500) }).unwrap();
    m.join(player(2, 1500)).unwrap();
    assert!(m.tick(0).is_empty());
    m.join(JoinRequest { recent_opponents: vec![1], ..player(3, 1500) }).unwrap();
    assert_eq!(all_ids(&m.tick(0)), [[2, 3]]);
}

#[test]
fn colour_balance_decides_colours_ties_are_drawn() {
    let mut m = mm_with(fixed(0.9));
    m.join(JoinRequest { color_balance: Some(2), ..player(1, 1500) }).unwrap();
    m.join(JoinRequest { color_balance: Some(0), ..player(2, 1500) }).unwrap();
    let p = m.tick(0).remove(0);
    assert_eq!((p.white.user_id, p.black.user_id), (2, 1));
    m.join(JoinRequest { color_balance: Some(-1), ..player(3, 1500) }).unwrap();
    m.join(JoinRequest { color_balance: Some(-3), ..player(4, 1500) }).unwrap();
    assert_eq!(m.tick(0)[0].white.user_id, 4);

    // Ties: random() < 0.5 gives White to the seeker (the longer wait).
    let mut low = mm_with(fixed(0.1));
    low.join(at(1, 1500, 0)).unwrap();
    low.join(at(2, 1500, 1)).unwrap();
    assert_eq!(low.tick(10)[0].white.user_id, 1);
    let mut high = mm_with(fixed(0.7));
    high.join(at(1, 1500, 0)).unwrap();
    high.join(at(2, 1500, 1)).unwrap();
    assert_eq!(high.tick(10)[0].white.user_id, 2);

    // The matchmaker remembers the balance when the caller does not pass one.
    assert_eq!(high.color_balance_of(2), 1);
    assert_eq!(high.color_balance_of(1), -1);
    high.join(at(1, 1500, 20)).unwrap();
    high.join(at(2, 1500, 20)).unwrap();
    assert_eq!(high.tick(20)[0].white.user_id, 1);
    assert_eq!(high.color_balance_of(1), 0);
    high.record_colors(5, 6);
    assert_eq!((high.color_balance_of(5), high.color_balance_of(6)), (1, -1));
}

#[test]
fn colour_balances_forget_the_least_recently_updated_beyond_the_cap() {
    let mut b = Balances::new(10);
    for u in 1..=10 {
        b.set(u, 1);
    }
    b.set(1, 2); // user 1 becomes the most recent
    b.set(11, -1); // 11 > cap: down to 9, dropping users 2 and 3
    assert_eq!(b.values.len(), 9);
    assert_eq!((b.get(1), b.get(2), b.get(3), b.get(4), b.get(11)), (2, 0, 0, 1, -1));
    b.set(4, 0);
    assert_eq!(b.values.len(), 8, "zero balances are not stored");
}

#[test]
fn pair_entries_carry_what_the_lobby_needs() {
    let mut m = mm();
    m.join(JoinRequest { username: "alice".into(), provisional: true, conn_id: 77, ..at(1, 1500, 0) })
        .unwrap();
    m.join(JoinRequest { username: "bob".into(), conn_id: 88, ..at(2, 1510, 500) }).unwrap();
    let p = m.tick(1000).remove(0);
    let a = if p.white.user_id == 1 { &p.white } else { &p.black };
    assert_eq!(
        *a,
        PairedPlayer {
            user_id: 1,
            username: "alice".into(),
            category: Arc::from("3+2"),
            rated: true,
            rating: 1500,
            provisional: true,
            conn_id: 77,
            color_balance: 0,
            joined_at: 0,
            wait_ms: 1000,
        }
    );

    // A pairing whose game could not be created goes back with its waiting time and balance.
    m.record_colors(p.black.user_id, p.white.user_id);
    m.hold_pair(1, 2, 1000 + PAIR_RETRY_DELAY_MS);
    m.join(p.white.rejoin_request()).unwrap();
    m.join(p.black.rejoin_request()).unwrap();
    assert_eq!(m.status_of(1, 1000).unwrap().wait_ms, 1000);
    assert_eq!((m.color_balance_of(1), m.color_balance_of(2)), (0, 0));
    assert!(m.tick(1000).is_empty());
    assert_eq!(all_ids(&m.tick(1000 + PAIR_RETRY_DELAY_MS)), [[1, 2]]);
}

#[test]
fn ratings_far_apart_and_extreme_values() {
    let mut m = mm();
    m.join(player(1, 0)).unwrap();
    m.join(player(2, 60)).unwrap();
    m.join(player(3, 65535)).unwrap();
    m.join(player(4, 70000)).unwrap(); // clamped to the u16 range
    m.join(player(5, 9000)).unwrap(); // grows the bucket index
    m.join(JoinRequest { category: "5+0".into(), ..player(6, -40) }).unwrap(); // clamped to 0
    let mut pairs = all_ids(&m.tick(0));
    pairs.sort();
    assert_eq!(pairs, [[1, 2], [3, 4]]);
    assert!(m.has(5));
}

// ---- reference implementation (brute force) -------------------------------------------------

struct RefEntry {
    user_id: UserId,
    rated: bool,
    rating: i64,
    provisional: bool,
    joined_at: i64,
    seq: u64,
    recent: HashSet<UserId>,
}

#[derive(Default)]
struct Reference {
    queues: IndexMap<String, Vec<RefEntry>>,
    counts: HashMap<(UserId, UserId), u32>,
    log: Vec<((UserId, UserId), i64)>,
    seq: u64,
    pairs: u32,
    repeat_skips: u32,
    recent_skips: u32,
    mutual_skips: u32,
}

impl Reference {
    fn key(a: UserId, b: UserId) -> (UserId, UserId) {
        if a < b { (a, b) } else { (b, a) }
    }

    fn join(&mut self, e: &JoinRequest) {
        self.seq += 1;
        let key = format!("{}{}", e.category, if e.rated { "|r" } else { "|c" });
        self.queues.entry(key).or_default().push(RefEntry {
            user_id: e.user_id,
            rated: e.rated,
            rating: e.rating,
            provisional: e.provisional,
            joined_at: e.joined_at,
            seq: self.seq,
            recent: e.recent_opponents.iter().copied().collect(),
        });
    }

    fn leave(&mut self, user_id: UserId) {
        for q in self.queues.values_mut() {
            q.retain(|x| x.user_id != user_id);
        }
    }

    fn tick(&mut self, now: i64, s: &MatchSettings) -> BTreeMap<String, Vec<[UserId; 2]>> {
        let counts = &mut self.counts;
        self.log.retain(|&(k, t)| {
            if t <= now - s.repeat_window_ms {
                *counts.get_mut(&k).unwrap() -= 1;
                return false;
            }
            true
        });
        let mut out = BTreeMap::new();
        for (key, q) in self.queues.iter_mut() {
            q.sort_by_key(|x| (x.joined_at, x.seq));
            let mut paired = vec![false; q.len()];
            let mut res = Vec::new();
            for ai in 0..q.len() {
                if paired[ai] {
                    continue;
                }
                let a = &q[ai];
                let wa = search_window(now - a.joined_at, a.provisional, s);
                let mut best: Option<usize> = None;
                let mut best_d = i64::MAX;
                for (bi, b) in q.iter().enumerate() {
                    if bi == ai || paired[bi] {
                        continue;
                    }
                    let d = (a.rating - b.rating).abs();
                    if d > wa {
                        continue;
                    }
                    if d > search_window(now - b.joined_at, b.provisional, s) {
                        self.mutual_skips += 1;
                        continue;
                    }
                    if a.recent.contains(&b.user_id) || b.recent.contains(&a.user_id) {
                        self.recent_skips += 1;
                        continue;
                    }
                    let k = Self::key(a.user_id, b.user_id);
                    if a.rated && self.counts.get(&k).copied().unwrap_or(0) >= s.repeat_limit {
                        self.repeat_skips += 1;
                        continue;
                    }
                    let better = match best {
                        None => true,
                        Some(x) => {
                            d < best_d || (d == best_d && (b.joined_at, b.seq) < (q[x].joined_at, q[x].seq))
                        }
                    };
                    if better {
                        best = Some(bi);
                        best_d = d;
                    }
                }
                if let Some(bi) = best {
                    paired[ai] = true;
                    paired[bi] = true;
                    self.pairs += 1;
                    let k = Self::key(a.user_id, q[bi].user_id);
                    res.push([k.0, k.1]);
                    if a.rated {
                        *self.counts.entry(k).or_insert(0) += 1;
                        self.log.push((k, now));
                    }
                }
            }
            let mut i = 0;
            q.retain(|_| {
                i += 1;
                !paired[i - 1]
            });
            if !res.is_empty() {
                out.insert(key.clone(), res);
            }
        }
        out
    }
}

#[test]
fn same_pairs_as_a_brute_force_reference_over_a_random_simulation() {
    let s = settings();
    let (mut pairs, mut repeat_skips, mut recent_skips, mut mutual_skips) = (0, 0, 0, 0);
    for seed in 1..=6u32 {
        let mut rnd = lcg(seed);
        let mut m = mm();
        let mut reference = Reference::default();
        let mut queued: IndexSet<UserId> = IndexSet::new();
        // Small pools with stable ratings meet the same opponents again (repeat limit,
        // exclusions); larger pools with random ratings stress the window rules.
        let pool = if seed % 2 == 1 { 30.0 } else { 150.0 };
        let base_rating = |u: UserId| 1300 + i64::from((u * 37) % 400);
        let mut uid = 1;
        for step in 0..400 {
            let now = step * 250;
            let joins = (rnd() * 6.0).floor() as u32;
            for _ in 0..joins {
                let user_id = 1 + (rnd() * pool).floor() as UserId;
                if queued.contains(&user_id) {
                    continue;
                }
                let rating = if pool < 100.0 {
                    base_rating(user_id) + (rnd() * 20.0).floor() as i64
                } else {
                    1200 + (rnd() * 900.0).floor() as i64
                };
                let category = if rnd() < 0.7 { "3+2" } else { "5+0" };
                let rated = rnd() < 0.8;
                let provisional = rnd() < 0.3;
                let joined_at = now - (rnd() * 3000.0).floor() as i64;
                let recent_opponents = if rnd() < 0.3 {
                    vec![user_id + 1 + (rnd() * 3.0).floor() as UserId]
                } else {
                    Vec::new()
                };
                let e = JoinRequest {
                    user_id,
                    username: format!("u{user_id}"),
                    category: category.to_string(),
                    rated,
                    rating,
                    provisional,
                    conn_id: uid,
                    color_balance: None,
                    joined_at,
                    recent_opponents,
                };
                uid += 1;
                reference.join(&e);
                assert_eq!(m.join(e), Ok(()));
                queued.insert(user_id);
            }
            if rnd() < 0.2 && !queued.is_empty() {
                let victim = queued[(rnd() * queued.len() as f64).floor() as usize];
                assert!(m.leave(victim));
                reference.leave(victim);
                queued.shift_remove(&victim);
            }
            let mut got: BTreeMap<String, Vec<[UserId; 2]>> = BTreeMap::new();
            for p in m.tick(now) {
                if p.rated {
                    m.record_pairing(p.white.user_id, p.black.user_id, now); // the lobby, once the game exists
                }
                let key = format!("{}{}", p.category, if p.rated { "|r" } else { "|c" });
                got.entry(key).or_default().push(ids(&p));
                queued.shift_remove(&p.white.user_id);
                queued.shift_remove(&p.black.user_id);
            }
            let want = reference.tick(now, &s);
            assert_eq!(got, want, "seed {seed} step {step}");
        }
        pairs += reference.pairs;
        repeat_skips += reference.repeat_skips;
        recent_skips += reference.recent_skips;
        mutual_skips += reference.mutual_skips;
    }
    // The simulation exercised every rule.
    let totals = format!("pairs {pairs} repeat {repeat_skips} recent {recent_skips} mutual {mutual_skips}");
    assert!(pairs > 1000, "{totals}");
    assert!(repeat_skips > 0 && recent_skips > 0 && mutual_skips > 0, "{totals}");
}

#[test]
fn one_hundred_thousand_waiting_players_one_tick_stays_fast_and_every_pair_is_valid() {
    let s = settings();
    let mut rnd = lcg(42);
    let mut m = mm_with(Box::new(lcg(42)));
    let config = Config::for_tests();
    let cats: Vec<&str> = config.categories.iter().map(|c| c.id.as_str()).collect();
    let n: u32 = 100_000;
    for i in 1..=n {
        // Roughly normal ratings around 1500.
        let r = (1500.0 + (rnd() + rnd() + rnd() + rnd() - 2.0) * 500.0 + 0.5).floor() as i64;
        m.join(JoinRequest {
            user_id: i,
            username: format!("u{i}"),
            category: cats[i as usize % cats.len()].to_string(),
            rated: (i >> 4) % 2 == 0,
            rating: r,
            provisional: rnd() < 0.2,
            conn_id: i,
            color_balance: None,
            joined_at: i64::from(i / 100),
            recent_opponents: Vec::new(),
        })
        .unwrap();
    }
    assert_eq!(m.len(), n as usize);
    let t0 = Instant::now();
    let pairs = m.tick(2000);
    let ms = t0.elapsed().as_secs_f64() * 1000.0;
    let mut seen = HashSet::new();
    for p in &pairs {
        let d = (p.white.rating - p.black.rating).abs();
        assert!(d <= search_window(p.white.wait_ms, p.white.provisional, &s));
        assert!(d <= search_window(p.black.wait_ms, p.black.provisional, &s));
        assert_eq!(p.white.category, p.black.category);
        assert!(seen.insert(p.white.user_id) && seen.insert(p.black.user_id));
    }
    assert_eq!(m.len() + seen.len(), n as usize);
    assert!(pairs.len() > n as usize / 2 - 200, "{} pairs", pairs.len());
    // A generous bound (unoptimized test builds included): catches accidental O(n^2) behaviour.
    assert!(ms < 1500.0, "tick took {ms:.1} ms");
    // A second tick over the few leftovers is immediate.
    let t1 = Instant::now();
    m.tick(2250);
    assert!(t1.elapsed().as_millis() < 100);
}
