// Rating refunds of the victims of a banned cheater (src/anticheat/refunds.js, store.refunds,
// migration 004), with the real store and the real FIDE rating function: the automatic ban of a
// certain cheat, a moderator's integrity confirm (--refund-since, --no-refund) and the refunds
// commands.

import test from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { DatabaseSync } from 'node:sqlite';
import { createAnticheat } from '../../src/anticheat/index.js';
import { runAdmin } from '../../src/anticheat/admin.js';
import { refundWindowStart } from '../../src/anticheat/refunds.js';
import { testConfig } from '../../src/config.js';
import { applyGame } from '../../src/match/elo.js';
import { enums } from '../../src/protocol/schema.js';
import { openStore, migrate } from '../../src/store/index.js';

const { GameStatus, EndReason } = enums;
const DAY = 86400000;
const NOW = Date.UTC(2026, 8, 1, 12);
const quiet = { debug() {}, info() {}, warn() {}, error() {}, security() {}, child() { return this; } };

let nextId = 7_000_000_000_000;
function game(white, black, status, endedAt, extra = {}) {
    const plies = 40;
    return {
        id: ++nextId, category: '3+2', rated: true, baseMs: 180000, incMs: 2000, whiteId: white, blackId: black, whiteName: 'W',
        blackName: 'B', startedAt: endedAt - 600000, endedAt, status, reason: status === GameStatus.Draw ? EndReason.Agreement : EndReason.Resignation,
        moves: new Uint16Array(plies), spentMs: new Uint32Array(plies), clockMs: new Uint32Array(plies), ...extra,
    };
}
const W = GameStatus.WhiteWins, B = GameStatus.BlackWins, D = GameStatus.Draw;

// Players with rated records (40 games, K 20) seeded in the file, and one newcomer (Nova).
function world(overrides = {}, t, log = undefined) {
    const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'scacelith-refunds-'));
    t.after(() => fs.rmSync(dir, { recursive: true, force: true }));
    const config = testConfig({ DB_PATH: path.join(dir, 'db.sqlite'), DATA_DIR: dir, ...overrides });
    const store = openStore(config, { applyGame, log });
    migrate(store);
    t.after(() => store.close());
    const id = {};
    for (const n of ['Cheat', 'Vic', 'Val', 'Vera', 'Vold', 'Omar', 'Nova']) id[n] = store.users.create({ username: n, email: `${n}@example.org` });
    const raw = new DatabaseSync(config.dbPath);
    const seed = raw.prepare(`INSERT INTO ratings (user_id, category, rating, games, wins, losses, peak, rated, updated_at)
        VALUES (?, '3+2', ?, 40, 20, 20, ?, 1, 0)`);
    for (const [n, r] of [['Cheat', 1500], ['Vic', 1500], ['Val', 1700], ['Vera', 1500], ['Vold', 1500], ['Omar', 1500]]) seed.run(id[n], r, r);
    raw.close();
    return { config, store, id };
}

// The games of the scenario, in the order they are committed; returns the games by name.
function play(store, id) {
    const g = {
        old: game(id.Cheat, id.Vold, W, NOW - 70 * DAY),              // outside the 60-day window
        vicLoss: game(id.Cheat, id.Vic, W, NOW - 20 * DAY),           // refunded
        casual: game(id.Vic, id.Cheat, B, NOW - 19 * DAY, { rated: false }),   // no rating change
        valDraw: game(id.Val, id.Cheat, D, NOW - 18 * DAY),           // the higher-rated Val loses points: refunded
        veraWin: game(id.Vera, id.Cheat, W, NOW - 17 * DAY),          // a win: untouched
        vicOmar: game(id.Omar, id.Vic, W, NOW - 16 * DAY),            // Vic loses to someone else: untouched
    };
    const batch = [g.old, g.vicLoss, g.casual, g.valDraw, g.veraWin, g.vicOmar];
    // Nova's unrated phase: a draw and three losses against Omar, then a fifth game lost to the
    // cheater, which gives Nova a first rating below the working rating (no K-formula loss: not
    // refunded).
    for (let i = 0; i < 4; i++) batch.push(game(id.Omar, id.Nova, i === 0 ? D : W, NOW - (15 - i) * DAY));
    g.novaFirst = game(id.Cheat, id.Nova, W, NOW - 10 * DAY);
    batch.push(g.novaFirst);
    g.vicAfter = game(id.Vic, id.Omar, W, NOW - 5 * DAY);            // Vic's rating moved on since
    batch.push(g.vicAfter);
    store.games.finishBatch(batch);
    return g;
}

