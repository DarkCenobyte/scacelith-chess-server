// Minimal SMTP client (RFC 5321) for the server's notification e-mails: one connection per
// message, EHLO, STARTTLS (required, never opportunistic, when security = 'starttls'), implicit
// TLS (security = 'tls', port 465), AUTH PLAIN or LOGIN, MAIL FROM / RCPT TO / DATA with dot
// stuffing, QUIT. Every step has a timeout. Messages are 7bit or quoted-printable (message.js),
// so neither 8BITMIME nor SMTPUTF8 is needed.
//
// STARTTLS response injection: bytes received after the 220 answer to STARTTLS and before the TLS
// handshake are an attack (or a broken server); the client aborts.

import net from 'node:net';
import os from 'node:os';
import tls from 'node:tls';
import { MailError } from './message.js';

export class SmtpError extends MailError {
    constructor(code, message, reply) { super(code, message); this.reply = reply || null; }
}

class Conn {
    constructor(socket, timeoutMs) {
        this.timeoutMs = timeoutMs;
        this.buf = '';
        this.lines = [];
        this.waiter = null;
        this.error = null;
        this.attach(socket);
    }
    attach(socket) {
        this.socket = socket;
        socket.setTimeout(this.timeoutMs);
        this.onData = (d) => this.feed(d);
        this.onError = (e) => this.fail(new SmtpError('connection', e.message));
        this.onClose = () => this.fail(new SmtpError('connection', 'connection closed by the server'));
        this.onTimeout = () => { this.fail(new SmtpError('timeout', 'SMTP server timeout')); socket.destroy(); };
        socket.on('data', this.onData);
        socket.on('error', this.onError);
        socket.on('close', this.onClose);
        socket.on('timeout', this.onTimeout);
    }
    detach() {
        const s = this.socket;
        s.off('data', this.onData);
        s.off('error', this.onError);
        s.off('close', this.onClose);
        s.off('timeout', this.onTimeout);
        s.setTimeout(0);
        return s;
    }
    feed(d) {
        this.buf += typeof d === 'string' ? d : d.toString('latin1');
        if (this.buf.length > 65536) { this.fail(new SmtpError('protocol', 'reply too long')); this.socket.destroy(); return; }
        let i;
        while ((i = this.buf.indexOf('\n')) >= 0) {
            this.lines.push(this.buf.slice(0, i).replace(/\r$/, ''));
            this.buf = this.buf.slice(i + 1);
        }
        // More than 512 lines: a server that never sends the last line of a reply (each chunk resets
        // the idle timeout) would be read forever.
        if (this.lines.length > 512) { this.fail(new SmtpError('protocol', 'reply too long')); this.socket.destroy(); return; }
        this.pump();
    }
    fail(err) {
        if (!this.error) this.error = err;
        this.pump();
    }
    // A complete reply: lines "ddd-text" ... "ddd text" (or a bare "ddd").
    takeReply() {
        for (let k = 0; k < this.lines.length; k++) {
            const l = this.lines[k];
            if (!/^\d{3}([ -]|$)/.test(l)) return { error: new SmtpError('protocol', 'malformed reply') };
            if (l.length === 3 || l[3] === ' ') {
                const lines = this.lines.splice(0, k + 1);
                return { reply: { code: +lines[0].slice(0, 3), lines: lines.map((x) => x.slice(4)) } };
            }
        }
        return null;
    }
    pump() {
        if (!this.waiter) return;
        const r = this.takeReply();
        const w = this.waiter;
        if (r) { this.waiter = null; if (r.error) w.reject(r.error); else w.resolve(r.reply); return; }
        if (this.error) { this.waiter = null; w.reject(this.error); }
    }
    read() {
        return new Promise((resolve, reject) => { this.waiter = { resolve, reject }; this.pump(); });
    }
    write(s) { this.socket.write(s, 'latin1'); }
    async cmd(line, expect, label) {
        this.write(line + '\r\n');
        const r = await this.read();
        if (!expect.includes(r.code)) throw new SmtpError('rejected', `${label || line.split(' ')[0]}: ${r.code} ${r.lines.join(' ')}`.slice(0, 300), r);
        return r;
    }
    hasPending() { return this.buf.length > 0 || this.lines.length > 0; }
}

function connectPlain(host, port, timeoutMs) {
    return new Promise((resolve, reject) => {
        const s = net.connect({ host, port });
        const t = setTimeout(() => { s.destroy(); reject(new SmtpError('timeout', 'SMTP connection timeout')); }, timeoutMs);
        s.once('connect', () => { clearTimeout(t); s.off('error', onErr); resolve(s); });
        const onErr = (e) => { clearTimeout(t); reject(new SmtpError('connection', e.message)); };
        s.once('error', onErr);
    });
}

