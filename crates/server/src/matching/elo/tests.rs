//! Port of test/unit/match.elo.test.js, plus the generator of test/fixtures/elo-vectors.json
//! (formerly tools/gen-elo-vectors.js): the `elo_vectors` test checks that the committed file is
//! what the generator writes, byte for byte, and that these rules reproduce every vector.
//! `SCACELITH_UPDATE_VECTORS=1 cargo test -p scacelith-server elo_vectors` rewrites the file.

use std::path::PathBuf;

use serde_json::{Value, json};

use super::*;
use crate::config::Config;

const CFG: EloSettings = EloSettings::GAME;

// Standard normal CDF: erf by its Taylor series (|x| < 3) or its continued fraction.
fn phi(z: f64) -> f64 {
    let x = z.abs() / std::f64::consts::SQRT_2;
    let erf = if x < 3.0 {
        let (mut t, mut sum) = (x, x);
        for n in 1..200 {
            t *= -x * x / n as f64;
            let add = t / (2 * n + 1) as f64;
            sum += add;
            if add.abs() < 1e-18 {
                break;
            }
        }
        2.0 / std::f64::consts::PI.sqrt() * sum
    } else {
        let mut f = 0.0;
        for k in (1..=60).rev() {
            f = k as f64 / 2.0 / (x + f);
        }
        1.0 - (-x * x).exp() / std::f64::consts::PI.sqrt() / (x + f)
    };
    if z < 0.0 { 0.5 * (1.0 - erf) } else { 0.5 * (1.0 + erf) }
}

fn rated(rating: i64, games: i64) -> Record {
    rated_peak(rating, games, rating)
}

fn rated_peak(rating: i64, games: i64, peak: i64) -> Record {
    let peak = rating.max(peak);
    Record {
        rating,
        games,
        wins: 0,
        draws: 0,
        losses: 0,
        peak,
        reached_senior: peak >= SENIOR_RATING,
        rated: true,
        counted_games: games,
        unrated_games: 0,
        unrated_opponents: 0,
        unrated_half_points: 0,
    }
}

struct Change {
    before: i64,
    after: i64,
    k: i64,
    opponent_after: i64,
}

// A record after the given scores against `opponent`, and the changes seen.
fn play(scores: &[f64], opponent: &Record, start: Record) -> (Record, Vec<Change>) {
    let mut r = start;
    let mut changes = Vec::new();
    for &s in scores {
        let res = apply_game(&r, opponent, s, &CFG).unwrap();
        changes.push(Change {
            before: res.white.before,
            after: res.white.after,
            k: res.white.k,
            opponent_after: res.black.after,
        });
        r = res.white.record;
    }
    (r, changes)
}

fn fresh() -> Record {
    Record::new(&CFG)
}

#[test]
fn fide_pd_table_against_the_normal_distribution() {
    let mut mismatches = Vec::new();
    for d in 0..=800 {
        let cdf = (100.0 * phi(d as f64 / (2000.0 / 7.0))).round() as i64;
        let table = scoring_probability(d);
        if cdf != table {
            mismatches.push(d);
            assert_eq!((cdf - table).abs(), 1, "D {d}: at most one hundredth apart");
        }
    }
    assert_eq!(mismatches, [54, 343, 344, 358, 392, 620]);
    let published: [(i64, i64, i64); 43] = [
        (0, 3, 50),
        (4, 10, 51),
        (11, 17, 52),
        (18, 25, 53),
        (26, 32, 54),
        (33, 39, 55),
        (40, 46, 56),
        (47, 53, 57),
        (54, 61, 58),
        (62, 68, 59),
        (69, 76, 60),
        (77, 83, 61),
        (84, 91, 62),
        (92, 98, 63),
        (99, 106, 64),
        (107, 113, 65),
        (114, 121, 66),
        (122, 129, 67),
        (130, 137, 68),
        (138, 145, 69),
        (146, 153, 70),
        (154, 162, 71),
        (163, 170, 72),
        (171, 179, 73),
        (180, 188, 74),
        (189, 197, 75),
        (198, 206, 76),
        (207, 215, 77),
        (216, 225, 78),
        (226, 235, 79),
        (236, 245, 80),
        (246, 256, 81),
        (257, 267, 82),
        (268, 278, 83),
        (279, 290, 84),
        (291, 302, 85),
        (303, 315, 86),
        (316, 328, 87),
        (329, 344, 88),
        (345, 357, 89),
        (358, 374, 90),
        (375, 391, 91),
        (392, 400, 92),
    ];
    for (lo, hi, pd) in published {
        for d in lo..=hi {
            assert_eq!(scoring_probability(d), pd, "D {d}");
        }
    }
    let mut lo = 0;
    for (i, (hi, pd)) in FIDE_PD_TABLE.iter().enumerate() {
        assert_eq!(*pd, 50 + i as i64);
        assert!(*hi >= lo);
        lo = hi + 1;
    }
    assert_eq!(lo, 736);
    assert_eq!(scoring_probability(735), 99);
    assert_eq!(scoring_probability(736), 100);
    assert_eq!(scoring_probability(-60), 58);
}

