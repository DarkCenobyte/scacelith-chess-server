use std::collections::HashMap;
use std::sync::{Arc, LazyLock};

use parking_lot::Mutex;
use serde_json::{Value, json};

use super::*;
use crate::anticheat::integrity::{IntegrityRecord, player_level};
use crate::anticheat::num::js_json;
use crate::anticheat::synthetic::{
    Rng, SideOptions, gauss, history_from, learn_population, synthetic_history, synthetic_side,
};
use crate::clock::ManualClock;

fn clock() -> SharedClock {
    ManualClock::new(0.0, 0)
}

fn priors_only() -> Population {
    Population::new(None, clock())
}

// One learned population shared by the tests (the server's own data after a while).
static LEARNED: LazyLock<Population> = LazyLock::new(|| {
    let pop = priors_only();
    learn_population(&pop, &PriorsOnly, &mut Rng::new(99), 30000, &["5+0", "15+10"]);
    pop
});

fn learned() -> &'static Population {
    &LEARNED
}

fn score(games: &[SideRecord], pop: &Population) -> PlayerScore {
    score_player(games, pop, &PriorsOnly)
}

fn first_level(hist: &[SideRecord], pop: &Population, level: IntegrityLevel) -> Option<usize> {
    (1..=hist.len()).find(|&k| score(&hist[..k], pop).level == level)
}

// The store's running statistics, as the integrity tables keep them: one Welford state per
// `<prefix>|<metric>` key, merged from the observations of each write job. With `text`, the
// statistics are handed back as JSON text (stores with text columns).
#[derive(Default)]
struct MemoryStats {
    rows: Mutex<HashMap<String, Welford>>,
    text: bool,
}

impl MemoryStats {
    fn apply(&self, observations: &[Observation]) {
        let mut rows = self.rows.lock();
        for o in observations {
            let row = rows.entry(o.key.clone()).or_default();
            row.push(o.value);
        }
    }

    fn row(&self, key: &str) -> Option<Welford> {
        self.rows.lock().get(key).copied()
    }
}

impl PopulationSource for MemoryStats {
    fn population_stats(&self, key: &str) -> Result<BucketStats, SourceError> {
        let rows = self.rows.lock();
        let mut out = serde_json::Map::new();
        for metric in Metric::ALL {
            if let Some(w) = rows.get(&format!("{key}|{}", metric.name())) {
                out.insert(metric.name().into(), json!({ "n": w.n, "mean": w.mean, "m2": w.m2 }));
            }
        }
        let v = Value::Object(out);
        Ok(BucketStats::from_json(&if self.text { json!(v.to_string()) } else { v }))
    }
}

// A source that always fails: the population keeps to the priors.
struct Failing;

impl PopulationSource for Failing {
    fn population_stats(&self, _key: &str) -> Result<BucketStats, SourceError> {
        Err("database is locked".into())
    }
}

fn side(values: &[(Metric, f64)], n_complex: f64) -> SideRecord {
    let mut s = SideRecord { n_complex, ..SideRecord::default() };
    for &(m, v) in values {
        s.set_value(m, Some(v));
    }
    s
}

fn features(game_id: u64, white: Value, black: Value, extra: Value) -> Value {
    let mut f = json!({ "v": 1, "gameId": game_id, "category": "5+0", "baseMs": 300000, "incMs": 0,
        "endedAt": game_id, "analysedAt": game_id, "white": white, "black": black });
    if let (Value::Object(f), Value::Object(extra)) = (&mut f, extra) {
        f.extend(extra);
    }
    f
}

fn blitz() -> TimeClass {
    time_class_of_category("5+0")
}

#[test]
fn rating_buckets() {
    assert_eq!(bucket_of_rating(1549.0), 1500);
    assert_eq!(bucket_of_rating(99.0), 500);
    assert_eq!(bucket_of_rating(3400.0), 2900);
    assert_eq!(bucket_of_rating(f64::NAN), 1500);
}