const lost = (store, g, side) => {
    const c = store.games.byId(g.id).ratingChanges[side];
    return c.before - c.after;
};
const rating = (store, userId) => store.ratings.get(userId, '3+2');

test('an automatic ban gives back what each victim lost to the cheater in the window, on their current rating', (t) => {
    const { config, store, id } = world({}, t);
    const g = play(store, id);
    const vicLost = lost(store, g.vicLoss, 'black');
    const valLost = lost(store, g.valDraw, 'white');
    assert.equal(vicLost, 10, 'equal ratings, K 20: 20 x 0.5');
    assert.ok(valLost > 0, 'a draw against a lower-rated player costs points');
    assert.ok(lost(store, g.novaFirst, 'black') > 0, "Nova's first rating is below the working rating");
    assert.ok(lost(store, g.old, 'black') > 0);
    const before = Object.fromEntries(Object.entries(id).map(([n, u]) => [n, rating(store, u)]));

    const sent = [];
    const primary = { request: async (type, payload) => { sent.push({ type, payload }); return {}; } };
    const ac = createAnticheat({ config, store, primary, log: quiet, now: () => NOW });
    const r = ac.sanctionCertain({ userId: id.Cheat, gameId: 999, kind: 'illegal_move' });
    assert.equal(r.applied, true);
    assert.equal(r.refunds, 2, 'Vic and Val');

    assert.equal(rating(store, id.Vic).rating, before.Vic.rating + vicLost, 'added to the current rating, nothing recomputed');
    assert.equal(rating(store, id.Val).rating, before.Val.rating + valLost);
    for (const n of ['Vera', 'Vold', 'Omar', 'Nova', 'Cheat']) assert.deepEqual(rating(store, id[n]), before[n], `${n} untouched`);
    // Counts are left alone: the games still happened.
    assert.equal(rating(store, id.Vic).games, before.Vic.games);

    const rows = store.refunds.list({ cheaterId: id.Cheat });
    assert.deepEqual(rows.map((x) => [x.gameId, x.victimName, x.points, x.source, x.category]).sort(),
        [[g.vicLoss.id, 'Vic', vicLost, 'auto', '3+2'], [g.valDraw.id, 'Val', valLost, 'auto', '3+2']].sort());
    const ban = store.sanctions.activeBan(id.Cheat, NOW);
    assert.ok(rows.every((x) => x.sanctionId === ban.id && x.createdAt === NOW && x.notifiedAt === null && x.createdBy === null));
    assert.deepEqual(store.refunds.pendingFor(id.Vic), { ids: [rows.find((x) => x.victimId === id.Vic).id], points: vicLost });
    return new Promise((resolve) => setImmediate(resolve)).then(() => {
        assert.deepEqual(sent, [{ type: 'sanction.applied', payload: { userId: id.Cheat, until: ban.endsAt, reason: 'certain_cheat:illegal_move', refunds: 2 } }]);
        ac.close();
    });
});

