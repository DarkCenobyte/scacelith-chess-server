//! Server bootstrap and lifecycle: builds every service from the configuration, applies the
//! migrations, recovers the journaled games, starts the host actors, the lobby, the listeners and
//! the background jobs, notifies systemd, and runs the graceful shutdown on SIGTERM/SIGINT
//! (SIGHUP reloads the certificates). See docs/RUST-PORT.md and deploy/systemd.
//!
//! The command line ([`crate::cli`]) loads and checks the configuration, then calls these entry
//! points; each returns the process exit code (0 success, 1 failure, 2 usage).
//!
//! # Start-up (`Instance::launch`)
//!
//! `STATUS=starting`, then in order: the checks of the shard range and of `ABUSE_EXEMPT`, the
//! process metrics, the open file limit, the data directory, the store (opened with the Elo rules
//! of [`crate::matching::elo`], migrated, its server id read), the lobby's inbox, the mailer and
//! the auth service (which announces revoked sessions into that inbox), the background jobs
//! (retention purge, analysis queue gauges, the sweep of the auth service's control counters every
//! 10 s, engine analysis pool), the GIF service, the anti-cheat
//! services (anomalies and sanctions, reports), the game hosts (`STATUS=recovering N games`: each
//! shard replays its journal and announces the games it recovers into the lobby's inbox), the
//! lobby actor, the realtime connections, the HTTP API, and the listeners, bound last. A failure
//! stops what was started (final commits, store closed) and exits with 1. The server serves from
//! the bind on; `/readyz`, `READY=1` and `STATUS=ready` follow, then the watchdog keep-alive
//! starts. A stop signal received during the start-up stops the server once it is started,
//! before `READY=1`; a second one exits at once with 1.
//!
//! # Signals (`run`)
//!
//! * SIGHUP: `RELOADING=1`, the certificate and key files are read again (a failure is logged
//!   and the current certificate kept), then `READY=1`, whatever the outcome.
//! * SIGTERM / SIGINT: `Instance::shutdown`, then exit 0; a second signal during the shutdown
//!   exits at once with 1.
//!
//! # Shutdown (`Instance::shutdown`)
//!
//! `STOPPING=1` and `STATUS=draining`; the listeners stop accepting (`/readyz` 503, upgrades 503,
//! the HTTP requests in progress finish), the lobby's periodic work stops, the engine analysis and
//! the retention purge stop, and the realtime connections drain (`Notice{ServerShutdown}`,
//! `SHUTDOWN_GRACE_MS`, then `Error{ShuttingDown}` and 4008). Then the game hosts make their
//! final commits and flush their journals, the lobby ends, the listeners end and the API handlers
//! still running finish (5 s at most), the anomalies still buffered go to the store writer, the
//! mailer sends what it holds (5 s at most), the route modules close (GIF render threads), the
//! auth service saves its security events, and the store closes. The watchdog keeps
//! pinging until the end.

use std::fmt;
use std::net::SocketAddr;
use std::ops::Range;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::signal::unix::{Signal, SignalKind, signal};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use crate::anticheat::reports::Reports;
use crate::anticheat::{AnalysisPool, Anticheat};
use crate::auth::{Auth, AuthDeps};
use crate::clock::{self, SharedClock};
use crate::config::Config;
use crate::events::{GameEnded, HostEvents, IncidentKind, RematchRequest};
use crate::game::{HostDeps, Hosts};
use crate::gifsvc::{ChessRules, GameRenderer, GifService};
use crate::http::pages::{self, PageDeps};
use crate::http::routes::account::AccountRouteDeps;
use crate::http::routes::account_export::ExportRouteDeps;
use crate::http::routes::account_games::{self, AccountGamesDeps};
use crate::http::routes::auth::AuthRouteDeps;
use crate::http::routes::games::GamesDeps;
use crate::http::routes::gif::GifDeps;
use crate::http::routes::info::InfoDeps;
use crate::http::routes::leaderboard::LeaderboardDeps;
use crate::http::routes::players::PlayersDeps;
use crate::http::routes::reports::{ReportDesk, ReportsDeps};
use crate::http::routes::{self, AuthGroups, RouteGroups};
use crate::http::{Api, Router};
use crate::ids::{GameId, UserId};
use crate::log::{self, Logger};
use crate::mail::Mailer;
use crate::matching::elo::{self, EloSettings, Record, SideChange};
use crate::metrics::{self, ProcessMetrics};
use crate::net::guard::IpGuard;
use crate::net::health::Readiness;
use crate::net::limits::SharedLimits;
use crate::net::server::{ListenerKind, Server, ServerError, ServerHandle, ServerParts};
use crate::net::upgrade::WsEndpoint;
use crate::net::ws::{CLOSE_TIMEOUT, WsSettings};
use crate::realtime::lobby::LobbyDeps;
use crate::realtime::{Admissions, GameHosts, Lobby, Realtime, RealtimeDeps, TokenValidator};
use crate::security::password::PasswordHasher;
use crate::security::ratelimit::LocalControl;
use crate::store::{
    GameOutcome, MigrationReport, RatingFn, RatingRecord, RetentionScheduler, SideOutcome, Store,
    StoreOptions,
};
use crate::{log_error, log_info, log_warn, systemd};
use scacelith_protocol::ErrorCode;

