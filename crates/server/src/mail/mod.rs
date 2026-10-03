//! Outgoing e-mail. `MAIL_TRANSPORT` selects the transport:
//!
//! * `smtp`: the `SMTP_*` settings ([`smtp`]), one connection per message;
//! * `log`: the message is written to the log (development: the verification link is there);
//! * `none`: nothing is sent.
//!
//! Sending never blocks a request: [`Mailer::send`] queues the message and returns a [`Receipt`]
//! that resolves to true or false once the message is handled (it never fails); callers normally
//! drop it. The queue is bounded (500 messages) and at most 2 messages are in flight.
//!
//! Owner: security. See docs/RUST-PORT.md.

pub mod message;
pub mod smtp;
pub mod templates;

use std::collections::VecDeque;
use std::fmt;
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::pin::Pin;
use std::sync::{Arc, LazyLock};
use std::task::{Context, Poll};
use std::time::Duration;

use parking_lot::Mutex;
use tokio::sync::{Notify, oneshot};

pub use message::{
    BuiltMessage, MailError, Mailbox, MessageParts, build_message, is_valid_address, parse_mailbox,
};
pub use smtp::{SmtpConfig, SmtpError, SmtpErrorKind, SmtpReply, send_smtp};
pub use templates::{Rendered, Template};

use crate::clock::{self, SharedClock};
use crate::config::{Config, MailTransport};
use crate::log::Logger;
use crate::metrics::{self, CounterVec};
use crate::{log_error, log_info, log_warn};

/// Messages waiting at most (in-flight ones not counted).
pub const DEFAULT_MAX_QUEUE: usize = 500;

/// Messages in flight at most.
pub const DEFAULT_CONCURRENCY: usize = 2;

static MESSAGES: LazyLock<CounterVec> =
    LazyLock::new(|| metrics::counter_vec("scacelith_mail_messages_total", "E-mails handled", &["result"]));

/// A message handed to a transport.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutgoingMail {
    /// The bare sender address.
    pub from: String,
    /// The bare recipient address.
    pub to: String,
    /// The complete message.
    pub raw: String,
    /// The subject, as given.
    pub subject: String,
    /// The body, as given.
    pub text: String,
}

/// Why a transport could not send a message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TransportError {
    /// A short code (`connection`, `timeout`, `tls`, `rejected`, ...).
    pub code: String,
    /// A description (never holds credentials).
    pub message: String,
}

impl fmt::Display for TransportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

impl std::error::Error for TransportError {}

impl TransportError {
    fn panicked() -> TransportError {
        TransportError { code: "internal".into(), message: "the transport panicked".into() }
    }
}

impl From<SmtpError> for TransportError {
    fn from(e: SmtpError) -> TransportError {
        TransportError { code: e.code().to_string(), message: e.message }
    }
}

/// The future of a transport's send.
pub type TransportFuture = Pin<Box<dyn Future<Output = Result<(), TransportError>> + Send>>;

/// A transport that replaces the configured one (tests, tools).
pub type CustomTransport = Arc<dyn Fn(OutgoingMail) -> TransportFuture + Send + Sync>;

enum Transport {
    None,
    Log,
    Smtp(Arc<SmtpConfig>),
    Custom(CustomTransport),
}

/// The settings of a [`Mailer`] beyond the configuration.
#[derive(Clone)]
pub struct MailerOptions {
    /// Messages in flight at most.
    pub concurrency: usize,
    /// Messages waiting at most.
    pub max_queue: usize,
    /// Replaces the configured transport.
    pub transport: Option<CustomTransport>,
    /// Replaces the SMTP settings of the configuration (other roots, timeout, EHLO name).
    pub smtp: Option<SmtpConfig>,
    /// The clock of the `Date` headers.
    pub clock: SharedClock,
}

impl Default for MailerOptions {
    fn default() -> MailerOptions {
        MailerOptions {
            concurrency: DEFAULT_CONCURRENCY,
            max_queue: DEFAULT_MAX_QUEUE,
            transport: None,
            smtp: None,
            clock: clock::system(),
        }
    }
}