test('refunds are given once per game and victim: a second ban and a moderator\'s apply give nothing twice', (t) => {
    const { config, store, id } = world({}, t);
    const g = play(store, id);
    let now = NOW;
    const ac = createAnticheat({ config, store, log: quiet, now: () => now });
    assert.equal(ac.sanctionCertain({ userId: id.Cheat, gameId: 1, kind: 'illegal_move' }).refunds, 2);
    const vic = rating(store, id.Vic).rating;
    // A later cheat in another game, after the first ban expired: a new ban, no new refund.
    now += 2 * DAY;
    const again = ac.sanctionCertain({ userId: id.Cheat, gameId: 2, kind: 'out_of_turn' });
    assert.equal(again.applied, true);
    assert.equal(again.refunds, 0);
    assert.equal(rating(store, id.Vic).rating, vic);
    assert.equal(store.refunds.list().length, 2);
    // The store itself refuses a second refund of a game (UNIQUE (game_id, victim_id)).
    assert.deepEqual(store.refunds.applyForCheater({ cheaterId: id.Cheat, since: 0, now, source: 'moderator', by: 'x' })
        .map((x) => x.gameId), [g.old.id], 'only the game outside the first window is new');
    assert.equal(store.refunds.applyForCheater({ cheaterId: id.Cheat, since: 0, now, source: 'moderator', by: 'x' }).length, 0);
    ac.close();
});

test('the victim\'s peak rises with a refund that takes them above it; an unban takes nothing back', async (t) => {
    const { config, store, id } = world({}, t);
    play(store, id);
    const vic = rating(store, id.Vic);
    assert.equal(vic.peak, 1500, 'the seeded peak');
    const ac = createAnticheat({ config, store, log: quiet, now: () => NOW });
    ac.sanctionCertain({ userId: id.Cheat, gameId: 1, kind: 'illegal_move' });
    const after = rating(store, id.Vic);
    assert.equal(after.peak, Math.max(1500, after.rating));
    assert.ok(after.rating > 1500, `${after.rating}`);
    const out = { write() {} };
    assert.equal(await runAdmin(['user', 'unban', 'Cheat'], { store, config, out, err: out, now: () => NOW + 1000, moderator: 'mod' }), 0);
    assert.equal(store.sanctions.activeBan(id.Cheat, NOW + 1000), null);
    assert.equal(rating(store, id.Vic).rating, after.rating);
    ac.close();
});

test('RATING_REFUND_DAYS=0: no automatic refunds; the window is counted back from the ban', (t) => {
    const { config, store, id } = world({ RATING_REFUND_DAYS: '0' }, t);
    play(store, id);
    const ac = createAnticheat({ config, store, log: quiet, now: () => NOW });
    assert.equal(ac.sanctionCertain({ userId: id.Cheat, gameId: 1, kind: 'illegal_move' }).refunds, 0);
    assert.deepEqual(store.refunds.list(), []);
    assert.equal(refundWindowStart(config, NOW), null);
    assert.equal(refundWindowStart(testConfig(), NOW), NOW - 60 * DAY);
    assert.equal(refundWindowStart(testConfig({ RATING_REFUND_DAYS: '90' }), NOW), NOW - 90 * DAY);
    ac.close();
});

async function admin(store, config, argv, now = NOW) {
    let out = '', err = '';
    const code = await runAdmin(argv, { store, config, out: { write: (x) => { out += x; } }, err: { write: (x) => { err += x; } }, now: () => now, moderator: 'mod-anna' });
    return { code, out, err, json: argv.includes('--json') && code === 0 ? JSON.parse(out) : null };
}