#[test]
fn population_blends_priors_with_welford_data_winsorised_text_and_array_formats() {
    let store = MemoryStats { text: true, ..MemoryStats::default() };
    let pop = priors_only();
    let p = pop.effective(&store, Metric::Accuracy, "5+0", 1500, blitz());
    assert_eq!(p.n, 0.0);
    assert!((p.mean - prior_for(Metric::Accuracy, 1550.0, TimeClass::Blitz).mean).abs() < 1e-9);
    let s = side(&[(Metric::Accuracy, 90.0), (Metric::Acpl, 30.0), (Metric::T1Complex, 1.0)], 0.0);
    for _ in 0..200 {
        let observations = pop.update(&store, "5+0", 1500, &s, blitz());
        assert_eq!(observations.len(), 2, "t1Complex needs enough complex positions");
        store.apply(&observations);
    }
    // Re-read from the store.
    let q = priors_only().effective(&store, Metric::Accuracy, "5+0", 1500, blitz());
    assert_eq!(q.n, 200.0);
    assert!((q.mean - (PRIOR_GAMES * p.mean + 200.0 * 90.0) / (PRIOR_GAMES + 200.0)).abs() < 1e-6);
    assert_eq!(store.row("5+0|1500|accuracy").map(|w| w.n), Some(200.0), "one running statistic per metric");
    assert_eq!(priors_only().raw(&store, "5+0", 1500).get(Metric::T1Complex), None);
    // An absurd value is clipped at 4 sd.
    let pop2 = priors_only();
    pop2.update(&PriorsOnly, "5+0", 1500, &side(&[(Metric::Acpl, 100000.0)], 0.0), blitz());
    let eff = pop2.effective(&PriorsOnly, Metric::Acpl, "5+0", 1500, blitz());
    assert!(eff.mean < 200.0, "winsorised mean {}", eff.mean);
    // Array rows as another store might return them.
    struct Rows;
    impl PopulationSource for Rows {
        fn population_stats(&self, _key: &str) -> Result<BucketStats, SourceError> {
            Ok(BucketStats::from_json(
                &json!([{ "metric": "accuracy", "n": 1000, "mean": 70, "m2": 999 * 25 }]),
            ))
        }
    }
    let e3 = priors_only().effective(&Rows, Metric::Accuracy, "5+0", 1500, blitz());
    assert_eq!(e3.n, 1000.0);
    assert!(e3.mean < 71.0 && e3.mean > 70.0);
}

#[test]
fn population_statistics_reach_the_store_and_survive_a_restart() {
    let store = MemoryStats::default();
    let pop = priors_only();
    let values = [80.0, 84.0, 76.0, 90.0, 70.0];
    for accuracy in values {
        store.apply(&pop.update(
            &store,
            "3+2",
            1500,
            &side(&[(Metric::Accuracy, accuracy), (Metric::Acpl, 40.0)], 0.0),
            blitz(),
        ));
    }
    let saved = store.population_stats("3+2|1500").expect("memory statistics");
    let present: Vec<Metric> = Metric::ALL.into_iter().filter(|m| saved.get(*m).is_some()).collect();
    assert_eq!(present, [Metric::Accuracy, Metric::Acpl]);
    let acc = saved.get(Metric::Accuracy).expect("accuracy statistics");
    assert_eq!(acc.n, 5.0);
    let m = values.iter().sum::<f64>() / 5.0;
    assert!((acc.mean - m).abs() < 1e-9);
    assert!((acc.m2 - values.iter().map(|x| (x - m) * (x - m)).sum::<f64>()).abs() < 1e-9);
    // A new process (or the periodic cache reload) reads the same statistics back.
    let again = priors_only().raw(&store, "3+2", 1500).get(Metric::Accuracy).expect("re-read");
    assert_eq!(again.n, 5.0);
    let cached = pop.raw(&store, "3+2", 1500).get(Metric::Accuracy).expect("cached");
    assert!((again.mean - cached.mean).abs() < 1e-9);
}

#[test]
fn the_cache_reloads_and_a_failed_read_falls_back_to_the_priors() {
    let manual = ManualClock::new(0.0, 0);
    let pop = Population::new(None, manual.clone()).with_reload_ms(1000);
    let store = MemoryStats::default();
    assert!(pop.raw(&store, "5+0", 1500).is_empty());
    store.apply(&[Observation { key: "5+0|1500|accuracy".into(), value: 80.0 }]);
    assert!(pop.raw(&store, "5+0", 1500).is_empty(), "cached until the reload");
    manual.set_wall(1000);
    assert_eq!(pop.raw(&store, "5+0", 1500).get(Metric::Accuracy).map(|w| w.n), Some(1.0));
    pop.invalidate();
    assert!(pop.raw(&Failing, "5+0", 1500).is_empty());
    let p = pop.effective(&Failing, Metric::Acpl, "5+0", 1500, blitz());
    assert_eq!(p.mean, prior_for(Metric::Acpl, 1550.0, TimeClass::Blitz).mean);
}

