import { test } from 'node:test';
import assert from 'node:assert/strict';
import { openStore, migrate } from '../../src/store/index.js';
import { testConfig } from '../../src/config.js';

const DAY = 86400000;
const T = 1_800_000_000_000;

function setup(overrides = {}) {
    const store = openStore(testConfig({ DB_PATH: ':memory:', ...overrides }));
    migrate(store);
    const ids = ['Ann', 'Ben', 'Cid', 'Dee'].map((n) => store.users.create({ username: n, email: `${n}@example.org` }));
    return { store, ids };
}

test('conduct: events, counts since, cooldown state', () => {
    const { store, ids: [a, b] } = setup();
    store.conduct.record(a, 'abandon', T);
    store.conduct.record(a, 'abandon', T + 1000);
    store.conduct.record(a, 'noshow', T + 2000);
    store.conduct.record(a, 'abort', T - DAY);
    store.conduct.record(b, 'abort', T);
    assert.deepEqual(store.conduct.countSince(a, T - 1), { abandon: 2, abort: 0, noshow: 1 });
    assert.deepEqual(store.conduct.countSince(a, T - DAY), { abandon: 2, abort: 1, noshow: 1 });
    assert.deepEqual(store.conduct.countSince(b, T + 1), { abandon: 0, abort: 0, noshow: 0 });
    assert.throws(() => store.conduct.record(a, 'rage', T), (e) => e.code === 'invalid');
    assert.deepEqual(store.conduct.cooldown(a), { until: 0, level: 0, updatedAt: 0 });
    store.conduct.setCooldown(a, T + 900000, 1, T);
    assert.deepEqual(store.conduct.cooldown(a), { until: T + 900000, level: 1, updatedAt: T });
    store.conduct.setCooldown(a, T + 3600000, 2, T + 5);
    assert.deepEqual(store.conduct.cooldown(a), { until: T + 3600000, level: 2, updatedAt: T + 5 });
    store.close();
});

test('sanctions: create, active ban, list, lift', () => {
    const { store, ids: [a, b] } = setup();
    assert.equal(store.sanctions.activeBan(a, T), null);
    const w = store.sanctions.create({ userId: a, kind: 'warning', reason: 'abuse', source: 'moderator', createdBy: 'mod1', startsAt: T, createdAt: T });
    const b1 = store.sanctions.create({ userId: a, kind: 'ban', reason: 'illegal_move', source: 'auto', gameId: 77, startsAt: T, endsAt: T + DAY, createdAt: T });
    const b2 = store.sanctions.create({ userId: a, kind: 'ban', reason: 'longer', source: 'moderator', startsAt: T, endsAt: T + 7 * DAY, createdAt: T + 1 });
    store.sanctions.create({ userId: b, kind: 'ban', reason: 'future', source: 'moderator', startsAt: T + DAY, endsAt: T + 2 * DAY });
    const ban = store.sanctions.activeBan(a, T + 10);
    assert.equal(ban.id, b2, 'the longest ban');
    assert.equal(ban.kind, 'ban');
    assert.equal(ban.source, 'moderator');
    assert.equal(store.sanctions.activeBan(a, T + 7 * DAY), null, 'expired');
    assert.equal(store.sanctions.activeBan(b, T), null, 'not started');
    assert.equal(store.sanctions.activeBan(b, T + DAY).reason, 'future');
    assert.equal(store.sanctions.lift(b2, 'mod2', T + 20), true);
    assert.equal(store.sanctions.lift(b2, 'mod2', T + 21), false);
    assert.equal(store.sanctions.activeBan(a, T + 30).id, b1);
    const perm = store.sanctions.create({ userId: a, kind: 'ban', reason: 'permanent', source: 'moderator', startsAt: T });
    assert.equal(store.sanctions.activeBan(a, T + 100 * DAY).id, perm);
    assert.equal(store.sanctions.activeBan(a, T + 40).id, perm, 'a permanent ban comes first');
    const list = store.sanctions.list(a);
    assert.equal(list.length, 4);
    assert.equal(list.find((s) => s.id === b2).liftedBy, 'mod2');
    assert.equal(list.find((s) => s.id === b1).gameId, 77);
    assert.equal(list.find((s) => s.id === w).createdBy, 'mod1');
    assert.deepEqual(store.sanctions.active(a, T + 40).map((s) => s.id).sort(), [w, b1, perm].sort());
    assert.throws(() => store.sanctions.create({ userId: a, kind: 'jail', source: 'auto' }), (e) => e.code === 'invalid');
    store.close();
});