/// How long the runtime waits for its tasks once [`start`] is done.
const RUNTIME_SHUTDOWN: Duration = Duration::from_secs(1);
/// How long the shutdown waits for the mailer to send the messages it holds.
const MAIL_DRAIN: Duration = Duration::from_secs(5);
/// How long the shutdown waits for the API requests still running once the listeners stopped.
const HANDLER_QUIESCE: Duration = Duration::from_secs(5);
/// Interval of the analysis backlog gauges.
const BACKLOG_EVERY: Duration = Duration::from_secs(5);
/// Interval of the sweep of the auth service's control counters and single-use keys.
const CONTROL_SWEEP_EVERY: Duration = Duration::from_secs(10);
/// Interval of the `recovering N games` status during the journal replay.
const RECOVERY_STATUS_EVERY: Duration = Duration::from_secs(1);
/// The header of the `101` answer that names the server (the `serverId` of `/api/v1/info`).
const SERVER_ID_HEADER: &str = "Scacelith-Server-Id";

/// `scacelith-server start`: runs the server until it is stopped. Returns 0 after a clean stop
/// (SIGTERM, SIGINT), 1 when the server could not start or a second signal cut the shutdown short.
pub fn start(config: Config) -> i32 {
    log::init(log::Options::from_config(&config));
    log::install_panic_hook();
    let threads = usize::try_from(config.workers).unwrap_or(1).max(1);
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(threads)
        .thread_name("scacelith-rt")
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            log_error!(Logger::root().child("server"), "async runtime not started", { "err": log::error(&e) });
            log::flush();
            return 1;
        }
    };
    let code = runtime.block_on(run(Arc::new(config)));
    runtime.shutdown_timeout(RUNTIME_SHUTDOWN);
    log::flush();
    code
}

/// `scacelith-server migrate`: applies the database migrations, prints
/// `{"ok": true, "applied": {"applied": [...], "version": N}, "serverId": "..."}` and exits.
/// Logs go to stderr, so that stdout holds the report only.
pub fn migrate(config: Config) -> i32 {
    log::init(log::Options { stderr: true, ..log::Options::from_config(&config) });
    log::install_panic_hook();
    let runtime = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("scacelith-server: async runtime not started: {e}");
            return 1;
        }
    };
    let outcome = runtime.block_on(migration_report(&config));
    log::flush();
    match outcome {
        Ok(report) => {
            println!("{}", serde_json::to_string_pretty(&report).unwrap_or_default());
            0
        }
        Err(e) => {
            eprintln!("scacelith-server: migrate failed: {e}");
            1
        }
    }
}

/// `scacelith-server admin ...`: the administration commands (`args` follow `admin`). Loads the
/// configuration itself, after the help, as the former `scacelith-admin` did.
pub fn admin(args: &[String]) -> i32 {
    crate::anticheat::admin::main(args)
}

/// Opens and migrates the database of `config` and describes the outcome (the `migrate` command).
async fn migration_report(config: &Config) -> Result<Value, StartError> {
    let store = open_store(config, clock::system()).await?;
    let migrated = migrate_store(&store).await;
    store.close().await;
    let (report, server_id) = migrated?;
    Ok(json!({
        "ok": true,
        "applied": { "applied": report.applied, "version": report.version },
        "serverId": server_id,
    }))
}

/// The store of `config` with the server's rating rules.
async fn open_store(config: &Config, clock: SharedClock) -> Result<Store, StartError> {
    let options =
        StoreOptions { rating: Some(server_rating(config)), clock: Some(clock), ..StoreOptions::default() };
    Store::open(config, options).await.map_err(StartError::at("database"))
}

/// Applies the migrations; returns their report and the server id they created.
async fn migrate_store(store: &Store) -> Result<(MigrationReport, String), StartError> {
    let report = store.migrate().await.map_err(StartError::at("database migrations"))?;
    let server_id = store.server_id().await.map_err(StartError::at("server id"))?;
    let server_id =
        server_id.ok_or_else(|| StartError::new("server id", "the migrations created no server id"))?;
    Ok((report, server_id))
}

