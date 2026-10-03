import { test } from 'node:test';
import assert from 'node:assert/strict';
import { testConfig } from '../../src/config.js';
import { enums } from '../../src/protocol/schema.js';
import {
    Challenges, normalizeCode, CODE_ALPHABET, CODE_LENGTH, MAX_PENDING_OUTGOING,
} from '../../src/match/challenges.js';

const { ErrorCode, ChallengeState, ColorPref } = enums;
const cfg = testConfig();   // CHALLENGE_TTL_MS 60 s, PRIVATE_GAME_TTL_MS 15 min, custom time controls allowed

function make(opts = {}) {
    let t = 1000;
    const draws = opts.draws ? [...opts.draws] : null;
    const ch = new Challenges({
        config: opts.config || cfg, now: () => t,
        randomInt: draws ? (max) => (draws.length ? draws.shift() % max : 0) : undefined,
    });
    ch.at = (v) => { t = v; return ch; };
    return ch;
}

const alice = { userId: 1, username: 'Alice', rating: 1500, provisional: false, shard: 0, connId: 11 };
const bob = { userId: 2, username: 'Bob', rating: 1600, provisional: true, shard: 1, connId: 22 };
const carol = { userId: 3, username: 'Carol', rating: 1400, provisional: false, shard: 2, connId: 33 };
const online = (p) => ({ userId: p.userId, username: p.username, acceptChallenges: true, online: true });

function direct(ch, from, to, extra = {}) {
    return ch.create({ from, target: to.username, targetUser: online(to), baseSec: 180, incSec: 2, rated: true, color: ColorPref.Random, ...extra });
}

test('challenges: direct challenge fields and colour offered to the receiver', () => {
    const ch = make();
    const r = direct(ch, alice, bob, { color: ColorPref.White });
    assert.equal(r.ok, true);
    const c = r.challenge;
    assert.equal(c.kind, 'direct');
    assert.equal(c.target, 'Bob');
    assert.equal(c.targetUserId, 2);
    assert.equal(c.code, '');
    assert.equal(c.category, '3+2');
    assert.equal(c.baseMs, 180000);
    assert.equal(c.incMs, 2000);
    assert.equal(c.rated, true);
    assert.equal(c.receiverColor, ColorPref.Black);
    assert.equal(c.state, ChallengeState.Pending);
    assert.equal(c.expiresAt, 1000 + cfg.challengeTtlMs);
    assert.ok(Number.isInteger(c.id) && c.id > 0);
    assert.equal(direct(ch, alice, carol, { color: ColorPref.Black }).challenge.receiverColor, ColorPref.White);
    assert.equal(direct(ch, bob, carol).challenge.receiverColor, ColorPref.Random);
    assert.deepEqual(ch.forUser(2).incoming.map((x) => x.id), [c.id]);
    assert.deepEqual(ch.forUser(2).outgoing.length, 1);
    assert.equal(ch.get(c.id), c);
});

test('challenges: time controls, rated and custom rules', () => {
    const ch = make();
    const base = { from: alice, target: '', color: ColorPref.Random };
    assert.deepEqual(ch.create({ ...base, baseSec: 10, incSec: 0, rated: false }), { error: ErrorCode.InvalidTimeControl });
    assert.deepEqual(ch.create({ ...base, baseSec: 10801, incSec: 0, rated: false }), { error: ErrorCode.InvalidTimeControl });
    assert.deepEqual(ch.create({ ...base, baseSec: 60, incSec: 181, rated: false }), { error: ErrorCode.InvalidTimeControl });
    assert.deepEqual(ch.create({ ...base, baseSec: 60.5, incSec: 0, rated: false }), { error: ErrorCode.InvalidTimeControl });
    // 4+1 is not official: never rated, casual allowed.
    assert.deepEqual(ch.create({ ...base, baseSec: 240, incSec: 1, rated: true }), { error: ErrorCode.RatedRequiresOfficialTc });
    const custom = ch.create({ ...base, baseSec: 240, incSec: 1, rated: false });
    assert.equal(custom.ok, true);
    assert.equal(custom.challenge.category, 'custom');
    // Without ALLOW_CUSTOM_TIME_CONTROLS only official time controls exist.
    const strict = make({ config: testConfig({ ALLOW_CUSTOM_TIME_CONTROLS: 'false' }) });
    assert.deepEqual(strict.create({ ...base, baseSec: 240, incSec: 1, rated: false }), { error: ErrorCode.InvalidTimeControl });
    assert.deepEqual(strict.create({ ...base, baseSec: 240, incSec: 1, rated: true }), { error: ErrorCode.RatedRequiresOfficialTc });
    const official = strict.create({ ...base, baseSec: 600, incSec: 5, rated: false });
    assert.equal(official.ok, true);
    assert.equal(official.challenge.category, '10+5');
});