test('anomalies and security events: batches in one transaction, reads', () => {
    const { store, ids: [a] } = setup();
    assert.equal(store.anomalies.insertBatch([
        { userId: a, gameId: 5, kind: 'illegal_move', severity: 'certain', detail: { move: 1234 }, at: T },
        { userId: a, gameId: 5, kind: 'desync', severity: 'info', detail: 'ply 7', at: T + 1 },
        { userId: 0, kind: 'malformed', severity: 'suspicious', at: T + 2 },
    ]), 3);
    const an = store.anomalies.forUser(a, 10);
    assert.equal(an.length, 2);
    assert.equal(an[0].kind, 'desync');
    assert.equal(an[0].detail, 'ply 7');
    assert.deepEqual(an[1].detail, { move: 1234 });
    assert.equal(store.anomalies.forUser(a, 1).length, 1);
    // One invalid row rolls back its whole batch.
    assert.throws(() => store.anomalies.insertBatch([
        { userId: a, kind: 'ok', severity: 'info', at: T }, { userId: a, kind: 'bad', severity: 'fatal', at: T }]));
    assert.equal(store.anomalies.forUser(a, 10).length, 2);
    assert.equal(store.anomalies.insertBatch([]), 0);

    assert.equal(store.security.insertBatch([
        { kind: 'login_failed', userId: a, ip: '192.0.2.1', detail: { reason: 'password' }, at: T },
        { kind: 'login_failed', userId: null, ip: '192.0.2.2', at: T + 1 },
    ]), 2);
    const sec = store.security.forUser(a, 10);
    assert.equal(sec.length, 1);
    assert.equal(sec[0].ip, '192.0.2.1');
    assert.deepEqual(sec[0].detail, { reason: 'password' });
    store.close();
});

test('integrity: defaults, set/merge, flagged list, population statistics', () => {
    const { store, ids: [a, b, c] } = setup();
    assert.deepEqual(store.integrity.get(a), { level: 'none', score: 0, evidence: null, updatedAt: 0, reviewedBy: null, reviewedAt: null, note: null });
    store.integrity.set(a, { level: 'suspected', score: 2.5, evidence: { signals: ['acpl'] }, updatedAt: T });
    store.integrity.set(b, { level: 'high_confidence', score: 4.1, updatedAt: T });
    store.integrity.set(c, { level: 'none', score: 0.3, updatedAt: T });
    store.integrity.set(a, { score: 3, updatedAt: T + 1 });
    const ia = store.integrity.get(a);
    assert.equal(ia.level, 'suspected', 'fields not given are kept');
    assert.equal(ia.score, 3);
    assert.deepEqual(ia.evidence, { signals: ['acpl'] });
    assert.equal(ia.updatedAt, T + 1);
    store.integrity.set(b, { level: 'confirmed', reviewedBy: 'mod1', reviewedAt: T + 2, note: 'engine match' });
    assert.equal(store.integrity.get(b).reviewedBy, 'mod1');
    assert.throws(() => store.integrity.set(a, { level: 'guilty' }), (e) => e.code === 'invalid');
    assert.deepEqual(store.integrity.listFlagged('suspected', 10).map((r) => [r.userId, r.level]), [[b, 'confirmed'], [a, 'suspected']]);
    assert.deepEqual(store.integrity.listFlagged('high_confidence', 10).map((r) => r.userId), [b]);
    assert.equal(store.integrity.listFlagged('none', 10).length, 2, 'level none is never listed');
    assert.equal(store.integrity.listFlagged('suspected', 10)[0].username, 'Ben');

    // Welford statistics: merging batches equals computing over all values.
    const values = [10, 12, 23, 23, 16, 23, 21, 16];
    store.integrity.updatePopulation('3+2|1500|acpl', values.slice(0, 3), T);
    store.integrity.updatePopulation([{ category: '3+2', ratingBucket: 1500, metric: 'acpl', values: values.slice(3, 7) }], T);
    store.integrity.updatePopulation('3+2|1500|acpl', values[7], T + 1);
    store.integrity.updatePopulation({ key: '3+2|1500|agree', n: 4, mean: 0.5, m2: 0.2 }, T);
    store.integrity.updatePopulation('3+2|15000|acpl', 99, T);
    const mean = values.reduce((s, x) => s + x, 0) / values.length;
    const variance = values.reduce((s, x) => s + (x - mean) ** 2, 0) / (values.length - 1);
    const pop = store.integrity.populationStats('3+2', 1500);
    assert.deepEqual(Object.keys(pop).sort(), ['acpl', 'agree']);
    assert.equal(pop.acpl.n, 8);
    assert.ok(Math.abs(pop.acpl.mean - mean) < 1e-9);
    assert.ok(Math.abs(pop.acpl.variance - variance) < 1e-9);
    assert.ok(Math.abs(pop.acpl.stdev - Math.sqrt(variance)) < 1e-9);
    assert.equal(pop.acpl.updatedAt, T + 1);
    assert.equal(pop.agree.n, 4);
    assert.deepEqual(store.integrity.populationStats('3+2|1500'), pop);
    assert.deepEqual(store.integrity.populationStats({ category: '3+2', ratingBucket: 1500 }), pop);
    assert.deepEqual(store.integrity.populationStats('5+0', 1500), {});
    store.close();
});

