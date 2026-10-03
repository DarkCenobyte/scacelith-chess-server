# Rust server: architecture and code layout

This document is the architecture reference of the dedicated server: the process model, the
crates and modules, the conventions every module follows, the interfaces between modules, the
storage formats and how the process meets its service manager. [DESIGN.md](DESIGN.md) is the
reference for *behaviour* (what the server does); this file says *how the code is organised*.
Installing and running the server is described in [DEPLOY.md](DEPLOY.md).

The Rust server replaced the former Node.js server (kept in Git history, last in commit
7531830). The Node server was never used in production, so the rewrite did not keep its internal
formats; what stayed identical is everything a game client sees through the HTTPS API
(section 1).

## 1. Compatibility with the former server

| Surface | Policy |
|---|---|
| HTTPS API (`/api/v1/...`, HTML pages) | **Identical** for clients: paths, methods, status codes, JSON field names and key order, error `code`/`message`/extra fields, headers the client reads (`Retry-After`, `Cache-Control`, download headers), rate limits and their windows, anti-abuse behaviour (proof of work, lockouts, address blocks), timeouts. [API.md](API.md) is the specification. Parser corner cases that depended on Node internals (llhttp error texts, `JSON.parse` accepting lone surrogates) follow hyper/serde_json behaviour where no client can tell. |
| Realtime protocol | **New frozen protocol v1** (section 7); the game client moved to it in the same change. |
| SQLite database | **Fresh schema**, same logical model, its own migrations starting at version 1. A Node database is not opened. |
| Game journal | **Native format** (section 6.3). A Node journal is not replayed. |
| Configuration | Same `.env`/environment mechanism and the same key names where the meaning is unchanged; keys tied to the Node process model are removed or redefined (section 9). |
| Logs, metrics | Same log messages and metric names where they still make sense (dashboards and [SIZING.md](SIZING.md) use them); syslog priorities for the journal added. |
| GIF output | Byte-identical to the Node renderer (golden hashes in the `scacelith-gif` tests). |
| Elo | Identical to the game's C++ (`test/fixtures/elo-vectors.json`, read by both test suites). |

## 2. Process model

One process, one tokio multi-thread runtime (`WORKERS` threads, `auto` = one per core, at most
16), plus a few dedicated OS threads for blocking work. There is no primary process, no IPC and
no shard bus: what the Node primary owned is in-process state.

```
                 TCP :443 ── TLS gate ── rustls ── hyper (HTTP/1.1) ──┬── /api/v1/* routes ── auth, store
                                                                        └── /ws upgrade ── connection tasks
                                                                                              │
   lobby actor (presence, matchmaker, challenges, conduct, rematch,       ◄──── lobby msgs ──┤
   sanctions, refund notices)                                                                │
                                                                                              │ game msgs
   host actors 0..N-1 (one per game shard: rooms, timers, journal, commit) ◄─────────────────┘
        │ journal I/O (one thread per shard)           │ finish_batch
        ▼                                              ▼
   journal/shard-<n>/                            store writer thread (one FIFO, BEGIN IMMEDIATE)
                                                 store reader pool (blocking pool)
   analysis pool: Stockfish child processes (nice 19)    GIF render threads (nice 19)    log writer thread
```

* **Host actors.** `N = WORKERS` game shards (shard numbers `SHARD_BASE .. SHARD_BASE+N-1`, at
  most 64 in total). A game id carries its shard (`ids::shard_of`), so routing needs no table.
  Each actor is a single tokio task that owns its rooms, timer set, journal handle and commit
  state, and processes one message at a time (run to completion). A 10 ms beat
  (`MissedTickBehavior::Delay`) runs the timers, detects stalls, starts commits and compacts the
  journal (`game::host`, DESIGN 5.3).
* **Lobby actor.** One task owning presence (one live connection per account), the matchmaker
  queues, challenges and private codes, conduct cooldowns, game creation and placement, rematch
  hand-off, the ban cache and the rating refund notices, and the set of players with a game in
  progress (fed by `HostEvents::game_recovered` at start and `HostEvents::game_ended` after each
  commit). It never waits on the database while it holds a decision: the connection tasks read
  what a request needs before posting it, and the work that needs the store or a host afterwards
  runs in spawned tasks that report back to the actor (`realtime::lobby`).