impl fmt::Debug for MailerOptions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MailerOptions")
            .field("concurrency", &self.concurrency)
            .field("max_queue", &self.max_queue)
            .field("custom_transport", &self.transport.is_some())
            .field("smtp", &self.smtp)
            .finish_non_exhaustive()
    }
}

/// The outcome of a queued message: resolves to true once it is sent, false when it was refused,
/// dropped or failed. Dropping it does not cancel the message.
#[derive(Debug)]
pub struct Receipt(oneshot::Receiver<bool>);

impl Future for Receipt {
    type Output = bool;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<bool> {
        Pin::new(&mut self.0).poll(cx).map(|r| r.unwrap_or(false))
    }
}

struct Job {
    template: &'static str,
    mail: OutgoingMail,
    done: oneshot::Sender<bool>,
}

#[derive(Default)]
struct State {
    queue: VecDeque<Job>,
    active: usize,
}

struct Inner {
    kind: &'static str,
    transport: Transport,
    mail_from: String,
    server_name: String,
    log: Logger,
    clock: SharedClock,
    concurrency: usize,
    max_queue: usize,
    state: Mutex<State>,
    idle: Notify,
}

/// The mail service: a bounded FIFO of messages sent by a configured transport. Cheap to clone.
#[derive(Clone)]
pub struct Mailer {
    inner: Arc<Inner>,
}

impl fmt::Debug for Mailer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let st = self.inner.state.lock();
        f.debug_struct("Mailer")
            .field("kind", &self.inner.kind)
            .field("queued", &st.queue.len())
            .field("active", &st.active)
            .finish()
    }
}

fn resolved(ok: bool) -> Receipt {
    let (tx, rx) = oneshot::channel();
    let _ = tx.send(ok);
    Receipt(rx)
}

impl Mailer {
    /// The mailer of the configuration (`MAIL_TRANSPORT`, `MAIL_FROM`, `SMTP_*`).
    pub fn new(config: &Config, log: Logger) -> Mailer {
        Mailer::with_options(config, log, MailerOptions::default())
    }

    /// The mailer of the configuration with other settings. Warns when e-mail confirmation is
    /// required but the transport cannot deliver it.
    pub fn with_options(config: &Config, log: Logger, opts: MailerOptions) -> Mailer {
        let (kind, transport) = match (opts.transport, config.mail_transport) {
            (Some(t), _) => ("custom", Transport::Custom(t)),
            (None, MailTransport::None) => ("none", Transport::None),
            (None, MailTransport::Log) => ("log", Transport::Log),
            (None, MailTransport::Smtp) => {
                let cfg = opts.smtp.unwrap_or_else(|| SmtpConfig::from_config(config));
                ("smtp", Transport::Smtp(Arc::new(cfg)))
            }
        };
        if config.require_email_verification && kind == "log" {
            log_warn!(
                log,
                "MAIL_TRANSPORT=log: verification and reset e-mails are only written to the log. Configure SMTP before opening the server to players."
            );
        }
        if config.require_email_verification && kind == "none" {
            log_warn!(
                log,
                "MAIL_TRANSPORT=none with REQUIRE_EMAIL_VERIFICATION: new accounts cannot be confirmed and passwords cannot be reset."
            );
        }
        Mailer {
            inner: Arc::new(Inner {
                kind,
                transport,
                mail_from: config.mail_from.clone(),
                server_name: config.server_name.clone(),
                log,
                clock: opts.clock,
                concurrency: opts.concurrency.max(1),
                max_queue: opts.max_queue,
                state: Mutex::new(State::default()),
                idle: Notify::new(),
            }),
        }
    }