#[test]
fn fide_dp_table_is_the_middle_of_each_pd_row() {
    assert_eq!(FIDE_DP_TABLE.len(), 51);
    assert_eq!(rating_difference(100), 800);
    assert_eq!(rating_difference(50), 0);
    let mut lo = 0;
    for (hi, pd) in FIDE_PD_TABLE {
        if pd > 50 {
            assert_eq!(rating_difference(pd), (lo + hi) / 2, "p {pd}");
        }
        lo = hi + 1;
    }
    for p in 0..=100 {
        assert_eq!(rating_difference(p), -rating_difference(100 - p), "p {p} mirrors 1 - p");
    }
    assert_eq!(
        [
            rating_difference(99),
            rating_difference(86),
            rating_difference(64),
            rating_difference(36),
            rating_difference(0)
        ],
        [677, 309, 102, -102, -800]
    );
    assert_eq!(rating_difference(150), 800);
    assert_eq!(rating_difference(-3), -800);
}

#[test]
fn expected_score_from_the_table_capped_and_symmetric() {
    assert_eq!(expected_score(1500, 1500), 0.5);
    assert_eq!(expected_score(1503, 1500), 0.5);
    assert_eq!(expected_score(1504, 1500), 0.51);
    assert_eq!(expected_score(1500, 1700), 0.24);
    assert_eq!(expected_score(1700, 1500), 0.76);
    assert_eq!(expected_score(1500, 1900), 0.08);
    assert_eq!(expected_score(1500, 3500), expected_score(1500, 1900));
    assert_eq!(expected_score(3000, 100), 0.92);
    for (a, b) in [(1500, 1700), (1234, 1987), (2000, 1000), (1500, 1554)] {
        assert!((expected_score(a, b) + expected_score(b, a) - 1.0).abs() < 1e-12);
    }
}

#[test]
fn rated_games_k_times_score_minus_pd_rounded_half_away_from_zero() {
    let r = rated(1500, 40);
    assert_eq!(rating_delta(&r, 1500, 1.0, &CFG), 10);
    assert_eq!(rating_delta(&r, 1600, 0.0, &CFG), -7);
    assert_eq!(rating_delta(&r, 1560, 0.5, &CFG), 2);
    assert_eq!(rating_delta(&r, 1100, 1.0, &CFG), 2);
    let senior = rated(2435, 200);
    assert_eq!(rating_delta(&senior, 2400, 1.0, &CFG), 5);
    assert_eq!(rating_delta(&senior, 2400, 0.0, &CFG), -6);
    let g = apply_game(&rated(1620, 40), &rated(1480, 40), 0.0, &CFG).unwrap();
    assert_eq!(g.white.delta, -g.black.delta);
    assert_eq!(g.white.delta, -14);
    assert_eq!(g.white.k, 20);
    assert_eq!(g.white.expected, 0.69);
}

#[test]
fn first_rating_after_five_games_hand_computed() {
    assert_eq!(initial_rating(5, 7500, 5), 1586);
    assert_eq!(initial_rating(5, 7500, 10), 1895);
    assert_eq!(initial_rating(5, 7500, 0), 1277);
    assert_eq!(initial_rating(5, 8000, 3), 1555);
    assert_eq!(initial_rating(5, 8000, 5), 1657);
    assert_eq!(initial_rating(5, 17500, 10), MAX_INITIAL_RATING);
    assert_eq!(initial_rating(5, 500, 0), 277);
}

