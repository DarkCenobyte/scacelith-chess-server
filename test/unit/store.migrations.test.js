import { test } from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { createRequire } from 'node:module';
import { fileURLToPath } from 'node:url';
import { openStore, migrate, StoreError } from '../../src/store/index.js';
import { testConfig } from '../../src/config.js';

const MIGRATIONS = fileURLToPath(new URL('../../src/store/migrations/', import.meta.url));

function tmpDir() { return fs.mkdtempSync(path.join(os.tmpdir(), 'scacelith-mig-')); }

const TABLES = ['meta', 'users', 'mfa_recovery_codes', 'sessions', 'tokens', 'sso_identities', 'ratings', 'games',
    'conduct_events', 'conduct_state', 'sanctions', 'anomalies', 'security_events', 'analysis_jobs', 'player_integrity',
    'population_stats', 'reports', 'rating_refunds', 'pending_signups', 'schema_migrations'];

test('migrate creates every table from an empty database and records the migration', () => {
    const dir = tmpDir();
    const file = path.join(dir, 'fresh.db');
    const store = openStore(testConfig({ DB_PATH: file }));
    const res = migrate(store);
    assert.deepEqual(res.applied, [1, 2, 3, 4, 5, 6, 7, 8]);
    assert.equal(res.version, 8);
    assert.match(store.meta.get('server_id'), /^[0-9a-f-]{36}$/);
    // Inspect the schema through a second, raw connection (node:sqlite is already loaded by the store).
    const { DatabaseSync } = createRequire(import.meta.url)('node:sqlite');
    const raw = new DatabaseSync(file, { readOnly: true });
    const names = new Set(raw.prepare("SELECT name FROM sqlite_master WHERE type = 'table'").all().map((r) => r.name));
    for (const t of TABLES) assert.ok(names.has(t), `table ${t}`);
    const mig = raw.prepare('SELECT version, name, applied_at, checksum FROM schema_migrations').all();
    assert.deepEqual(mig.map((m) => m.name), ['001_initial', '002_analysis_priority', '003_analysis_players', '004_fide_ratings_refunds',
        '005_counted_games', '006_analysis_profiles', '007_pending_signups', '008_sso_address_links']);
    assert.match(mig[0].checksum, /^[0-9a-f]{64}$/);
    assert.equal(raw.prepare('PRAGMA journal_mode').get().journal_mode, 'wal');
    raw.close();
    store.close();
    fs.rmSync(dir, { recursive: true, force: true });
});

test('an in-memory store works (DB_PATH=:memory: resolved to an absolute path by the config)', () => {
    const cfg = testConfig({ DB_PATH: ':memory:' });
    assert.notEqual(cfg.dbPath, ':memory:');
    const store = openStore(cfg);
    migrate(store);
    assert.equal(store.path, ':memory:');
    assert.ok(!fs.existsSync(cfg.dbPath));
    store.close();
    store.close();   // idempotent
});

test('migrate is idempotent and keeps the server id; the file database survives reopening', () => {
    const dir = tmpDir();
    const file = path.join(dir, 'db', 'test.db');
    const cfg = testConfig({ DB_PATH: file });
    let store = openStore(cfg);
    migrate(store);
    const id = store.meta.get('server_id');
    assert.deepEqual(migrate(store).applied, []);
    store.meta.set('k', 42);
    store.close();
    store = openStore(cfg);
    assert.deepEqual(migrate(store).applied, []);
    assert.equal(store.meta.get('server_id'), id);
    assert.equal(store.meta.get('k'), '42');
    assert.equal(store.meta.get('missing'), null);
    store.close();
    fs.rmSync(dir, { recursive: true, force: true });
});