    /// The transport: `smtp`, `log`, `none` or `custom`.
    pub fn kind(&self) -> &'static str {
        self.inner.kind
    }

    /// Queues a message to `to` (a bare address).
    pub fn send(&self, to: &str, subject: &str, text: &str) -> Receipt {
        self.enqueue("custom", to, subject, text)
    }

    /// Queues one of the server's e-mails to `to` (a bare address).
    pub fn send_template(&self, template: &Template<'_>, to: &str) -> Receipt {
        let r = template.render(&self.inner.server_name);
        self.enqueue(template.name(), to, &r.subject, &r.text)
    }

    fn enqueue(&self, template: &'static str, to: &str, subject: &str, text: &str) -> Receipt {
        let inner = &self.inner;
        if !is_valid_address(to) {
            log_warn!(inner.log, "e-mail not sent: invalid address", { "template": template });
            return resolved(false);
        }
        let mut st = inner.state.lock();
        if st.queue.len() >= inner.max_queue {
            MESSAGES.with(&["dropped"]).inc();
            log_warn!(inner.log, "e-mail queue full, message dropped", { "template": template });
            return resolved(false);
        }
        let parts = MessageParts {
            from: &inner.mail_from,
            to,
            subject,
            text,
            date_ms: inner.clock.wall_ms(),
            message_id: None,
        };
        let built = match build_message(&parts) {
            Ok(b) => b,
            Err(e) => {
                log_error!(inner.log, "e-mail not built", {
                    "template": template,
                    "err": { "code": e.code, "message": e.message },
                });
                return resolved(false);
            }
        };
        let (done, rx) = oneshot::channel();
        let mail = OutgoingMail {
            from: built.from,
            to: built.to,
            raw: built.raw,
            subject: subject.to_string(),
            text: text.to_string(),
        };
        st.queue.push_back(Job { template, mail, done });
        Inner::pump(inner, &mut st);
        Receipt(rx)
    }

    /// Resolves when every queued message has been handled (tests, shutdown).
    pub async fn idle(&self) {
        loop {
            let notified = self.inner.idle.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.inner.state.lock().is_idle() {
                return;
            }
            notified.await;
        }
    }

    /// Waits at most `within` for the queue to empty (shutdown); true when it did.
    pub async fn drain(&self, within: Duration) -> bool {
        tokio::time::timeout(within, self.idle()).await.is_ok()
    }
}

impl State {
    fn is_idle(&self) -> bool {
        self.active == 0 && self.queue.is_empty()
    }
}

impl Inner {
    /// Starts queued messages while fewer than `concurrency` are in flight.
    fn pump(self: &Arc<Inner>, st: &mut State) {
        while st.active < self.concurrency {
            let Some(job) = st.queue.pop_front() else { break };
            st.active += 1;
            match tokio::runtime::Handle::try_current() {
                Ok(rt) => {
                    rt.spawn(self.clone().run(job));
                }
                Err(_) => {
                    // Without a runtime nothing can be sent (never the case in the server).
                    st.active -= 1;
                    self.finish(
                        job,
                        Err(TransportError { code: "internal".into(), message: "no runtime".into() }),
                    );
                }
            }
        }
    }

    /// The send of one message, as a task of its own (a panic is a failure, not a lost slot).
    fn transport_future(&self, mail: &OutgoingMail) -> TransportFuture {
        match &self.transport {
            Transport::None => Box::pin(async { Ok(()) }),
            Transport::Log => {
                log_info!(self.log, "mail (log transport)", {
                    "to": mail.to, "subject": mail.subject, "text": mail.text,
                });
                Box::pin(async { Ok(()) })
            }
            Transport::Smtp(cfg) => {
                let (cfg, mail) = (cfg.clone(), mail.clone());
                Box::pin(async move {
                    send_smtp(&cfg, &mail.from, &mail.to, &mail.raw)
                        .await
                        .map(drop)
                        .map_err(TransportError::from)
                })
            }
            // A transport that panics before returning its future fails like one that panics in it.
            Transport::Custom(f) => match std::panic::catch_unwind(AssertUnwindSafe(|| f(mail.clone()))) {
                Ok(fut) => fut,
                Err(_) => Box::pin(async { Err(TransportError::panicked()) }),
            },
        }
    }

    async fn run(self: Arc<Inner>, job: Job) {
        let result = match tokio::spawn(self.transport_future(&job.mail)).await {
            Ok(r) => r,
            Err(e) if e.is_panic() => Err(TransportError::panicked()),
            Err(_) => {
                Err(TransportError { code: "internal".into(), message: "the send was cancelled".into() })
            }
        };
        self.finish(job, result);
        let mut st = self.state.lock();
        st.active -= 1;
        self.pump(&mut st);
        if st.is_idle() {
            self.idle.notify_waiters();
        }
    }