#[test]
fn unrated_phase_against_a_rated_opponent() {
    let novice = rated(800, 1000);
    let (record, changes) = play(&[1.0, 1.0, 0.5, 0.0, 1.0], &novice, fresh());
    let seen: Vec<_> = changes.iter().map(|c| [c.before, c.after, c.k, c.opponent_after]).collect();
    assert_eq!(
        seen,
        [
            [1500, 1500, 0, 800],
            [1500, 1500, 0, 800],
            [1500, 1500, 0, 800],
            [1500, 1500, 0, 800],
            [1500, 1188, 0, 800]
        ]
    );
    assert_eq!(
        record,
        Record {
            rating: 1188,
            games: 5,
            wins: 3,
            draws: 1,
            losses: 1,
            peak: 1188,
            reached_senior: false,
            rated: true,
            counted_games: 5,
            unrated_games: 0,
            unrated_opponents: 0,
            unrated_half_points: 0,
        }
    );
    let next = apply_game(&record, &novice, 1.0, &CFG).unwrap();
    assert_eq!(next.white.k, 40);
    assert_eq!(next.white.after, 1192);
    assert_eq!(next.white.record.counted_games, 6);
    assert!(next.white.provisional);
    let (four, _) = play(&[1.0, 1.0, 0.5, 0.0], &novice, fresh());
    assert!(four.is_unrated());
    assert_eq!(
        [four.unrated_games, four.unrated_opponents, four.unrated_half_points, four.games, four.rating],
        [4, 3200, 5, 4, 1500]
    );
    // The zero-score rule: five losses to a 3500 engine leave the player unrated.
    let (lost, lost_changes) = play(&[0.0; 5], &rated(3500, 1000), fresh());
    assert_eq!(
        (
            lost.rated,
            lost.rating,
            lost.games,
            lost.losses,
            lost.unrated_games,
            lost.unrated_opponents,
            lost.counted_games
        ),
        (false, 1500, 5, 5, 0, 0, 0)
    );
    assert!(lost_changes.iter().all(|c| c.before == 1500 && c.after == 1500 && c.k == 0));
    // Once the player has scored, the losses count; the peak becomes the first rating.
    let (scored, _) = play(&[0.0, 0.5, 0.0, 0.0, 0.0, 0.0], &rated(1500, 100), fresh());
    assert_eq!(
        (scored.rating, scored.peak, scored.rated, scored.games, scored.losses),
        (1356, 1356, true, 6, 5)
    );
}

#[test]
fn rated_player_against_an_unrated_one_keeps_the_rating() {
    let r = apply_game(&rated(1700, 45), &fresh(), 0.0, &CFG).unwrap();
    assert_eq!([r.white.before, r.white.after, r.white.k], [1700, 1700, 0]);
    assert_eq!([r.white.record.games, r.white.record.losses, r.white.record.counted_games], [46, 1, 45]);
    assert_eq!(
        [r.black.record.unrated_games, r.black.record.unrated_opponents, r.black.record.unrated_half_points],
        [1, 1700, 2]
    );
    assert_eq!(r.black.after, 1500);
    let five =
        Record { games: 4, unrated_games: 4, unrated_opponents: 6000, unrated_half_points: 8, ..fresh() };
    let g = apply_game(&rated(1800, 80), &five, 0.0, &CFG).unwrap();
    assert_eq!(g.white.after, 1800);
    assert!(g.black.record.rated);
    assert_eq!(g.black.after, initial_rating(5, 7800, 10));
}

#[test]
fn game_between_two_unrated_players_counts_for_both() {
    let first = apply_game(&fresh(), &fresh(), 0.0, &CFG).unwrap();
    assert_eq!(
        [
            first.white.record.unrated_games,
            first.white.record.losses,
            first.black.record.unrated_games,
            first.black.record.unrated_opponents,
            first.black.record.wins,
            first.black.record.games
        ],
        [0, 1, 0, 0, 1, 1]
    );
    let (mut w, mut b) = (fresh(), fresh());
    for s in [0.5, 1.0, 1.0, 0.0] {
        let r = apply_game(&w, &b, s, &CFG).unwrap();
        assert_eq!([r.white.before, r.white.after, r.black.before, r.black.after], [1500; 4]);
        w = r.white.record;
        b = r.black.record;
    }
    assert_eq!([w.unrated_games, w.unrated_opponents, w.unrated_half_points], [4, 6000, 5]);
    assert_eq!([b.unrated_games, b.unrated_opponents, b.unrated_half_points], [4, 6000, 3]);
    let r = apply_game(&w, &b, 1.0, &CFG).unwrap();
    assert_eq!(r.white.after, 1586 + 102);
    assert_eq!(r.black.after, 1586 - 102);
    assert_eq!((r.white.record.rated, r.black.record.rated, r.white.k, r.black.k), (true, true, 0, 0));
    let c1800 = EloSettings { initial_rating: 1800, ..CFG };
    let g = apply_game(&Record::new(&c1800), &Record::new(&c1800), 0.5, &c1800).unwrap();
    assert_eq!(g.white.record.unrated_opponents, 1800);
}

