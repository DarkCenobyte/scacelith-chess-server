// Configuration keys of the auth family limits, the account budget and the animated GIFs
// (abuse design 3.7, gif design "GIF HTTP contract" and "Password recovery"): defaults, bounds,
// the checks between keys, and the generated .env.example / docs/CONFIG.md being up to date.

import test from 'node:test';
import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import { CONFIG_KEYS, testConfig } from '../../src/config.js';

const ROOT = fileURLToPath(new URL('../..', import.meta.url));

test('defaults of the auth family limits, the account budget and the GIF settings', () => {
    const c = testConfig();
    assert.deepEqual({
        authRegisterPerHour: c.authRegisterPerHour, authMailPerHour: c.authMailPerHour, authForgotPerHour: c.authForgotPerHour,
        authForgotPerDay: c.authForgotPerDay, authResetPerHour: c.authResetPerHour, authMfaPerAccount: c.authMfaPerAccount,
        authReauthPerUser: c.authReauthPerUser, userRatePerMin: c.userRatePerMin,
    }, {
        authRegisterPerHour: 10, authMailPerHour: 10, authForgotPerHour: 3, authForgotPerDay: 10, authResetPerHour: 10,
        authMfaPerAccount: 10, authReauthPerUser: 10, userRatePerMin: 120,
    });
    assert.deepEqual({
        gifEnabled: c.gifEnabled, gifThreads: c.gifThreads, gifQueueMax: c.gifQueueMax, gifQueueTimeoutMs: c.gifQueueTimeoutMs,
        gifRenderTimeoutMs: c.gifRenderTimeoutMs, gifMaxPlies: c.gifMaxPlies, gifCacheMb: c.gifCacheMb,
        gifUserRendersPerMin: c.gifUserRendersPerMin, gifUserRendersPerHour: c.gifUserRendersPerHour,
        gifIpRendersPerMin: c.gifIpRendersPerMin, gifIpRendersPerHour: c.gifIpRendersPerHour,
    }, {
        gifEnabled: true, gifThreads: 1, gifQueueMax: 4, gifQueueTimeoutMs: 10000, gifRenderTimeoutMs: 30000, gifMaxPlies: 600,
        gifCacheMb: 32, gifUserRendersPerMin: 4, gifUserRendersPerHour: 30, gifIpRendersPerMin: 12, gifIpRendersPerHour: 120,
    });
});

test('the auth keys live in the limits section, the GIF keys in their own', () => {
    const section = (name) => CONFIG_KEYS.find((k) => k.name === name)?.section;
    for (const n of ['AUTH_REGISTER_PER_HOUR', 'AUTH_MAIL_PER_HOUR', 'AUTH_FORGOT_PER_HOUR', 'AUTH_FORGOT_PER_DAY', 'AUTH_RESET_PER_HOUR',
        'AUTH_MFA_PER_ACCOUNT', 'AUTH_REAUTH_PER_USER', 'USER_RATE_PER_MIN']) assert.equal(section(n), 'limits', n);
    for (const n of ['GIF_ENABLED', 'GIF_THREADS', 'GIF_QUEUE_MAX', 'GIF_QUEUE_TIMEOUT_MS', 'GIF_RENDER_TIMEOUT_MS', 'GIF_MAX_PLIES', 'GIF_CACHE_MB',
        'GIF_USER_RENDERS_PER_MIN', 'GIF_USER_RENDERS_PER_HOUR', 'GIF_IP_RENDERS_PER_MIN', 'GIF_IP_RENDERS_PER_HOUR']) assert.equal(section(n), 'gif', n);
});

test('bounds of the new keys', () => {
    for (const n of ['AUTH_REGISTER_PER_HOUR', 'AUTH_MAIL_PER_HOUR', 'AUTH_FORGOT_PER_HOUR', 'AUTH_RESET_PER_HOUR', 'AUTH_MFA_PER_ACCOUNT',
        'AUTH_REAUTH_PER_USER', 'USER_RATE_PER_MIN', 'GIF_USER_RENDERS_PER_MIN', 'GIF_IP_RENDERS_PER_MIN', 'GIF_THREADS']) {
        assert.throws(() => testConfig({ [n]: '0' }), new RegExp(`${n}: at least 1`), n);
    }
    assert.throws(() => testConfig({ GIF_THREADS: '9' }), /GIF_THREADS: at most 8/);
    assert.throws(() => testConfig({ GIF_QUEUE_MAX: '65' }), /GIF_QUEUE_MAX: at most 64/);
    assert.throws(() => testConfig({ GIF_QUEUE_TIMEOUT_MS: '99' }), /GIF_QUEUE_TIMEOUT_MS: at least 100/);
    assert.throws(() => testConfig({ GIF_RENDER_TIMEOUT_MS: '120001' }), /GIF_RENDER_TIMEOUT_MS: at most 120000/);
    assert.throws(() => testConfig({ GIF_MAX_PLIES: '1201' }), /GIF_MAX_PLIES: at most 1200/, 'the renderer draws 1200 plies at most');
    assert.throws(() => testConfig({ GIF_CACHE_MB: '-1' }), /GIF_CACHE_MB: at least 0/);
    assert.throws(() => testConfig({ GIF_ENABLED: 'maybe' }), /GIF_ENABLED: true or false expected/);
    assert.equal(testConfig({ GIF_QUEUE_MAX: '0', GIF_CACHE_MB: '0', GIF_ENABLED: 'false' }).gifEnabled, false, 'no queue, no cache, off');
});

test('a longer window may not allow fewer than a shorter one', () => {
    assert.throws(() => testConfig({ AUTH_FORGOT_PER_HOUR: '5', AUTH_FORGOT_PER_DAY: '4' }), /AUTH_FORGOT_PER_DAY must be at least AUTH_FORGOT_PER_HOUR/);
    assert.throws(() => testConfig({ GIF_USER_RENDERS_PER_MIN: '31' }), /GIF_USER_RENDERS_PER_HOUR must be at least GIF_USER_RENDERS_PER_MIN/);
    assert.throws(() => testConfig({ GIF_IP_RENDERS_PER_HOUR: '11' }), /GIF_IP_RENDERS_PER_HOUR must be at least GIF_IP_RENDERS_PER_MIN/);
    const c = testConfig({ AUTH_FORGOT_PER_HOUR: '10', AUTH_FORGOT_PER_DAY: '10', GIF_USER_RENDERS_PER_MIN: '30', GIF_IP_RENDERS_PER_MIN: '120' });
    assert.equal(c.authForgotPerDay, 10, 'equal is allowed');
});

test('.env.example and docs/CONFIG.md are generated from the current keys (npm run gen:env)', () => {
    // --check exits 1 (execFileSync throws) when a generated file is out of date.
    execFileSync(process.execPath, ['tools/gen-env-example.js', '--check'], { cwd: ROOT, stdio: 'pipe' });
});