test('challenges: target checks and self-challenge', () => {
    const ch = make();
    const req = { from: alice, baseSec: 180, incSec: 2, rated: false, color: ColorPref.Random };
    assert.deepEqual(ch.create({ ...req, target: 'nobody', targetUser: null }), { error: ErrorCode.UserUnavailable });
    assert.deepEqual(ch.create({ ...req, target: 'Bob', targetUser: { ...online(bob), online: false } }), { error: ErrorCode.UserUnavailable });
    assert.deepEqual(ch.create({ ...req, target: 'Bob', targetUser: { ...online(bob), acceptChallenges: false } }), { error: ErrorCode.UserUnavailable });
    assert.deepEqual(ch.create({ ...req, target: 'alice', targetUser: null }), { error: ErrorCode.CannotChallengeSelf });
    assert.deepEqual(ch.create({ ...req, target: 'Alice2', targetUser: online(alice) }), { error: ErrorCode.CannotChallengeSelf });
    assert.throws(() => ch.create({ ...req, from: {}, target: '' }), TypeError);
    assert.equal(ch.size, 0);
});

test('challenges: at most 3 pending outgoing, one per target', () => {
    const ch = make();
    const dave = { userId: 4, username: 'Dave' };
    const erin = { userId: 5, username: 'Erin' };
    assert.equal(direct(ch, alice, bob).ok, true);
    assert.deepEqual(direct(ch, alice, bob), { error: ErrorCode.ChallengeLimit });   // same target
    assert.equal(direct(ch, alice, carol).ok, true);
    const priv = ch.create({ from: alice, target: '', baseSec: 300, incSec: 0, rated: false, color: ColorPref.White });
    assert.equal(priv.ok, true);
    assert.equal(MAX_PENDING_OUTGOING, 3);
    assert.deepEqual(direct(ch, alice, dave), { error: ErrorCode.ChallengeLimit });
    assert.deepEqual(ch.create({ from: alice, target: '', baseSec: 300, incSec: 0, rated: false, color: 0 }), { error: ErrorCode.ChallengeLimit });
    // Others are not limited by Alice's challenges.
    assert.equal(direct(ch, bob, dave).ok, true);
    // A cancelled one frees a slot.
    assert.equal(ch.cancel(priv.challenge.id, alice.userId).ok, true);
    assert.equal(direct(ch, alice, dave).ok, true);
    assert.deepEqual(direct(ch, alice, erin), { error: ErrorCode.ChallengeLimit });
    // Expired ones do not count, even before expire() ran.
    ch.at(1000 + cfg.challengeTtlMs);
    assert.equal(direct(ch, alice, erin).ok, true);
});

test('challenges: accept by the target returns the game spec; others are refused', () => {
    const ch = make();
    const c = direct(ch, alice, bob, { color: ColorPref.Black }).challenge;
    assert.deepEqual(ch.accept(c.id, carol), { error: ErrorCode.ChallengeNotFound });
    assert.deepEqual(ch.accept(c.id, alice), { error: ErrorCode.CannotChallengeSelf });
    assert.deepEqual(ch.accept(c.id + 1000, bob), { error: ErrorCode.ChallengeNotFound });
    const r = ch.accept(c.id, bob);
    assert.equal(r.ok, true);
    assert.equal(r.challenge.state, ChallengeState.Accepted);
    assert.deepEqual(r.game, {
        white: bob, black: alice, baseMs: 180000, incMs: 2000, category: '3+2', rated: true,
        challengeId: c.id, rematchOf: 0,
    });
    // Gone once accepted.
    assert.deepEqual(ch.accept(c.id, bob), { error: ErrorCode.ChallengeNotFound });
    assert.deepEqual(ch.decline(c.id, bob.userId), { error: ErrorCode.ChallengeNotFound });
    assert.equal(ch.size, 0);
    assert.deepEqual(ch.forUser(1), { outgoing: [], incoming: [] });
    assert.deepEqual(ch.forUser(2), { outgoing: [], incoming: [] });

    // White preference; random preference uses the RNG.
    const w = direct(ch, alice, bob, { color: ColorPref.White }).challenge;
    assert.equal(ch.accept(w.id, bob).game.white.userId, alice.userId);
    const rnd = make({ draws: [1, 0] });
    const r1 = direct(rnd, alice, bob).challenge;
    assert.equal(rnd.accept(r1.id, bob).game.white.userId, bob.userId);     // draw 1: creator Black
    const r2 = direct(rnd, alice, bob).challenge;
    assert.equal(rnd.accept(r2.id, bob).game.white.userId, alice.userId);   // draw 0: creator White
});