#[test]
fn zero_score_rule_closes_the_unrated_booster() {
    let mut booster = fresh();
    for _ in 0..3 {
        let mut f = fresh();
        for _ in 0..30 {
            let r = apply_game(&f, &booster, 1.0, &CFG).unwrap();
            assert_eq!([r.white.after, r.white.k, r.black.after], [1500, 0, 1500]);
            f = r.white.record;
            booster = r.black.record;
        }
        assert_eq!((f.rated, f.games, f.wins, f.counted_games, f.unrated_opponents), (false, 30, 30, 0, 0));
        assert!(f.is_provisional(&CFG));
    }
    assert_eq!((booster.rated, booster.games, booster.losses, booster.counted_games), (false, 90, 90, 0));
    let (mut a, mut b) = (fresh(), fresh());
    for s in [0.5, 1.0, 1.0, 1.0, 1.0] {
        let r = apply_game(&a, &b, s, &CFG).unwrap();
        a = r.white.record;
        b = r.black.record;
    }
    assert_eq!(
        (a.rated, a.rating, a.counted_games, b.rated, b.rating, b.counted_games),
        (true, 1816, 5, true, 1356, 5)
    );
    let next = apply_game(&a, &b, 1.0, &CFG).unwrap();
    assert_eq!([next.white.k, next.black.k, next.black.after], [40, 40, 1353]);
    let scorer = apply_game(&fresh(), &fresh(), 1.0, &CFG).unwrap().white.record;
    assert_eq!([scorer.wins, scorer.counted_games], [1, 0]);
    let g = apply_game(&fresh(), &scorer, 1.0, &CFG).unwrap();
    assert_eq!(
        [
            g.white.record.counted_games,
            g.white.record.unrated_opponents,
            g.black.record.counted_games,
            g.black.record.unrated_half_points
        ],
        [1, 1500, 1, 0]
    );
}

#[test]
fn k_factor_boundaries() {
    let r = rated(1500, 5);
    assert_eq!(r.k_factor(&CFG), 40);
    assert_eq!(Record { games: 29, counted_games: 29, ..r }.k_factor(&CFG), 40);
    assert_eq!(Record { games: 30, counted_games: 30, ..r }.k_factor(&CFG), 20);
    assert_eq!(Record { games: 80, counted_games: 29, ..r }.k_factor(&CFG), 40);
    assert!(Record { games: 80, counted_games: 29, ..r }.is_provisional(&CFG));
    assert!(!Record { games: 80, counted_games: 30, ..r }.is_provisional(&CFG));
    // A record without the field (stored before it existed) counts all its games.
    let legacy = PartialRecord {
        rating: Some(1500),
        games: Some(30),
        peak: Some(1500),
        rated: Some(true),
        ..Default::default()
    };
    assert_eq!(legacy.k_factor(&CFG), 20);
    assert!(!PartialRecord { games: Some(30), rated: Some(true), ..Default::default() }.is_provisional(&CFG));
    assert_eq!(Record { games: 5, rating: 2400, peak: 2400, ..r }.k_factor(&CFG), 10);
    assert_eq!(Record { games: 80, rating: 2300, peak: 2410, ..r }.k_factor(&CFG), 10);
    assert_eq!(Record { games: 80, rating: 2300, peak: 2300, reached_senior: true, ..r }.k_factor(&CFG), 10);
    let cfg10 = EloSettings { provisional_games: 10, ..CFG };
    assert_eq!(Record { games: 10, counted_games: 10, ..r }.k_factor(&cfg10), 20);
    assert!(PartialRecord { games: Some(9), rated: Some(true), ..Default::default() }.is_provisional(&cfg10));
    assert!(
        !PartialRecord { games: Some(10), rated: Some(true), ..Default::default() }.is_provisional(&cfg10)
    );
    let cfg0 = EloSettings { provisional_games: 0, ..CFG };
    assert!(PartialRecord { games: Some(3), rated: Some(false), ..Default::default() }.is_provisional(&cfg0));
    assert!(Record::new(&cfg0).is_provisional(&cfg0));
    let senior = apply_game(&rated(2390, 50), &rated(2500, 50), 1.0, &CFG).unwrap();
    assert_eq!(senior.white.after, 2403);
    assert!(senior.white.record.reached_senior);
    let next =
        apply_game(&Record { rating: 2350, ..senior.white.record }, &rated(1500, 50), 0.0, &CFG).unwrap();
    assert_eq!(next.white.k, 10);
}