#[test]
fn stored_statistics_formats() {
    let v = json!({ "metrics": { "accuracy": { "n": 3, "mean": 80, "variance": 4 }, "acpl": { "n": "2", "mean": 50, "m2": 8 },
        "t1Deep": { "mean": 1 }, "timeCv": { "n": null, "mean": 1, "m2": null, "variance": 2 } } });
    let s = BucketStats::from_json(&v);
    assert_eq!(
        s.get(Metric::Accuracy),
        Some(Welford { n: 3.0, mean: 80.0, m2: 8.0 }),
        "m2 from the variance"
    );
    assert_eq!(s.get(Metric::Acpl), Some(Welford { n: 2.0, mean: 50.0, m2: 8.0 }));
    assert_eq!(s.get(Metric::T1Deep), None, "no count");
    assert_eq!(s.get(Metric::TimeCv), Some(Welford { n: 0.0, mean: 1.0, m2: 0.0 }), "+null is 0");
    let text = json!(json!({ "accuracy": { "n": 1, "mean": 2, "m2": 0 } }).to_string());
    assert_eq!(BucketStats::from_json(&text).get(Metric::Accuracy).map(|w| w.mean), Some(2.0));
    assert!(BucketStats::from_json(&json!("{broken")).is_empty());
    assert!(BucketStats::from_json(&Value::Null).is_empty());
    let rows = json!([{ "metric": "acpl", "n": 4, "mean": "30" }, { "metric": "nope", "n": 1 }, { "n": 2 }]);
    assert_eq!(
        BucketStats::from_json(&rows).get(Metric::Acpl),
        Some(Welford { n: 4.0, mean: 30.0, m2: 0.0 })
    );
}

#[test]
fn population_statistics_are_kept_per_profile_and_a_player_is_scored_on_one_profile() {
    let store = MemoryStats::default();
    let a = "Stockfish 16; nn-5af11540bbfe.nnue; depth 10/18; hash 32; analysis 1";
    let b = "Stockfish 19; nn-1a298aa575a0.nnue; depth 9/16; hash 32; analysis 1";
    let pop_a = Population::new(Some(a.into()), clock());
    let pop_b = Population::new(Some(b.into()), clock());
    let f = features(
        1,
        json!({ "userId": 1, "rating": 1500, "n": 30, "accuracy": 80, "acpl": 50 }),
        json!({ "userId": 2, "rating": 1500, "n": 30, "accuracy": 85, "acpl": 40 }),
        json!({ "profile": a }),
    );
    let none = |_| IntegrityLevel::None;
    assert_eq!(
        update_population_from_game(&pop_b, &store, &f, none).added,
        0,
        "a game of another profile is refused"
    );
    let up = update_population_from_game(&pop_a, &store, &f, none);
    assert_eq!(up.added, 2);
    store.apply(&up.observations);
    assert_eq!(store.row(&format!("{a}|5+0|1500|accuracy")).map(|w| w.n), Some(2.0));
    let reread = Population::new(Some(a.into()), clock()).raw(&store, "5+0", 1500);
    assert_eq!(reread.get(Metric::Accuracy).map(|w| w.n), Some(2.0), "re-read from the store");
    assert!(pop_b.raw(&store, "5+0", 1500).is_empty(), "the other profile starts from the priors");

    let o = SideOptions { user_id: 1, rating: 1500.0, ..SideOptions::default() };
    let mut hist = synthetic_history(&mut Rng::new(8), 12, Some(0), 1.0, &o);
    for (i, g) in hist.iter_mut().enumerate() {
        g.profile = Some(if i < 8 { a } else { b }.to_string());
    }
    assert_eq!(score(&hist, &pop_a).games, 8);
    assert_eq!(score(&hist, &pop_b).games, 4);
    assert_eq!(score(&hist, &priors_only()).games, 0, "records of a profile are never scored without it");
}

fn perfect(i: usize) -> SideRecord {
    let mut g = side(
        &[
            (Metric::Accuracy, 100.0),
            (Metric::Acpl, 0.0),
            (Metric::T1Deep, 1.0),
            (Metric::T1Fast, 1.0),
            (Metric::T1Complex, 1.0),
            (Metric::TimeCorr, -0.5),
            (Metric::TimeCv, 0.05),
        ],
        15.0,
    );
    g.game_id = i as f64;
    g.category = "5+0".into();
    g.rating = NumField::Number(1200.0);
    g.rating_games = NumField::Number(100.0);
    g.n = 40.0;
    g.n_timed = 40.0;
    g.ended_at = i as f64;
    g
}

