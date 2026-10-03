//! Port of test/unit/match.challenges.test.js.

use std::collections::{HashSet, VecDeque};

use super::*;

// CHALLENGE_TTL_MS 60 s, PRIVATE_GAME_TTL_MS 15 min, custom time controls allowed.
fn config() -> Config {
    Config::for_tests()
}

fn make_with(config: &Config, random_int: RandomInt) -> Challenges {
    Challenges::new(ChallengeSettings::from_config(config), Categories::from_config(config), random_int)
}

fn make() -> Challenges {
    make_with(&config(), os_random_int())
}

// Draws taken in order (modulo max), then 0.
fn draws(list: &[u32]) -> RandomInt {
    let mut q: VecDeque<u32> = list.iter().copied().collect();
    Box::new(move |max| q.pop_front().map_or(0, |v| v % max))
}

const T0: i64 = 1000;

fn alice() -> ChallengePlayer {
    ChallengePlayer { user_id: 1, username: "Alice".into(), rating: 1500, provisional: false, conn_id: 11 }
}

fn bob() -> ChallengePlayer {
    ChallengePlayer { user_id: 2, username: "Bob".into(), rating: 1600, provisional: true, conn_id: 22 }
}

fn carol() -> ChallengePlayer {
    ChallengePlayer { user_id: 3, username: "Carol".into(), rating: 1400, provisional: false, conn_id: 33 }
}

fn named(user_id: UserId, username: &str) -> ChallengePlayer {
    ChallengePlayer { user_id, username: username.into(), ..ChallengePlayer::default() }
}

fn online(p: &ChallengePlayer) -> TargetUser {
    TargetUser { user_id: p.user_id, username: p.username.clone(), accept_challenges: true, online: true }
}

fn direct_req(from: &ChallengePlayer, to: &ChallengePlayer) -> CreateRequest {
    CreateRequest {
        from: from.clone(),
        target: to.username.clone(),
        target_user: Some(online(to)),
        base_sec: 180,
        inc_sec: 2,
        rated: true,
        color: ColorPref::Random,
    }
}

fn private_req(
    from: &ChallengePlayer,
    base_sec: i64,
    inc_sec: i64,
    rated: bool,
    color: ColorPref,
) -> CreateRequest {
    CreateRequest {
        from: from.clone(),
        target: String::new(),
        target_user: None,
        base_sec,
        inc_sec,
        rated,
        color,
    }
}

fn direct(
    ch: &mut Challenges,
    from: &ChallengePlayer,
    to: &ChallengePlayer,
) -> Result<Challenge, MatchError> {
    ch.create(direct_req(from, to), T0)
}

fn ids(list: &[&Challenge]) -> Vec<u32> {
    list.iter().map(|c| c.id).collect()
}

#[test]
fn direct_challenge_fields_and_colour_offered_to_the_receiver() {
    let mut ch = make();
    let c = ch.create(CreateRequest { color: ColorPref::White, ..direct_req(&alice(), &bob()) }, T0).unwrap();
    assert_eq!(c.kind, ChallengeKind::Direct);
    assert_eq!(c.target, "Bob");
    assert_eq!(c.target_user_id, 2);
    assert_eq!(c.code, "");
    assert_eq!(c.category, "3+2");
    assert_eq!((c.base_sec, c.inc_sec), (180, 2));
    assert_eq!(c.base_ms, 180000);
    assert_eq!(c.inc_ms, 2000);
    assert!(c.rated);
    assert_eq!(c.receiver_color, ColorPref::Black);
    assert_eq!(c.state, ChallengeState::Pending);
    assert_eq!(c.created_at, T0);
    assert_eq!(c.expires_at, T0 + config().challenge_ttl_ms);
    assert!(c.id > 0);
    let black =
        ch.create(CreateRequest { color: ColorPref::Black, ..direct_req(&alice(), &carol()) }, T0).unwrap();
    assert_eq!(black.receiver_color, ColorPref::White);
    assert_eq!(direct(&mut ch, &bob(), &carol()).unwrap().receiver_color, ColorPref::Random);
    assert_eq!(ids(&ch.for_user(2, T0).incoming), [c.id]);
    assert_eq!(ch.for_user(2, T0).outgoing.len(), 1);
    assert_eq!(ch.get(c.id, T0), Some(&c));
}

