// The default port (443) and the actionable error of a listen refused on a privileged port
// (net/listeners.js listenHint / ListenError, logged by the worker before it exits).

import assert from 'node:assert/strict';
import { execFileSync, spawnSync } from 'node:child_process';
import fs from 'node:fs';
import { describe, it } from 'node:test';
import { fileURLToPath } from 'node:url';
import { CONFIG_KEYS, testConfig } from '../../src/config.js';
import { publicBaseUrl } from '../../src/auth/index.js';
import { Registry } from '../../src/metrics.js';
import { Listeners, ListenError, listenHint, makeClientIp } from '../../src/net/listeners.js';
import { WsServer } from '../../src/net/ws.js';

const LISTENERS = fileURLToPath(new URL('../../src/net/listeners.js', import.meta.url));

describe('API_PORT', () => {
    it('defaults to 443, with the WebSocket on the same port', () => {
        const c = testConfig();
        assert.equal(c.apiPort, 443);
        assert.equal(c.wsPort, 443);
        assert.equal(c.publicApiPort, 443);
        assert.equal(c.publicWsPort, 443);
        const spec = CONFIG_KEYS.find((k) => k.name === 'API_PORT');
        assert.equal(spec.default, 443);
        assert.match(spec.desc, /443, the HTTPS port: firewalls and proxies let it through; any free port works for a community server/);
    });

    it('leaves the port out of e-mail links on 443, and keeps another one; the Google sign-in origin always has it', () => {
        const c = testConfig({ TLS_MODE: 'native', ALLOW_INSECURE_DEV: '0', TLS_CERT_FILE: '/x/cert.pem', TLS_KEY_FILE: '/x/key.pem', SERVER_PUBLIC_HOST: 'chess.example.org' });
        assert.equal(publicBaseUrl(c), 'https://chess.example.org');
        assert.equal(c.ssoOrigin, 'chess.example.org:443');
        const other = testConfig({ TLS_MODE: 'native', ALLOW_INSECURE_DEV: '0', TLS_CERT_FILE: '/x/cert.pem', TLS_KEY_FILE: '/x/key.pem', SERVER_PUBLIC_HOST: 'chess.example.org', API_PORT: '8443' });
        assert.equal(publicBaseUrl(other), 'https://chess.example.org:8443');
        assert.equal(other.ssoOrigin, 'chess.example.org:8443');
    });

    it('.env.example and docs/CONFIG.md carry the new default', () => {
        const env = fs.readFileSync(new URL('../../.env.example', import.meta.url), 'utf8');
        assert.match(env, /^# API_PORT=443$/m);
        const doc = fs.readFileSync(new URL('../../docs/CONFIG.md', import.meta.url), 'utf8');
        assert.match(doc, /^\| `API_PORT` \| port \(0-65535\) \| `443` \|/m);
    });
});

describe('listen errors on a privileged port', () => {
    const eacces = () => Object.assign(new Error('listen EACCES: permission denied 0.0.0.0:443'), { code: 'EACCES', syscall: 'listen' });

    it('names the three fixes for EACCES / EPERM below 1024', () => {
        for (const code of ['EACCES', 'EPERM']) {
            const hint = listenHint({ code }, 443, '/usr/bin/node');
            assert.ok(hint.startsWith(`Cannot listen on port 443 (${code}): ports below 1024 need the CAP_NET_BIND_SERVICE capability.`), hint);
            assert.ok(hint.includes('AmbientCapabilities=CAP_NET_BIND_SERVICE'), hint);
            assert.ok(hint.includes('CapabilityBoundingSet=CAP_NET_BIND_SERVICE'), hint);
            assert.ok(hint.includes('sudo setcap cap_net_bind_service=+ep /usr/bin/node'), hint);
            assert.ok(hint.includes('sysctl -w net.ipv4.ip_unprivileged_port_start=443'), hint);
            assert.ok(hint.includes('API_PORT'), hint);
        }
        assert.ok(listenHint({ code: 'EACCES' }, 80).includes('ip_unprivileged_port_start=80'));
    });

    it('has no advice for other errors or unprivileged ports', () => {
        assert.equal(listenHint({ code: 'EADDRINUSE' }, 443), null);
        assert.equal(listenHint({ code: 'EACCES' }, 1024), null);
        assert.equal(listenHint({ code: 'EACCES' }, 8443), null);
        assert.equal(listenHint({ code: 'EACCES' }, 0), null);
        assert.equal(listenHint(null, 443), null);
    });

    function listenersOn(port) {
        const config = { ...testConfig(), tlsMode: 'proxy', bindAddress: '127.0.0.1', apiPort: port, wsPort: port };
        const wss = new WsServer({ registry: new Registry(), clientIp: makeClientIp(config) });
        return new Listeners({ config, wsServer: wss, apiHandler: (req, res) => res.end() });
    }

    it('Listeners.listen() rejects with a ListenError carrying the hint, the code, the port and the cause', async () => {
        const lst = listenersOn(443);
        const err0 = eacces();
        lst.servers[0].server.listen = function () { process.nextTick(() => this.emit('error', err0)); return this; };
        const err = await lst.listen().then(() => null, (e) => e);
        assert.ok(err instanceof ListenError);
        assert.equal(err.name, 'ListenError');
        assert.equal(err.code, 'EACCES');
        assert.equal(err.port, 443);
        assert.equal(err.cause, err0);
        assert.ok(err.message.includes('CAP_NET_BIND_SERVICE'));
    });

    it('Listeners.listen() passes other errors through unchanged', async () => {
        const lst = listenersOn(443);
        const busy = Object.assign(new Error('listen EADDRINUSE'), { code: 'EADDRINUSE' });
        lst.servers[0].server.listen = function () { process.nextTick(() => this.emit('error', busy)); return this; };
        assert.equal(await lst.listen().then(() => null, (e) => e), busy);
    });

    // A real refusal: an unprivileged process (nobody) binding 443. Needs root to drop to nobody,
    // setpriv, and the kernel's default ip_unprivileged_port_start (1024).
    const canDrop = (() => {
        try {
            if (process.getuid?.() !== 0) return false;
            if (Number(fs.readFileSync('/proc/sys/net/ipv4/ip_unprivileged_port_start', 'utf8')) <= 443) return false;
            execFileSync('setpriv', ['--version'], { stdio: 'ignore' });
            return true;
        } catch { return false; }
    })();
    it('an unprivileged process gets the hint from a real bind of 443', { skip: !canDrop && 'needs root, setpriv and ip_unprivileged_port_start > 443' }, () => {
        const script = `import(${JSON.stringify(LISTENERS)}).then(async (m) => {
            const { createServer } = await import('node:net');
            const s = createServer();
            s.on('error', (e) => { process.stdout.write(JSON.stringify({ code: e.code, hint: m.listenHint(e, 443) })); });
            s.listen(443, '127.0.0.1', () => { process.stdout.write(JSON.stringify({ bound: true })); s.close(); });
        });`;
        const r = spawnSync('setpriv', ['--reuid=65534', '--regid=65534', '--clear-groups', process.execPath, '-e', script], { encoding: 'utf8', timeout: 20000 });
        const out = JSON.parse(r.stdout);
        assert.equal(out.code, 'EACCES');
        assert.ok(out.hint.includes('Cannot listen on port 443 (EACCES)'));
        assert.ok(out.hint.includes(`setcap cap_net_bind_service=+ep ${process.execPath}`));
    });
});
