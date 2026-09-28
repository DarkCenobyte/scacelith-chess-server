import test from 'node:test';
import assert from 'node:assert/strict';
import { execFileSync } from 'node:child_process';
import fs from 'node:fs';
import net from 'node:net';
import os from 'node:os';
import path from 'node:path';
import tls from 'node:tls';
import { dotStuff, sendSmtp, SmtpError } from '../../src/mail/smtp.js';
import { buildMessage } from '../../src/mail/message.js';

// A self-signed certificate for localhost / 127.0.0.1 made with the openssl command line (TLS
// tests are skipped when it is not installed).
function makeCert() {
    try {
        const dir = fs.mkdtempSync(path.join(os.tmpdir(), 'scacelith-smtp-'));
        const key = path.join(dir, 'key.pem'), cert = path.join(dir, 'cert.pem');
        execFileSync('openssl', ['req', '-x509', '-newkey', 'rsa:2048', '-nodes', '-keyout', key, '-out', cert, '-days', '1',
            '-subj', '/CN=localhost', '-addext', 'subjectAltName=DNS:localhost,IP:127.0.0.1'], { stdio: 'ignore' });
        const out = { key: fs.readFileSync(key), cert: fs.readFileSync(cert) };
        fs.rmSync(dir, { recursive: true, force: true });
        return out;
    } catch {
        return null;
    }
}
const CERT = makeCert();

/**
 * Fake SMTP server. Options: ext (EHLO keywords), starttls (offer it), implicitTls, inject
 * (bytes sent right after the STARTTLS 220), rcptCode, silent (never greets), credentials.
 */
async function fakeSmtp(opts = {}) {
    const { ext = ['8BITMIME', 'AUTH PLAIN LOGIN'], starttls = false, implicitTls = false, inject = '', rcptCode = 250,
        silent = false, user = 'mailer', pass = 'p4ss w0rd' } = opts;
    const rec = { commands: [], messages: [], auth: null, secureAtMail: null };
    const onConn = (sock0) => {
        let secure = implicitTls;
        let buf = '', mode = 'cmd', data = [], authStep = null, authUser = null;
        const attach = (sock) => {
            const write = (s) => sock.write(s + '\r\n');
            if (!silent && !attach.greeted) { attach.greeted = true; write('220 fake.test ESMTP ready'); }
            sock.on('error', () => {});
            sock.on('data', (d) => {
                buf += d.toString('latin1');
                let i;
                while ((i = buf.indexOf('\r\n')) >= 0) {
                    const line = buf.slice(0, i);
                    buf = buf.slice(i + 2);
                    if (mode === 'data') {
                        if (line === '.') { rec.messages.push(data.join('\r\n')); data = []; mode = 'cmd'; write('250 2.0.0 queued as 42'); } else data.push(line);
                        continue;
                    }
                    if (authStep === 'user') { authUser = Buffer.from(line, 'base64').toString(); authStep = 'pass'; write('334 UGFzc3dvcmQ6'); continue; }
                    if (authStep === 'pass') {
                        const p = Buffer.from(line, 'base64').toString();
                        authStep = null;
                        rec.auth = { mech: 'LOGIN', user: authUser, pass: p };
                        write(authUser === user && p === pass ? '235 2.7.0 ok' : '535 5.7.8 bad credentials');
                        continue;
                    }
                    rec.commands.push(line);
                    const verb = line.split(' ')[0].toUpperCase();
                    if (verb === 'EHLO') {
                        const kws = [...ext, ...(starttls && !secure ? ['STARTTLS'] : [])];
                        const lines = ['fake.test greets you', ...kws];
                        lines.forEach((l, k) => write(`250${k === lines.length - 1 ? ' ' : '-'}${l}`));
                    } else if (verb === 'STARTTLS') {
                        sock.removeAllListeners('data');
                        sock.write('220 2.0.0 go ahead\r\n' + inject);
                        if (inject) return;
                        const s = new tls.TLSSocket(sock, { isServer: true, secureContext: tls.createSecureContext({ key: CERT.key, cert: CERT.cert }) });
                        secure = true;
                        attach(s);
                        return;
                    } else if (verb === 'AUTH') {
                        const [, mech, arg] = line.split(' ');
                        if (mech === 'PLAIN') {
                            const [, u, p] = Buffer.from(arg, 'base64').toString().split('\u0000');
                            rec.auth = { mech: 'PLAIN', user: u, pass: p };
                            write(u === user && p === pass ? '235 2.7.0 ok' : '535 5.7.8 bad credentials');
                        } else if (mech === 'LOGIN') { authStep = 'user'; write('334 VXNlcm5hbWU6'); }
                        else write('504 unrecognized');
                    } else if (verb === 'MAIL') { rec.secureAtMail = secure; write('250 2.1.0 ok'); }
                    else if (verb === 'RCPT') write(rcptCode === 250 ? '250 2.1.5 ok' : `${rcptCode} 5.1.1 no such user`);
                    else if (verb === 'DATA') { mode = 'data'; write('354 end with <CRLF>.<CRLF>'); }
                    else if (verb === 'QUIT') { write('221 bye'); sock.end(); }
                    else write('502 5.5.2 unknown command');
                }
            });
        };
        attach(sock0);
    };
    const server = implicitTls ? tls.createServer({ key: CERT.key, cert: CERT.cert }, onConn) : net.createServer(onConn);
    await new Promise((r) => server.listen(0, '127.0.0.1', r));
    return { rec, port: server.address().port, close: () => new Promise((r) => { server.close(r); setImmediate(() => server.emit('close')); }) };
}