    fn finish(&self, job: Job, result: Result<(), TransportError>) {
        let ok = match result {
            Ok(()) => {
                MESSAGES.with(&["sent"]).inc();
                true
            }
            Err(e) => {
                MESSAGES.with(&["failed"]).inc();
                log_error!(self.log, "e-mail not sent", {
                    "template": job.template,
                    "err": { "name": "Error", "code": e.code, "message": e.message },
                });
                false
            }
        };
        let _ = job.done.send(ok);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use super::*;
    use crate::config::SmtpSecurity;
    use crate::mail::smtp::tests::{Fake, config as smtp_config, fake_smtp, test_cert};

    fn test_config() -> Config {
        let mut c = Config::for_tests();
        c.server_name = "Test Server".into();
        c.mail_from = "Test <no-reply@example.org>".into();
        c
    }

    fn logger() -> Logger {
        Logger::root().child("mail-test")
    }

    fn custom(f: impl Fn(OutgoingMail) -> TransportFuture + Send + Sync + 'static) -> MailerOptions {
        MailerOptions { transport: Some(Arc::new(f)), ..MailerOptions::default() }
    }

    #[tokio::test]
    async fn queue_custom_transport_failures_never_rejects() {
        let got = Arc::new(Mutex::new(Vec::<OutgoingMail>::new()));
        let fail = Arc::new(AtomicBool::new(false));
        let (g, f) = (got.clone(), fail.clone());
        let m = Mailer::with_options(
            &test_config(),
            logger(),
            custom(move |mail| {
                let (g, f) = (g.clone(), f.clone());
                Box::pin(async move {
                    if f.load(Ordering::SeqCst) {
                        return Err(TransportError { code: "rejected".into(), message: "boom".into() });
                    }
                    g.lock().push(mail);
                    Ok(())
                })
            }),
        );
        assert_eq!(m.kind(), "custom");
        let t =
            Template::Verification { username: "alice", link: "https://x/verify-email?token=abc", hours: 24 };
        let sent_before = MESSAGES.with(&["sent"]).get();
        assert!(m.send_template(&t, "alice@example.com").await);
        assert!(MESSAGES.with(&["sent"]).get() > sent_before);
        {
            let got = got.lock();
            assert_eq!(got.len(), 1);
            assert_eq!(got[0].to, "alice@example.com");
            assert_eq!(got[0].from, "no-reply@example.org");
            assert!(got[0].raw.contains("Subject: Confirm your e-mail address for Test Server"));
            assert!(got[0].raw.contains("From: \"Test\" <no-reply@example.org>"));
        }
        assert!(!m.send("not-an-address", "s", "t").await);
        assert!(!m.send("x@example.com", "bad\r\nBcc: y@example.com", "t").await, "not built");
        fail.store(true, Ordering::SeqCst);
        let failed_before = MESSAGES.with(&["failed"]).get();
        assert!(!m.send("bob@example.com", "s", "t").await);
        assert!(MESSAGES.with(&["failed"]).get() > failed_before);
        m.idle().await;
        assert_eq!(got.lock().len(), 1);
    }

    #[tokio::test]
    async fn log_and_none_transports_succeed() {
        let mut c = test_config();
        c.mail_transport = MailTransport::Log;
        let m = Mailer::new(&c, logger());
        assert_eq!(m.kind(), "log");
        assert!(m.send("alice@example.com", "Hi", "link: https://x/verify-email?token=abc").await);
        c.mail_transport = MailTransport::None;
        let m = Mailer::new(&c, logger());
        assert_eq!(m.kind(), "none");
        assert!(m.send("alice@example.com", "Hi", "text").await);
        m.idle().await;
    }

    /// A transport that holds every message until released, counting the messages in flight.
    struct Gate {
        open: Arc<Notify>,
        released: Arc<AtomicBool>,
        in_flight: Arc<AtomicUsize>,
        max_in_flight: Arc<AtomicUsize>,
    }

    fn gate() -> (Gate, MailerOptions) {
        let g = Gate {
            open: Arc::new(Notify::new()),
            released: Arc::new(AtomicBool::new(false)),
            in_flight: Arc::new(AtomicUsize::new(0)),
            max_in_flight: Arc::new(AtomicUsize::new(0)),
        };
        let (open, released, in_flight, max) =
            (g.open.clone(), g.released.clone(), g.in_flight.clone(), g.max_in_flight.clone());
        let opts = custom(move |_| {
            let (open, released, in_flight, max) =
                (open.clone(), released.clone(), in_flight.clone(), max.clone());
            Box::pin(async move {
                let n = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                max.fetch_max(n, Ordering::SeqCst);
                loop {
                    let wait = open.notified();
                    tokio::pin!(wait);
                    wait.as_mut().enable();
                    if released.load(Ordering::SeqCst) {
                        break;
                    }
                    wait.await;
                }
                in_flight.fetch_sub(1, Ordering::SeqCst);
                Ok(())
            })
        });
        (g, opts)
    }

    impl Gate {
        fn release(&self) {
            self.released.store(true, Ordering::SeqCst);
            self.open.notify_waiters();
        }
    }

    #[tokio::test]
    async fn bounded_queue_two_in_flight_fifo_and_drain() {
        let (g, opts) = gate();
        let m = Mailer::with_options(&test_config(), logger(), MailerOptions { max_queue: 3, ..opts });
        let receipts: Vec<Receipt> = (0..5).map(|i| m.send(&format!("u{i}@example.com"), "s", "t")).collect();
        let dropped_before = MESSAGES.with(&["dropped"]).get();
        // 2 in flight and 3 waiting: the next one is dropped.
        assert!(!m.send("late@example.com", "s", "t").await);
        assert!(MESSAGES.with(&["dropped"]).get() > dropped_before);
        assert!(!m.drain(Duration::from_millis(50)).await, "stuck messages are not drained");
        assert_eq!(g.in_flight.load(Ordering::SeqCst), 2);
        g.release();
        for r in receipts {
            assert!(r.await);
        }
        assert!(m.drain(Duration::from_secs(1)).await);
        assert_eq!(g.max_in_flight.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn a_panicking_transport_is_a_failure_and_frees_its_slot() {
        let calls = Arc::new(AtomicUsize::new(0));
        let c = calls.clone();
        let m = Mailer::with_options(
            &test_config(),
            logger(),
            MailerOptions {
                concurrency: 1,
                ..custom(move |_| {
                    let first = c.fetch_add(1, Ordering::SeqCst) == 0;
                    Box::pin(async move {
                        assert!(!first, "transport bug");
                        Ok(())
                    })
                })
            },
        );
        assert!(!m.send("a@example.com", "s", "t").await);
        assert!(m.send("b@example.com", "s", "t").await);
        m.idle().await;
    }

    #[tokio::test]
    async fn smtp_transport_end_to_end() {
        let cert = test_cert();
        let s = fake_smtp(Fake { starttls: true, ..Fake::default() }, Some(&cert)).await;
        let mut cfg = smtp_config(s.port, SmtpSecurity::Starttls);
        cfg.tls = cert.client.clone();
        cfg.user = "mailer".into();
        cfg.password = zeroize::Zeroizing::new("p4ss w0rd".into());
        let mut c = test_config();
        c.mail_transport = MailTransport::Smtp;
        let m = Mailer::with_options(
            &c,
            logger(),
            MailerOptions { smtp: Some(cfg.clone()), ..Default::default() },
        );
        assert_eq!(m.kind(), "smtp");
        let t = Template::PasswordReset {
            username: "alice",
            link: "https://h/reset-password?token=T",
            minutes: 60,
        };
        assert!(m.send_template(&t, "alice@example.com").await);
        {
            let rec = s.rec.lock();
            assert_eq!(rec.messages.len(), 1);
            assert!(rec.messages[0].contains("Subject: Reset your Test Server password"));
            assert_eq!(rec.secure_at_mail, Some(true));
        }
        // An untrusted relay: the message fails, the receipt says so.
        cfg.tls = smtp::default_tls_config();
        let m = Mailer::with_options(&c, logger(), MailerOptions { smtp: Some(cfg), ..Default::default() });
        assert!(!m.send_template(&t, "alice@example.com").await);
    }
}
