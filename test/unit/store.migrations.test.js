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
    'population_stats', 'reports', 'schema_migrations'];

test('migrate creates every table from an empty database and records the migration', () => {
    const dir = tmpDir();
    const file = path.join(dir, 'fresh.db');
    const store = openStore(testConfig({ DB_PATH: file }));
    const res = migrate(store);
    assert.deepEqual(res.applied, [1]);
    assert.equal(res.version, 1);
    assert.match(store.meta.get('server_id'), /^[0-9a-f-]{36}$/);
    // Inspect the schema through a second, raw connection (node:sqlite is already loaded by the store).
    const { DatabaseSync } = createRequire(import.meta.url)('node:sqlite');
    const raw = new DatabaseSync(file, { readOnly: true });
    const names = new Set(raw.prepare("SELECT name FROM sqlite_master WHERE type = 'table'").all().map((r) => r.name));
    for (const t of TABLES) assert.ok(names.has(t), `table ${t}`);
    const mig = raw.prepare('SELECT version, name, applied_at, checksum FROM schema_migrations').all();
    assert.equal(mig.length, 1);
    assert.equal(mig[0].name, '001_initial');
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