/// The server's Elo rules ([`crate::matching::elo`]) as the store's rating function.
fn server_rating(config: &Config) -> RatingFn {
    fn to_record(r: &RatingRecord) -> Record {
        Record {
            rating: r.rating,
            games: r.games,
            wins: r.wins,
            draws: r.draws,
            losses: r.losses,
            peak: r.peak,
            reached_senior: r.reached_senior,
            rated: r.rated,
            counted_games: r.counted_games,
            unrated_games: r.unrated_games,
            unrated_opponents: r.unrated_opponents,
            unrated_half_points: r.unrated_half_points,
        }
    }
    fn to_rating(r: &Record) -> RatingRecord {
        RatingRecord {
            rating: r.rating,
            games: r.games,
            wins: r.wins,
            draws: r.draws,
            losses: r.losses,
            peak: r.peak,
            reached_senior: r.reached_senior,
            rated: r.rated,
            counted_games: r.counted_games,
            unrated_games: r.unrated_games,
            unrated_opponents: r.unrated_opponents,
            unrated_half_points: r.unrated_half_points,
        }
    }
    fn side(c: &SideChange) -> SideOutcome {
        SideOutcome { before: c.before, after: c.after, k: Some(c.k), record: to_rating(&c.record) }
    }
    let settings = EloSettings::from_config(config);
    Arc::new(move |white: &RatingRecord, black: &RatingRecord, score: f64| {
        // The store passes White's score of the result: 1, 0.5 or 0, the scores Elo accepts.
        let change = elo::apply_game(&to_record(white), &to_record(black), score, &settings)
            .expect("the store passes a score of 1, 0.5 or 0");
        GameOutcome { white: side(&change.white), black: side(&change.black) }
    })
}

/// Why the server could not start: the step that failed and its error.
#[derive(Debug)]
pub(crate) struct StartError {
    step: &'static str,
    source: Box<dyn std::error::Error + Send + Sync>,
}

impl StartError {
    fn new(step: &'static str, message: &str) -> StartError {
        StartError { step, source: message.into() }
    }

    /// Wraps the error of `step` (for `map_err`).
    fn at<E: std::error::Error + Send + Sync + 'static>(step: &'static str) -> impl FnOnce(E) -> StartError {
        move |e| StartError { step, source: Box::new(e) }
    }

    /// Logs the failure. A port that cannot be bound is logged as its own message, which carries
    /// the fix (`CAP_NET_BIND_SERVICE`...), so that the operator reads it first.
    fn log(&self, log: &Logger) {
        if let Some(ServerError::Listen(e)) = self.source.downcast_ref::<ServerError>() {
            log_error!(log, &e.to_string(), { "errorCode": e.code, "port": e.addr.port() });
        } else {
            log_error!(log, "server failed to start", { "step": self.step, "err": log::error_dyn(&*self.source) });
        }
    }
}

impl fmt::Display for StartError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.step, self.source)
    }
}

impl std::error::Error for StartError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&*self.source)
    }
}

/// The signals the server handles, installed before anything starts (a signal received during
/// the start-up waits in its stream).
struct Signals {
    term: Signal,
    int: Signal,
    hup: Signal,
}

impl Signals {
    fn install() -> std::io::Result<Signals> {
        Ok(Signals {
            term: signal(SignalKind::terminate())?,
            int: signal(SignalKind::interrupt())?,
            hup: signal(SignalKind::hangup())?,
        })
    }

    /// The next signal.
    async fn next(&mut self) -> Received {
        tokio::select! {
            Some(()) = self.term.recv() => Received::Stop("SIGTERM"),
            Some(()) = self.int.recv() => Received::Stop("SIGINT"),
            Some(()) = self.hup.recv() => Received::Reload,
            else => std::future::pending().await,
        }
    }

    /// The next SIGTERM or SIGINT, by name (a SIGHUP meanwhile is ignored: the certificate is
    /// loaded when the listeners are bound, and there is nothing to reload once they close).
    async fn stop(&mut self) -> &'static str {
        loop {
            if let Received::Stop(signal) = self.next().await {
                return signal;
            }
        }
    }
}

