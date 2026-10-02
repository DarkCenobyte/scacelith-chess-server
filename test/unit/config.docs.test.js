// Descriptions in src/config.js (which .env.example and docs/CONFIG.md are generated from) that
// state what the game client does: they are checked against the client's source when it is there
// (the repository holds both; a copy of dedicated-server/ alone skips the test). Also formulas of
// the server that a description and docs/API.md both state.

import assert from 'node:assert/strict';
import fs from 'node:fs';
import test from 'node:test';
import { CONFIG_KEYS, testConfig } from '../../src/config.js';
import { register as registerGif } from '../../src/http/routes/gif.js';

const CLIENT = new URL('../../../src/net/online_client.cpp', import.meta.url);
const haveClient = fs.existsSync(CLIENT);

test('HEARTBEAT_INTERVAL_MS gives the game client\'s probe and dead-connection bounds', { skip: !haveClient && 'client sources not present' }, () => {
    const src = fs.readFileSync(CLIENT, 'utf8');
    // uint32_t silence = std::max<uint32_t>(10000, std::min<uint32_t>(rt.heartbeatMs, 60000) * 2);
    const m = /silence\s*=\s*std::max<uint32_t>\(\s*(\d+)\s*,\s*std::min<uint32_t>\(\s*rt\.heartbeatMs\s*,\s*(\d+)\s*\)\s*\*\s*2\s*\)/.exec(src);
    assert.ok(m, 'the liveness limit of online_client.cpp changed: update HEARTBEAT_INTERVAL_MS and this test');
    assert.match(src, /quiet > std::chrono::milliseconds\(silence \/ 4 \* 3\)/, 'the probe comes at 3/4 of the limit (1.5 intervals)');
    const floorS = Number(m[1]) / 1000, capS = Number(m[2]) * 2 / 1000;
    const desc = CONFIG_KEYS.find((k) => k.name === 'HEARTBEAT_INTERVAL_MS').desc;
    assert.ok(desc.includes(`dead after twice this (at least ${floorS} s, at most ${capS} s)`), desc);
    assert.ok(desc.includes(`after 1.5 times this with nothing received (at least ${floorS * 0.75} s, at most ${capS * 0.75} s)`), desc);
});

test('USER_RATE_PER_MIN and API.md give the share of the account budget that each worker allows', () => {
    // http/server.js createUserBudget: max(1, min(L, ceil(2 L / WORKERS))); with 1 worker that is L,
    // not 2 L (test/unit/http.quotas.test.js checks the handler: 8 a minute, a burst of 4).
    const desc = CONFIG_KEYS.find((k) => k.name === 'USER_RATE_PER_MIN').desc;
    assert.ok(desc.includes('max(1, min(this, ceil(2 x this / WORKERS)))'), desc);
    const api = fs.readFileSync(new URL('../../docs/API.md', import.meta.url), 'utf8').replace(/\s+/g, ' ');
    assert.ok(api.includes('its share, max(1, min(`USER_RATE_PER_MIN`, ceil(2 x `USER_RATE_PER_MIN` / `WORKERS`))) per minute'));
});

test('HTTP_BODY_LIMIT and API.md name the body limit of POST /gif, which HTTP_BODY_LIMIT does not set', () => {
    let opts = null;
    registerGif({ get: () => {}, post: (p, h, o) => { if (p === '/gif') opts = o; } }, { config: testConfig(), store: {}, log: {}, gifService: {} });
    const bytes = opts.bodyLimit.toLocaleString('en-US');
    assert.equal(bytes, '135,168');
    const desc = CONFIG_KEYS.find((k) => k.name === 'HTTP_BODY_LIMIT').desc;
    assert.ok(desc.includes(`except POST /api/v1/gif, which has its own fixed limit of ${bytes} bytes`), desc);
    const api = fs.readFileSync(new URL('../../docs/API.md', import.meta.url), 'utf8').replace(/\s+/g, ' ');
    assert.ok(api.includes(`\`POST /gif\` has a limit of its own, ${bytes} bytes`));
    assert.ok(api.includes(`a body above ${bytes} bytes`));
});
