import test from 'node:test';
import assert from 'node:assert/strict';
import { buildMessage, encodeHeaderText, parseMailbox, quotedPrintable, rfc5322Date } from '../../src/mail/message.js';
import { createMailer } from '../../src/mail/index.js';
import { templates } from '../../src/mail/templates.js';
import { testConfig } from '../../src/config.js';
import { capturedLogs } from './helpers/auth-fakes.js';
import { logger } from '../../src/log.js';

function headers(raw) {
    const [head] = raw.split('\r\n\r\n');
    const out = {};
    for (const l of head.replace(/\r\n /g, ' ').split('\r\n')) { const i = l.indexOf(':'); out[l.slice(0, i)] = l.slice(i + 2); }
    return out;
}
function decodeWords(s) {
    return s.replace(/=\?UTF-8\?B\?([^?]+)\?=\s?/g, (_, b) => Buffer.from(b, 'base64').toString('utf8'));
}
function decodeQp(s) {
    const bytes = [];
    const text = s.replace(/=\r\n/g, '');
    for (let i = 0; i < text.length; i++) {
        if (text[i] === '=') { bytes.push(parseInt(text.slice(i + 1, i + 3), 16)); i += 2; } else bytes.push(text.charCodeAt(i));
    }
    return Buffer.from(bytes).toString('utf8');
}

test('RFC 5322 headers of an ASCII message (7bit)', () => {
    const m = buildMessage({ from: 'Scacelith <no-reply@chess.example.org>', to: 'alice@example.com', subject: 'Confirm', text: 'Hello\nworld', date: new Date(Date.UTC(2026, 8, 28, 14, 3, 5)) });
    const h = headers(m.raw);
    assert.equal(h.From, '"Scacelith" <no-reply@chess.example.org>');
    assert.equal(h.To, 'alice@example.com');
    assert.equal(h.Subject, 'Confirm');
    assert.equal(h.Date, 'Mon, 28 Sep 2026 14:03:05 +0000');
    assert.match(h['Message-ID'], /^<[0-9a-f]{32}@chess\.example\.org>$/);
    assert.equal(h['MIME-Version'], '1.0');
    assert.equal(h['Content-Type'], 'text/plain; charset=utf-8');
    assert.equal(h['Content-Transfer-Encoding'], '7bit');
    assert.ok(m.raw.endsWith('\r\n\r\nHello\r\nworld'));
    assert.equal(m.from, 'no-reply@chess.example.org');
    assert.equal(rfc5322Date(new Date(0)), 'Thu, 01 Jan 1970 00:00:00 +0000');
});

test('UTF-8 subject as encoded words, UTF-8 body as quoted-printable', () => {
    const subject = 'Échec et mat ♔ '.repeat(6).trim();
    const text = 'Voilà un message très long avec des caractères accentués et un = signe, ' + 'é'.repeat(80) + '\nfin ';
    const m = buildMessage({ from: 'Échecs <no-reply@example.org>', to: 'bob@example.com', subject, text });
    const h = headers(m.raw);
    assert.equal(decodeWords(h.Subject), subject);
    assert.match(h.From, /^=\?UTF-8\?B\?/);
    assert.equal(h['Content-Transfer-Encoding'], 'quoted-printable');
    const body = m.raw.split('\r\n\r\n').slice(1).join('\r\n\r\n');
    for (const l of body.split('\r\n')) assert.ok(l.length <= 76, `line too long: ${l.length}`);
    assert.ok(/^[\x20-\x7e\r\n]*$/.test(m.raw), 'the message is pure ASCII');
    assert.equal(decodeQp(body), text.replace(/\n/g, '\r\n'));
    assert.equal(decodeQp(quotedPrintable('a \r\nb\t')), 'a \r\nb\t');
});

test('header injection and bad addresses are refused', () => {
    assert.throws(() => buildMessage({ from: 'a@example.org', to: 'x@example.com', subject: 'hi\r\nBcc: evil@example.com', text: '' }), /line break/);
    assert.throws(() => buildMessage({ from: 'a@example.org', to: 'x@example.com\r\nBcc: e@x.com', text: '', subject: '' }));
    assert.throws(() => parseMailbox('not an address'));
    assert.throws(() => encodeHeaderText('a\nb'));
    assert.deepEqual(parseMailbox('"Chess Club" <club@example.org>'), { name: 'Chess Club', address: 'club@example.org' });
});

test('templates: subjects and links, no secrets', () => {
    const v = templates.verification({ serverName: 'S', username: 'alice', link: 'https://h/verify-email?token=T', hours: 24 });
    assert.match(v.subject, /Confirm your e-mail address/);
    assert.match(v.text, /https:\/\/h\/verify-email\?token=T/);
    assert.match(templates.passwordReset({ serverName: 'S', username: 'a', link: 'L', minutes: 60 }).text, /valid for 60 minutes/);
    assert.match(templates.registrationAttempt({ serverName: 'S', username: 'a' }).subject, /Someone tried to register/);
    assert.match(templates.mfaDisabled({ serverName: 'S', username: 'a', when: new Date(0) }).subject, /turned off/);
    assert.match(templates.passwordChanged({ serverName: 'S', username: 'a', when: new Date(0), byReset: true }).text, /reset with an e-mail link/);
});

test('mailer: queue, custom transport, failures logged, never rejects', async () => {
    const config = testConfig({ SERVER_NAME: 'Test Server', MAIL_FROM: 'Test <no-reply@example.org>' });
    const got = [];
    let fail = false;
    const log = logger.child('mail-test');
    const m = createMailer({ config, log, transport: { async send(msg) { if (fail) throw Object.assign(new Error('boom'), { code: 'rejected' }); got.push(msg); } } });
    assert.equal(await m.sendTemplate('verification', 'alice@example.com', { username: 'alice', link: 'https://x/verify-email?token=abc', hours: 24 }), true);
    assert.equal(got.length, 1);
    assert.equal(got[0].to, 'alice@example.com');
    assert.match(got[0].raw, /Subject: Confirm your e-mail address for Test Server/);
    assert.equal(await m.send({ to: 'not-an-address', subject: 's', text: 't' }), false);
    fail = true;
    assert.equal(await m.send({ to: 'bob@example.com', subject: 's', text: 't' }), false);
    assert.ok(capturedLogs.some((l) => l.includes('e-mail not sent')));
    await m.idle();
    assert.throws(() => m.sendTemplate('nope', 'a@example.com', {}));
});

test('log transport writes the message and warns when e-mail confirmation depends on it', async () => {
    const before = capturedLogs.length;
    const config = testConfig({ MAIL_TRANSPORT: 'log', REQUIRE_EMAIL_VERIFICATION: '1' });
    const m = createMailer({ config, log: logger.child('mail-test') });
    assert.equal(m.kind, 'log');
    const lines = capturedLogs.slice(before);
    assert.ok(lines.some((l) => l.includes('MAIL_TRANSPORT=log')));
    await m.send({ to: 'alice@example.com', subject: 'Hi', text: 'link: https://x/verify-email?token=abc' });
    assert.ok(capturedLogs.slice(before).some((l) => l.includes('mail (log transport)') && l.includes('verify-email?token=abc')));
});