const MSG = buildMessage({ from: 'Scacelith <no-reply@chess.example.org>', to: 'alice@example.com', subject: 'Hello', text: 'line one\n.starts with a dot\n..two dots\nend' });
const base = (port) => ({ host: '127.0.0.1', port, from: MSG.from, to: MSG.to, raw: MSG.raw, timeoutMs: 3000, heloName: 'test.local' });

test('dot stuffing and terminator', () => {
    assert.equal(dotStuff('a\n.b\r\n..c'), 'a\r\n..b\r\n...c\r\n.\r\n');
    assert.equal(dotStuff('x\r\n'), 'x\r\n.\r\n');
});

test('plain relay with AUTH PLAIN: full dialogue, dot stuffing', async () => {
    const s = await fakeSmtp();
    try {
        const r = await sendSmtp({ ...base(s.port), security: 'none', user: 'mailer', password: 'p4ss w0rd' });
        assert.equal(r.code, 250);
        assert.deepEqual(s.rec.commands.map((c) => c.split(' ')[0]), ['EHLO', 'AUTH', 'MAIL', 'RCPT', 'DATA', 'QUIT']);
        assert.equal(s.rec.commands[0], 'EHLO test.local');
        assert.deepEqual(s.rec.auth, { mech: 'PLAIN', user: 'mailer', pass: 'p4ss w0rd' });
        assert.equal(s.rec.commands[2], 'MAIL FROM:<no-reply@chess.example.org>');
        assert.equal(s.rec.commands[3], 'RCPT TO:<alice@example.com>');
        const got = s.rec.messages[0];
        // The server sees the stuffed lines; un-stuffing gives the original body back.
        assert.match(got, /\r\n\.\.starts with a dot\r\n\.\.\.two dots\r\n/);
        assert.equal(got.split('\r\n').map((l) => (l.startsWith('.') ? l.slice(1) : l)).join('\r\n'), MSG.raw);
    } finally { await s.close(); }
});

test('AUTH LOGIN when PLAIN is not offered; no AUTH without credentials', async () => {
    const s = await fakeSmtp({ ext: ['AUTH LOGIN'] });
    try {
        await sendSmtp({ ...base(s.port), security: 'none', user: 'mailer', password: 'p4ss w0rd' });
        assert.deepEqual(s.rec.auth, { mech: 'LOGIN', user: 'mailer', pass: 'p4ss w0rd' });
    } finally { await s.close(); }
    const t = await fakeSmtp();
    try {
        await sendSmtp({ ...base(t.port), security: 'none' });
        assert.equal(t.rec.auth, null);
    } finally { await t.close(); }
});