#[test]
fn time_controls_rated_and_custom_rules() {
    let mut ch = make();
    let base = |base_sec, inc_sec, rated| private_req(&alice(), base_sec, inc_sec, rated, ColorPref::Random);
    for (b, i) in [(10, 0), (10801, 0), (60, 181), (60, -1), (-60, 0)] {
        assert_eq!(ch.create(base(b, i, false), T0), Err(MatchError::InvalidTimeControl), "{b}+{i}");
    }
    // 4+1 is not official: never rated, casual allowed.
    assert_eq!(ch.create(base(240, 1, true), T0), Err(MatchError::RatedRequiresOfficialTc));
    let custom = ch.create(base(240, 1, false), T0).unwrap();
    assert_eq!(custom.category, "custom");
    // Without ALLOW_CUSTOM_TIME_CONTROLS only official time controls exist.
    let mut strict_config = config();
    strict_config.allow_custom_time_controls = false;
    let mut strict = make_with(&strict_config, os_random_int());
    assert_eq!(strict.create(base(240, 1, false), T0), Err(MatchError::InvalidTimeControl));
    assert_eq!(strict.create(base(240, 1, true), T0), Err(MatchError::RatedRequiresOfficialTc));
    let official = strict.create(base(600, 5, false), T0).unwrap();
    assert_eq!(official.category, "10+5");
    // The bounds themselves are valid.
    assert!(ch.create(private_req(&bob(), 15, 0, false, ColorPref::Random), T0).is_ok());
    assert!(ch.create(private_req(&bob(), 10800, 180, false, ColorPref::Random), T0).is_ok());
}

#[test]
fn target_checks_and_self_challenge() {
    let mut ch = make();
    let req = |target: &str, target_user: Option<TargetUser>| CreateRequest {
        target: target.into(),
        target_user,
        rated: false,
        ..direct_req(&alice(), &bob())
    };
    assert_eq!(ch.create(req("nobody", None), T0), Err(MatchError::UserUnavailable));
    let offline = TargetUser { online: false, ..online(&bob()) };
    assert_eq!(ch.create(req("Bob", Some(offline)), T0), Err(MatchError::UserUnavailable));
    let refusing = TargetUser { accept_challenges: false, ..online(&bob()) };
    assert_eq!(ch.create(req("Bob", Some(refusing)), T0), Err(MatchError::UserUnavailable));
    let invalid = TargetUser { user_id: 0, ..online(&bob()) };
    assert_eq!(ch.create(req("Bob", Some(invalid)), T0), Err(MatchError::UserUnavailable));
    assert_eq!(ch.create(req("alice", None), T0), Err(MatchError::CannotChallengeSelf));
    assert_eq!(ch.create(req(" ALICE\u{a0}", None), T0), Err(MatchError::CannotChallengeSelf));
    assert_eq!(ch.create(req("Alice2", Some(online(&alice()))), T0), Err(MatchError::CannotChallengeSelf));
    assert_eq!(ch.len(), 0);
    // The target name comes from the account; a blank target is a private game.
    let c = ch.create(req("  bob ", Some(online(&bob()))), T0).unwrap();
    assert_eq!(c.target, "Bob");
    let unnamed = TargetUser { username: String::new(), ..online(&carol()) };
    assert_eq!(ch.create(req(" carol", Some(unnamed)), T0).unwrap().target, "carol");
    assert_eq!(ch.create(req(" \t", Some(online(&bob()))), T0).unwrap().kind, ChallengeKind::Private);
}

#[test]
fn at_most_3_pending_outgoing_one_per_target() {
    let mut ch = make();
    let (dave, erin) = (named(4, "Dave"), named(5, "Erin"));
    assert!(direct(&mut ch, &alice(), &bob()).is_ok());
    assert_eq!(direct(&mut ch, &alice(), &bob()), Err(MatchError::ChallengeLimit)); // same target
    assert!(direct(&mut ch, &alice(), &carol()).is_ok());
    let private = ch.create(private_req(&alice(), 300, 0, false, ColorPref::White), T0).unwrap();
    assert_eq!(MAX_PENDING_OUTGOING, 3);
    assert_eq!(direct(&mut ch, &alice(), &dave), Err(MatchError::ChallengeLimit));
    assert_eq!(
        ch.create(private_req(&alice(), 300, 0, false, ColorPref::Random), T0),
        Err(MatchError::ChallengeLimit)
    );
    // Others are not limited by Alice's challenges.
    assert!(direct(&mut ch, &bob(), &dave).is_ok());
    // A cancelled one frees a slot.
    assert!(ch.cancel(private.id, alice().user_id, T0).is_ok());
    assert!(direct(&mut ch, &alice(), &dave).is_ok());
    assert_eq!(direct(&mut ch, &alice(), &erin), Err(MatchError::ChallengeLimit));
    // Expired ones do not count, even before expire() ran.
    let later = T0 + config().challenge_ttl_ms;
    assert!(ch.create(direct_req(&alice(), &erin), later).is_ok());
}