#[test]
fn floor_peak_results_and_score_validation() {
    let r = apply_game(&Record { peak: 1500, ..rated(101, 50) }, &rated(101, 50), 0.0, &CFG).unwrap();
    assert_eq!(r.white.after, RATING_FLOOR);
    assert_eq!(r.white.record.peak, 1500);
    assert_eq!(r.black.after, 111);
    let (mut w, mut b) = (rated(1500, 40), rated(1500, 40));
    for s in [1.0, 0.5, 0.0] {
        let g = apply_game(&w, &b, s, &CFG).unwrap();
        w = g.white.record;
        b = g.black.record;
    }
    assert_eq!([w.wins, w.draws, w.losses, b.wins, b.draws, b.losses, w.games], [1, 1, 1, 1, 1, 1, 43]);
    assert_eq!(w.peak, 1510);
    assert_eq!(apply_game(&fresh(), &fresh(), 2.0, &CFG), Err(InvalidScore(2.0)));
    assert!(apply_game(&fresh(), &fresh(), f64::NAN, &CFG).is_err());
    assert_eq!(InvalidScore(2.0).to_string(), "elo: score must be 0..1, got 2");
}

#[test]
fn new_records_start_unrated_and_stored_records_with_games_stay_rated() {
    let c1800 = EloSettings { initial_rating: 1800, ..CFG };
    assert_eq!(
        Record::new(&c1800),
        Record {
            rating: 1800,
            games: 0,
            wins: 0,
            draws: 0,
            losses: 0,
            peak: 1800,
            reached_senior: false,
            rated: false,
            counted_games: 0,
            unrated_games: 0,
            unrated_opponents: 0,
            unrated_half_points: 0,
        }
    );
    assert_eq!(PartialRecord::default().normalize(&CFG), fresh());
    let old = PartialRecord { rating: Some(1650), games: Some(3), peak: Some(1700), ..Default::default() };
    let n = old.normalize(&CFG);
    assert!(n.rated);
    assert_eq!(n.counted_games, 3, "all its games were rated");
    assert!(!old.is_unrated());
    let g = apply_game(&n, &rated(1650, 50), 1.0, &CFG).unwrap();
    assert_eq!([g.white.after, g.white.k], [1670, 40]);
    assert!(
        !PartialRecord { rating: Some(1500), games: Some(0), ..Default::default() }.normalize(&CFG).rated
    );
    assert!(PartialRecord { games: Some(0), ..Default::default() }.is_unrated());
    assert!(!PartialRecord { games: Some(2), ..Default::default() }.is_unrated());
    let p = |games, rated, unrated: Option<i64>, counted: Option<i64>| PartialRecord {
        rating: Some(1500),
        games: Some(games),
        rated: Some(rated),
        unrated_games: unrated,
        counted_games: counted,
        ..Default::default()
    };
    assert_eq!(p(9, true, Some(3), None).normalize(&CFG).unrated_games, 0);
    assert_eq!(p(9, true, None, Some(12)).normalize(&CFG).counted_games, 9);
    assert_eq!(p(9, true, None, Some(7)).normalize(&CFG).counted_games, 7);
    assert_eq!(p(9, false, Some(2), Some(9)).normalize(&CFG).counted_games, 2);
    let r = apply_game(&fresh(), &fresh(), 1.0, &CFG).unwrap();
    assert_eq!((r.white.after, r.black.after, r.white.provisional), (1500, 1500, true));
}

#[test]
fn categories() {
    let cats = Categories::from_config(&Config::for_tests());
    assert_eq!(cats.category_of(180000, 2000), "3+2");
    assert_eq!(cats.category_of(60000, 0), "1+0");
    assert_eq!(cats.category_of(5400000, 30000), "90+30");
    assert_eq!(cats.category_of(180000, 1000), "custom");
    assert_eq!(cats.category_of(15000, 0), "custom");
    assert_eq!(cats.parse("10+5"), Some(&Category { id: "10+5".into(), base_ms: 600000, inc_ms: 5000 }));
    assert_eq!(cats.parse("custom"), None);
    assert_eq!(cats.parse("4+0"), None);
    assert!(cats.is_official("3+0"));
    let only = Categories::new(&[Category { id: "4+4".into(), base_ms: 240000, inc_ms: 4000 }]);
    assert_eq!(only.category_of(240000, 4000), "4+4");
    assert_eq!(only.category_of(180000, 2000), "custom");
}