/// A signal received.
enum Received {
    /// SIGTERM or SIGINT, by name.
    Stop(&'static str),
    /// SIGHUP.
    Reload,
}

/// Runs the server of `config` until it is stopped; returns the exit code.
async fn run(config: Arc<Config>) -> i32 {
    let log = Logger::root().child("server");
    let mut signals = match Signals::install() {
        Ok(s) => s,
        Err(e) => {
            log_error!(log, "signal handlers not installed", { "err": log::error(&e) });
            return 1;
        }
    };
    let launch = Instance::launch(config, LaunchOptions::default());
    tokio::pin!(launch);
    let mut stop_requested = None;
    let launched = loop {
        tokio::select! {
            launched = &mut launch => break launched,
            signal = signals.stop() => {
                if stop_requested.is_some() {
                    log_warn!(log, "second signal: exiting now", { "signal": signal });
                    return 1;
                }
                log_info!(log, "stop requested during the start-up: the server stops once started", { "signal": signal });
                stop_requested = Some(signal);
            }
        }
    };
    let instance = match launched {
        Ok(instance) => instance,
        Err(e) => {
            e.log(&log);
            return 1;
        }
    };
    let watchdog = Watchdog::start(instance.lobby.clone(), instance.hosts.clone(), log.clone());
    let signal = match stop_requested {
        Some(signal) => signal,
        None => {
            instance.announce_ready();
            loop {
                match signals.next().await {
                    Received::Stop(signal) => break signal,
                    Received::Reload => instance.reload_certificates().await,
                }
            }
        }
    };
    if let Some(w) = &watchdog {
        w.shutting_down();
    }
    tokio::select! {
        () = instance.shutdown(signal) => 0,
        second = signals.stop() => {
            log_warn!(log, "second signal: exiting now", { "signal": second });
            1
        }
    }
}

/// What [`Instance::launch`] lets a caller replace (tests).
#[derive(Clone)]
pub(crate) struct LaunchOptions {
    /// The clock of every service.
    pub clock: SharedClock,
    /// The password hasher of the auth service (default: Argon2id with the production cost).
    pub password_hasher: Option<Arc<dyn PasswordHasher>>,
    /// Starts the process metrics sampler (once per process).
    pub process_metrics: bool,
    /// The OpenID provider of Google sign-in (default: Google's), for the embedded server of the
    /// live check (`crate::embedded`).
    pub oidc: Option<crate::auth::OidcOptions>,
}

impl Default for LaunchOptions {
    fn default() -> LaunchOptions {
        LaunchOptions { clock: clock::system(), password_hasher: None, process_metrics: true, oidc: None }
    }
}

impl fmt::Debug for LaunchOptions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LaunchOptions")
            .field("process_metrics", &self.process_metrics)
            .finish_non_exhaustive()
    }
}

/// The lobby as the game hosts see it, counting the games they recover (the status shown during
/// the journal replay).
struct HostEventsOf {
    lobby: Lobby,
    recovered: AtomicUsize,
}

impl HostEvents for HostEventsOf {
    fn game_ended(&self, ended: GameEnded) {
        self.lobby.game_ended(ended);
    }

    fn game_recovered(&self, game: GameId, white: UserId, black: UserId) {
        self.recovered.fetch_add(1, Ordering::Relaxed);
        self.lobby.game_recovered(game, white, black);
    }

    fn rematch(&self, request: RematchRequest, reply: oneshot::Sender<Result<GameId, ErrorCode>>) {
        self.lobby.rematch(request, reply);
    }

    fn conduct(&self, user: UserId, kind: IncidentKind) {
        self.lobby.conduct(user, kind);
    }
}

/// A started server: every service, the listeners bound and served.
pub(crate) struct Instance {
    config: Arc<Config>,
    log: Logger,
    server_id: String,
    store: Store,
    background: Background,
    mailer: Mailer,
    hosts: Arc<Hosts>,
    /// Kept to flush its buffered anomalies after the final commits.
    anticheat: Anticheat,
    recovered: usize,
    lobby: Lobby,
    lobby_task: JoinHandle<()>,
    auth: Auth,
    realtime: Realtime,
    api: Arc<Api>,
    net: ServerHandle,
    serving: JoinHandle<()>,
    readiness: Readiness,
    addresses: Vec<(ListenerKind, SocketAddr)>,
    tls: bool,
    process: Option<ProcessMetrics>,
}

impl fmt::Debug for Instance {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Instance").field("addresses", &self.addresses).finish_non_exhaustive()
    }
}