* **Connection tasks.** One task per WebSocket connection reads frames, runs the Hello state
  machine and the rate buckets, answers pings, forwards game messages to the host actor of the
  game and lobby requests to the lobby actor; a second task per authenticated connection writes
  its byte-accounted outbound queue (`realtime::Outbound`) to the socket. Over
  `WS_SEND_BUFFER_LIMIT` bytes queued the connection is closed with 4303; droppable frames (the
  opponent's gestures) are skipped above a quarter of it.
* **Blocking work never runs on runtime threads.** SQLite (the writer thread, and the reader
  pool on tokio's blocking threads), journal file I/O and fsync (one thread per shard), password
  hashing (blocking threads, at most `PASSWORD_HASH_CONCURRENCY` at a time), GIF rendering (its
  own threads) and the log writer have their own threads. The engines of the analysis pool are
  child processes whose pipes the analysis loops read asynchronously.
* **Time.** `clock::Clock::mono_ms()` (epoch-anchored monotonic, fractional ms) for game clocks,
  timers, buckets, heartbeats and protocol `serverTime`; `clock::Clock::wall_ms()` for stored
  timestamps and expiries. Modules that need deterministic tests take a `SharedClock`.

## 3. Workspace

```
dedicated-server/
  Cargo.toml              workspace (shared dependency versions, lints, profiles)
  rust-toolchain.toml     Rust 1.99.0 (+ x86_64-unknown-linux-musl for the static release build)
  protocol/scacelith-v1.json   realtime protocol schema (source of all generated codecs)
  protocol/frozen/        manifests of the released protocol minors (v1.0.json)
  crates/protocol         scacelith-protocol: generated codec, constants, the `protogen` generator
  crates/chess            scacelith-chess: rules, SAN/UCI/FEN, digest, PGN (no dependencies)
  crates/gif              scacelith-gif: GIF renderer and encoder
  crates/server           scacelith-server: the server library and binary, migrations
  crates/client           scacelith-client: Rust client SDK (integration tests, `scacelith-bench`)
  assets/                 GIF fonts and piece set (embedded at build time)
  test/fixtures           shared vectors (Elo, protocol, chess cross-check), also read by the C++ tests
  tools/                  generator of the chess cross-check vectors (C++)
  bench/                  benchmark harness (docs/BENCHMARK.md)
  deploy/systemd          example unit and its companion files (docs/DEPLOY.md)
  docs/                   DESIGN, API, PROTOCOL, CONFIG, DEPLOY, SIZING, ANTICHEAT, BENCHMARK, RUST-PORT
```

Server modules (`crates/server/src`):

| Module | Content |
|---|---|
| `main`, `cli`, `app` | command line, bootstrap, signals, shutdown |
| `config` | the key table, loading, validation, `check-config`, the generated `.env.example` and `docs/CONFIG.md` |
| `clock`, `ids`, `log`, `metrics`, `systemd`, `util`, `sys`, `events` | foundations (section 5); `sys` holds the only `unsafe` code (libc calls) |
| `store` | SQLite schema and migrations, writer thread, reader pool, typed table API, retention purge |
| `journal` | game journal of a host shard: segments, group flush, compaction, recovery |
| `game` | game clock, room, host actors, timers, test doubles |
| `matching` | Elo, matchmaker, challenges, conduct (pure state machines owned by the lobby) |
| `realtime` | connection tasks, outbound queues, presence and admission, lobby actor, drain |
| `net` | listeners, native TLS and its reload, TLS gate, per-address guard, abuse tracker, limiters, HTTP/1.1 serving on hyper, WebSocket upgrade and codec, metrics endpoint |
| `http` | router, answers, bodies, schema validation, route rates, handler timeout, the `/api/v1` routes and the HTML pages |
| `auth`, `security`, `mail` | accounts, sessions, MFA, SSO and their security events; password hashing (Argon2id), TOTP, secret box, keys, proof of work, auth rate counters; SMTP |
| `gifsvc` | GIF render pool, cache and quotas |
| `anticheat` | anomalies, sanctions, refunds and their notices, reports, the statistical model, the analysis pool, `scacelith-server admin` (docs/ANTICHEAT.md, section 9) |

## 4. Conventions

* Rust 2024 edition, `rustfmt` (`max_width = 110`), `cargo clippy --workspace --all-targets --
  -D warnings` and `cargo doc --workspace --no-deps` clean. Comments, docs and identifiers in
  English.
* Every public item has a short doc comment. Module docs say what the module owns and which
  section of DESIGN.md it implements.
* No `unsafe` outside `crate::sys` (the workspace denies `unsafe_code`; `sys` allows it, with a
  comment per block saying why it is sound).
* Errors: hand-written enums or structs implementing `Display` and `std::error::Error` (no
  `anyhow` or `thiserror`). `unwrap`/`expect` only on invariants, with an `expect` message saying
  why.
* Panics in a handler, a room or a job are caught at the boundary (`catch_unwind` or task join
  errors), logged, and turned into `Internal`/500.
* Logging: `crate::log::Logger` and the `log_debug!/log_info!/log_warn!/log_security!/log_error!`
  macros (fields with `serde_json::json!` syntax). Never log secrets, tokens or full addresses:
  use `log::ip()` for client addresses.
* Metrics: `crate::metrics` statics (`LazyLock`). Metric names, help texts, labels and buckets
  are those of the former server where they kept their meaning (DESIGN 8, docs/SIZING.md); the
  `shard` label is gone (one process). New metrics use the `scacelith_` prefix.
* Async: tokio. Channels: `tokio::sync::mpsc` for actor inboxes (bounded where a producer can be a
  client, with an explicit overflow policy), `oneshot` for replies. No `async-trait`; traits return
  boxed futures or `impl Future` where needed.
* JSON: `serde_json::Value` with `preserve_order` (key order = insertion order, as JavaScript).
  Answers keep the key order of the API specification. Integers stay integers (`i64`/`u64`),
  never `f64` unless the value is fractional.
* Tests: unit tests next to the code (a `tests` module or a `tests.rs` beside the module),
  black-box tests in `crates/*/tests`, end-to-end tests of a whole server in `realtime::e2e`, and
  shared vectors in `test/fixtures`.

## 5. Foundations

* `clock`: `Clock` trait (`mono_ms`, `wall_ms`, `now_ms`), `SystemClock`, `ManualClock`,
  `SharedClock = Arc<dyn Clock>`, free functions `clock::mono_ms()`, `clock::wall_ms()`.
* `ids`: `UserId = u32`, `GameId = u64`, `ConnId = u32`, `GameIdAllocator`, `shard_of`,
  `is_game_id`, `created_ms`, `ID53_LIMIT`, `MAX_SHARDS`. A game id is
  `(ms since 2026-01-01) << 12 | shard << 6 | sequence`.
* `log`: `log::init(Options)`, `log::install_panic_hook()`, `Logger::root().child("name")`, the
  macros, `log::flush()`, `log::capture_logs(level)` for tests, `scrub`, `ip`, `iso_time`. Lines go
  through a bounded queue to the log writer thread.
* `metrics`: `counter`, `counter_vec`, `counter_fn`, `gauge`, `gauge_vec`, `gauge_fn`,
  `histogram`, `histogram_vec`, the process metrics, `registry().render()`, `js_number`.
* `systemd`: `notify`, `ready`, `reloading`, `stopping`, `status`, `watchdog`,
  `watchdog_interval`, `journald` (stdout is the journal), `journald_stderr`.
* `events`: the cross-module events and traits of section 8.
* `config::Config`: every setting as a typed field (section 9), `Config::for_tests()`.
* `util`: JavaScript-compatible number formatting and `JSON.stringify`, encodings, hashes,
  random tokens, address helpers.

## 6. Storage

### 6.1 SQLite

* `rusqlite` with the bundled SQLite. One file (`DB_PATH`), WAL, `synchronous=FULL`,
  `foreign_keys=ON`, `secure_delete=ON` (privacy guarantee of DESIGN 7), `busy_timeout` 5 s,
  `journal_size_limit` 64 MiB, a statement cache of 256 per connection.
* One **writer thread** owns the only writable connection and runs jobs from one FIFO, each job in
  `BEGIN IMMEDIATE`. All writes of the process go through it, which also gives the ordering the
  anti-cheat needs (anomalies before the game batch that queues analysis).
* A **reader pool** (`query_only` connections) serves reads on tokio's blocking threads.
* Public shape: `Store` (cheap `Clone`) with `store.read(|db| ...)` and `store.write(|db| ...)`,
  whose closure receives a `Db`, the synchronous typed API of every table on that connection
  (`db.users()`, `db.games()`...), and async table handles on top (`store.users().by_id(id)`),
  each call one job. Read-modify-write operations run inside one writer job. Errors:
  `StoreError` with a `kind()` (`Busy`, the constraint kinds, `NotFound`, `Closed`, `Sqlite`,
  the migration kinds...) and, for a failed game commit, the `game_id()` of the record at fault.
* Schema: designed afresh from the logical model of the former schema
  (`crates/server/migrations/001_initial.sql`): `STRICT` tables, integer milliseconds for times,
  `INTEGER` booleans, BLOB move lists (u16 LE moves, u32 LE spent and clock times), TEXT hashes,
  an index for every query (tests check the query plans of the hot queries). Migrations are
  numbered SQL files embedded with `include_str!`, tracked in `schema_migrations` with a checksum
  and applied in `BEGIN IMMEDIATE` by `scacelith-server migrate` and at start; a database with a
  migration this server does not know, or whose applied file changed, is refused.
* Retention purge (DESIGN 7): a scheduled task every `RETENTION_INTERVAL_MS`, chunked writer jobs
  with an adaptive chunk size; a failed run is retried at the next interval.

### 6.2 Store writer ordering

A write job is queued when `Store::write` (or an async table method that writes) is called, not
when its future is first polled: jobs submitted one after the other run in that order. Callers
that must observe their own writes do it inside one job. Game commits (`finish_batch`) carry the
game id in their errors, so a failing batch can be retried game by game.

### 6.3 Game journal

* One journal per host shard in `JOURNAL_DIR/shard-<n>/`, append-only numbered segments
  (`segment-<seq>.log`).
* Record: `len u32 | kind u8 | game u64 | at f64 | payload | crc32c u32` (little-endian; CRC-32C
  over everything before it). Kinds: created, move, event, ended, committed, snapshot. A torn or
  corrupt record ends the reading of its segment; replay continues with the next segment.
* Group flush: appends are buffered by the actor and written by the shard's I/O thread (`write`,
  plus `fdatasync` when `JOURNAL_FSYNC`), one batch in flight; the actor reads
  `has_unwritten()` and `failed_writes()` synchronously (the commit gate, DESIGN 5.5).
* Compaction writes a snapshot of the long games and deletes segments once no game needs them
  (directory fsync before the deletions that need it).
* Recovery gives the games without a `committed` record, each from its latest snapshot.
* Fault injection hook (`Journal::set_write_batch_override`) for the commit-gate tests.

## 7. Realtime protocol v1

The protocol is specified, and frozen as version 1, in [PROTOCOL.md](PROTOCOL.md). Its single
source is `protocol/scacelith-v1.json`; `protogen` (`cargo run -p scacelith-protocol --features
gen --bin protogen`) validates it and writes the derived files:

| File | Content |
|---|---|
| `crates/protocol/src/gen.rs`, `crates/protocol/src/gen_json.rs` | the Rust codec and its JSON bridge |
| `../src/net/protocol_gen.h`, `../src/net/protocol_gen.cpp` | the game's C++ codec |
| `docs/PROTOCOL.md` | the tables between the `protogen` markers (the prose is written by hand) |
| `test/fixtures/protocol-vectors.json` | the golden vectors, read by the Rust and C++ tests |

`protogen --check` writes nothing and fails when a derived file is stale; a test of the
`scacelith-protocol` crate runs the same check. Every released minor is frozen as
`protocol/frozen/v1.<minor>.json` (`protogen --freeze`), and `protogen` refuses a schema that
breaks the append-only rules of PROTOCOL.md "Versions and evolution" against those manifests.

On the server side, client messages decode strictly (`ClientMsg::decode`): a connection task
reads the Hello prefix first (`HelloPrefix::read`), then decodes the Hello of any minor
(`decode_hello`), and every fatal `Error` is followed by the close code of `close_code_for(code)`
(4303, slow consumer, has no `Error` frame). Server messages are encoded once into `Bytes` and
shared by the recipients. `/api/v1/info` announces the protocol as `{min: 1, max: 1, schema,
subprotocol: "scacelith.rt1"}`.

## 8. Interfaces between modules

Cross-module events and notification traits live in `crate::events` (`NewGame`, `GameEnded`,
`RematchRequest`, `Anomaly`, `SanctionPending`, `SanctionApplied`, `HostEvents`, `AnomalySink`,
`SessionEvents`, `SanctionEvents`, `Noop`). A module calls a trait object
(`Arc<dyn HostEvents>`...) and never the concrete type of its peer; `app` wires the
implementations. Trait methods never block: they post to an actor or enqueue a store job.

| Trait | Called by | Implemented by |
|---|---|---|
| `HostEvents` (`game_ended`, `game_recovered`, `rematch`, `conduct`) | game host actors | the lobby |
| `AnomalySink` (`record`, `sanction_certain`) | game host actors, connection tasks | the anti-cheat service |
| `SessionEvents` (`sessions_revoked`) | auth | the lobby (closes the connections) |
| `SanctionEvents` (`sanction_applied`, `refunds_pending`) | the anti-cheat service | the lobby |

### 8.1 Game host

`crate::game` exposes:

```rust
pub struct Hosts { /* one HostHandle per shard */ }
impl Hosts {
    pub async fn start(deps: HostDeps, shards: Range<u32>) -> Result<Hosts, HostError>; // recovers the journals first
    pub fn get(&self, game: GameId) -> Option<&HostHandle>;           // by ids::shard_of
    pub fn pick(&self, preferred: Option<u32>) -> &HostHandle;        // placement (fewest games)
    pub fn handles(&self) -> &[HostHandle];                           // every host, in shard order
    pub fn stall_during(&self, since_mono_ms: f64) -> bool;           // RTT sample filter
    pub async fn shutdown(&self);                                     // final commits, journal flush
}

#[derive(Clone)]
pub struct HostHandle { /* shard number, unbounded inbox sender, load counters */ }
impl HostHandle {
    pub fn shard(&self) -> u32;
    /// A strictly decoded game request (Move, Resign, DrawOffer, DrawAnswer, DrawClaim, Abort,
    /// Resync, Rematch) with the connection it came from and its read time (mono ms).
    pub fn client(&self, user: UserId, msg: ClientMsg, ep: Endpoint, recv_at: f64);
    pub fn gesture(&self, game: GameId, user: UserId, frame: Bytes);  // raw C_Gesture
    pub fn attach(&self, game: GameId, user: UserId, ep: Endpoint);   // sends a GameSnapshot
    pub fn detach(&self, game: GameId, user: UserId, conn: ConnId);   // only if still that endpoint
    pub fn rtt(&self, game: GameId, user: UserId, rtt_ms: u32);
    pub fn forfeit_user(&self, user: UserId);                         // sanction
    pub fn decline_rematch(&self, game: GameId, user: UserId);
    pub async fn create(&self, game: NewGame) -> Result<GameId, ErrorCode>;
    pub fn cancel(&self, game: GameId);                               // ServerAborted, no conduct
    pub fn load(&self) -> HostLoad;                                   // games, players (atomics)
    pub fn stall_during(&self, since_mono_ms: f64) -> bool;
    pub async fn stats(&self) -> Option<HostStats>;                   // None once shut down
}

pub struct HostDeps {
    pub config: Arc<Config>, pub clock: SharedClock, pub store: Store,
    pub events: Arc<dyn HostEvents>, pub anomalies: Arc<dyn AnomalySink>,
}
```

`Hosts::start` fails (`HostError`) on a shard range beyond the 64 shards a game id can address,
an unreadable database, a journal that cannot be opened or a failed recovery; it returns once
every shard has replayed its journal on a blocking thread. The host owns rooms, timers, the
shard journal (`JOURNAL_DIR/shard-<n>`), commits through the store's `finish_batch`, sends
`RatingUpdate` after the commit, then calls `HostEvents::game_ended`. Game ids come from one
`GameIdAllocator` per shard, seeded from the database and the journal. Inbox messages are
processed one at a time; a connection's frames and its detach stay ordered because they travel
through the same inbox. The realtime layer reaches the hosts through its `GameHosts` trait
(`realtime::deps`), which `Hosts` implements by routing on the game id.

### 8.2 Realtime

* Connection tasks drive `net`'s WebSocket reader/writer and an `Outbound` queue
  (`realtime::endpoint`). Hello: `HelloPrefix::read` first, then the checks of PROTOCOL.md
  "Connection lifecycle"; Welcome negotiates `minor = min`, `caps = and`. Session tokens are
  validated through the `TokenValidator` trait (`realtime::deps`), which `Auth` implements.
* The lobby actor (`realtime::lobby`) owns presence (one live connection per account), the
  `matching` state machines, conduct cooldowns, rematches, the ban cache, refund notices and the
  players' games in progress, and implements `HostEvents`, `SessionEvents` and `SanctionEvents` by
  posting to its inbox. It answers each lobby request with exactly one `Ack` or `Error`.
* `realtime::admission` counts connections per address and for the whole server at the upgrade,
  and gives the TLS gate its server-full signal; `realtime::drain` runs the shutdown phases of
  the connections.
* `app` builds every service, recovers the games, binds the listeners, notifies systemd and runs
  the shutdown (the order is in the module documentation of `app`).

### 8.3 Auth

```rust
pub struct SessionInfo { pub user_id: UserId, pub username: String, pub session_id: i64,
                         pub email_verified: bool, pub token_hash: [u8; 32] }
impl Auth {
    pub async fn validate_token(&self, token: &str) -> Result<Option<SessionInfo>, AuthError>;
}
```

Used by the HTTP Bearer hook and by the Hello. Revocations call `SessionEvents`.

### 8.4 Anti-cheat

`Anticheat` implements `AnomalySink` (the first anomaly of a user, game and kind enqueued on the
store writer before `record` returns, its repeats merged into that row once a second; the
automatic sanction of certain cheats, announced to the lobby before `sanction_certain` returns;
`Anticheat::flush` at shutdown), applies sanctions and refunds (calls `SanctionEvents`), and runs
the analysis pool (`AnalysisPool`: Stockfish engines, `anticheat::analysis`) on the analysis queue
with the integrity updates. `Reports` serves the report route; `anticheat::admin` is
`scacelith-server admin`. [ANTICHEAT.md](ANTICHEAT.md) has the behaviour and a code map.

### 8.5 Routes

Each route group exposes `pub fn register(router: &mut Router, deps: <Group>Deps)` with a deps
struct of the services it needs (config, store, auth, report desk, GIF service, logger...).
`http::routes::register` registers every group in the order of the former server's route modules
(the order of the `Allow` header lists), then `http::pages::register` adds the HTML pages.

## 9. Configuration

* Sources: environment, then the `.env` file of the working directory, or the file named by
  `SCACELITH_ENV_FILE` (empty: no file); secrets may come from `<KEY>_FILE` (systemd
  `LoadCredential=`). One declarative key table (`config/keys.rs`) drives the loader,
  `check-config` and the generated `.env.example` and `docs/CONFIG.md` (`scacelith-server
  gen-config-docs [--check]`, with a test that the committed files are current).
* `check-config` prints the effective configuration (secrets as `<set>`/`<unset>`) and its
  warnings, or every error.
* Keys of the Node process model are gone: `SHARD_OVERLOAD_LAG_MS`, `LISTEN_REUSE_PORT`,
  `UV_THREADPOOL_SIZE`, `WS_MAX_MESSAGE_BYTES` and `GOOGLE_REDIRECT_URI`, still set, get a
  warning, never an error; any other key the server does not know is ignored. `WORKERS` is the
  number of game shards and runtime threads, and the limits the former server applied per worker
  process are whole-server values whose defaults scale with `WORKERS` (docs/CONFIG.md).

## 10. Operations

* Logs: JSON lines on stdout (or `LOG_FORMAT=pretty`), with a `<N>` syslog priority at the start
  of each line when `JOURNAL_STREAM` names that stream (systemd connects stdout and stderr to the
  journal by default): debug 7, info 6, security 5, warn 4, error 3. No log files. `migrate`
  logs on stderr (stdout holds its report), and the administration commands log their warnings
  and errors as readable text on stderr, with the same detection. A panic is logged as an `error` record
  (`log::install_panic_hook`); what comes before logging starts (an invalid configuration) and a
  backtrace asked with `RUST_BACKTRACE` are plain text on stderr.
* systemd (`Type=notify-reload`): `READY=1` once the migrations are applied, the games recovered
  and the listeners bound, with `STATUS=` texts on the way (`starting`, `recovering N games`,
  `ready`, `draining`); `SIGHUP` sends `RELOADING=1`, reloads the TLS certificate and sends
  `READY=1` again, even when the reload failed; `SIGTERM`/`SIGINT` send `STOPPING=1` and drain
  (`Notice{ServerShutdown}`, `SHUTDOWN_GRACE_MS`, final commits, journal flush, store close); a
  second signal exits at once with code 1. When the unit sets `WatchdogSec=`, `WATCHDOG=1` is
  sent at half that interval while the lobby actor and every host actor answer in time (a stuck
  actor stops the pings and systemd restarts the server), and at every tick during the shutdown.
  The example unit is in [`deploy/systemd/`](../deploy/systemd/); [DEPLOY.md](DEPLOY.md) installs
  it and explains what it expects.
* Exit codes: 0 success, 1 failure, 2 usage.
* Release build: static `x86_64-unknown-linux-musl` binary ([DEPLOY.md](DEPLOY.md), section 1),
  GPL-3.0-or-later, its source in this repository.