// ---- the shared vectors ----------------------------------------------------------------------

const GENERATOR: &str = "dedicated-server/crates/server/src/matching/elo/tests.rs (cargo test elo_vectors; \
                         SCACELITH_UPDATE_VECTORS=1 rewrites it); checked by tests/elo_tests.cpp";

fn vectors_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../test/fixtures/elo-vectors.json")
}

/// The PRNG of the generator (mulberry32), as JavaScript computes it.
struct Mulberry32(u32);

impl Mulberry32 {
    fn next(&mut self) -> f64 {
        self.0 = self.0.wrapping_add(0x6d2b_79f5);
        let mut t = self.0;
        t = (t ^ (t >> 15)).wrapping_mul(t | 1);
        t ^= t.wrapping_add((t ^ (t >> 7)).wrapping_mul(t | 61));
        (t ^ (t >> 14)) as f64 / 4294967296.0
    }

    fn between(&mut self, lo: i64, hi: i64) -> i64 {
        lo + (self.next() * (hi - lo + 1) as f64).floor() as i64
    }
}

fn gen_rated(rating: i64, games: i64, peak: i64, counted: i64) -> Record {
    Record { counted_games: counted, reached_senior: false, ..rated_peak(rating, games, peak) }
}

// An unrated record whose counted games scored `half` half points, as wins, then a draw.
fn gen_unrated(games: i64, opponents: i64, half: i64) -> Record {
    let wins = half / 2;
    let draws = half % 2;
    Record {
        rating: DEFAULT_INITIAL_RATING,
        games,
        wins,
        draws,
        losses: games - wins - draws,
        peak: DEFAULT_INITIAL_RATING,
        reached_senior: false,
        rated: false,
        counted_games: games,
        unrated_games: games,
        unrated_opponents: opponents,
        unrated_half_points: half,
    }
}

// The same record after `lost` zero scores and `beaten` wins against zero scores.
fn after_losses(r: Record, lost: i64, beaten: i64) -> Record {
    Record { games: r.games + lost + beaten, losses: r.losses + lost, wins: r.wins + beaten, ..r }
}

fn edge_games() -> Vec<(Record, Record, f64)> {
    let u = gen_unrated;
    let r = |rating, games| gen_rated(rating, games, rating, games);
    let rp = |rating, games, peak| gen_rated(rating, games, peak, games);
    let zero = || u(0, 0, 0);
    vec![
        (zero(), zero(), 1.0),
        (zero(), zero(), 0.5),
        (after_losses(zero(), 0, 1), zero(), 0.0),
        (u(4, 6000, 4), u(4, 6000, 4), 0.5),
        (u(4, 6000, 8), zero(), 1.0),
        (u(4, 6000, 8), after_losses(zero(), 12, 0), 1.0),
        (u(4, 6000, 8), after_losses(zero(), 0, 1), 1.0),
        (u(4, 6000, 8), after_losses(zero(), 3, 2), 0.0),
        (r(1700, 45), zero(), 1.0),
        (r(1700, 45), after_losses(zero(), 2, 1), 1.0),
        (u(4, 14000, 8), r(3500, 400), 1.0),
        (u(4, 400, 1), r(100, 60), 0.0),
        (zero(), r(3500, 400), 0.0),
        (after_losses(zero(), 4, 0), r(3500, 400), 0.0),
        (after_losses(zero(), 4, 0), r(3500, 400), 0.5),
        (after_losses(u(3, 9000, 1), 2, 0), r(3500, 400), 0.0),
        (after_losses(u(4, 6800, 3), 27, 0), r(1500, 100), 1.0),
        (r(1700, 45), u(2, 3000, 2), 0.0),
        (r(2450, 300), u(4, 9000, 7), 1.0),
        (rp(1500, 29, 1520), rp(1700, 30, 1750), 1.0),
        (rp(1500, 30, 1520), rp(1700, 30, 1750), 0.5),
        (gen_rated(1500, 60, 1520, 29), gen_rated(1700, 60, 1750, 30), 1.0),
        (gen_rated(1500, 45, 1500, 5), gen_rated(1480, 45, 1500, 45), 0.0),
        (rp(1800, 100, 1850), rp(1200, 100, 1300), 1.0),
        (rp(1800, 100, 1850), rp(1200, 100, 1300), 0.0),
        (rp(1800, 100, 1850), rp(1200, 100, 1300), 0.5),
        (r(2390, 50), r(2300, 60), 1.0),
        (rp(2350, 80, 2410), rp(2380, 90, 2390), 0.0),
        (r(2400, 5), r(2000, 5), 0.5),
        (rp(110, 40, 1500), r(1600, 40), 0.0),
        (rp(101, 50, 1500), r(101, 50), 0.0),
        (rp(2600, 200, 2700), rp(2650, 300, 2700), 0.5),
        (r(1999, 29), r(2011, 29), 1.0),
        (rp(1503, 10, 1510), r(1497, 10), 0.5),
        (rp(2050, 60, 2450), r(2399, 60), 0.5),
        (rp(1850, 33, 1900), r(2420, 3), 1.0),
        (r(1500, 1), r(1100, 1), 1.0),
        (r(1500, 1), r(1099, 1), 1.0),
        (r(1500, 40), r(1504, 40), 0.5),
        (r(1500, 40), r(1503, 40), 0.5),
        (r(1500, 12), r(1554, 12), 0.5),
        (r(1500, 12), r(1553, 12), 0.5),
        (r(1500, 12), r(1892, 12), 1.0),
        (r(1500, 12), r(1891, 12), 1.0),
    ]
}