impl Instance {
    /// Builds and starts every service of `config` (module documentation), up to the listeners,
    /// which serve once this returns. `/readyz`, `READY=1` and the status follow with
    /// [`Instance::announce_ready`].
    pub(crate) async fn launch(config: Arc<Config>, options: LaunchOptions) -> Result<Instance, StartError> {
        let log = Logger::root().child("server");
        systemd::status("starting");
        log_info!(log, "starting", { "config": config.describe() });
        for warning in config.warnings() {
            log_warn!(log, "configuration works against the design", { "warning": warning });
        }
        let clock = options.clock.clone();
        let shards = shard_range(&config)?;
        let guard = IpGuard::new(&config, clock.clone(), Logger::root().child("guard"))
            .map_err(StartError::at("ABUSE_EXEMPT"))?;
        let guard = Arc::new(guard);
        let process = options.process_metrics.then(metrics::start_process_metrics);
        let nofile = match crate::sys::raise_nofile_limit() {
            Ok(limit) => Some(limit),
            Err(e) => {
                log_warn!(log, "open file limit not raised", { "err": log::error(&e) });
                None
            }
        };
        tokio::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o750)
            .create(&config.data_dir)
            .await
            .map_err(StartError::at("data directory"))?;

        let store = open_store(&config, clock.clone()).await?;
        let (migrations, server_id) = match migrate_store(&store).await {
            Ok(migrated) => migrated,
            Err(e) => {
                store.close().await;
                return Err(e);
            }
        };
        log_info!(log, "database ready", {
            "path": store.path(), "applied": migrations.applied, "version": migrations.version, "serverId": server_id,
        });
        // The lobby's handle exists before its actor: the auth service announces revoked
        // sessions into its inbox, and the hosts the games they recover.
        let (lobby, inbox) = Lobby::channel();
        let mailer = Mailer::new(&config, Logger::root().child("mail"));
        // The HTTP layer counts with its own `SharedLimits` (swept by the lobby): the control
        // counters are the auth service's alone, swept by a background job.
        let control = Arc::new(LocalControl::new(clock.clone()));
        let mut auth_deps =
            AuthDeps::new(config.clone(), store.clone(), mailer.clone(), Arc::new(lobby.clone()));
        auth_deps.clock = clock.clone();
        auth_deps.control = Some(control.clone());
        auth_deps.password_hasher = options.password_hasher.clone();
        if let Some(oidc) = &options.oidc {
            auth_deps.oidc = oidc.clone();
        }
        let auth = match Auth::new(auth_deps) {
            Ok(auth) => auth,
            Err(e) => {
                store.close().await;
                return Err(StartError::at("auth service")(e));
            }
        };
        let mut background = Background {
            retention: RetentionScheduler::for_store(&store, &config),
            backlog: spawn_backlog_gauges(store.clone()),
            sweep: spawn_control_sweep(control),
            pool: AnalysisPool::start(&config, store.clone(), clock.clone()),
        };
        let limits = Arc::new(SharedLimits::new(clock.clone()));
        let gifs = GifService::new(&config, Arc::new(GameRenderer::<ChessRules>::new()));
        let anticheat = Anticheat::new(&config, store.clone(), clock.clone());
        let reports: Arc<dyn ReportDesk> = Arc::new(Reports::new(&config, store.clone()));

        let events = Arc::new(HostEventsOf { lobby: lobby.clone(), recovered: AtomicUsize::new(0) });
        let host_deps = HostDeps {
            config: config.clone(),
            clock: clock.clone(),
            store: store.clone(),
            events: events.clone(),
            anomalies: Arc::new(anticheat.clone()),
        };
        systemd::status("recovering games");
        let status = spawn_recovery_status(events.clone());
        let started = Hosts::start(host_deps, shards).await;
        status.abort();
        let hosts = match started {
            Ok(hosts) => Arc::new(hosts),
            Err(e) => {
                background.stop().await;
                store.close().await;
                return Err(StartError::at("game hosts")(e));
            }
        };
        let recovered = events.recovered.load(Ordering::Relaxed);
        let game_hosts: Arc<dyn GameHosts> = hosts.clone();
        let lobby_deps =
            LobbyDeps::new(config.clone(), clock.clone(), store.clone(), game_hosts.clone(), limits.clone());
        let lobby_task = inbox.start(lobby_deps);
        anticheat.set_sanction_events(Arc::new(lobby.clone()));

        let tokens: Arc<dyn TokenValidator> = Arc::new(auth.clone());
        let realtime = Realtime::new(RealtimeDeps {
            config: config.clone(),
            clock: clock.clone(),
            store: store.clone(),
            hosts: game_hosts,
            tokens,
            anomalies: Arc::new(anticheat.clone()),
            lobby: lobby.clone(),
        });