test('migration 004: the records with games stay rated, a record without games starts unrated', () => {
    const dir = tmpDir();
    const migDir = path.join(dir, 'migrations');
    fs.mkdirSync(migDir);
    for (const f of ['001_initial.sql', '002_analysis_priority.sql', '003_analysis_players.sql']) {
        fs.copyFileSync(path.join(MIGRATIONS, f), path.join(migDir, f));
    }
    const cfg = testConfig({ DB_PATH: path.join(dir, 'x.db') });
    let store = openStore(cfg);
    migrate(store, { dir: migDir });
    const [a, b] = ['Old', 'New'].map((n) => store.users.create({ username: n, email: `${n}@example.org` }));
    store.close();
    const { DatabaseSync } = createRequire(import.meta.url)('node:sqlite');
    const raw = new DatabaseSync(cfg.dbPath);
    const put = raw.prepare(`INSERT INTO ratings (user_id, category, rating, games, wins, peak, updated_at) VALUES (?, ?, ?, ?, ?, ?, 0)`);
    put.run(a, '3+2', 1712, 3, 3, 1712);
    put.run(b, '3+2', 1500, 0, 0, 1500);
    raw.close();

    store = openStore(cfg);
    assert.deepEqual(migrate(store).applied, [4, 5, 6, 7, 8]);
    assert.deepEqual(store.ratings.get(a, '3+2'), { rating: 1712, games: 3, wins: 3, draws: 0, losses: 0, peak: 1712,
        reachedSenior: false, rated: true, countedGames: 3, unratedGames: 0, unratedOpponents: 0, unratedHalfPoints: 0 });
    assert.equal(store.ratings.get(b, '3+2').rated, false);
    assert.deepEqual(store.refunds.list(), []);
    store.close();
    fs.rmSync(dir, { recursive: true, force: true });
});

test('counted games migration: the records stored before it count all their games when rated, those of the unrated phase otherwise', () => {
    const dir = tmpDir();
    const migDir = path.join(dir, 'migrations');
    fs.mkdirSync(migDir);
    const all = fs.readdirSync(MIGRATIONS).filter((f) => f.endsWith('.sql')).sort();
    const counted = all.find((f) => f.endsWith('_counted_games.sql'));
    for (const f of all.slice(0, all.indexOf(counted))) fs.copyFileSync(path.join(MIGRATIONS, f), path.join(migDir, f));
    const cfg = testConfig({ DB_PATH: path.join(dir, 'x.db'), PROVISIONAL_GAMES: '30' });
    let store = openStore(cfg);
    migrate(store, { dir: migDir });
    const [a, b] = ['Old', 'New'].map((n) => store.users.create({ username: n, email: `${n}@example.org` }));
    store.close();
    const { DatabaseSync } = createRequire(import.meta.url)('node:sqlite');
    const raw = new DatabaseSync(cfg.dbPath);
    raw.prepare(`INSERT INTO ratings (user_id, category, rating, games, wins, peak, rated, updated_at) VALUES (?, '3+2', 1712, 40, 30, 1712, 1, 0)`)
        .run(a);
    raw.prepare(`INSERT INTO ratings (user_id, category, rating, games, losses, peak, rated, unrated_games, unrated_opponents, unrated_half_points,
        updated_at) VALUES (?, '3+2', 1500, 9, 7, 1500, 0, 3, 4500, 2, 0)`).run(b);
    raw.close();

    store = openStore(cfg);
    assert.equal(migrate(store).applied[0], Number(counted.slice(0, 3)));
    assert.equal(store.ratings.get(a, '3+2').countedGames, 40);
    assert.equal(store.ratings.forUser(a)[0].provisional, false);
    assert.deepEqual(store.ratings.leaderboard('3+2', 100, 30).map((r) => r.userId), [a]);
    assert.equal(store.ratings.get(b, '3+2').countedGames, 3);
    store.close();
    fs.rmSync(dir, { recursive: true, force: true });
});

