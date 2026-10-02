// Outgoing e-mail. MAIL_TRANSPORT selects the transport:
//   smtp  the SMTP_* settings (src/mail/smtp.js), one connection per message
//   log   the message is written to the log (development: the verification link is there)
//   none  nothing is sent
// Sending never blocks a request: send() queues the message and returns a promise that resolves
// to true/false once it is handled (it never rejects); callers normally do not await it. The
// queue is bounded and at most `concurrency` messages are in flight.

import { buildMessage, isValidAddress } from './message.js';
import { sendSmtp } from './smtp.js';
import { templates } from './templates.js';
import { metrics } from '../metrics.js';

/**
 * Text of a secretText setting (SMTP_PASSWORD, GOOGLE_CLIENT_SECRET), which config.js keeps as
 * written. A Buffer holding UTF-8 text is also accepted.
 * @param {string|Buffer|null} v
 * @returns {string}
 */
export function secretText(v) {
    if (v == null) return '';
    return typeof v === 'string' ? v : Buffer.from(v).toString('utf8');
}

const sentTotal = metrics.counter('scacelith_mail_messages_total', 'E-mails handled', ['result']);
const sentOk = sentTotal.labels('sent');
const sentFailed = sentTotal.labels('failed');
const sentDropped = sentTotal.labels('dropped');

/**
 * @param {{ config: object, log: object, transport?: { send(msg: { from: string, to: string, raw: string,
 *           subject: string, text: string }): Promise<void> }, concurrency?: number, maxQueue?: number,
 *           smtpOptions?: object }} opts
 *   `transport` replaces the configured one (tests); `smtpOptions` is merged into sendSmtp's options.
 * @returns {{ send(m: { to: string, subject: string, text: string }): Promise<boolean>,
 *             sendTemplate(name: string, to: string, vars: object): Promise<boolean>,
 *             idle(): Promise<void>, kind: string }}
 */
export function createMailer({ config, log, transport, concurrency = 2, maxQueue = 500, smtpOptions = {} }) {
    const kind = transport ? 'custom' : config.mailTransport;
    if (config.requireEmailVerification && kind === 'log') {
        log.warn('MAIL_TRANSPORT=log: verification and reset e-mails are only written to the log. Configure SMTP before opening the server to players.');
    }
    if (config.requireEmailVerification && kind === 'none') {
        log.warn('MAIL_TRANSPORT=none with REQUIRE_EMAIL_VERIFICATION: new accounts cannot be confirmed and passwords cannot be reset.');
    }

    const impl = transport || {
        async send(msg) {
            if (kind === 'none') return;
            if (kind === 'log') {
                log.info('mail (log transport)', { to: msg.to, subject: msg.subject, text: msg.text });
                return;
            }
            await sendSmtp({
                host: config.smtpHost, port: config.smtpPort, security: config.smtpSecurity,
                user: config.smtpUser, password: secretText(config.smtpPassword),
                from: msg.from, to: msg.to, raw: msg.raw, ...smtpOptions,
            });
        },
    };

    const queue = [];
    let active = 0;
    let idleWaiters = [];

    function checkIdle() {
        if (active === 0 && queue.length === 0 && idleWaiters.length) {
            const w = idleWaiters;
            idleWaiters = [];
            for (const f of w) f();
        }
    }

    function pump() {
        while (active < concurrency && queue.length) {
            const job = queue.shift();
            active++;
            Promise.resolve()
                .then(() => impl.send(job.msg))
                .then(() => { sentOk.inc(); job.done(true); },
                    (err) => {
                        sentFailed.inc();
                        log.error('e-mail not sent', { template: job.template, err: { name: err.name, code: err.code, message: err.message } });
                        job.done(false);
                    })
                .finally(() => { active--; pump(); checkIdle(); });
        }
    }

    function send({ to, subject, text, template = 'custom' }) {
        return new Promise((resolve) => {
            if (!isValidAddress(to)) { log.warn('e-mail not sent: invalid address', { template }); resolve(false); return; }
            if (queue.length >= maxQueue) { sentDropped.inc(); log.warn('e-mail queue full, message dropped', { template }); resolve(false); return; }
            let built;
            try {
                built = buildMessage({ from: config.mailFrom, to, subject, text });
            } catch (err) {
                log.error('e-mail not built', { template, err: { code: err.code, message: err.message } });
                resolve(false);
                return;
            }
            queue.push({ template, msg: { from: built.from, to: built.to, raw: built.raw, subject, text }, done: resolve });
            pump();
        });
    }

    function sendTemplate(name, to, vars) {
        const t = templates[name];
        if (!t) throw new Error(`mail: unknown template ${name}`);
        const { subject, text } = t({ serverName: config.serverName, ...vars });
        return send({ to, subject, text, template: name });
    }

    return {
        kind,
        send,
        sendTemplate,
        /** Resolves when every queued message has been handled (tests, shutdown). */
        idle() {
            if (active === 0 && queue.length === 0) return Promise.resolve();
            return new Promise((r) => idleWaiters.push(r));
        },
    };
}
