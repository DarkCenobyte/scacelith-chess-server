// Configuration of the protection per address (src/config.js section 'abuse', used by
// src/net/ipguard.js and src/cluster/abuse.js): defaults, bounds, the rules between keys, the
// check-config warning, and the generated .env.example and docs/CONFIG.md.

import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import fs from 'node:fs';
import { describe, it } from 'node:test';
import { fileURLToPath } from 'node:url';
import { CONFIG_KEYS, configWarnings, testConfig } from '../../src/config.js';

const SHIELD_KEYS = ['HTTP_RATE_PER_IP', 'HTTP_RATE_PER_PREFIX', 'IP_CONN_RATE', 'IP_MAX_CONNECTIONS', 'IP_MAX_INFLIGHT',
    'ABUSE_BLOCK_REFUSALS_PER_MIN', 'ABUSE_BLOCK_BASE_SEC', 'ABUSE_BLOCK_MAX_SEC', 'ABUSE_EXEMPT'];

describe('config: protection per address', () => {
    it('defaults sized for a VPS-1, a class or a carrier\'s shared address', () => {
        const c = testConfig();
        assert.deepEqual(
            [c.httpRatePerIp, c.httpRatePerPrefix, c.ipConnRate, c.ipMaxConnections, c.ipMaxInflight],
            [600, 2400, 10, 128, 32]);
        assert.deepEqual([c.abuseBlockRefusalsPerMin, c.abuseBlockBaseSec, c.abuseBlockMaxSec, c.abuseExempt], [600, 60, 3600, []]);
        assert.equal(c.maxConnectionsPerIp, 64, 'WebSockets per address: 64 (was 16)');
        for (const name of SHIELD_KEYS) {
            const k = CONFIG_KEYS.find((x) => x.name === name);
            assert.ok(k, name);
            assert.equal(k.section, 'abuse', `${name} is in the per-address section`);
            assert.ok(k.desc && k.desc.length > 40, `${name} has a description`);
        }
    });

    it('HTTP_RATE_PER_PREFIX: 0 means 4 times HTTP_RATE_PER_IP, a value set must be at least it', () => {
        assert.equal(testConfig({ HTTP_RATE_PER_IP: '100' }).httpRatePerPrefix, 400);
        assert.equal(testConfig({ HTTP_RATE_PER_IP: '100', HTTP_RATE_PER_PREFIX: '100' }).httpRatePerPrefix, 100);
        assert.equal(testConfig({ HTTP_RATE_PER_IP: '100', HTTP_RATE_PER_PREFIX: '5000' }).httpRatePerPrefix, 5000);
        assert.throws(() => testConfig({ HTTP_RATE_PER_IP: '100', HTTP_RATE_PER_PREFIX: '99' }), /HTTP_RATE_PER_PREFIX must be 0 or at least HTTP_RATE_PER_IP/);
    });

    it('bounds: the rates and caps are at least 1; ABUSE_BLOCK_REFUSALS_PER_MIN=0 turns blocking off', () => {
        for (const name of ['HTTP_RATE_PER_IP', 'IP_CONN_RATE', 'IP_MAX_CONNECTIONS', 'IP_MAX_INFLIGHT', 'ABUSE_BLOCK_BASE_SEC', 'ABUSE_BLOCK_MAX_SEC']) {
            assert.throws(() => testConfig({ [name]: '0' }), new RegExp(`${name}: at least 1`), name);
            assert.throws(() => testConfig({ [name]: 'many' }), new RegExp(name), name);
        }
        assert.throws(() => testConfig({ HTTP_RATE_PER_PREFIX: '-1' }), /HTTP_RATE_PER_PREFIX/);
        assert.throws(() => testConfig({ IP_CONN_RATE: '100001' }), /IP_CONN_RATE: at most 100000/);
        assert.throws(() => testConfig({ ABUSE_BLOCK_MAX_SEC: '604801' }), /ABUSE_BLOCK_MAX_SEC: at most 604800/);
        assert.equal(testConfig({ ABUSE_BLOCK_REFUSALS_PER_MIN: '0' }).abuseBlockRefusalsPerMin, 0);
    });

    it('ABUSE_BLOCK_BASE_SEC must not exceed ABUSE_BLOCK_MAX_SEC', () => {
        assert.equal(testConfig({ ABUSE_BLOCK_BASE_SEC: '600', ABUSE_BLOCK_MAX_SEC: '600' }).abuseBlockBaseSec, 600);
        assert.throws(() => testConfig({ ABUSE_BLOCK_BASE_SEC: '601', ABUSE_BLOCK_MAX_SEC: '600' }), /ABUSE_BLOCK_BASE_SEC must not exceed ABUSE_BLOCK_MAX_SEC/);
        assert.throws(() => testConfig({ ABUSE_BLOCK_BASE_SEC: '7200' }), /ABUSE_BLOCK_BASE_SEC must not exceed/, 'against the default maximum of 3600');
    });

    it('ABUSE_EXEMPT: addresses and CIDR subnets, parsed at load; an invalid entry stops the start', () => {
        assert.deepEqual(testConfig({ ABUSE_EXEMPT: ' 203.0.113.7 , 2001:db8:12::/48,10.0.0.0/8 ' }).abuseExempt,
            ['203.0.113.7', '2001:db8:12::/48', '10.0.0.0/8']);
        for (const bad of ['school.example.org', '10.0.0.0/33', '2001:db8::/129', '300.1.2.3']) {
            assert.throws(() => testConfig({ ABUSE_EXEMPT: `203.0.113.7,${bad}` }), /ABUSE_EXEMPT/, bad);
        }
    });

    it('check-config warns when IP_MAX_CONNECTIONS is below twice MAX_CONNECTIONS_PER_IP', () => {
        const warns = (env) => configWarnings(testConfig(env), {}).filter((w) => w.startsWith('IP_MAX_CONNECTIONS'));
        assert.deepEqual(warns({}), [], 'the defaults: 128 and 64');
        assert.deepEqual(warns({ IP_MAX_CONNECTIONS: '200', MAX_CONNECTIONS_PER_IP: '100' }), []);
        const [w] = warns({ IP_MAX_CONNECTIONS: '100', MAX_CONNECTIONS_PER_IP: '64' });
        assert.match(w, /IP_MAX_CONNECTIONS \(100\) is below twice MAX_CONNECTIONS_PER_IP \(64\)/);
        assert.match(w, /ABUSE_EXEMPT/, 'names the way out for a known shared address');
    });

    it('.env.example and docs/CONFIG.md are generated from these keys (npm run gen:env)', () => {
        const r = spawnSync(process.execPath, [fileURLToPath(new URL('../../tools/gen-env-example.js', import.meta.url)), '--check'], { encoding: 'utf8' });
        assert.equal(r.status, 0, `run npm run gen:env: ${r.stdout}${r.stderr}`);
        const env = fs.readFileSync(new URL('../../.env.example', import.meta.url), 'utf8');
        const doc = fs.readFileSync(new URL('../../docs/CONFIG.md', import.meta.url), 'utf8');
        assert.match(env, /Protection per address \(background layer\)/);
        assert.match(env, /^# HTTP_RATE_PER_IP=600$/m);
        assert.match(env, /^# MAX_CONNECTIONS_PER_IP=64$/m);
        for (const name of SHIELD_KEYS) assert.ok(env.includes(name) && doc.includes(name), name);
    });
});
