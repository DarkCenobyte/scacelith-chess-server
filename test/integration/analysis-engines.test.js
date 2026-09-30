// The analysis engines seen from outside, on the real server: the analysis process starts
// ANALYSIS_WORKERS engines (a fake UCI engine reporting its network the way Stockfish 19 does),
// logs each start with where the engine keeps its network, and its gauges reach the primary's
// /metrics. Needs the openssl command line (skipped without it).
import test, { before, after } from 'node:test';
import assert from 'node:assert/strict';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { startServer, haveOpenssl } from './helpers/harness.js';

const skip = !haveOpenssl() && 'openssl not available';
const FAKE = fileURLToPath(new URL('../unit/helpers/fake-uci-engine.js', import.meta.url));

let srv, dir;
before(async () => {
    if (skip) return;
    dir = fs.mkdtempSync(path.join(os.tmpdir(), 'scacelith-engine-'));
    const engine = path.join(dir, 'engine');
    fs.writeFileSync(engine, `#!/bin/sh\nexec "${process.execPath}" "${FAKE}" "NNUE evaluation using nn-1a298aa575a0.nnue" "Network replica 1: Shared memory."\n`, { mode: 0o755 });
    srv = await startServer({ workers: 1, env: { ANALYSIS_ENGINE_PATH: engine, ANALYSIS_WORKERS: '2', LOG_LEVEL: 'info' } });
});
after(async () => {
    if (srv) await srv.stop();
    if (dir) fs.rmSync(dir, { recursive: true, force: true });
});

test('every analysis engine start is logged with its network memory, and /metrics counts the engines that share it', { skip }, async () => {
    const starts = [];
    for (const engine of [0, 1]) {
        starts.push(await srv.waitLog((r) => r.msg === 'analysis engine started' && r.engine === engine, 15000));
    }
    for (const s of starts) {
        assert.equal(s.level, 'info');
        assert.deepEqual([s.name, s.net, s.network], ['Fake Engine 1', 'nn-1a298aa575a0.nnue', 'shared memory']);
    }
    let body = '';
    for (const end = Date.now() + 10000; Date.now() < end; await new Promise((r) => setTimeout(r, 100))) {
        body = await srv.metrics();
        if (/scacelith_anticheat_analysis_engines_shared 2/.test(body)) break;
    }
    assert.match(body, /scacelith_anticheat_analysis_engines 2/);
    assert.match(body, /scacelith_anticheat_analysis_engines_shared 2/);
    assert.match(body, /scacelith_anticheat_analysis_games_total/, 'the analysis process\'s other metrics come along');
});
