// Loading of the configuration (src/config.js loadConfig): the env file, the _FILE secrets, the
// checks between keys and the warnings check-config prints.

import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import crypto from 'node:crypto';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import test from 'node:test';
import { fileURLToPath } from 'node:url';
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

test('Google sign-in: the origin and its tag; warnings for confirmation off, a leftover GOOGLE_REDIRECT_URI and localhost, which check-config prints', (t) => {
    const sso = { SSO_GOOGLE_ENABLED: '1', GOOGLE_CLIENT_ID: 'id.apps.googleusercontent.com', GOOGLE_CLIENT_SECRET: 'GOCSPX-x', SERVER_PUBLIC_HOST: 'chess.example.org' };
    const official = testConfig({ SERVER_PUBLIC_HOST: 'Play.Scacelith.Example', PUBLIC_API_PORT: '443', API_PORT: '8443' });
    assert.deepEqual([official.ssoOrigin, official.ssoRedirectTag], ['play.scacelith.example:443', 'IhcScoV7eDOzTEcSnqPUPt']);
    assert.equal(testConfig({ SERVER_PUBLIC_HOST: '::1', API_PORT: '8443' }).ssoOrigin, '[::1]:8443');
    assert.equal(testConfig({ SERVER_PUBLIC_HOST: '[::1]', API_PORT: '8443' }).ssoRedirectTag, 'XToJm0DG5PjciEVmZa9Cho');
    assert.ok(!('googleRedirectUri' in official));

    assert.deepEqual(configWarnings(testConfig(sso), {}), []);
    assert.deepEqual(configWarnings(testConfig({ REQUIRE_EMAIL_VERIFICATION: '0', SERVER_PUBLIC_HOST: 'localhost' }), {}), [], 'Google sign-in off');
    let w = configWarnings(testConfig({ ...sso, REQUIRE_EMAIL_VERIFICATION: '0' }), {});
    assert.equal(w.length, 1);
    assert.match(w[0], /^SSO_GOOGLE_ENABLED with REQUIRE_EMAIL_VERIFICATION=false: anyone can register a password account with someone else's e-mail address\./);
    w = configWarnings(testConfig({ ...sso, SERVER_PUBLIC_HOST: 'localhost', API_PORT: '8443' }), {});
    assert.deepEqual(w, ['SSO_GOOGLE_ENABLED with SERVER_PUBLIC_HOST=localhost: Google sign-in only works for players who add this server as localhost:8443.']);
    // GOOGLE_REDIRECT_URI is no key any more: in the environment or the .env file, it is reported, whatever SSO_GOOGLE_ENABLED.
    const old = 'GOOGLE_REDIRECT_URI is no longer used: Google sign-in now returns to the game on 127.0.0.1. Remove it and use a "Desktop app" OAuth client.';
    assert.deepEqual(configWarnings(testConfig({ GOOGLE_REDIRECT_URI: 'https://chess.example.org/auth/sso/google/callback' }), {}), [old]);
    assert.deepEqual(configWarnings(testConfig({ GOOGLE_REDIRECT_URI: '' }), {}), []);
    const dir = tmpDir(t);
    fs.writeFileSync(path.join(dir, '.env'), 'GOOGLE_REDIRECT_URI=https://chess.example.org/auth/sso/google/callback\n');
    assert.deepEqual(configWarnings(loadConfig({ env: { ...BASE, SERVER_SECRET: SECRET }, cwd: dir }), {}), [old]);

    const r = spawnSync(process.execPath, [fileURLToPath(new URL('../../bin/scacelith-server.js', import.meta.url)), 'check-config'], {
        cwd: os.tmpdir(), encoding: 'utf8',
        env: { PATH: process.env.PATH, SCACELITH_ENV_FILE: '', SERVER_SECRET: SECRET, TLS_MODE: 'off', ALLOW_INSECURE_DEV: '1', ...sso,
            SERVER_PUBLIC_HOST: 'localhost', REQUIRE_EMAIL_VERIFICATION: 'false', GOOGLE_REDIRECT_URI: 'https://x/cb' },
    });
    assert.equal(r.status, 0, r.stderr);
    const printed = JSON.parse(r.stdout);
    assert.deepEqual([printed.ssoOrigin, printed.ssoRedirectTag], ['localhost:443', testConfig({ SERVER_PUBLIC_HOST: 'localhost', API_PORT: '443' }).ssoRedirectTag]);
    for (const re of [/^warning: GOOGLE_REDIRECT_URI is no longer used/m, /^warning: SSO_GOOGLE_ENABLED with REQUIRE_EMAIL_VERIFICATION=false/m,
        /^warning: SSO_GOOGLE_ENABLED with SERVER_PUBLIC_HOST=localhost/m]) assert.match(r.stderr, re);
});