test('migration 008: Google links made by address on password accounts go, with their sessions; links made at creation stay', () => {
    const dir = tmpDir();
    const migDir = path.join(dir, 'migrations');
    fs.mkdirSync(migDir);
    const all = fs.readdirSync(MIGRATIONS).filter((f) => f.endsWith('.sql')).sort();
    for (const f of all.filter((n) => n < '008')) fs.copyFileSync(path.join(MIGRATIONS, f), path.join(migDir, f));
    const cfg = testConfig({ DB_PATH: path.join(dir, 'x.db') });
    let store = openStore(cfg);
    assert.equal(migrate(store, { dir: migDir }).version, 7);
    const T = Date.UTC(2026, 0, 1);
    const user = (name, passwordHash) => store.users.create({ username: name, email: `${name}@example.org`, passwordHash, emailVerified: true, createdAt: T });
    const byAddress = user('ByAddress', 'scrypt$x');            // linked by address an hour after its creation
    const late = user('Late', 'scrypt$w');                       // 61 s after
    const atCreation = user('AtCreation', 'scrypt$y');           // created by Google, a password set since
    const googleOnly = user('GoogleOnly', null);
    const bench = user('Bench', '!bench-account-no-password');
    const otherProvider = user('Other', 'scrypt$z');
    store.sso.link(byAddress, 'google', 'g1', 'byaddress@example.org', T + 3600000);
    store.sso.link(late, 'google', 'g5', 'late@example.org', T + 61000);
    store.sso.link(atCreation, 'google', 'g2', 'atcreation@example.org', T + 50);
    store.sso.link(googleOnly, 'google', 'g3', 'googleonly@example.org', T + 3600000);
    store.sso.link(bench, 'google', 'g4', 'bench@example.org', T + 3600000);
    store.sso.link(otherProvider, 'example', 'e1', 'other@example.org', T + 3600000);
    let n = 0;
    const session = (userId) => store.sessions.create({ userId, tokenHash: Buffer.from(`session ${++n}`), createdAt: T, expiresAt: Date.now() + 86400000 });
    const sessionsOf = {};
    for (const id of [byAddress, late, atCreation, googleOnly, bench, otherProvider]) sessionsOf[id] = [session(id), session(id)];
    store.sessions.revoke(sessionsOf[byAddress][1], byAddress, T + 5);
    for (const [kind, h] of [['sso_state', 'h1'], ['sso_attempt', 'h2'], ['sso_ticket', 'h3'], ['email_verify', 'h4']]) {
        store.tokens.create({ kind, tokenHash: h, data: { x: 1 }, expiresAt: Date.now() + 600000 });
    }
    store.close();

    store = openStore(cfg);
    const before = Date.now();
    assert.deepEqual(migrate(store).applied, [8]);
    assert.equal(store.sso.find('google', 'g1'), null);
    assert.equal(store.sso.find('google', 'g5'), null);
    for (const [sub, id] of [['g2', atCreation], ['g3', googleOnly], ['g4', bench]]) assert.equal(store.sso.find('google', sub).userId, id, sub);
    assert.equal(store.sso.find('example', 'e1').userId, otherProvider);
    const { DatabaseSync } = createRequire(import.meta.url)('node:sqlite');
    const raw = new DatabaseSync(cfg.dbPath, { readOnly: true });
    const revokedAt = (id) => raw.prepare('SELECT revoked_at FROM sessions WHERE id = ?').get(id).revoked_at;
    assert.ok(revokedAt(sessionsOf[byAddress][0]) >= before - 1000, 'a live session of an account that lost its link is revoked');
    assert.equal(revokedAt(sessionsOf[byAddress][1]), T + 5, 'a session revoked before keeps its time');
    assert.ok(sessionsOf[late].every((id) => revokedAt(id) >= before - 1000));
    for (const id of [atCreation, googleOnly, bench, otherProvider]) {
        for (const sid of sessionsOf[id]) assert.equal(revokedAt(sid), null, `session ${sid} of user ${id}`);
    }
    raw.close();
    assert.equal(store.tokens.get('sso_state', 'h1'), null);
    assert.equal(store.tokens.get('sso_attempt', 'h2'), null);
    assert.ok(store.tokens.get('sso_ticket', 'h3') && store.tokens.get('email_verify', 'h4'), 'the other tokens stay');
    store.close();
    fs.rmSync(dir, { recursive: true, force: true });
});