test('moderator: integrity confirm refunds the window, --refund-since widens it, --no-refund skips; refunds apply / list', async (t) => {
    const { config, store, id } = world({}, t);
    const g = play(store, id);
    const vold = rating(store, id.Vold).rating;

    // --no-refund: a ban without refunds; --refund-since and --no-refund exclude each other.
    assert.equal((await admin(store, config, ['integrity', 'confirm', 'Cheat', '--reason', 'r', '--no-refund', '--refund-since', '2026-01-01'])).code, 1);
    assert.equal((await admin(store, config, ['integrity', 'confirm', 'Cheat', '--reason', 'r', '--refund-since', '2099-01-01'])).code, 1);
    assert.equal((await admin(store, config, ['integrity', 'confirm', 'Cheat', '--reason', 'r', '--refund-since', '2026-02-29'])).code, 1, 'no such day');
    assert.equal(store.integrity.get(id.Cheat).level, 'none', 'a refused command writes nothing');
    assert.equal(store.sanctions.activeBan(id.Cheat, NOW), null);
    const noRefund = await admin(store, config, ['integrity', 'confirm', 'Cheat', '--reason', 'engine', '--no-refund', '--json']);
    assert.equal(noRefund.code, 0, noRefund.err);
    assert.deepEqual(noRefund.json.refunds, []);
    assert.equal(noRefund.json.refundSince, null);
    assert.deepEqual(store.refunds.list(), []);

    // refunds apply: a confirmed cheater only; the window counts back from the latest ban for cheating.
    assert.equal((await admin(store, config, ['refunds', 'apply', 'Vic'])).code, 1, 'Vic is not a confirmed cheater');
    assert.equal((await admin(store, config, ['refunds', 'apply', 'Cheat', '--since', '2099-01-01'])).code, 1, 'a date in the future');
    assert.equal((await admin(store, config, ['refunds', 'apply', 'Cheat', '--since', 'yesterday'])).code, 1);
    // An impossible calendar date is refused, not rolled over into the next month.
    for (const typo of ['2026-02-31', '2026-04-31T10:00Z', '2026-00-10', '2026-06-00']) {
        const r = await admin(store, config, ['refunds', 'apply', 'Cheat', '--since', typo]);
        assert.equal(r.code, 1, typo);
        assert.match(r.err, /expects a date/);
    }
    const apply = await admin(store, config, ['refunds', 'apply', 'Cheat', '--json'], NOW + 3 * DAY);
    assert.equal(apply.code, 0, apply.err);
    assert.equal(apply.json.since, NOW - 60 * DAY, 'RATING_REFUND_DAYS before the ban, not before the command');
    assert.deepEqual(apply.json.refunds.map((r) => r.gameId).sort(), [g.vicLoss.id, g.valDraw.id].sort());
    assert.equal(rating(store, id.Vold).rating, vold, 'outside the window');

    // --since reaches the older game; the games already refunded are skipped.
    const since = new Date(NOW - 80 * DAY).toISOString().slice(0, 10);
    const wider = await admin(store, config, ['refunds', 'apply', 'Cheat', '--since', since]);
    assert.equal(wider.code, 0, wider.err);
    assert.match(wider.out, /1 game\(s\), 10 point\(s\) to 1 player\(s\)/);
    assert.equal(rating(store, id.Vold).rating, vold + 10);
    const rows = store.refunds.list();
    assert.equal(rows.length, 3);
    assert.ok(rows.every((r) => r.source === 'moderator' && r.createdBy === 'mod-anna'));

    // Audit: one moderator_action per command, one rating_refund event per refund.
    const audit = new DatabaseSync(config.dbPath, { readOnly: true });
    const kinds = audit.prepare(`SELECT kind, json_extract(detail, '$.action') AS action FROM security_events ORDER BY id`).all().map((r) => r.action || r.kind);
    audit.close();
    assert.deepEqual(kinds, ['integrity_confirm', 'rating_refund', 'rating_refund', 'refunds_apply', 'rating_refund', 'refunds_apply']);

    // refunds list: by cheater, by victim, all.
    const byCheater = await admin(store, config, ['refunds', 'list', 'Cheat', '--json']);
    assert.equal(byCheater.json.length, 3);
    const byVictim = await admin(store, config, ['refunds', 'list', '--victim', 'Vold', '--json']);
    assert.deepEqual(byVictim.json.map((r) => [r.gameId, r.points, r.cheaterName]), [[g.old.id, 10, 'Cheat']]);
    assert.equal((await admin(store, config, ['refunds', 'list', 'Cheat', '--victim', 'Vold'])).code, 1);
    const text = await admin(store, config, ['refunds', 'list']);
    assert.match(text.out, /Vold\s+3\+2\s+10\s+moderator\s+mod-anna\s+not yet/);
});