#[test]
fn accept_by_the_target_returns_the_game_spec_others_are_refused() {
    let mut ch = make();
    let c = ch.create(CreateRequest { color: ColorPref::Black, ..direct_req(&alice(), &bob()) }, T0).unwrap();
    assert_eq!(ch.accept(c.id, carol(), T0), Err(MatchError::ChallengeNotFound));
    assert_eq!(ch.accept(c.id, alice(), T0), Err(MatchError::CannotChallengeSelf));
    assert_eq!(ch.accept(c.id + 1000, bob(), T0), Err(MatchError::ChallengeNotFound));
    let r = ch.accept(c.id, bob(), T0).unwrap();
    assert_eq!(r.challenge.state, ChallengeState::Accepted);
    assert_eq!(r.challenge.id, c.id);
    assert_eq!(
        r.game,
        GameSpec {
            white: bob(),
            black: alice(),
            base_ms: 180000,
            inc_ms: 2000,
            category: "3+2".into(),
            rated: true,
            challenge_id: c.id,
            rematch_of: 0,
        }
    );
    // Gone once accepted.
    assert_eq!(ch.accept(c.id, bob(), T0), Err(MatchError::ChallengeNotFound));
    assert_eq!(ch.decline(c.id, bob().user_id, T0), Err(MatchError::ChallengeNotFound));
    assert_eq!(ch.len(), 0);
    assert_eq!(ch.for_user(1, T0), UserChallenges::default());
    assert_eq!(ch.for_user(2, T0), UserChallenges::default());

    // White preference; random preference uses the RNG.
    let w = ch.create(CreateRequest { color: ColorPref::White, ..direct_req(&alice(), &bob()) }, T0).unwrap();
    assert_eq!(ch.accept(w.id, bob(), T0).unwrap().game.white.user_id, alice().user_id);
    let mut rnd = make_with(&config(), draws(&[1, 0]));
    let r1 = direct(&mut rnd, &alice(), &bob()).unwrap();
    assert_eq!(rnd.accept(r1.id, bob(), T0).unwrap().game.white.user_id, bob().user_id); // draw 1: creator Black
    let r2 = direct(&mut rnd, &alice(), &bob()).unwrap();
    assert_eq!(rnd.accept(r2.id, bob(), T0).unwrap().game.white.user_id, alice().user_id); // draw 0: creator White
}

#[test]
fn decline_and_cancel() {
    let mut ch = make();
    let c = direct(&mut ch, &alice(), &bob()).unwrap();
    assert_eq!(ch.decline(c.id, carol().user_id, T0), Err(MatchError::ChallengeNotFound));
    assert_eq!(ch.decline(c.id, alice().user_id, T0), Err(MatchError::ChallengeNotFound));
    let d = ch.decline(c.id, bob().user_id, T0).unwrap();
    assert_eq!(d.state, ChallengeState::Declined);
    assert_eq!(ch.accept(c.id, bob(), T0), Err(MatchError::ChallengeNotFound));

    let c2 = direct(&mut ch, &alice(), &bob()).unwrap();
    assert_eq!(ch.cancel(c2.id, bob().user_id, T0), Err(MatchError::ChallengeNotFound));
    let x = ch.cancel(c2.id, alice().user_id, T0).unwrap();
    assert_eq!(x.state, ChallengeState::Cancelled);
    assert_eq!(ch.cancel(c2.id, alice().user_id, T0), Err(MatchError::ChallengeNotFound));
    assert_eq!(ch.len(), 0);
}