#[test]
fn no_flag_on_small_samples_however_extreme() {
    let games: Vec<SideRecord> = (0..4).map(perfect).collect();
    for pop in [&priors_only(), learned()] {
        let r = score(&games, pop);
        assert_eq!(r.level, IntegrityLevel::None);
        assert!(r.groups.q.expect("Q") > 0.0);
    }
    // Games with too few scored moves are ignored entirely.
    let tiny: Vec<SideRecord> =
        (0..8).map(|i| SideRecord { n: 5.0, game_id: i as f64, ..perfect(i % 4) }).collect();
    assert_eq!(score(&tiny, learned()).games, 0);
}

#[test]
fn no_flag_for_strong_but_consistent_honest_players() {
    let mut r = Rng::new(7);
    for i in 0..60u32 {
        let rating = [1100.0, 1600.0, 2100.0, 2500.0][i as usize % 4];
        // 2.5 between-player sd above peers of the same rating, every game, for 30 games.
        let time_style = 0.5 * gauss(&mut r);
        let category = if i % 2 == 1 { "5+0" } else { "15+10" };
        let o = SideOptions {
            user_id: i + 1,
            rating,
            category,
            theta: 1.0,
            time_style,
            ..SideOptions::default()
        };
        let hist = synthetic_history(&mut r, 30, None, 1.0, &o);
        for pop in [learned(), &priors_only()] {
            let res = score(&hist, pop);
            assert_eq!(res.level, IntegrityLevel::None, "rating {rating}: {}", res.groups.to_json());
        }
    }
}

#[test]
fn a_provisional_rating_gets_the_benefit_of_the_doubt() {
    // A 1900-strength player whose rating still says 1500.
    let g = synthetic_side(
        &mut Rng::new(3),
        &SideOptions { user_id: 1, rating: 1900.0, ..SideOptions::default() },
    );
    let with = |rating_games: f64| SideRecord {
        rating: NumField::Number(1500.0),
        rating_games: NumField::Number(rating_games),
        ..g.clone()
    };
    let est = game_z(&with(100.0), learned(), &PriorsOnly);
    let prov = game_z(&with(5.0), learned(), &PriorsOnly);
    // Judged against peers up to 400 points stronger instead of 100.
    let (pq, eq) = (prov.zq.expect("zQ"), est.zq.expect("zQ"));
    assert!(pq < eq - 0.15, "provisional {pq} vs established {eq}");
    assert!(prov.z[Metric::Accuracy.index()] < est.z[Metric::Accuracy.index()]);
}

#[test]
fn assisted_player_flagged_only_after_enough_games_then_high_confidence() {
    let mut r = Rng::new(11);
    for i in 0..10 {
        let o = SideOptions { user_id: 1000 + i, rating: 1500.0, ..SideOptions::default() };
        let hist = synthetic_history(&mut r, 30, Some(0), 1.0, &o);
        let s = first_level(&hist, learned(), IntegrityLevel::Suspected)
            .or_else(|| first_level(&hist, learned(), IntegrityLevel::HighConfidence));
        let h = first_level(&hist, learned(), IntegrityLevel::HighConfidence);
        assert!(s.is_none_or(|s| s >= model::suspected::MIN_GAMES));
        assert!(h.is_some_and(|h| h >= model::high::MIN_GAMES), "high at {h:?}");
        let last = score(&hist, learned());
        assert_eq!(last.level, IntegrityLevel::HighConfidence);
        assert!(last.reasons.iter().any(|x| x.starts_with("Move quality")));
    }
}

#[test]
fn high_confidence_needs_independent_agreement() {
    let mut r = Rng::new(12);
    // Engine moves with human timing stay suspected.
    for i in 0..10 {
        let o = SideOptions {
            user_id: 2000 + i,
            rating: 1500.0,
            engine_timing: Some(false),
            ..SideOptions::default()
        };
        let hist = synthetic_history(&mut r, 30, Some(0), 1.0, &o);
        for k in 1..=30 {
            assert_ne!(score(&hist[..k], learned()).level, IntegrityLevel::HighConfidence);
        }
        assert_eq!(score(&hist, learned()).level, IntegrityLevel::Suspected);
    }
    // Timing alone (a very regular, flat thinker) never flags anyone.
    for i in 0..10 {
        let o = SideOptions { user_id: 3000 + i, rating: 1500.0, time_style: 3.0, ..SideOptions::default() };
        let hist = synthetic_history(&mut r, 30, None, 1.0, &o);
        let res = score(&hist, learned());
        assert_eq!(res.level, IntegrityLevel::None);
        assert!(res.groups.t.expect("T") > 2.0, "timing signal {:?}", res.groups.t);
    }
}