test('moderator: integrity confirm refunds RATING_REFUND_DAYS back by default, or from --refund-since', async (t) => {
    const { config, store, id } = world({}, t);
    const g = play(store, id);
    const conf = await admin(store, config, ['integrity', 'confirm', 'Cheat', '--reason', 'engine', '--refund-since', new Date(NOW - 80 * DAY).toISOString()]);
    assert.equal(conf.code, 0, conf.err);
    assert.match(conf.out, /3 game\(s\)/);
    const ban = store.sanctions.activeBan(id.Cheat, NOW);
    const rows = store.refunds.list({ cheaterId: id.Cheat });
    assert.deepEqual(rows.map((r) => r.gameId).sort(), [g.old.id, g.vicLoss.id, g.valDraw.id].sort());
    assert.ok(rows.every((r) => r.sanctionId === ban.id && r.source === 'moderator'));

    const { config: c2, store: s2, id: id2 } = world({}, t);
    play(s2, id2);
    const def = await admin(s2, c2, ['integrity', 'confirm', 'Cheat', '--reason', 'engine', '--json']);
    assert.equal(def.code, 0, def.err);
    assert.equal(def.json.refundSince, NOW - 60 * DAY);
    assert.equal(def.json.refunds.length, 2);
});

test('moderator: refunds that fail leave the ban standing and audited, and say how to give them later', async (t) => {
    const { config, store, id } = world({}, t);
    const g = play(store, id);
    const failing = { ...store, refunds: { ...store.refunds, applyForCheater: () => { throw new Error('database is locked'); } } };
    const conf = await admin(failing, config, ['integrity', 'confirm', 'Cheat', '--reason', 'engine']);
    assert.equal(conf.code, 1);
    assert.match(conf.err, /banned until .*, but the rating refunds failed \(database is locked\): give them with `refunds apply Cheat`/);
    assert.ok(store.sanctions.activeBan(id.Cheat, NOW));
    assert.equal(store.integrity.get(id.Cheat).level, 'confirmed');
    const raw = new DatabaseSync(config.dbPath, { readOnly: true });
    const detail = JSON.parse(raw.prepare(`SELECT detail FROM security_events WHERE kind = 'moderator_action'`).get().detail);
    raw.close();
    assert.deepEqual([detail.action, detail.refundError, detail.refunds], ['integrity_confirm', 'database is locked', 0]);
    const later = await admin(store, config, ['refunds', 'apply', 'Cheat', '--json']);
    assert.deepEqual(later.json.refunds.map((r) => r.gameId).sort(), [g.vicLoss.id, g.valDraw.id].sort());
});