        let readiness = Readiness::new();
        let router = api_routes(&config, &store, &auth, reports, gifs);
        let api = Api::builder(config.clone(), router)
            .clock(clock.clone())
            .logger(Logger::root().child("http"))
            .authenticator(auth.clone())
            .shared_limits(limits)
            .guard(guard.clone())
            .readiness(readiness.clone())
            .page_renderer(pages::layout::error_page_renderer(config.server_name.clone()))
            .build();
        let admissions = Admissions::from_config(&config, clock.clone());
        let settings = WsSettings {
            max_message_bytes: scacelith_protocol::MAX_CLIENT_MESSAGE,
            close_timeout: CLOSE_TIMEOUT,
            clock: clock.clone(),
            log: Logger::root().child("ws"),
        };
        let bound = async {
            let ws = WsEndpoint::new(&config, settings, realtime.on_connection())
                .admission(admissions.clone())
                .upgrade_header(SERVER_ID_HEADER, &server_id)
                .map_err(StartError::at("server id header"))?;
            let full = Some(admissions.full_signal());
            let parts = ServerParts { api: api.clone(), ws, guard, readiness: readiness.clone(), full };
            Server::bind(&config, parts, Logger::root().child("listen"))
                .await
                .map_err(StartError::at("listeners"))
        };
        let server = match bound.await {
            Ok(server) => server,
            Err(e) => {
                // Nothing was served: the recovered games stay in the journals for the next start.
                hosts.shutdown().await;
                lobby.stop();
                anticheat.flush();
                background.stop().await;
                api.close().await;
                auth.close().await;
                store.close().await;
                return Err(e);
            }
        };
        let addresses = server.addresses();
        let tls = server.tls().is_some();
        let net = server.handle();
        let serving = tokio::spawn(server.run());
        log_info!(log, "started", {
            "serverId": server_id,
            "shards": hosts.handles().len(),
            "recovered": recovered,
            "threads": config.workers,
            "nofile": nofile,
            "listeners": addresses.iter().map(|(k, a)| json!({ "kind": k.as_str(), "address": a.to_string() })).collect::<Vec<_>>(),
        });
        Ok(Instance {
            config,
            log,
            server_id,
            store,
            background,
            mailer,
            hosts,
            anticheat,
            recovered,
            lobby,
            lobby_task,
            auth,
            realtime,
            api,
            net,
            serving,
            readiness,
            addresses,
            tls,
            process,
        })
    }

    /// Marks the server ready: `/readyz` answers 200, `READY=1` and `STATUS=ready` go to systemd.
    pub(crate) fn announce_ready(&self) {
        self.readiness.set(true);
        systemd::ready();
        systemd::status(&format!("ready, {} games recovered", self.recovered));
        log_info!(self.log, "ready", { "serverId": self.server_id });
    }

    /// The address of the listener of `kind` (tests).
    #[cfg(test)]
    pub(crate) fn address(&self, kind: ListenerKind) -> Option<SocketAddr> {
        self.addresses.iter().find(|(k, _)| *k == kind).map(|(_, a)| *a)
    }

    /// The auth service (tests open sessions with it).
    #[cfg(test)]
    pub(crate) fn auth(&self) -> &Auth {
        &self.auth
    }

    /// The store (tests).
    #[cfg(test)]
    pub(crate) fn store(&self) -> &Store {
        &self.store
    }

    /// The SIGHUP reload: `RELOADING=1`, the certificate and key read again (the outcome is
    /// logged; a failure keeps the current certificate), then `READY=1` whatever the outcome.
    pub(crate) async fn reload_certificates(&self) {
        systemd::reloading();
        if self.tls {
            self.net.reload_certificates().await;
        } else {
            log_info!(self.log, "SIGHUP: no certificate to reload", { "tlsMode": self.config.tls_mode.as_str() });
        }
        systemd::ready();
    }

    /// The graceful shutdown (module documentation); `signal` names its cause in the log.
    pub(crate) async fn shutdown(self, signal: &str) {
        let Instance {
            config,
            log,
            store,
            mut background,
            mailer,
            hosts,
            anticheat,
            lobby,
            lobby_task,
            auth,
            realtime,
            api,
            net,
            serving,
            process,
            ..
        } = self;
        let grace = Duration::from_millis(u64::try_from(config.shutdown_grace_ms).unwrap_or(0));
        log_info!(log, "shutting down", {
            "signal": signal, "graceMs": config.shutdown_grace_ms, "connections": realtime.connections(),
        });
        systemd::stopping();
        systemd::status("draining");
        net.shutdown();
        lobby.stop_timers();
        let ((), drained) = tokio::join!(background.stop(), realtime.drain(grace));
        if !drained {
            log_warn!(log, "connections still open after the drain", { "connections": realtime.connections() });
        }

        systemd::status("stopping: final commits");
        hosts.shutdown().await;
        lobby.stop();
        if let Err(e) = lobby_task.await {
            log_error!(log, "lobby failed", { "err": e.to_string() });
        }
        // The listeners are stopped; the requests still running (handlers outlive their route
        // timeout) may queue e-mails, anomalies or renders: let them end first.
        if let Err(e) = serving.await {
            log_error!(log, "listeners failed", { "err": e.to_string() });
        }
        if !api.quiesce(HANDLER_QUIESCE).await {
            log_warn!(log, "requests still running at the stop", {
                "handlers": api.handlers_running(), "waitedMs": HANDLER_QUIESCE.as_millis() as u64,
            });
        }
        // The anomalies of the last moves and of the drained connections, before the store closes.
        anticheat.flush();
        if !mailer.drain(MAIL_DRAIN).await {
            log_warn!(log, "e-mails not sent before the stop", { "waitedMs": MAIL_DRAIN.as_millis() as u64 });
        }
        api.close().await;
        auth.close().await;
        store.close().await;
        if let Some(p) = process {
            p.stop();
        }
        log_info!(log, "stopped");
    }
}

