// Loading of the configuration (src/config.js loadConfig): the env file, the _FILE secrets, the
// checks between keys and the warnings check-config prints.

import assert from 'node:assert/strict';
import crypto from 'node:crypto';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import test from 'node:test';
import { ConfigError, configWarnings, describe, loadConfig, parseEnvFile, testConfig } from '../../src/config.js';

const SECRET = Buffer.alloc(48, 7).toString('base64');
const BASE = { TLS_MODE: 'off', ALLOW_INSECURE_DEV: '1', WORKERS: '1', MAIL_TRANSPORT: 'none', LOG_LEVEL: 'error', METRICS_PORT: '0' };

function tmpDir(t) {
    const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'scacelith-config-'));
    t.after(() => fs.rmSync(dir, { recursive: true, force: true }));
    return dir;
}

test('a SCACELITH_ENV_FILE that cannot be read stops the start; a missing ./.env does not', (t) => {
    const dir = tmpDir(t);
    const missing = path.join(dir, 'typo.env');
    assert.throws(() => loadConfig({ env: { ...BASE, SERVER_SECRET: SECRET, SCACELITH_ENV_FILE: missing }, cwd: dir }),
        (e) => e instanceof ConfigError && e.message.includes(`SCACELITH_ENV_FILE: cannot read ${missing} (ENOENT)`));
    assert.throws(() => loadConfig({ env: { ...BASE, SERVER_SECRET: SECRET }, envFile: missing, cwd: dir }), /cannot read/);
    // No ./.env in the directory, and '' meaning 'no file': both load.
    assert.equal(loadConfig({ env: { ...BASE, SERVER_SECRET: SECRET }, cwd: dir }).registration, 'open');
    assert.equal(loadConfig({ env: { ...BASE, SERVER_SECRET: SECRET, SCACELITH_ENV_FILE: '' }, cwd: dir }).registration, 'open');
    const file = path.join(dir, 'server.env');
    fs.writeFileSync(file, 'REGISTRATION=closed\n');
    assert.equal(loadConfig({ env: { ...BASE, SERVER_SECRET: SECRET, SCACELITH_ENV_FILE: file }, cwd: dir }).registration, 'closed');
});

test('TLS_KEY_FILE_FILE is refused and never read', (t) => {
    const dir = tmpDir(t);
    const key = path.join(dir, 'key.pem');
    fs.writeFileSync(key, '-----BEGIN PRIVATE KEY-----\nMIGHAgEAMBMGByqGSM49AgEGCCqGSM49AwEHBG0wawIBAQQg\n-----END PRIVATE KEY-----\n');
    let err = null;
    try { testConfig({ TLS_KEY_FILE_FILE: key }); } catch (e) { err = e; }
    assert.ok(err instanceof ConfigError);
    assert.match(err.message, /TLS_KEY_FILE already names the key file; TLS_KEY_FILE_FILE is not supported/);
    assert.ok(!err.message.includes('BEGIN'), 'the key text is not in the message');
    const cfg = testConfig({ TLS_KEY_FILE: key, TLS_KEY_FILE_FILE: '' });
    assert.equal(cfg.tlsKeyFile, key);
    assert.ok(!JSON.stringify(describe(cfg)).includes('BEGIN'));
});

test('an empty KEY= does not hide KEY_FILE of a required secret; an optional one is reported', (t) => {
    const dir = tmpDir(t);
    const secretFile = path.join(dir, 'secret');
    const mfaFile = path.join(dir, 'mfa');
    fs.writeFileSync(secretFile, crypto.randomBytes(48).toString('base64') + '\n');
    fs.writeFileSync(mfaFile, crypto.randomBytes(32).toString('hex') + '\n');
    // The .env.example line 'SERVER_SECRET=' next to SERVER_SECRET_FILE.
    const envFile = path.join(dir, '.env');
    fs.writeFileSync(envFile, `SERVER_SECRET=\nSERVER_SECRET_FILE=${secretFile}\n`);
    const c = loadConfig({ env: { ...BASE }, cwd: dir });
    assert.deepEqual(c.serverSecret, Buffer.from(fs.readFileSync(secretFile, 'utf8').trim(), 'base64'));
    assert.deepEqual(configWarnings(c, {}), []);
    // A required secret empty without a file is still refused.
    assert.throws(() => loadConfig({ env: { ...BASE, SERVER_SECRET: '' }, envFile: '' }), /SERVER_SECRET is required/);
    // An optional secret: the empty value wins as before, and check-config says so.
    const m = loadConfig({ env: { ...BASE, SERVER_SECRET: SECRET, MFA_ENCRYPTION_KEY: '', MFA_ENCRYPTION_KEY_FILE: mfaFile }, envFile: '' });
    assert.equal(m.mfaEncryptionKey, null);
    const w = configWarnings(m, {});
    assert.equal(w.length, 1);
    assert.match(w[0], /^MFA_ENCRYPTION_KEY is set but empty, so MFA_ENCRYPTION_KEY_FILE is not read/);
    // KEY_FILE alone is read, as before.
    const f = loadConfig({ env: { ...BASE, SERVER_SECRET: SECRET, MFA_ENCRYPTION_KEY_FILE: mfaFile }, envFile: '' });
    assert.deepEqual(f.mfaEncryptionKey, Buffer.from(fs.readFileSync(mfaFile, 'utf8').trim(), 'hex'));
    assert.deepEqual(configWarnings(f, {}), []);
});