test('a game recorded while the opponent is a confirmed cheater under an active ban is refunded as it is recorded', async (t) => {
    const logged = [];
    const { config, store, id } = world({}, t, { ...quiet, security: (event, f) => logged.push([event, f]) });
    // finishBatch records the games at the real time: the moderator confirms now, while games
    // against the cheater are still being played or on their way to the database.
    const now = Date.now();
    const conf = await admin(store, config, ['integrity', 'confirm', 'Cheat', '--reason', 'engine', '--json'], now);
    assert.equal(conf.code, 0, conf.err);
    assert.deepEqual(conf.json.refunds, [], 'no game recorded yet');
    const ban = store.sanctions.activeBan(id.Cheat, now);
    const g = {
        late: game(id.Cheat, id.Vic, W, now - 1000),              // ended before the confirm, recorded after it
        inPlay: game(id.Vold, id.Cheat, B, now + 1000),           // in progress at the confirm
        valDraw: game(id.Val, id.Cheat, D, now + 2000),           // the higher-rated Val loses points
        veraWin: game(id.Vera, id.Cheat, W, now + 3000),          // a win: untouched
        vicOmar: game(id.Omar, id.Vic, W, now + 4000),            // a loss to someone else: untouched
    };
    const res = store.games.finishBatch(Object.values(g));
    const lostIn = (i, side) => res[i].ratings[side].before - res[i].ratings[side].after;
    assert.equal(lostIn(0, 'black'), 10, 'the games keep their rating changes as played');
    assert.ok(lostIn(1, 'white') > 0 && lostIn(2, 'white') > 0);
    assert.equal(rating(store, id.Vic).rating, 1490, 'the loss to the cheater is given back, the loss to Omar stands');
    assert.equal(rating(store, id.Vold).rating, 1500);
    assert.equal(rating(store, id.Val).rating, 1700);
    assert.ok(rating(store, id.Vera).rating > 1500);

    const rows = store.refunds.list({ cheaterId: id.Cheat });
    assert.deepEqual(rows.map((x) => [x.gameId, x.victimName, x.points, x.source, x.sanctionId, x.createdBy]).sort(),
        [[g.late.id, 'Vic', 10, 'auto', ban.id, null], [g.inPlay.id, 'Vold', lostIn(1, 'white'), 'auto', ban.id, null],
            [g.valDraw.id, 'Val', lostIn(2, 'white'), 'auto', ban.id, null]].sort());
    // The victims are told as for any refund (Notice{RatingRestored}, out of a game).
    assert.deepEqual(store.refunds.pendingFor(id.Vic).points, 10);
    const raw = new DatabaseSync(config.dbPath, { readOnly: true });
    const events = raw.prepare(`SELECT user_id AS userId, json_extract(detail, '$.gameId') AS gameId, json_extract(detail, '$.source') AS source,
        json_extract(detail, '$.sanctionId') AS sanctionId FROM security_events WHERE kind = 'rating_refund' ORDER BY id`).all();
    raw.close();
    assert.deepEqual(events.map((e) => ({ ...e })), [
        { userId: id.Vic, gameId: g.late.id, source: 'auto', sanctionId: ban.id },
        { userId: id.Vold, gameId: g.inPlay.id, source: 'auto', sanctionId: ban.id },
        { userId: id.Val, gameId: g.valDraw.id, source: 'auto', sanctionId: ban.id },
    ]);
    assert.deepEqual(logged.map(([event, f]) => [event, f.cheaterId, f.gameId, f.points]), [
        ['rating.refund', id.Cheat, g.late.id, 10], ['rating.refund', id.Cheat, g.inPlay.id, lostIn(1, 'white')],
        ['rating.refund', id.Cheat, g.valDraw.id, lostIn(2, 'white')],
    ]);

    // Nothing twice: a game committed again after a crash, or a moderator's later refunds apply.
    assert.equal(store.games.finishBatch([g.late])[0].duplicate, true);
    const again = await admin(store, config, ['refunds', 'apply', 'Cheat', '--json'], now);
    assert.deepEqual(again.json.refunds, []);
    assert.equal(store.refunds.list().length, 3);
    assert.equal(rating(store, id.Vic).rating, 1490);
});

test('no refund as a game is recorded without both a confirmed level and an active ban, nor with RATING_REFUND_DAYS=0', async (t) => {
    const { config, store, id } = world({}, t);
    const now = Date.now();
    const lossTo = (victim) => store.games.finishBatch([game(id.Cheat, victim, W, now)]);
    // A ban for something else (`user ban`): not a confirmed cheater.
    assert.equal((await admin(store, config, ['user', 'ban', 'Cheat', '--hours', '24', '--reason', 'abuse'], now)).code, 0);
    lossTo(id.Vic);
    // Confirmed, then unbanned: no active ban.
    assert.equal((await admin(store, config, ['integrity', 'confirm', 'Cheat', '--reason', 'engine', '--no-refund'], now)).code, 0);
    assert.equal((await admin(store, config, ['user', 'unban', 'Cheat'], now)).code, 0);
    lossTo(id.Vera);
    assert.deepEqual(store.refunds.list(), []);
    assert.equal(rating(store, id.Vic).rating, 1490);
    assert.ok(rating(store, id.Vera).rating < 1500);

    // RATING_REFUND_DAYS=0 turns these refunds off with the others.
    const off = world({ RATING_REFUND_DAYS: '0' }, t);
    assert.equal((await admin(off.store, off.config, ['integrity', 'confirm', 'Cheat', '--reason', 'engine'], now)).code, 0);
    off.store.games.finishBatch([game(off.id.Cheat, off.id.Vic, W, now)]);
    assert.deepEqual(off.store.refunds.list(), []);
    assert.equal(rating(off.store, off.id.Vic).rating, 1490);
});