#[test]
fn expiry() {
    let mut ch = make();
    let private_ttl = config().private_game_ttl_ms;
    let c1 = direct(&mut ch, &alice(), &bob()).unwrap(); // expires at 61000
    let c2 = ch.create(direct_req(&carol(), &bob()), 2000).unwrap(); // expires at 62000
    let p = ch.create(private_req(&bob(), 180, 2, true, ColorPref::Random), 2000).unwrap(); // 902000
    assert!(ch.expire(60999).is_empty());
    // Refused as soon as the time is over, before expire() runs.
    assert_eq!(ch.accept(c1.id, bob(), 61000), Err(MatchError::ChallengeNotFound));
    assert_eq!(ids(&ch.for_user(2, 61000).incoming), [c2.id]);
    let out = ch.expire(61000);
    assert_eq!(out.iter().map(|c| c.id).collect::<Vec<_>>(), [c1.id]);
    assert_eq!(out[0].state, ChallengeState::Expired);
    assert!(ch.expire(61500).is_empty());
    let out = ch.expire(100000);
    assert_eq!(out.iter().map(|c| c.id).collect::<Vec<_>>(), [c2.id]);
    assert!(ch.join_code(&p.code, alice(), 901999).is_ok()); // created at 2000: 15 min
    // A private code: 15 minutes.
    let q = ch.create(private_req(&bob(), 180, 2, true, ColorPref::Random), 100000).unwrap();
    assert_eq!(q.expires_at, 100000 + private_ttl);
    assert_eq!(ch.get_code(&q.code, q.expires_at - 1), Some(&q));
    assert_eq!(ch.get_code(&q.code, q.expires_at), None);
    assert_eq!(ch.join_code(&q.code, alice(), q.expires_at), Err(MatchError::CodeInvalid));
    let out = ch.expire(q.expires_at);
    assert_eq!(out.iter().map(|c| c.id).collect::<Vec<_>>(), [q.id]);
    assert_eq!(ch.len(), 0);
    // Cancelled challenges are skipped by expire().
    let c3 = direct(&mut ch, &alice(), &bob()).unwrap();
    ch.cancel(c3.id, alice().user_id, T0).unwrap();
    assert!(ch.expire(1_000_000_000).is_empty());
}

#[test]
fn expire_skips_the_stale_entries_of_reused_ids() {
    let mut ch = make();
    let p = ch.create(private_req(&alice(), 60, 0, false, ColorPref::Random), 0).unwrap();
    let d = ch.create(direct_req(&bob(), &carol()), 900_000).unwrap();
    // The id of a cancelled challenge is reused after a wrap-around: its stale expiry entry
    // must not expire the new challenge early.
    ch.cancel(d.id, bob().user_id, 900_000).unwrap();
    ch.last_id = d.id - 1;
    let reused = ch.create(direct_req(&carol(), &bob()), 2_000_000).unwrap();
    assert_eq!(reused.id, d.id);
    let out: Vec<u32> = ch.expire(1_000_000).iter().map(|c| c.id).collect();
    assert_eq!(out, [p.id]);
    assert!(ch.get(reused.id, 1_000_000).is_some());
    let out: Vec<_> = ch.expire(i64::MAX).iter().map(|c| (c.id, c.kind)).collect();
    assert_eq!(out, [(reused.id, ChallengeKind::Direct)]);
}

#[test]
fn private_games_with_a_code() {
    let mut ch = make();
    let c = ch.create(private_req(&alice(), 300, 3, true, ColorPref::Black), T0).unwrap();
    assert_eq!(c.kind, ChallengeKind::Private);
    assert_eq!(c.target, "");
    assert_eq!(c.target_user_id, 0);
    assert_eq!(c.code.len(), CODE_LENGTH);
    assert!(c.code.chars().all(|x| CODE_ALPHABET.contains(x)));
    assert!(!CODE_ALPHABET.chars().any(|x| "01OIL".contains(x)));
    assert_eq!(c.category, "5+3");
    assert!(ch.for_user(2, T0).incoming.is_empty());
    // Not by id, not by its creator, not with a wrong code.
    assert_eq!(ch.accept(c.id, bob(), T0), Err(MatchError::ChallengeNotFound));
    assert_eq!(ch.join_code(&c.code, alice(), T0), Err(MatchError::CannotChallengeSelf));
    let wrong = if c.code == "ZZZZZZ" { "YYYYYY" } else { "ZZZZZZ" };
    assert_eq!(ch.join_code(wrong, bob(), T0), Err(MatchError::CodeInvalid));
    assert_eq!(ch.join_code("0O1IL0", bob(), T0), Err(MatchError::CodeInvalid));
    assert_eq!(ch.join_code("", bob(), T0), Err(MatchError::CodeInvalid));
    // Case, spaces and dashes are ignored; get_code() finds the game and leaves the code usable.
    let typed = format!(" {}-{} ", c.code[..3].to_lowercase(), &c.code[3..]);
    assert_eq!(ch.get_code(&typed, T0), Some(&c));
    assert_eq!(ch.get_code("0O1IL0", T0), None);
    assert_eq!(ch.get_code("", T0), None);
    let j = ch.join_code(&typed, bob(), T0).unwrap();
    assert_eq!(j.game.black.user_id, alice().user_id);
    assert_eq!(j.game.white.user_id, bob().user_id);
    assert!(j.game.rated);
    assert_eq!(ch.get_code(&c.code, T0), None);
    assert_eq!(ch.join_code(&c.code, carol(), T0), Err(MatchError::CodeInvalid));
    // The creator may cancel a private game by id.
    let c2 = ch.create(private_req(&alice(), 300, 3, false, ColorPref::Random), T0).unwrap();
    assert!(ch.cancel(c2.id, alice().user_id, T0).is_ok());
    assert_eq!(ch.join_code(&c2.code, bob(), T0), Err(MatchError::CodeInvalid));

    assert_eq!(normalize_code("ab-c d23").as_deref(), Some("ABCD23"));
    assert_eq!(normalize_code("\u{feff}ab\u{3000}cd\t23").as_deref(), Some("ABCD23"));
    assert_eq!(normalize_code("ABCDE"), None);
    assert_eq!(normalize_code("ABCDEFG"), None);
    assert_eq!(normalize_code("ABCDÉ2"), None);
}