test('challenges: decline and cancel', () => {
    const ch = make();
    const c = direct(ch, alice, bob).challenge;
    assert.deepEqual(ch.decline(c.id, carol.userId), { error: ErrorCode.ChallengeNotFound });
    assert.deepEqual(ch.decline(c.id, alice.userId), { error: ErrorCode.ChallengeNotFound });
    const d = ch.decline(c.id, bob.userId);
    assert.equal(d.ok, true);
    assert.equal(d.challenge.state, ChallengeState.Declined);
    assert.deepEqual(ch.accept(c.id, bob), { error: ErrorCode.ChallengeNotFound });

    const c2 = direct(ch, alice, bob).challenge;
    assert.deepEqual(ch.cancel(c2.id, bob.userId), { error: ErrorCode.ChallengeNotFound });
    const x = ch.cancel(c2.id, alice.userId);
    assert.equal(x.ok, true);
    assert.equal(x.challenge.state, ChallengeState.Cancelled);
    assert.deepEqual(ch.cancel(c2.id, alice.userId), { error: ErrorCode.ChallengeNotFound });
    assert.equal(ch.size, 0);
});

test('challenges: expiry', () => {
    const ch = make();
    const c1 = direct(ch, alice, bob).challenge;                         // expires at 61000
    ch.at(2000);
    const c2 = direct(ch, carol, bob).challenge;                         // expires at 62000
    const p = ch.create({ from: bob, target: '', baseSec: 180, incSec: 2, rated: true, color: 0 }).challenge;  // 902000
    assert.deepEqual(ch.expire(60999), []);
    // Refused as soon as the time is over, before expire() runs.
    ch.at(61000);
    assert.deepEqual(ch.accept(c1.id, bob), { error: ErrorCode.ChallengeNotFound });
    assert.deepEqual(ch.forUser(2).incoming.map((c) => c.id), [c2.id]);
    let out = ch.expire(61000);
    assert.deepEqual(out.map((c) => c.id), [c1.id]);
    assert.equal(out[0].state, ChallengeState.Expired);
    assert.deepEqual(ch.expire(61500), []);
    out = ch.expire(100000);
    assert.deepEqual(out.map((c) => c.id), [c2.id]);
    assert.equal(ch.joinCode(p.code, alice, 901999).ok, true);                  // created at 2000: 15 min
    // A private code: 15 minutes.
    const q = ch.create({ from: bob, target: '', baseSec: 180, incSec: 2, rated: true, color: 0 }, 100000).challenge;
    assert.equal(q.expiresAt, 100000 + cfg.privateGameTtlMs);
    assert.equal(ch.getCode(q.code, q.expiresAt - 1), q);
    assert.equal(ch.getCode(q.code, q.expiresAt), null);
    assert.deepEqual(ch.joinCode(q.code, alice, q.expiresAt), { error: ErrorCode.CodeInvalid });
    out = ch.expire(q.expiresAt);
    assert.deepEqual(out.map((c) => c.id), [q.id]);
    assert.equal(ch.size, 0);
    // Cancelled challenges are skipped by expire().
    const c3 = direct(ch, alice, bob, {}).challenge;
    ch.cancel(c3.id, alice.userId);
    assert.deepEqual(ch.expire(10 ** 9), []);
});