test('wrong credentials and refused recipients are errors', async () => {
    const s = await fakeSmtp();
    try {
        await assert.rejects(sendSmtp({ ...base(s.port), security: 'none', user: 'mailer', password: 'nope' }), (e) => e instanceof SmtpError && e.code === 'rejected');
        assert.equal(s.rec.messages.length, 0);
    } finally { await s.close(); }
    const t = await fakeSmtp({ rcptCode: 550 });
    try {
        await assert.rejects(sendSmtp({ ...base(t.port), security: 'none' }), (e) => e.code === 'rejected' && /550/.test(e.message));
    } finally { await t.close(); }
});

test('STARTTLS required: refused when the server does not offer it', async () => {
    const s = await fakeSmtp({ starttls: false });
    try {
        await assert.rejects(sendSmtp({ ...base(s.port), security: 'starttls', user: 'mailer', password: 'p4ss w0rd' }), (e) => e.code === 'starttls_unavailable');
        assert.deepEqual(s.rec.commands.map((c) => c.split(' ')[0]), ['EHLO'], 'no credentials or message sent in clear');
    } finally { await s.close(); }
});

test('STARTTLS upgrade, then EHLO again, AUTH and the message over TLS', { skip: !CERT && 'openssl not available' }, async () => {
    const s = await fakeSmtp({ starttls: true });
    try {
        await sendSmtp({ ...base(s.port), security: 'starttls', user: 'mailer', password: 'p4ss w0rd', tlsOptions: { ca: CERT.cert } });
        assert.deepEqual(s.rec.commands.map((c) => c.split(' ')[0]), ['EHLO', 'STARTTLS', 'EHLO', 'AUTH', 'MAIL', 'RCPT', 'DATA', 'QUIT']);
        assert.equal(s.rec.secureAtMail, true);
        assert.equal(s.rec.messages.length, 1);
    } finally { await s.close(); }
});

test('STARTTLS: an untrusted certificate is refused', { skip: !CERT && 'openssl not available' }, async () => {
    const s = await fakeSmtp({ starttls: true });
    try {
        await assert.rejects(sendSmtp({ ...base(s.port), security: 'starttls' }), (e) => e.code === 'tls');
        assert.equal(s.rec.messages.length, 0);
    } finally { await s.close(); }
});

test('STARTTLS response injection is detected', { skip: !CERT && 'openssl not available' }, async () => {
    const s = await fakeSmtp({ starttls: true, inject: '250 injected\r\n' });
    try {
        await assert.rejects(sendSmtp({ ...base(s.port), security: 'starttls', tlsOptions: { ca: CERT.cert } }), (e) => e.code === 'protocol');
    } finally { await s.close(); }
});

test('implicit TLS (port 465 style)', { skip: !CERT && 'openssl not available' }, async () => {
    const s = await fakeSmtp({ implicitTls: true });
    try {
        await sendSmtp({ ...base(s.port), security: 'tls', user: 'mailer', password: 'p4ss w0rd', tlsOptions: { ca: CERT.cert } });
        assert.equal(s.rec.secureAtMail, true);
        assert.equal(s.rec.messages.length, 1);
    } finally { await s.close(); }
});

test('a silent server times out', async () => {
    const s = await fakeSmtp({ silent: true });
    try {
        const t0 = Date.now();
        await assert.rejects(sendSmtp({ ...base(s.port), security: 'none', timeoutMs: 200 }), (e) => e.code === 'timeout');
        assert.ok(Date.now() - t0 < 2000);
    } finally { await s.close(); }
});

test('connection refused is an error', async () => {
    const srv = net.createServer();
    await new Promise((r) => srv.listen(0, '127.0.0.1', r));
    const port = srv.address().port;
    await new Promise((r) => srv.close(r));
    await assert.rejects(sendSmtp({ ...base(port), security: 'none' }), (e) => e.code === 'connection');
});