#[test]
fn codes_are_collision_free() {
    // The RNG repeats the same code first: the second game gets another one.
    let same = [0, 1, 2, 3, 4, 5];
    let list: Vec<u32> = [&same[..], &same[..], &same[..], &[6, 7, 8, 9, 10, 11][..]].concat();
    let mut ch = make_with(&config(), draws(&list));
    let a = ch.create(private_req(&alice(), 60, 0, false, ColorPref::Random), T0).unwrap();
    let b = ch.create(private_req(&bob(), 60, 0, false, ColorPref::Random), T0).unwrap();
    assert_eq!(a.code, "234567");
    assert_eq!(b.code, "89ABCD");
    assert_ne!(a.id, b.id);
    // Many real codes: all distinct, all valid.
    let mut big = make();
    let mut codes = HashSet::new();
    for i in 0..3000 {
        let from = named(100 + i, &format!("u{i}"));
        let c = big.create(private_req(&from, 60, 0, false, ColorPref::Random), T0).unwrap();
        assert_eq!(normalize_code(&c.code).as_deref(), Some(c.code.as_str()));
        codes.insert(c.code);
    }
    assert_eq!(codes.len(), 3000);
}

#[test]
fn crypto_random_int_stays_in_range_and_covers_it() {
    let mut seen = [0u32; 31];
    for _ in 0..31 * 200 {
        seen[crypto_random_int(31) as usize] += 1;
    }
    assert!(seen.iter().all(|&n| n > 0), "{seen:?}");
    assert_eq!(crypto_random_int(1), 0);
    assert!(crypto_random_int(u32::MAX) < u32::MAX);
}

#[test]
fn drop_user_and_ids() {
    let mut ch = make();
    let out1 = direct(&mut ch, &alice(), &bob()).unwrap();
    let in1 = direct(&mut ch, &carol(), &alice()).unwrap();
    let other = direct(&mut ch, &carol(), &bob()).unwrap();
    let dropped: Vec<_> = ch.drop_user(alice().user_id).iter().map(|c| (c.id, c.state)).collect();
    assert_eq!(dropped, [(out1.id, ChallengeState::Cancelled), (in1.id, ChallengeState::Unavailable)]);
    assert_eq!(ch.len(), 1);
    assert_eq!(ch.get(other.id, T0), Some(&other));
    assert!(ch.drop_user(99).is_empty());
    // Ids wrap around within u32 and skip live ones.
    ch.last_id = 0xFFFF_FFFE;
    let a = direct(&mut ch, &alice(), &bob()).unwrap();
    let b = direct(&mut ch, &alice(), &carol()).unwrap();
    assert_eq!(a.id, 0xFFFF_FFFF);
    assert_eq!(b.id, 1);
    assert_eq!(other.id, 3);
    let c = direct(&mut ch, &bob(), &alice()).unwrap();
    assert_eq!(c.id, 2);
    let d = ch.create(direct_req(&bob(), &named(9, "Ivan")), T0).unwrap();
    assert_eq!(d.id, 4, "id 3 is still live");
}