test('SERVER_NAME fits Welcome.serverName: at most 64 bytes in UTF-8', () => {
    assert.equal(testConfig({ SERVER_NAME: 'x'.repeat(64) }).serverName, 'x'.repeat(64));
    const accented = 'Sunday club ' + 'é'.repeat(30);    // 42 characters, 72 bytes
    assert.ok(accented.length <= 64 && Buffer.byteLength(accented) > 64);
    assert.throws(() => testConfig({ SERVER_NAME: accented }), /SERVER_NAME: at most 64 bytes in UTF-8/);
    assert.throws(() => testConfig({ SERVER_NAME: '€'.repeat(24) }), /SERVER_NAME: at most 64 bytes in UTF-8/);
    assert.throws(() => testConfig({ SERVER_NAME: 'x'.repeat(65) }), /SERVER_NAME: at most 64 characters/);
    assert.throws(() => testConfig({ SERVER_NAME: 'a\0b' }), /SERVER_NAME: no NUL character/);
    assert.equal(testConfig({ SERVER_NAME: 'Club é' }).serverName, 'Club é');
});

test('TRUSTED_PROXIES is checked at load with TLS_MODE=proxy only', () => {
    assert.throws(() => testConfig({ TLS_MODE: 'proxy', TRUSTED_PROXIES: '127.0.0.1,localhost' }), /TRUSTED_PROXIES: /);
    assert.throws(() => testConfig({ TLS_MODE: 'proxy', TRUSTED_PROXIES: '10.0.0.0/33' }), /TRUSTED_PROXIES: /);
    assert.deepEqual(testConfig({ TLS_MODE: 'proxy', TRUSTED_PROXIES: '127.0.0.1, 10.0.0.0/8, ::1' }).trustedProxies, ['127.0.0.1', '10.0.0.0/8', '::1']);
    // Unused in the other modes: a stale value does not stop a server that starts today.
    assert.deepEqual(testConfig({ TRUSTED_PROXIES: 'localhost' }).trustedProxies, ['localhost']);
});

test('check-config warns when HEARTBEAT_TIMEOUT_MS would close healthy idle connections', () => {
    assert.deepEqual(configWarnings(testConfig(), {}), []);
    assert.deepEqual(configWarnings(testConfig({ HEARTBEAT_INTERVAL_MS: '10000', HEARTBEAT_TIMEOUT_MS: '12250' }), {}), []);
    for (const timeout of ['5000', '10000', '12000']) {
        const w = configWarnings(testConfig({ HEARTBEAT_INTERVAL_MS: '10000', HEARTBEAT_TIMEOUT_MS: timeout }), {});
        assert.equal(w.length, 1, timeout);
        assert.match(w[0], new RegExp(`^HEARTBEAT_TIMEOUT_MS \\(${timeout}\\) is less than HEARTBEAT_INTERVAL_MS \\(10000\\) \\+ 2250`));
    }
});

test('a quoted .env value followed by a comment keeps its quotes, and check-config says so', (t) => {
    const notes = [];
    assert.deepEqual(parseEnvFile('A="x y" # c\nB=\'x\' # c\nC="a#b"\nD=x #c\nE="q"\n', notes),
        { A: '"x y"', B: "'x'", C: 'a#b', D: 'x', E: 'q' }, 'values unchanged');
    assert.equal(notes.length, 2);
    assert.match(notes[0], /^A: the value is quoted and followed by a comment/);
    assert.match(notes[1], /^B: /);
    const dir = tmpDir(t);
    const hex = crypto.randomBytes(48).toString('hex');
    fs.writeFileSync(path.join(dir, '.env'), `SERVER_SECRET="${hex}" # rotated\n`);
    const c = loadConfig({ env: { ...BASE }, cwd: dir });
    assert.deepEqual(c.serverSecret, Buffer.from(`"${hex}"`, 'base64'), 'the effective secret does not change');
    const w = configWarnings(c, {});
    assert.equal(w.length, 1);
    assert.match(w[0], /^SERVER_SECRET: the value is quoted and followed by a comment/);
    assert.ok(!w[0].includes(hex), 'the warning never shows the value');
});