test('reports: create, uniqueness, counts, open list, resolution', () => {
    const { store, ids: [a, b, c] } = setup();
    const r1 = store.reports.create({ reporterId: a, reportedId: b, gameId: 11, category: 'cheating', comment: 'engine', weight: 0.8, at: T });
    store.reports.create({ reporterId: c, reportedId: b, gameId: 11, category: 'abuse', at: T + 1 });
    store.reports.create({ reporterId: a, reportedId: b, category: 'other', at: T + 2 });
    assert.throws(() => store.reports.create({ reporterId: a, reportedId: b, gameId: 11, category: 'abuse', at: T + 3 }),
        (e) => e.code === 'duplicate');
    assert.throws(() => store.reports.create({ reporterId: a, reportedId: c, gameId: 11, category: 'spam', at: T }),
        (e) => e.code === 'invalid');
    assert.equal(store.reports.exists(a, b, 11), true);
    assert.equal(store.reports.exists(a, b, 0), true);
    assert.equal(store.reports.exists(a, b), true);
    assert.equal(store.reports.exists(b, a, 11), false);
    assert.equal(store.reports.countByReporterSince(a, T), 2);
    assert.equal(store.reports.countByReporterSince(a, T + 1), 1);
    const open = store.reports.listOpen(10);
    assert.equal(open.length, 3);
    assert.equal(open[0].id, r1);
    assert.equal(open[0].reporterName, 'Ann');
    assert.equal(open[0].reportedName, 'Ben');
    assert.equal(open[0].weight, 0.8);
    assert.equal(open[2].gameId, null);
    assert.equal(store.reports.resolve(r1, 'actioned', 'mod1', T + 10), true);
    assert.equal(store.reports.resolve(r1, 'dismissed', 'mod1', T + 11), false);
    assert.throws(() => store.reports.resolve(r1, 'maybe', 'mod1', T), (e) => e.code === 'invalid');
    assert.equal(store.reports.listOpen(10).length, 2);
    const forB = store.reports.forReported(b);
    assert.equal(forB.length, 3);
    assert.equal(forB.find((r) => r.id === r1).status, 'actioned');
    assert.equal(forB.find((r) => r.id === r1).resolvedBy, 'mod1');
    store.close();
});

