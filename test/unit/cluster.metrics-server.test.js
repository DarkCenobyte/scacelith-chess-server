import test from 'node:test';
import assert from 'node:assert/strict';
import http from 'node:http';
import { testConfig } from '../../src/config.js';
import { createMetricsServer } from '../../src/cluster/metrics-server.js';

async function start(t, token) {
    const config = { ...testConfig(token === undefined ? {} : { METRICS_TOKEN: token }), metricsPort: 0, metricsBind: '127.0.0.1' };
    const srv = createMetricsServer({ config, ready: () => true, collect: async () => [] });
    const port = await srv.listen();
    t.after(() => srv.close());
    return (method, path, bearer) => new Promise((resolve, reject) => {
        const headers = bearer === undefined ? {} : { Authorization: `Bearer ${bearer}` };
        const req = http.request({ host: '127.0.0.1', port, method, path, headers }, (res) => {
            res.resume();
            res.on('end', () => resolve({ status: res.statusCode, allow: res.headers.allow }));
        });
        req.on('error', reject);
        req.end();
    });
}

test('METRICS_TOKEN: only the exact text is accepted', async (t) => {
    const get = await start(t, 'hunter2');
    assert.equal((await get('GET', '/metrics', 'hunter2')).status, 200);
    assert.equal((await get('HEAD', '/metrics', 'hunter2')).status, 200);
    // The same bytes once decoded as base64, which a decoding comparison accepted.
    for (const b of ['hunter2!', 'hunter2?', 'hunter3', 'hunter']) assert.equal((await get('GET', '/metrics', b)).status, 401, b);
    assert.equal((await get('GET', '/metrics')).status, 401);
    assert.equal((await get('GET', '/healthz')).status, 200, 'health needs no token');
});

test('METRICS_TOKEN: a hex token is compared as written', async (t) => {
    const hex = 'a1b2c3d4e5f60718293a4b5c6d7e8f90';
    const get = await start(t, hex);
    assert.equal((await get('GET', '/metrics', hex)).status, 200);
    assert.equal((await get('GET', '/metrics', Buffer.from(hex, 'hex').toString('base64'))).status, 401, 'another spelling of the bytes');
    assert.equal((await get('GET', '/metrics', hex.toUpperCase())).status, 401);
});

test('METRICS_TOKEN: a token without base64 characters does not open the endpoint', async (t) => {
    const get = await start(t, '!!!!!!!!');
    assert.equal((await get('GET', '/metrics', '.')).status, 401);
    assert.equal((await get('GET', '/metrics', '!!!!!!!!')).status, 200);
});

test('without METRICS_TOKEN /metrics is open; other methods get 405 with every allowed method', async (t) => {
    const get = await start(t);
    assert.equal((await get('GET', '/metrics')).status, 200);
    const r = await get('POST', '/metrics');
    assert.deepEqual([r.status, r.allow], [405, 'GET, HEAD']);
});