/// The background jobs: the retention purge, the analysis queue gauges, the sweep of the auth
/// service's control counters and the engine analysis.
struct Background {
    retention: RetentionScheduler,
    backlog: JoinHandle<()>,
    sweep: JoinHandle<()>,
    pool: AnalysisPool,
}

impl Background {
    /// Stops every job (the engines are closed; the store can be closed afterwards).
    async fn stop(&mut self) {
        self.backlog.abort();
        self.sweep.abort();
        tokio::join!(self.pool.stop(), self.retention.stop());
    }
}

/// The game shards of this instance: `SHARD_BASE .. SHARD_BASE + WORKERS`.
fn shard_range(config: &Config) -> Result<Range<u32>, StartError> {
    let first = u32::try_from(config.shard_base);
    let count = u32::try_from(config.workers);
    match (first, count) {
        (Ok(first), Ok(count)) if count > 0 => Ok(first..first.saturating_add(count)),
        _ => Err(StartError::new("game hosts", "SHARD_BASE and WORKERS give no shard")),
    }
}

/// Shows `recovering N games` while the hosts replay their journals.
fn spawn_recovery_status(events: Arc<HostEventsOf>) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut shown = 0;
        let mut tick = tokio::time::interval(RECOVERY_STATUS_EVERY);
        loop {
            tick.tick().await;
            let n = events.recovered.load(Ordering::Relaxed);
            if n != shown {
                systemd::status(&format!("recovering {n} games"));
                shown = n;
            }
        }
    })
}

/// The analysis queue gauges (`scacelith_anticheat_analysis_queue_*`), read every
/// [`BACKLOG_EVERY`].
fn spawn_backlog_gauges(store: Store) -> JoinHandle<()> {
    let ordinary = metrics::gauge(
        "scacelith_anticheat_analysis_queue_ordinary",
        "Ordinary games waiting for engine analysis (at most ANALYSIS_QUEUE_MAX)",
    );
    let priority = metrics::gauge(
        "scacelith_anticheat_analysis_queue_priority",
        "Reported, flagged or moderator-requested games waiting for engine analysis",
    );
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(BACKLOG_EVERY);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tick.tick().await;
            if let Ok(backlog) = store.analysis().backlog().await {
                ordinary.set(backlog.ordinary as f64);
                priority.set(backlog.priority as f64);
            }
        }
    })
}

/// Removes the auth service's expired control windows and single-use keys every
/// [`CONTROL_SWEEP_EVERY`] (their expiry is otherwise lazy), giving the memory of a burst back.
fn spawn_control_sweep(control: Arc<LocalControl>) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(CONTROL_SWEEP_EVERY);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        tick.tick().await;
        loop {
            tick.tick().await;
            control.sweep();
        }
    })
}

/// Every endpoint and page of the API, in the order of the Node server's route modules.
fn api_routes(
    config: &Arc<Config>,
    store: &Store,
    auth: &Auth,
    reports: Arc<dyn ReportDesk>,
    gifs: GifService,
) -> Router {
    let http_log = Logger::root().child("http");
    let auth_deps = AuthRouteDeps { config: config.clone(), auth: auth.clone() };
    let groups = RouteGroups {
        info: InfoDeps { config: config.clone(), store: store.clone() },
        auth: AuthGroups {
            auth: auth_deps.clone(),
            account: AccountRouteDeps { config: config.clone(), auth: auth.clone() },
            sso: auth_deps,
            export: ExportRouteDeps {
                config: config.clone(),
                store: store.clone(),
                auth: auth.clone(),
                history_summary: account_games::history_summary,
                log: http_log.clone(),
            },
        },
        players: PlayersDeps { config: config.clone(), store: store.clone(), log: http_log.clone() },
        games: GamesDeps {
            config: config.clone(),
            store: store.clone(),
            reports: reports.clone(),
            log: http_log.clone(),
        },
        leaderboard: LeaderboardDeps { config: config.clone(), store: store.clone(), log: http_log.clone() },
        reports: ReportsDeps { config: config.clone(), desk: reports },
        account_games: AccountGamesDeps {
            config: config.clone(),
            store: store.clone(),
            log: http_log.clone(),
        },
        gif: GifDeps { config: config.clone(), store: store.clone(), gifs, log: http_log },
    };
    let mut router = Router::new();
    routes::register(&mut router, groups);
    pages::register(&mut router, PageDeps { config: config.clone(), auth: auth.clone() });
    router
}