test('challenges: private games with a code', () => {
    const ch = make();
    const r = ch.create({ from: alice, target: '', baseSec: 300, incSec: 3, rated: true, color: ColorPref.Black });
    assert.equal(r.ok, true);
    const c = r.challenge;
    assert.equal(c.kind, 'private');
    assert.equal(c.target, '');
    assert.equal(c.code.length, CODE_LENGTH);
    assert.ok([...c.code].every((x) => CODE_ALPHABET.includes(x)));
    assert.ok(!/[01OIL]/.test(CODE_ALPHABET));
    assert.equal(c.category, '5+3');
    assert.deepEqual(ch.forUser(2).incoming, []);
    // Not by id, not by its creator, not with a wrong code.
    assert.deepEqual(ch.accept(c.id, bob), { error: ErrorCode.ChallengeNotFound });
    assert.deepEqual(ch.joinCode(c.code, alice), { error: ErrorCode.CannotChallengeSelf });
    assert.deepEqual(ch.joinCode('ZZZZZZ' === c.code ? 'YYYYYY' : 'ZZZZZZ', bob), { error: ErrorCode.CodeInvalid });
    assert.deepEqual(ch.joinCode('0O1IL0', bob), { error: ErrorCode.CodeInvalid });
    assert.deepEqual(ch.joinCode(undefined, bob), { error: ErrorCode.CodeInvalid });
    // Case, spaces and dashes are ignored; getCode() finds the game and leaves the code usable.
    const typed = ` ${c.code.slice(0, 3).toLowerCase()}-${c.code.slice(3)} `;
    assert.equal(ch.getCode(typed), c);
    assert.equal(ch.getCode('0O1IL0'), null);
    assert.equal(ch.getCode(undefined), null);
    const j = ch.joinCode(typed, bob);
    assert.equal(j.ok, true);
    assert.equal(j.game.black.userId, alice.userId);
    assert.equal(j.game.white.userId, bob.userId);
    assert.equal(j.game.rated, true);
    assert.equal(ch.getCode(c.code), null);
    assert.deepEqual(ch.joinCode(c.code, carol), { error: ErrorCode.CodeInvalid });
    // The creator may cancel a private game by id.
    const c2 = ch.create({ from: alice, target: '', baseSec: 300, incSec: 3, rated: false, color: 0 }).challenge;
    assert.equal(ch.cancel(c2.id, alice.userId).ok, true);
    assert.deepEqual(ch.joinCode(c2.code, bob), { error: ErrorCode.CodeInvalid });

    assert.equal(normalizeCode('ab-c d23'), 'ABCD23');
    assert.equal(normalizeCode('ABCDE'), null);
    assert.equal(normalizeCode('ABCDEFG'), null);
});

test('challenges: codes are collision-free', () => {
    // The RNG repeats the same code first: the second game gets another one.
    const same = [0, 1, 2, 3, 4, 5];
    const ch = make({ draws: [...same, ...same, ...same, 6, 7, 8, 9, 10, 11] });
    const a = ch.create({ from: alice, target: '', baseSec: 60, incSec: 0, rated: false, color: 0 }).challenge;
    const b = ch.create({ from: bob, target: '', baseSec: 60, incSec: 0, rated: false, color: 0 }).challenge;
    assert.equal(a.code, '234567');
    assert.equal(b.code, '89ABCD');
    assert.notEqual(a.id, b.id);
    // Many real codes: all distinct, all valid.
    const big = make();
    const codes = new Set();
    for (let i = 0; i < 3000; i++) {
        const c = big.create({ from: { userId: 100 + i, username: 'u' + i }, target: '', baseSec: 60, incSec: 0, rated: false, color: 0 }).challenge;
        assert.equal(normalizeCode(c.code), c.code);
        codes.add(c.code);
    }
    assert.equal(codes.size, 3000);
});

test('challenges: dropUser and ids', () => {
    const ch = make();
    const out1 = direct(ch, alice, bob).challenge;
    const in1 = direct(ch, carol, alice).challenge;
    const other = direct(ch, carol, bob).challenge;
    const dropped = ch.dropUser(alice.userId);
    assert.deepEqual(dropped.map((c) => [c.id, c.state]).sort(), [[out1.id, ChallengeState.Cancelled], [in1.id, ChallengeState.Unavailable]].sort());
    assert.equal(ch.size, 1);
    assert.equal(ch.get(other.id), other);
    assert.deepEqual(ch.dropUser(99), []);
    // Ids wrap around within u32 and skip live ones.
    ch.lastId = 0xFFFFFFFE;
    const a = direct(ch, alice, bob).challenge;
    const b = direct(ch, alice, carol).challenge;
    assert.equal(a.id, 0xFFFFFFFF);
    assert.equal(b.id, 1);
});