fn random_record(rnd: &mut Mulberry32) -> Record {
    if rnd.next() < 0.3 {
        let n = rnd.between(0, UNRATED_GAMES - 1);
        let mut sum = 0;
        for _ in 0..n {
            sum += rnd.between(100, 3500);
        }
        // The first counted game scored, after 0 to 3 losses and 0 to 2 wins left out.
        let half = if n > 0 { rnd.between(1, 2 * n) } else { 0 };
        let base = gen_unrated(n, sum, half);
        let lost = rnd.between(0, 3);
        let beaten = rnd.between(0, 2);
        return after_losses(base, lost, beaten);
    }
    let rating = rnd.between(100, 2900);
    let peak = if rnd.next() < 0.5 { rating } else { 3000.min(rating + rnd.between(0, 400)) };
    let games = rnd.between(1, 120);
    // Half of them count all their games (records stored before the counted games existed).
    let counted = if rnd.next() < 0.5 { games } else { rnd.between(games.min(UNRATED_GAMES), games) };
    let mut r = gen_rated(rating, games, peak, counted);
    r.wins = rnd.between(0, r.games);
    r.losses = rnd.between(0, r.games - r.wins);
    r.draws = r.games - r.wins - r.losses;
    r
}

/// A record with exactly the fields of the C++ `elo::Record`.
fn cpp_record(r: &Record) -> Value {
    json!({
        "rating": r.rating, "games": r.games, "wins": r.wins, "draws": r.draws, "losses": r.losses, "peak": r.peak,
        "rated": r.rated, "countedGames": r.counted_games, "unratedGames": r.unrated_games,
        "unratedOpponents": r.unrated_opponents, "unratedHalfPoints": r.unrated_half_points,
    })
}

fn record_of(v: &Value) -> Record {
    let i = |k: &str| v[k].as_i64().unwrap_or_else(|| panic!("field {k}"));
    Record {
        rating: i("rating"),
        games: i("games"),
        wins: i("wins"),
        draws: i("draws"),
        losses: i("losses"),
        peak: i("peak"),
        reached_senior: false,
        rated: v["rated"].as_bool().expect("rated"),
        counted_games: i("countedGames"),
        unrated_games: i("unratedGames"),
        unrated_opponents: i("unratedOpponents"),
        unrated_half_points: i("unratedHalfPoints"),
    }
}

fn score_json(score: f64) -> Value {
    if score == 0.5 { json!(0.5) } else { json!(score as i64) }
}

fn game_json(white: &Record, black: &Record, score: f64) -> Value {
    let res = apply_game(white, black, score, &CFG).unwrap();
    let side = |s: &SideChange| json!({"before": s.before, "after": s.after, "k": s.k, "record": cpp_record(&s.record)});
    json!({
        "white": cpp_record(white), "black": cpp_record(black), "score": score_json(score),
        "result": {"white": side(&res.white), "black": side(&res.black)},
    })
}