#[test]
fn a_sudden_lasting_jump_is_flagged_but_not_one_outstanding_game_or_a_plausible_improvement() {
    let mut r = Rng::new(21);
    let mut flagged = 0;
    for i in 0..10 {
        // 20 honest games, then 10 engine games with human-looking timing.
        let o = SideOptions {
            user_id: 4000 + i,
            rating: 1300.0,
            theta: 0.2,
            engine_timing: Some(false),
            ..SideOptions::default()
        };
        let hist = synthetic_history(&mut r, 30, Some(20), 1.0, &o);
        let res = score(&hist, learned());
        assert!(res.jump.lasting);
        assert!(res.jump.score > 0.0);
        // The 30-game window alone is diluted by the honest games...
        assert!(res.window_all.q[0].expect("Q score") < model::suspected::ACCURACY_TYPE);
        if res.level != IntegrityLevel::None {
            flagged += 1;
        }
    }
    assert!(flagged >= 8, "jump flagged in {flagged}/10");
    for i in 0..20 {
        let user_id = 5000 + i;
        let o = SideOptions { user_id, rating: 1600.0, ..SideOptions::default() };
        let mut hist = synthetic_history(&mut r, 30, None, 1.0, &o);
        let (game_id, ended_at) = (hist[25].game_id, hist[25].ended_at);
        hist[25] = synthetic_side(&mut r, &SideOptions { engine: 1.0, game_id, ended_at, ..o.clone() });
        let res = score(&hist, learned());
        assert_eq!(res.jump.score, 0.0);
        assert_eq!(res.level, IntegrityLevel::None);
        // Honest improvement of 0.75 per-game sd (rating lagging behind).
        let o = SideOptions { user_id: 6000 + i, rating: 1600.0, ..SideOptions::default() };
        let mut better = synthetic_history(&mut r, 30, Some(20), 0.0, &o);
        for g in better.iter_mut().skip(20) {
            let (game_id, ended_at) = (g.game_id, g.ended_at);
            *g = synthetic_side(&mut r, &SideOptions { theta: 0.75, game_id, ended_at, ..o.clone() });
        }
        assert_eq!(score(&better, learned()).level, IntegrityLevel::None);
    }
}

#[test]
fn honest_population_no_flags_at_any_sample_size() {
    let mut r = Rng::new(5);
    let mut flags = 0;
    for i in 0..400u32 {
        let rating = 800.0 + (r.next() * 1800.0).floor();
        let theta = 0.4 * gauss(&mut r);
        let time_style = 0.5 * gauss(&mut r);
        let category = if i % 2 == 1 { "5+0" } else { "15+10" };
        let o =
            SideOptions { user_id: 7000 + i, rating, category, theta, time_style, ..SideOptions::default() };
        let hist = synthetic_history(&mut r, 30, None, 1.0, &o);
        for k in [5, 10, 20, 30] {
            if score(&hist[..k], learned()).level != IntegrityLevel::None {
                flags += 1;
            }
        }
    }
    assert_eq!(flags, 0);
}

// Scores the games, applies the memory rules to the stored record and stores the outcome.
fn update_integrity(
    rec: &mut IntegrityRecord,
    games: &[SideRecord],
    pop: &Population,
    now: i64,
) -> LevelUpdateView {
    let result = score(games, pop);
    let up = player_level(rec, games, &result, pop, now);
    *rec = IntegrityRecord { level: up.level, score: up.score, evidence: up.evidence.clone() };
    LevelUpdateView { level: up.level, result }
}

struct LevelUpdateView {
    level: IntegrityLevel,
    result: PlayerScore,
}