function connectTls(opts, timeoutMs) {
    return new Promise((resolve, reject) => {
        const s = tls.connect(opts);
        const t = setTimeout(() => { s.destroy(); reject(new SmtpError('timeout', 'TLS handshake timeout')); }, timeoutMs);
        s.once('secureConnect', () => { clearTimeout(t); s.off('error', onErr); resolve(s); });
        const onErr = (e) => { clearTimeout(t); reject(new SmtpError('tls', e.message)); };
        s.once('error', onErr);
    });
}

function extensions(reply) {
    const ext = new Map();
    for (const l of reply.lines.slice(1)) {
        const [k, ...rest] = l.trim().split(/\s+/);
        if (k) ext.set(k.toUpperCase(), rest.map((x) => x.toUpperCase()));
    }
    return ext;
}

/**
 * Dot stuffing (RFC 5321 4.5.2) and CRLF normalisation of a message for DATA.
 * @param {string} raw
 * @returns {string} ready to write, including the final ".\r\n"
 */
export function dotStuff(raw) {
    let s = raw.replace(/\r?\n/g, '\r\n');
    s = s.replace(/^\./gm, '..');
    if (!s.endsWith('\r\n')) s += '\r\n';
    return s + '.\r\n';
}

/**
 * Sends one message.
 * @param {{ host: string, port: number, security: 'starttls'|'tls'|'none', user?: string, password?: string,
 *           from: string, to: string, raw: string, timeoutMs?: number, heloName?: string, tlsOptions?: object }} opts
 *   `from`/`to` are bare envelope addresses; `raw` is the message from buildMessage().
 * @returns {Promise<{ code: number, lines: string[] }>} the server's answer to the message
 */
export async function sendSmtp({ host, port, security = 'starttls', user = '', password = '', from, to, raw,
    timeoutMs = 30000, heloName = os.hostname() || 'localhost', tlsOptions = {} }) {
    const servername = net.isIP(host) ? undefined : host;
    let socket;
    if (security === 'tls') socket = await connectTls({ host, port, servername, minVersion: 'TLSv1.2', ...tlsOptions }, timeoutMs);
    else socket = await connectPlain(host, port, timeoutMs);
    const c = new Conn(socket, timeoutMs);
    const helo = String(heloName).replace(/[^A-Za-z0-9.-]/g, '') || 'localhost';
    try {
        const greet = await c.read();
        if (greet.code !== 220) throw new SmtpError('rejected', `greeting: ${greet.code}`, greet);
        let ehlo = await c.cmd(`EHLO ${helo}`, [250]);
        let ext = extensions(ehlo);
        if (security === 'starttls') {
            if (!ext.has('STARTTLS')) throw new SmtpError('starttls_unavailable', 'the SMTP server does not offer STARTTLS');
            await c.cmd('STARTTLS', [220]);
            if (c.hasPending()) throw new SmtpError('protocol', 'data received before the TLS handshake (STARTTLS injection)');
            const rawSocket = c.detach();
            const secure = await connectTls({ socket: rawSocket, servername, minVersion: 'TLSv1.2', ...tlsOptions }, timeoutMs);
            c.attach(secure);
            ehlo = await c.cmd(`EHLO ${helo}`, [250]);
            ext = extensions(ehlo);
        }
        if (user) {
            const mech = ext.get('AUTH') || [];
            if (mech.includes('PLAIN')) {
                const tok = Buffer.from(`\u0000${user}\u0000${password}`, 'utf8').toString('base64');
                await c.cmd(`AUTH PLAIN ${tok}`, [235], 'AUTH PLAIN');
            } else if (mech.includes('LOGIN')) {
                await c.cmd('AUTH LOGIN', [334]);
                await c.cmd(Buffer.from(user, 'utf8').toString('base64'), [334], 'AUTH LOGIN user');
                await c.cmd(Buffer.from(password, 'utf8').toString('base64'), [235], 'AUTH LOGIN password');
            } else {
                throw new SmtpError('auth_unavailable', 'the SMTP server offers neither AUTH PLAIN nor AUTH LOGIN');
            }
        }
        await c.cmd(`MAIL FROM:<${from}>`, [250]);
        await c.cmd(`RCPT TO:<${to}>`, [250, 251]);
        await c.cmd('DATA', [354]);
        c.write(dotStuff(raw));
        const done = await c.read();
        if (done.code !== 250) throw new SmtpError('rejected', `message: ${done.code} ${done.lines.join(' ')}`.slice(0, 300), done);
        try { await c.cmd('QUIT', [221]); } catch { /* the message is accepted already */ }
        return done;
    } finally {
        c.socket.end();
        setTimeout(() => c.socket.destroy(), 1000).unref();
    }
}