/// The systemd watchdog keep-alive (`WATCHDOG=1`), from READY until the process exits.
///
/// [`systemd::watchdog_interval`] is already half of the unit's `WatchdogSec=`: the keep-alive
/// ticks at that interval. While serving, a tick pings only when the lobby actor and every game
/// host actor have answered within the interval (a stuck actor stops the pings and systemd
/// restarts the server); once the shutdown has started it pings at every tick, through the drain
/// and the final commits (`TimeoutStopSec=` bounds the shutdown).
struct Watchdog {
    stopping: Arc<AtomicBool>,
    task: JoinHandle<()>,
}

impl Watchdog {
    /// Starts the keep-alive when the service manager asked for one.
    fn start(lobby: Lobby, hosts: Arc<Hosts>, log: Logger) -> Option<Watchdog> {
        let every = systemd::watchdog_interval()?;
        let stopping = Arc::new(AtomicBool::new(false));
        let task = tokio::spawn(keep_alive(every, lobby, hosts, stopping.clone(), log));
        Some(Watchdog { stopping, task })
    }

    /// The shutdown started: ping at every tick from now on.
    fn shutting_down(&self) {
        self.stopping.store(true, Ordering::Release);
    }
}

impl Drop for Watchdog {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn keep_alive(
    every: Duration,
    lobby: Lobby,
    hosts: Arc<Hosts>,
    stopping: Arc<AtomicBool>,
    log: Logger,
) {
    let mut tick = tokio::time::interval(every);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        if stopping.load(Ordering::Acquire) {
            systemd::watchdog();
            continue;
        }
        let answered = async { lobby.ping().await && GameHosts::ping(&*hosts).await };
        match tokio::time::timeout(every, answered).await {
            Ok(true) => {
                systemd::watchdog();
            }
            Ok(false) => log_error!(log, "watchdog: the lobby or a game host has stopped"),
            Err(_) => log_warn!(log, "watchdog: the lobby or a game host did not answer in time", {
                "waitedMs": every.as_millis() as u64,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::test_config;
    use crate::store::tests::support::TempDir;

    #[tokio::test]
    async fn migrate_reports_the_applied_versions_and_a_stable_server_id() {
        let dir = TempDir::new("app-migrate");
        let db = dir.file("scacelith.db");
        let config = test_config(&[("DB_PATH", db.as_str())]).unwrap();
        let first = migration_report(&config).await.unwrap();
        assert_eq!(first["ok"], true);
        let applied = first["applied"]["applied"].as_array().unwrap();
        assert!(!applied.is_empty());
        assert_eq!(applied.last(), Some(&first["applied"]["version"]));
        let keys: Vec<&String> = first.as_object().unwrap().keys().collect();
        assert_eq!(keys, ["ok", "applied", "serverId"]);

        let second = migration_report(&config).await.unwrap();
        assert_eq!(second["applied"]["applied"], json!([]));
        assert_eq!(second["applied"]["version"], first["applied"]["version"]);
        assert_eq!(second["serverId"], first["serverId"]);
        assert_eq!(first["serverId"].as_str().unwrap().len(), 36);
    }

    #[test]
    fn the_rating_function_applies_the_elo_rules() {
        let config = test_config(&[]).unwrap();
        let rate = server_rating(&config);
        let fresh = RatingRecord::initial(config.initial_rating);
        let outcome = rate(&fresh, &fresh, 1.0);
        let settings = EloSettings::from_config(&config);
        let expected =
            elo::apply_game(&Record::new(&settings), &Record::new(&settings), 1.0, &settings).unwrap();
        assert_eq!(outcome.white.after, expected.white.after);
        assert_eq!(outcome.black.after, expected.black.after);
        assert_eq!(outcome.white.record.games, 1);
    }

    #[test]
    fn the_shards_follow_shard_base_and_workers() {
        let config = test_config(&[("SHARD_BASE", "4"), ("WORKERS", "3")]).unwrap();
        assert_eq!(shard_range(&config).unwrap(), 4..7);
    }
}