#[test]
fn integrity_stores_level_score_and_evidence_with_numbers_and_never_touches_confirmed() {
    let mut r = Rng::new(33);
    let (cheat, honest) = (1, 2);
    let o = SideOptions { user_id: cheat, rating: 1500.0, ..SideOptions::default() };
    let hist = synthetic_history(&mut r, 25, Some(0), 1.0, &o);
    let opp = synthetic_history(&mut r, 25, None, 1.0, &SideOptions { user_id: honest, ..o.clone() });
    let (mut cheat_rec, mut honest_rec) = (IntegrityRecord::default(), IntegrityRecord::default());
    let mut last = None;
    for i in 0..hist.len() {
        let now = 1_900_000_000_000 + i as i64;
        last = Some(update_integrity(&mut cheat_rec, &hist[..=i], learned(), now).level);
        update_integrity(&mut honest_rec, &opp[..=i], learned(), now);
    }
    assert_eq!(last, Some(IntegrityLevel::HighConfidence));
    assert_eq!(cheat_rec.level, IntegrityLevel::HighConfidence);
    assert!(cheat_rec.score >= 3.0);
    let stats = &cheat_rec.evidence["statistics"];
    assert_eq!(stats["games"], 25);
    assert!(stats["trigger"].as_str().expect("trigger").contains("agree"));
    assert_eq!(cheat_rec.evidence["peak"]["level"], "high_confidence");
    assert_eq!(
        honest_rec.evidence["statistics"].get("reasons"),
        None,
        "compact evidence for unremarkable players"
    );
    let reasons: Vec<&str> =
        stats["reasons"].as_array().expect("reasons").iter().filter_map(Value::as_str).collect();
    let joined = reasons.join(" ");
    let accuracy = joined.split("accuracy ").nth(1).expect("an accuracy reason");
    let words: Vec<&str> = accuracy.split(' ').take(4).collect();
    let one_decimal =
        |s: &str| s.split_once('.').is_some_and(|(a, b)| a.parse::<u32>().is_ok() && b.len() == 1);
    assert!(
        one_decimal(words[0]) && words[1] == "vs" && one_decimal(words[2]) && words[3] == "expected,",
        "{joined}"
    );
    assert_eq!(honest_rec.level, IntegrityLevel::None);

    let mut confirmed = IntegrityRecord::from_stored(
        Some("confirmed"),
        Some(0.0),
        Some(&json!({ "certain": [{ "kind": "illegal_move" }] })),
    );
    let res = update_integrity(&mut confirmed, &[], learned(), 0);
    assert_eq!(res.level, IntegrityLevel::Confirmed);
    assert_eq!(confirmed.evidence["certain"].as_array().map(Vec::len), Some(1));
}

#[test]
fn high_confidence_falls_back_to_suspected_and_a_cleared_player_needs_new_evidence() {
    let mut u =
        IntegrityRecord { level: IntegrityLevel::HighConfidence, score: 5.0, ..IntegrityRecord::default() };
    assert_eq!(update_integrity(&mut u, &[], learned(), 0).level, IntegrityLevel::Suspected);

    let mut r = Rng::new(44);
    let o = SideOptions { user_id: 3, rating: 1200.0, engine_timing: Some(false), ..SideOptions::default() };
    let hist = history_from(&mut r, 20, Some(0), 1.0, 1000.0, &o);
    let score = score(&hist, learned()).score;
    let cleared_at = 1_800_000_000_000i64;
    let mut v = IntegrityRecord::from_stored(
        Some("none"),
        Some(score),
        Some(&json!({ "review": { "clearedAt": cleared_at, "clearedScore": score, "by": "mod" } })),
    );
    let res = update_integrity(&mut v, &hist, learned(), cleared_at + 1);
    assert_ne!(res.result.level, IntegrityLevel::None, "the model alone would flag");
    assert_eq!(res.level, IntegrityLevel::None, "but the moderator cleared this evidence");
    // New evidence: five games analysed since the review, with a clearly higher score.
    let mut newer = hist.clone();
    for g in newer.iter_mut().skip(15) {
        g.analysed_at = (cleared_at + 10) as f64;
    }
    let mut v2 = IntegrityRecord::from_stored(
        Some("none"),
        Some(score - 1.0),
        Some(&json!({ "review": { "clearedAt": cleared_at, "clearedScore": score - 1.0 } })),
    );
    assert_eq!(update_integrity(&mut v2, &newer, learned(), cleared_at + 20).level, res.result.level);
}

#[test]
fn suspected_has_hysteresis() {
    let mut r = Rng::new(45);
    let o = SideOptions { user_id: 1, rating: 1500.0, engine_timing: Some(false), ..SideOptions::default() };
    let hist = synthetic_history(&mut r, 30, Some(0), 0.8, &o);
    let threshold = model::suspected::ACCURACY_TYPE - model::suspected::HYSTERESIS;
    let k = (5..=30)
        .find(|&k| {
            let res = score(&hist[..k], learned());
            res.level == IntegrityLevel::None && res.groups.accuracy_type.is_some_and(|a| a >= threshold)
        })
        .expect("a history just under the threshold");
    let games = &hist[..k];
    let mut rec =
        IntegrityRecord { level: IntegrityLevel::Suspected, score: 3.6, ..IntegrityRecord::default() };
    assert_eq!(update_integrity(&mut rec, games, learned(), 0).level, IntegrityLevel::Suspected);
    let mut rec = IntegrityRecord::default();
    assert_eq!(
        update_integrity(&mut rec, games, learned(), 0).level,
        IntegrityLevel::None,
        "no flag from below"
    );
}