test('retention: expired sessions and tokens, old security events and anomalies, IP erasure', () => {
    const { store, ids: [a] } = setup({ RETENTION_SECURITY_DAYS: '90', RETENTION_IP_DAYS: '30' });
    const now = T + 200 * DAY;
    // Sessions: expired, idle-expired, revoked two days ago, live but old (IP erased), live and recent.
    store.sessions.create({ userId: a, tokenHash: 's-exp', createdAt: now - 100 * DAY, expiresAt: now - 1, idleExpiresAt: now + DAY, ip: '1.1.1.1' });
    store.sessions.create({ userId: a, tokenHash: 's-idle', createdAt: now - 10 * DAY, expiresAt: now + DAY, idleExpiresAt: now - 1, ip: '1.1.1.2' });
    const rev = store.sessions.create({ userId: a, tokenHash: 's-rev', createdAt: now - 10 * DAY, expiresAt: now + DAY, idleExpiresAt: now + DAY });
    store.sessions.revoke(rev, a, now - 2 * DAY);
    const recentRev = store.sessions.create({ userId: a, tokenHash: 's-rev2', createdAt: now - 10 * DAY, expiresAt: now + DAY, idleExpiresAt: now + DAY });
    store.sessions.revoke(recentRev, a, now - 1000);
    store.sessions.create({ userId: a, tokenHash: 's-old', createdAt: now - 40 * DAY, expiresAt: now + 50 * DAY, idleExpiresAt: now + DAY, ip: '1.1.1.3' });
    store.sessions.create({ userId: a, tokenHash: 's-new', createdAt: now - DAY, expiresAt: now + 50 * DAY, idleExpiresAt: now + DAY, ip: '1.1.1.4' });
    store.tokens.create({ kind: 'reset', tokenHash: 't-exp', userId: a, expiresAt: now - 1 });
    store.tokens.create({ kind: 'reset', tokenHash: 't-live', userId: a, expiresAt: now + 1000 });
    store.security.insertBatch([
        { kind: 'old', ip: '2.2.2.1', at: now - 91 * DAY },
        { kind: 'mid', ip: '2.2.2.2', userId: a, at: now - 31 * DAY },
        { kind: 'new', ip: '2.2.2.3', userId: a, at: now - DAY },
    ]);
    store.anomalies.insertBatch([
        { userId: a, kind: 'desync', severity: 'info', at: now - 91 * DAY },
        { userId: a, kind: 'bad_seq', severity: 'suspicious', at: now - 91 * DAY },
        { userId: a, kind: 'illegal_move', severity: 'certain', at: now - 91 * DAY },
        { userId: a, kind: 'desync', severity: 'info', at: now - DAY },
    ]);
    store.conduct.record(a, 'abandon', now - 31 * DAY);
    store.conduct.record(a, 'abandon', now - DAY);

    const counts = store.retention.run(now);
    // IPs erased: the 40-day-old live session and the 31-day-old event (older rows were deleted outright).
    assert.deepEqual(counts, { sessions: 3, tokens: 1, securityEvents: 1, anomalies: 2, conductEvents: 1, analysisJobs: 0, ipErased: 2 });
    assert.equal(store.sessions.byTokenHash('s-exp'), null);
    assert.equal(store.sessions.byTokenHash('s-idle'), null);
    assert.equal(store.sessions.byTokenHash('s-rev'), null);
    assert.ok(store.sessions.byTokenHash('s-rev2'), 'recently revoked sessions are kept a day');
    const live = new Map(store.sessions.listForUser(a).map((s) => [s.ip, s]));
    assert.ok(live.has(null) && live.has('1.1.1.4') && !live.has('1.1.1.3'));
    assert.equal(store.tokens.get('reset', 't-exp'), null);
    assert.ok(store.tokens.get('reset', 't-live'));
    const sec = store.security.forUser(a, 10);
    assert.deepEqual(sec.map((e) => [e.kind, e.ip]), [['new', '2.2.2.3'], ['mid', null]]);
    assert.deepEqual(store.anomalies.forUser(a, 10).map((x) => x.kind).sort(), ['desync', 'illegal_move']);
    assert.deepEqual(store.conduct.countSince(a, 0), { abandon: 1, abort: 0, noshow: 0 });
    // A second run has nothing left to do.
    assert.deepEqual(store.retention.run(now), { sessions: 0, tokens: 0, securityEvents: 0, anomalies: 0, conductEvents: 0, analysisJobs: 0, ipErased: 0 });
    store.close();
});

test('retention deletes in chunks larger than one statement', () => {
    const { store, ids: [a] } = setup();
    const events = [];
    for (let i = 0; i < 2500; i++) events.push({ kind: 'login_failed', userId: a, ip: '192.0.2.9', at: T });
    store.security.insertBatch(events);
    const res = store.security.purge(T + 100 * DAY);
    assert.equal(res.deleted, 2500);
    assert.equal(store.security.forUser(a, 10).length, 0);
    store.close();
});

test('store.transaction: atomic multi-call writes, nested savepoints', () => {
    const { store, ids: [a, b] } = setup();
    assert.throws(() => store.transaction(() => {
        store.sanctions.create({ userId: a, kind: 'ban', source: 'auto', startsAt: T, endsAt: T + DAY });
        store.integrity.set(a, { level: 'confirmed' });
        throw new Error('abort');
    }), /abort/);
    assert.equal(store.sanctions.list(a).length, 0);
    assert.equal(store.integrity.get(a).level, 'none');
    store.transaction(() => {
        store.conduct.record(b, 'abort', T);
        try {
            store.transaction(() => { store.conduct.record(b, 'noshow', T); throw new Error('inner'); });
        } catch { /* inner rolled back only */ }
        store.conduct.record(b, 'abandon', T);
    });
    assert.deepEqual(store.conduct.countSince(b, 0), { abandon: 1, abort: 1, noshow: 0 });
    store.close();
});