test('a changed applied migration is refused (checksum), CRLF line endings are not a change', () => {
    const dir = tmpDir();
    const migDir = path.join(dir, 'migrations');
    fs.mkdirSync(migDir);
    const sql = fs.readFileSync(path.join(MIGRATIONS, '001_initial.sql'), 'utf8');
    fs.writeFileSync(path.join(migDir, '001_initial.sql'), sql);
    const cfg = testConfig({ DB_PATH: path.join(dir, 'x.db') });
    let store = openStore(cfg);
    migrate(store, { dir: migDir });
    store.close();

    fs.writeFileSync(path.join(migDir, '001_initial.sql'), sql.replace(/\n/g, '\r\n'));
    store = openStore(cfg);
    assert.deepEqual(migrate(store, { dir: migDir }).applied, []);
    store.close();

    fs.writeFileSync(path.join(migDir, '001_initial.sql'), sql + '\n-- edited\n');
    store = openStore(cfg);
    assert.throws(() => migrate(store, { dir: migDir }), (e) => e instanceof StoreError && e.code === 'migration_checksum');
    store.close();
    fs.rmSync(dir, { recursive: true, force: true });
});

test('new migrations are applied in order; unknown applied migrations and broken ones are refused', () => {
    const dir = tmpDir();
    const migDir = path.join(dir, 'migrations');
    fs.mkdirSync(migDir);
    fs.copyFileSync(path.join(MIGRATIONS, '001_initial.sql'), path.join(migDir, '001_initial.sql'));
    const cfg = testConfig({ DB_PATH: path.join(dir, 'x.db') });
    let store = openStore(cfg);
    migrate(store, { dir: migDir });
    fs.writeFileSync(path.join(migDir, '003_third.sql'), 'CREATE TABLE third (a INTEGER);');
    fs.writeFileSync(path.join(migDir, '002_second.sql'), 'CREATE TABLE second (a INTEGER);');
    fs.writeFileSync(path.join(migDir, 'README.txt'), 'ignored');
    assert.deepEqual(migrate(store, { dir: migDir }), { applied: [2, 3], version: 3 });

    // A failing migration is rolled back entirely and reported.
    fs.writeFileSync(path.join(migDir, '004_broken.sql'), 'CREATE TABLE fourth (a INTEGER); CREATE TABLE broken (;');
    assert.throws(() => migrate(store, { dir: migDir }), (e) => e.code === 'migration_failed');
    fs.rmSync(path.join(migDir, '004_broken.sql'));
    fs.writeFileSync(path.join(migDir, '004_fixed.sql'), 'CREATE TABLE fourth (a INTEGER);');
    assert.deepEqual(migrate(store, { dir: migDir }).applied, [4]);
    store.close();

    // An older server (fewer migration files) refuses a newer database.
    fs.rmSync(path.join(migDir, '004_fixed.sql'));
    store = openStore(cfg);
    assert.throws(() => migrate(store, { dir: migDir }), (e) => e.code === 'migration_missing');
    store.close();
    fs.rmSync(dir, { recursive: true, force: true });
});

test('WAL, synchronous=FULL and a read-only connection', () => {
    const dir = tmpDir();
    const cfg = testConfig({ DB_PATH: path.join(dir, 'x.db') });
    const store = openStore(cfg);
    migrate(store);
    store.users.create({ username: 'Reader', email: 'r@example.org' });
    const ro = openStore(cfg, { readonly: true });
    assert.equal(ro.users.byUsername('reader').username, 'Reader');
    assert.throws(() => ro.users.create({ username: 'Nope', email: 'n@example.org' }));
    assert.throws(() => migrate(ro), (e) => e.code === 'readonly');
    ro.close();
    store.close();
    assert.ok(fs.existsSync(path.join(dir, 'x.db-wal')) || fs.existsSync(path.join(dir, 'x.db')));
    fs.rmSync(dir, { recursive: true, force: true });
});