#[test]
fn a_new_profile_keeps_the_level_until_enough_games_are_scored() {
    let o = SideOptions { user_id: 1, rating: 1500.0, ..SideOptions::default() };
    let mut hist = synthetic_history(&mut Rng::new(46), 12, None, 1.0, &o);
    for (i, g) in hist.iter_mut().enumerate() {
        g.profile = Some(if i < 9 { "old" } else { "new" }.to_string());
    }
    let pop = Population::new(Some("new".into()), clock());
    let mut rec = IntegrityRecord { level: IntegrityLevel::Suspected, ..IntegrityRecord::default() };
    let up = update_integrity(&mut rec, &hist, &pop, 0);
    assert_eq!((up.result.games, up.level), (3, IntegrityLevel::Suspected), "three games of the new profile");
    let mut rec = IntegrityRecord { level: IntegrityLevel::Suspected, ..IntegrityRecord::default() };
    for g in hist.iter_mut() {
        g.profile = Some("new".into());
    }
    assert_eq!(
        update_integrity(&mut rec, &hist, &pop, 0).level,
        IntegrityLevel::None,
        "judged on the new profile"
    );
}

#[test]
fn population_updates_skip_flagged_players_and_short_games() {
    let pop = priors_only();
    let f = features(
        1,
        json!({ "userId": 1, "rating": 1500, "n": 30, "accuracy": 80, "acpl": 50 }),
        json!({ "userId": 2, "rating": 1500, "n": 30, "accuracy": 99, "acpl": 5 }),
        json!({}),
    );
    let up = update_population_from_game(&pop, &PriorsOnly, &f, |uid| {
        if uid == 2 { IntegrityLevel::HighConfidence } else { IntegrityLevel::None }
    });
    assert_eq!(up.added, 1);
    assert_eq!(up.observations.len(), 2);
    let acc = pop.raw(&PriorsOnly, "5+0", 1500).get(Metric::Accuracy).expect("accuracy statistics");
    assert_eq!((acc.n, acc.mean), (1.0, 80.0));
    let short = features(
        2,
        json!({ "userId": 1, "rating": 1500, "n": 4, "accuracy": 10 }),
        json!({ "userId": 3, "rating": 1500, "n": 4, "accuracy": 10 }),
        json!({}),
    );
    assert_eq!(update_population_from_game(&pop, &PriorsOnly, &short, |_| IntegrityLevel::None).added, 0);
    assert_eq!(
        update_population_from_game(&pop, &PriorsOnly, &json!("not json"), |_| IntegrityLevel::None).added,
        0
    );
}

#[test]
fn sides_of_stored_features() {
    let f = features(
        7,
        json!({ "userId": 1, "rating": null, "n": 30, "accuracy": 80, "t1Complex": null, "moves": [[16, 0]] }),
        json!({ "userId": 2, "rating": 1500, "ratingGames": 3, "n": 30 }),
        json!({ "endedAt": 0, "analysedAt": 99, "profile": "" }),
    );
    let text = json!(f.to_string());
    let w = side_of(&text, 1).expect("white side");
    assert_eq!((w.game_id, w.ended_at, w.analysed_at, w.base_ms), (7.0, 99.0, 99.0, 300000.0));
    assert_eq!((w.rating, w.rating_games), (NumField::Null, NumField::Absent));
    assert_eq!(w.value(Metric::Accuracy), Some(80.0));
    assert_eq!(w.value(Metric::T1Complex), None);
    assert_eq!(w.profile, None);
    let b = side_of(&f, 2).expect("black side");
    assert_eq!((b.rating, b.rating_games), (NumField::Number(1500.0), NumField::Number(3.0)));
    assert!(side_of(&f, 3).is_none());
    assert!(side_of(&json!({ "white": { "userId": 1 } }), 1).is_none());

    let shown = side_json(&f, 1).expect("white side");
    let want = r#"{"gameId":7,"category":"5+0","baseMs":300000,"incMs":0,"endedAt":99,"analysedAt":99,"profile":null,"userId":1,"rating":null,"n":30,"accuracy":80,"t1Complex":null,"moves":[[16,0]]}"#;
    assert_eq!(shown.to_string(), want);
}

// ---- parity with the former server ------------------------------------------------------------

// Reference outputs of the former server (Node 22) for the same synthetic histories.
static VECTORS: LazyLock<Value> =
    LazyLock::new(|| serde_json::from_str(include_str!("vectors.json")).expect("valid reference vectors"));

