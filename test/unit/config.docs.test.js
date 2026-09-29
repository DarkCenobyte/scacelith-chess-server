// Descriptions in src/config.js (which .env.example and docs/CONFIG.md are generated from) that
// state what the game client does: they are checked against the client's source when it is there
// (the repository holds both; a copy of dedicated-server/ alone skips the test).

import assert from 'node:assert/strict';
import fs from 'node:fs';
import test from 'node:test';
import { CONFIG_KEYS } from '../../src/config.js';

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
