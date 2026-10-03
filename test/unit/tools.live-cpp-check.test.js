// tools/live-cpp-check.js, its sso part run with a stand-in for the C++ test: that part's server
// logs in the harness's own process, so the harness prints the warnings and errors it logged (the
// reason of a failed Google sign-in is only in the log: the client gets a bare 502 sso_failed).

import assert from 'node:assert/strict';
import { spawnSync } from 'node:child_process';
import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';
import test from 'node:test';
import { fileURLToPath } from 'node:url';

const TOOL = fileURLToPath(new URL('../../tools/live-cpp-check.js', import.meta.url));
const CLIENT = new URL('../../src/client/index.js', import.meta.url).href;

test('a failed sso part prints the warnings and errors its server logged', (t) => {
    const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'scacelith-live-check-'));
    t.after(() => fs.rmSync(dir, { recursive: true, force: true }));
    // As `scacelith_tests net_live_sso`: a Google sign-in finished with a state that is not its own.
    const fake = path.join(dir, 'fake-cpp.mjs');
    fs.writeFileSync(fake, `
import crypto from 'node:crypto';
import { ApiClient } from ${JSON.stringify(CLIENT)};
const port = Number(process.env.SCACELITH_NET_LIVE_SSO.split(':')[1]);
const api = new ApiClient({ host: '127.0.0.1', port, insecure: true });
const verifier = 'v'.repeat(43);
const start = await api.post('/auth/sso/google/start', { codeChallenge: crypto.createHash('sha256').update(verifier).digest('base64url'), redirectPort: 50000 });
const finish = await api.post('/auth/sso/google/finish', { attemptId: start.body.attemptId, codeVerifier: verifier, state: 'x'.repeat(43), code: 'code' });
console.log('fake finish', finish.status, finish.body.error);
api.close();
process.exit(1);
`);
    const env = { ...process.env };
    delete env.LIVE_HOST;
    const r = spawnSync(process.execPath, [TOOL, '--only=sso', process.execPath, fake], { cwd: dir, env, encoding: 'utf8', timeout: 60000 });
    const out = r.stdout + r.stderr;
    assert.equal(r.status, 1, out);
    assert.match(out, /fake finish 502 sso_failed/, out);
    assert.match(out, /\[sso\] server warnings and errors logged:/, out);
    const logged = out.split('\n').filter((l) => l.startsWith('{')).map((l) => JSON.parse(l));
    assert.ok(logged.some((l) => l.level === 'warn' && l.msg === 'google sign-in failed' && l.reason === 'state_mismatch'), out);
    assert.ok(logged.every((l) => l.level === 'warn' || l.level === 'error'), out);
    assert.match(out, /== sso: FAILED/, out);
});