fn vector(key: &str) -> &'static str {
    VECTORS[key].as_str().expect("a reference vector")
}

fn without_per_game(res: &PlayerScore) -> String {
    let mut v = res.to_json();
    v.as_object_mut().expect("an object").shift_remove("perGame");
    js_json(&v)
}

#[test]
fn parity_priors_only_scoring_and_evidence() {
    let o = SideOptions { user_id: 1, rating: 1500.0, ..SideOptions::default() };
    let hist = synthetic_history(&mut Rng::new(7), 20, Some(8), 1.0, &o);
    let pop = priors_only();
    let res = score(&hist, &pop);
    assert_eq!(without_per_game(&res), vector("A"));
    let z: Vec<Value> = res
        .per_game
        .iter()
        .map(|g| json!([json_num(g.game_id), json_opt(g.zq), json_opt(g.ze), json_opt(g.zt)]))
        .collect();
    assert_eq!(js_json(&Value::Array(z)), vector("A_z"));
    assert_eq!(js_json(&res.per_game[0].to_json()), vector("A_perGame0"));

    let prev =
        IntegrityRecord::from_stored(Some("none"), Some(0.0), Some(&json!({ "review": { "by": "m" } })));
    let up = player_level(&prev, &hist, &res, &pop, 1_900_000_000_000);
    let written: Value = serde_json::from_str(vector("A_written")).expect("valid JSON");
    assert_eq!(up.level.as_str(), vector("A_level"));
    assert_eq!(json_num(up.score), written["score"]);
    let want = js_json(&written["evidence"]);
    assert_eq!(js_json(&Value::Object(up.evidence)), want);
}

#[test]
fn parity_learned_population() {
    let rows: Value = serde_json::from_str(vector("learned")).expect("valid JSON");
    for row in rows.as_array().expect("rows") {
        let (category, bucket) = (row[0].as_str().expect("category"), row[1].as_i64().expect("bucket"));
        let metric = Metric::parse(row[2].as_str().expect("metric")).expect("a metric");
        let e = learned().effective(&PriorsOnly, metric, category, bucket, time_class_of_category(category));
        let got = json!([js_number(e.mean), js_number(e.sd), json_num(e.n)]);
        assert_eq!(got, json!([row[3], row[4], row[5]]), "{category} {bucket} {metric}");
    }
    let o = SideOptions { user_id: 1000, rating: 1500.0, ..SideOptions::default() };
    let assisted = synthetic_history(&mut Rng::new(11), 30, Some(0), 1.0, &o);
    assert_eq!(without_per_game(&score(&assisted, learned())), vector("B"));
    let o = SideOptions {
        user_id: 4000,
        rating: 1300.0,
        theta: 0.2,
        engine_timing: Some(false),
        ..SideOptions::default()
    };
    let jump = synthetic_history(&mut Rng::new(21), 30, Some(20), 1.0, &o);
    assert_eq!(without_per_game(&score(&jump, learned())), vector("C"));

    let zs = |rating: NumField, rating_games: NumField| {
        let gz = game_z(&SideRecord { rating, rating_games, ..jump[0].clone() }, learned(), &PriorsOnly);
        let s = |x: Option<f64>| x.map_or(Value::Null, |x| json!(js_number(x)));
        let mut v = vec![s(gz.zq), s(gz.ze), s(gz.zt), s(Some(gz.n)), s(Some(gz.n_e)), s(Some(gz.n_t))];
        v.extend(Metric::ALL.iter().map(|m| s(gz.z[m.index()])));
        Value::Array(v)
    };
    let got = json!([
        zs(NumField::Number(1500.0), NumField::Number(100.0)),
        zs(NumField::Number(1500.0), NumField::Number(5.0)),
        zs(NumField::Null, NumField::Null),
    ]);
    assert_eq!(js_json(&got), vector("D"));
}

#[test]
fn shared_population_is_usable_from_threads() {
    let pop = Arc::new(priors_only());
    let threads: Vec<_> = (0..4)
        .map(|i| {
            let pop = Arc::clone(&pop);
            std::thread::spawn(move || {
                let s = side(&[(Metric::Accuracy, 80.0 + f64::from(i))], 0.0);
                pop.update(&PriorsOnly, "5+0", 1500 + 100 * i64::from(i), &s, TimeClass::Blitz).len()
            })
        })
        .collect();
    for t in threads {
        assert_eq!(t.join().expect("no panic"), 1);
    }
}