test('no refund as a game is recorded under a ban for something else, nor after integrity confirm --no-refund', async (t) => {
    const { config, store, id } = world({}, t);
    const now = Date.now();
    const lossTo = (victim) => store.games.finishBatch([game(id.Cheat, victim, W, now)]);
    // Confirmed with refunds ten days ago: that ban (24 h) is over, the level stays 'confirmed'.
    assert.equal((await admin(store, config, ['integrity', 'confirm', 'Cheat', '--reason', 'engine'], now - 10 * DAY)).code, 0);
    assert.equal(store.sanctions.activeBan(id.Cheat, now), null);
    // A `user ban` cannot pass for a ban for cheating.
    for (const reason of ['confirmed: engine', 'confirmed, no refund: engine']) {
        const refused = await admin(store, config, ['user', 'ban', 'Cheat', '--hours', '24', '--reason', reason], now);
        assert.equal(refused.code, 1, reason);
        assert.match(refused.err, /marks a ban for cheating: use integrity confirm/);
    }
    assert.equal(store.sanctions.activeBan(id.Cheat, now), null, 'a refused command writes nothing');
    // A ban for something else: the game in progress at it, recorded during it, stands.
    assert.equal((await admin(store, config, ['user', 'ban', 'Cheat', '--hours', '24', '--reason', 'abusive chat'], now)).code, 0);
    lossTo(id.Vic);
    // A new confirm with --no-refund: not even the game in progress at it is refunded.
    const conf = await admin(store, config, ['integrity', 'confirm', 'Cheat', '--reason', 'engine again', '--no-refund', '--json'], now);
    assert.equal(conf.code, 0, conf.err);
    assert.equal(store.sanctions.list(id.Cheat).find((s) => s.id === conf.json.sanctionId).reason, 'confirmed, no refund: engine again');
    lossTo(id.Vera);
    assert.deepEqual(store.refunds.list(), []);
    assert.equal(rating(store, id.Vic).rating, 1490);
    assert.ok(rating(store, id.Vera).rating < 1500);
});

test('a certain cheat under a ban that does not refund gets a ban of its own, and the games recorded during it are refunded', async (t) => {
    const others = [['user', 'ban', 'Cheat', '--hours', '240', '--reason', 'abusive chat'],
        ['integrity', 'confirm', 'Cheat', '--reason', 'engine', '--hours', '240', '--no-refund']];
    for (const other of others) {
        const { config, store, id } = world({}, t);
        const now = Date.now();
        assert.equal((await admin(store, config, other, now)).code, 0);
        const ac = createAnticheat({ config, store, log: quiet, now: () => now });
        assert.equal(ac.sanctionCertain({ userId: id.Cheat, gameId: 5, kind: 'illegal_move' }).applied, true, other[0]);
        const auto = store.sanctions.list(id.Cheat).find((s) => s.source === 'auto');
        assert.equal(auto.reason, 'certain_cheat:illegal_move');
        store.games.finishBatch([game(id.Cheat, id.Vic, W, now)]);     // on its way to the database at the ban
        assert.deepEqual(store.refunds.list().map((x) => [x.victimName, x.points, x.sanctionId]), [['Vic', 10, auto.id]], other[0]);
        ac.close();
    }
});

test('refunds apply counts its window back from the latest ban for cheating, not from a later ban for something else', async (t) => {
    const { config, store, id } = world({}, t);
    const g = play(store, id);
    const conf = await admin(store, config, ['integrity', 'confirm', 'Cheat', '--reason', 'engine', '--no-refund', '--json']);
    assert.equal(conf.code, 0, conf.err);
    const later = NOW + 100 * DAY;
    assert.equal((await admin(store, config, ['user', 'ban', 'Cheat', '--hours', '24', '--reason', 'abusive chat'], later)).code, 0);
    const apply = await admin(store, config, ['refunds', 'apply', 'Cheat', '--json'], later);
    assert.equal(apply.code, 0, apply.err);
    assert.deepEqual([apply.json.since, apply.json.sanctionId], [NOW - 60 * DAY, conf.json.sanctionId]);
    assert.deepEqual(apply.json.refunds.map((r) => r.gameId).sort(), [g.vicLoss.id, g.valDraw.id].sort());
});