/// The vectors file as the generator writes it: tables on one line each, one vector per line.
fn render_vectors() -> String {
    let pd: Vec<i64> = (0..=800).map(scoring_probability).collect();
    let dp: Vec<i64> = (0..=100).map(rating_difference).collect();
    let mut rnd = Mulberry32(0x5ca1e17);
    let mut initial = Vec::new();
    let fixed = [
        (5, 7500, 5),
        (5, 7500, 10),
        (5, 7500, 0),
        (5, 4000, 10),
        (5, 4000, 0),
        (5, 17500, 10),
        (5, 500, 0),
        (5, 12000, 7),
        (5, 9321, 3),
        (6, 9000, 6),
        (3, 4500, 3),
    ];
    let first = |n, sum, half| {
        json!({"unratedGames": n, "unratedOpponents": sum, "unratedHalfPoints": half,
            "rating": initial_rating(n, sum, half)})
    };
    for (n, sum, half) in fixed {
        initial.push(first(n, sum, half));
    }
    for _ in 0..40 {
        let mut sum = 0;
        for _ in 0..UNRATED_GAMES {
            sum += 100 + (rnd.next() * 3401.0).floor() as i64;
        }
        let half = (rnd.next() * (2 * UNRATED_GAMES + 1) as f64).floor() as i64;
        initial.push(first(UNRATED_GAMES, sum, half));
    }
    let mut games: Vec<Value> = edge_games().iter().map(|(w, b, s)| game_json(w, b, *s)).collect();
    for _ in 0..200 {
        let w = random_record(&mut rnd);
        let b = random_record(&mut rnd);
        let s = [0.0, 0.5, 1.0][(rnd.next() * 3.0).floor() as usize];
        games.push(game_json(&w, &b, s));
    }
    let list = |a: &[Value]| a.iter().map(|x| format!("    {x}")).collect::<Vec<_>>().join(",\n");
    format!(
        "{{\n  \"generator\": {},\n  \"seniorRating\": {},\n  \"pd\": {},\n  \"dp\": {},\n  \"initial\": [\n{}\n  ],\n  \"games\": [\n{}\n  ]\n}}\n",
        Value::from(GENERATOR),
        SENIOR_RATING,
        Value::from(pd),
        Value::from(dp),
        list(&initial),
        list(&games),
    )
}

#[test]
fn elo_vectors() {
    let path = vectors_path();
    let text = render_vectors();
    if std::env::var_os("SCACELITH_UPDATE_VECTORS").is_some() {
        std::fs::write(&path, &text).expect("write the vectors");
    }
    let current = std::fs::read_to_string(&path).expect("read test/fixtures/elo-vectors.json");
    assert!(
        current == text,
        "elo-vectors.json is stale: run SCACELITH_UPDATE_VECTORS=1 cargo test elo_vectors"
    );
    let v: Value = serde_json::from_str(&current).unwrap();
    assert_eq!(v["seniorRating"], SENIOR_RATING);
    for (d, x) in v["pd"].as_array().unwrap().iter().enumerate() {
        assert_eq!(scoring_probability(d as i64), x.as_i64().unwrap());
    }
    for (p, x) in v["dp"].as_array().unwrap().iter().enumerate() {
        assert_eq!(rating_difference(p as i64), x.as_i64().unwrap());
    }
    for x in v["initial"].as_array().unwrap() {
        let i = |k: &str| x[k].as_i64().unwrap();
        assert_eq!(
            initial_rating(i("unratedGames"), i("unratedOpponents"), i("unratedHalfPoints")),
            i("rating")
        );
    }
    let games = v["games"].as_array().unwrap();
    assert!(games.len() >= 200);
    let (mut unrated_seen, mut established) = (0, 0);
    for x in games {
        let (w, b) = (record_of(&x["white"]), record_of(&x["black"]));
        let r = apply_game(&w, &b, x["score"].as_f64().unwrap(), &CFG).unwrap();
        for (side, got, input) in [("white", &r.white, &w), ("black", &r.black, &b)] {
            let want = &x["result"][side];
            assert_eq!(
                (got.before, got.after, got.k, cpp_record(&got.record)),
                (
                    want["before"].as_i64().unwrap(),
                    want["after"].as_i64().unwrap(),
                    want["k"].as_i64().unwrap(),
                    want["record"].clone()
                ),
                "{x}"
            );
            if !input.rated {
                unrated_seen += 1;
                if want["record"]["rated"] == true {
                    established += 1;
                }
            }
        }
    }
    assert!(unrated_seen > 20 && established > 3, "the vectors cover the unrated phase");
    assert_eq!(UNRATED_GAMES, 5);
}
