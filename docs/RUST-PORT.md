# Rust server: architecture and code layout

This document is the contract between the parts of the Rust dedicated server. It describes the
process model, the crates and modules, who owns which file during the rewrite, the conventions
every module follows, and the interfaces between modules. `docs/DESIGN.md` remains the reference
for *behaviour* (what the server does); this file says *how the code is organised*.

The Rust server replaces the former Node.js server (kept in Git history, last in commit 7531830).
The Node server was never used in production, so the rewrite does not keep compatibility with its
internal formats. What must stay identical is everything a game client sees through the HTTPS
API.

## 1. Compatibility policy

| Surface | Policy |
|---|---|
| HTTPS API (`/api/v1/...`, HTML pages) | **Identical** for clients: paths, methods, status codes, JSON field names and key order, error `code`/`message`/extra fields, headers the client reads (`Retry-After`, `Cache-Control`, download headers), rate limits and their windows, anti-abuse behaviour (proof of work, lockouts, address blocks), timeouts. `docs/API.md` and the Node route tests are the specification. Parser corner cases that depended on Node internals (llhttp error texts, `JSON.parse` accepting lone surrogates) follow hyper/serde_json behaviour when no client can tell. |
| Realtime protocol | **New frozen protocol v1** (section 7). The game client is updated in the same change. |
| SQLite database | **Fresh schema**, same logical model, own migrations starting at version 1. A Node database is not opened. |
| Game journal | **Native format** (section 6.3). A Node journal is not replayed. |
| Configuration | Same `.env`/environment mechanism and the same key names where the meaning is unchanged; keys tied to the Node process model are removed or redefined (section 9). |
| Logs, metrics | Same log messages and metric names where they still make sense (dashboards and `docs/SIZING.md` use them); journald priorities added. |
| GIF output | Byte-identical to the Node renderer (the golden hashes are test oracles). |
| Elo | Identical to the game's C++ (`test/fixtures/elo-vectors.json`). |

## 2. Process model

One process, one tokio multi-thread runtime (`WORKERS` threads, default = available cores up to
16), plus a few dedicated OS threads for blocking work. There is no primary, no IPC and no shard
bus any more: what the Node primary owned becomes in-process state.

```
                 TCP :443 ── TLS gate ── rustls ── hyper (HTTP/1.1) ──┬── /api/v1/* routes ── auth, store
                                                                        └── /ws upgrade ── connection task
                                                                                              │
   lobby actor (presence, matchmaker, challenges, conduct, rematch,       ◄──── lobby msgs ──┤
   sanctions, refund notices)                                                                │
                                                                                              │ game msgs
   host actors 0..N-1 (one per game shard: rooms, timers, journal, commit) ◄─────────────────┘
        │ journal I/O (blocking thread per shard)      │ finish_batch
        ▼                                              ▼
   journal/shard-<n>/                            store writer thread (one FIFO, BEGIN IMMEDIATE)
                                                 store reader pool (spawn_blocking)
   analysis pool: Stockfish subprocesses (nice 19)    GIF render threads (nice 19)    log writer thread
```

* **Host actors.** `N = WORKERS` game shards (shard numbers `SHARD_BASE .. SHARD_BASE+N-1`, at
  most 64 in total). A game id carries its shard (`ids::shard_of`), so routing needs no table.
  Each actor is a single tokio task that owns its rooms, timer set, journal handle and commit
  state, and processes one message at a time (run to completion). The loop is
  `select! { biased; msg = inbox.recv() => .., _ = beat.tick() => .. }` with a 10 ms beat
  (`MissedTickBehavior::Delay`) for timers, stall detection, commits and compaction.
* **Lobby actor.** One task owning presence (one live connection per account, connection counts),
  the matchmaker queues, challenges and private codes, conduct cooldowns, rematch hand-off, the
  sanction cache and refund notices. It talks to connections through their outbound handles and
  to host actors through their inboxes.
* **Connection tasks.** One task per WebSocket connection: reads frames, runs the Hello state
  machine and the rate buckets, answers pings, forwards game messages to the host actor of the
  game and lobby requests to the lobby actor. Writes go through a byte-accounted outbound queue
  (`realtime::Outbound`) drained by the same task; over `WS_SEND_BUFFER_LIMIT` bytes queued the
  connection is closed with 4303, droppable frames (gestures) are skipped above a quarter of it.
* **Blocking work never runs on runtime threads.** SQLite (writer thread + reader pool via
  `spawn_blocking`), journal file I/O and fsync (one blocking thread per shard), password hashing
  (bounded pool), GIF rendering and Stockfish I/O have their own threads.
* **Time.** `clock::Clock::mono_ms()` (epoch-anchored monotonic, fractional ms) for game clocks,
  timers, buckets, heartbeats and protocol `serverTime`; `clock::Clock::wall_ms()` for stored
  timestamps and expiries. Modules that need deterministic tests take a `SharedClock`.

## 3. Workspace

```
dedicated-server/
  Cargo.toml              workspace (shared dependency versions, lints, profiles)
  rust-toolchain.toml     Rust 1.99.0 (+ x86_64-unknown-linux-musl for the static release build)
  protocol/scacelith-v1.json   realtime protocol schema (source of all generated codecs)
  crates/protocol         scacelith-protocol: generated codec + constants + `protogen` generator
  crates/chess            scacelith-chess: rules, SAN/UCI/FEN, digest, PGN (no dependencies)
  crates/gif              scacelith-gif: GIF renderer and encoder
  crates/server           scacelith-server: the server library and binary
  crates/client           scacelith-client: Rust client SDK, integration tests, `scacelith-bench`
  test/fixtures           shared vectors (Elo, protocol, chess cross-check), also read by the C++ tests
  deploy/systemd          example unit and installation guide
  docs/                   DESIGN, API, PROTOCOL, CONFIG, SIZING, ANTICHEAT, BENCHMARK, RUST-PORT
```

Server modules (`crates/server/src`):

| Module | Content | Owner (rewrite) |
|---|---|---|
| `clock`, `ids`, `log`, `metrics`, `systemd`, `util`, `config`, `cli`, `sys` | foundations, configuration, command line, the only `unsafe` (libc calls) | foundation |
| `store`, `journal`, `store::retention` | SQLite schema, writer thread, reader pool, typed API; game journal | store |
| `security`, `mail` | passwords, TOTP, secret box, keys, proof of work, auth rate counters; SMTP | security |
| `net`, `http` (framework) | listener, TLS + reload, TLS gate, IP guard, abuse tracker, limiters, hyper glue, WebSocket codec, metrics endpoint; router, answers, bodies, schema validation, route rates, handler timeout | net |
| `matching`, `anticheat` (pure) | Elo, matchmaker, challenges, conduct; priors, scoring, UCI engine driver, analyzer | matching |
| `gifsvc` | render pool, cache, quotas (routes come later) | gif |
| `auth`, `http::routes` (auth, account, MFA, SSO, export), `http::pages` | account services and their routes | auth (wave 2) |
| `http::routes` (info, players, leaderboard, games, account games, reports, GIF) | read-side routes | routes (wave 2) |
| `game` | game clock, room, host actor, timers, test doubles | game (wave 2) |
| `realtime`, `app` | connection task, outbound queue, lobby actor, bootstrap, signals, shutdown | realtime (wave 2) |
| `anticheat` (store side), admin CLI | anomalies, sanctions, refunds, reports, analysis queue worker, `admin` commands | anticheat (wave 2) |

The client crates and the C++ game are owned by: protocol (crate `scacelith-protocol`, generated
C++ codec, client adaptation), client (crate `scacelith-client`, integration tests, bench).

**Ownership rule during the rewrite:** an agent edits only the files of its modules. A change
needed in another module goes in the agent's final report (or, when trivial and blocking, as a
separate small commit that says so). New dependencies are added to the workspace `Cargo.toml` and
the crate's manifest in their own commit.

## 4. Conventions

* Rust 2024 edition, `rustfmt` (`max_width = 110`), `cargo clippy --all-targets -- -D warnings`
  clean. Comments, docs and identifiers in English.
* Every public item has a short doc comment. Module docs say what the module owns and which
  section of DESIGN.md it implements.
* No `unsafe` outside `crate::sys` (allowed there with a `// SAFETY:` comment per block).
* Errors: hand-written enums implementing `Display` and `std::error::Error` (no `anyhow` or
  `thiserror`). `unwrap`/`expect` only on invariants, with an `expect` message saying why.
* Panics in a handler, a room or a job are caught at the boundary (`catch_unwind` or task join
  errors), logged, and turned into `Internal`/500, as the Node server turned exceptions.
* Logging: `crate::log::Logger` and the `log_debug!/log_info!/log_warn!/log_security!/log_error!`
  macros (fields with `serde_json::json!` syntax). Never log secrets, tokens or full addresses:
  use `log::ip()` for client addresses.
* Metrics: `crate::metrics` statics (`LazyLock`). Keep the Node metric names, help texts, labels
  and buckets listed in `docs/SIZING.md` and DESIGN 8 unless they lost their meaning; the
  `shard` label is dropped (one process). New metrics use the `scacelith_` prefix.
* Async: tokio. Channels: `tokio::sync::mpsc` for actor inboxes (bounded where a producer can be a
  client, with an explicit overflow policy), `oneshot` for replies. No `async-trait`; traits with
  `impl Future` returns where needed.
* JSON: `serde_json::Value` with `preserve_order` (key order = insertion order, as JavaScript).
  Answers that the client parses keep the Node key order. Integers stay integers (`i64`/`u64`),
  never `f64` unless the Node value was fractional.
* Tests: the Node test suites in `/home/user/rsw/node-ref/dedicated-server/test` (removed from the
  branch) are the specification. Port the assertions of your area as Rust tests (unit tests next
  to the code, black-box tests in `crates/*/tests`). Keep the vector files in `test/fixtures`.
* Commits: small, compiling, with clear messages. Never commit generated build output.

## 5. Foundations (available now)

* `clock`: `Clock` trait (`mono_ms`, `wall_ms`, `now_ms`), `SystemClock`, `ManualClock`,
  `SharedClock = Arc<dyn Clock>`, free functions `clock::mono_ms()`, `clock::wall_ms()`.
* `ids`: `UserId = u32`, `GameId = u64`, `ConnId = u32`, `GameIdAllocator`, `shard_of`,
  `is_game_id`, `created_ms`, `ID53_LIMIT`.
* `log`: `log::init(Options)`, `Logger::root().child("name")`, macros, `log::flush()`,
  `log::capture()` for tests, `scrub`, `iso_time`, `civil_from_days`. Journald priorities when
  `JOURNAL_STREAM` is set.
* `metrics`: `counter`, `counter_vec`, `gauge`, `gauge_vec`, `gauge_fn`, `histogram`,
  `histogram_vec`, `registry().render()`, `js_number`.
* `systemd`: `notify`, `ready`, `stopping`, `journald`.
* `config::Config`: every setting as a typed field (see section 9), `Config::for_tests()`.

## 6. Storage

### 6.1 SQLite

* `rusqlite` with the bundled SQLite. One file (`DB_PATH`), WAL, `synchronous=FULL`,
  `foreign_keys=ON`, `secure_delete=ON` (privacy guarantee of DESIGN 7), `busy_timeout=5000`,
  `journal_size_limit` 64 MiB, statement cache ~256.
* One **writer thread** owns the only writable connection and runs jobs from one FIFO, each job in
  `BEGIN IMMEDIATE`. All writes of the process go through it, which also gives the ordering the
  anti-cheat needs (anomalies before the game batch that queues analysis).
* A **reader pool** (`query_only`) serves reads through `spawn_blocking`.
* Public shape: `Store` (cheap `Clone`) with `async fn read(|conn| ...)` and
  `async fn write(|tx| ...)`, and typed table APIs on top (`store.users()`, `store.sessions()`,
  `store.games()`...) whose method names follow the Node store API in snake_case. Read-modify-write
  operations run inside one writer job. Errors: `StoreError { Busy, Constraint(kind), NotFound,
  Closed, Sqlite(..) }`.
* Schema: designed afresh from the logical model of the Node schema (store notes section 3):
  `STRICT` tables, integer milliseconds for times, `INTEGER` booleans, BLOB move lists (u16 LE
  moves, u32 LE spent and clock times), TEXT hashes, sensible indexes for every query (tests check
  query plans of the hot queries). Migrations are numbered SQL files embedded with
  `include_str!`, tracked in `schema_migrations` with a checksum, applied in `BEGIN IMMEDIATE`.
* Retention purge (DESIGN 7): scheduled task, chunked jobs, adaptive chunk size, `busy` = retry
  next interval.

### 6.2 Store writer ordering contract

Callers that must observe their own writes do it inside one job. Game commits (`finish_batch`)
carry the game id in errors so a failing batch can be retried game by game.

### 6.3 Game journal

* One journal per host shard in `JOURNAL_DIR/shard-<n>/`, append-only numbered segments.
* Record: `len u32 | kind u8 | game u64 | at f64 | payload | crc32c u32` (little-endian; CRC-32C
  over everything before it). Kinds: created, move/event payloads produced by the room, snapshot,
  committed. A torn or corrupt tail ends a segment; replay continues with the next segment.
* Group flush: appends are buffered by the actor, written by the shard's blocking I/O thread
  (`write` + `fdatasync` when `JOURNAL_FSYNC`), one batch in flight; the actor reads
  `has_unwritten()` and `failed_writes()` synchronously.
* Compaction writes a snapshot for long games and deletes segments once every game in them is
  committed or superseded (directory fsync before deletion).
* Fault injection hook (`write_batch` override) for the commit-gate tests.

## 7. Realtime protocol v1

Frozen as "version 1" at the end of the rewrite. Generated from `protocol/scacelith-v1.json` by
`protogen` (Rust codec, the game's C++ codec, `docs/PROTOCOL.md` tables, golden vectors).

* **Transport:** WebSocket (RFC 6455) on the API port, path `/ws`, subprotocol **`scacelith.rt1`**
  (the old token `scacelith.v1` meant protocol 3 and gets HTTP 426 with
  `{"error":"unsupported_protocol","supported":["scacelith.rt1"]}`). Binary frames only, one
  message per frame, no extensions; text frame = close 1003; client message <= 512 bytes (checked
  from the frame header), server message <= 64 KiB.
* **Encoding:** `u8 type | fields`, little-endian, no padding, exact length. Types 0x01-0x7F
  client to server, 0x80-0xFF server to client, 0x00 invalid. Field types `u8 u16 u32 i32 f64
  id53 bool str8 enum struct list16` with the validation rules of protocol 3 (finite f64, id53 <
  2^53, strict UTF-8 without NUL, bounds on byte lengths).
* **Frozen forever:**
  1. Hello prefix `0x01 | seq u32 (=1) | proto u16 (=1) | minor u16 | caps u64 | ...`. The server
     reads the prefix before decoding the rest, so any later client gets a proper refusal.
  2. Error layout `0x81 | ref u32 | code u8 | fatal u8 | game id53` and the meaning of the existing
     error codes and close codes.
  3. Welcome starts with `proto u16, minor u16 (negotiated = min), caps u64 (negotiated = and)`.
  4. Ping/Pong layouts `nonce u32 [serverTime f64]`.
* **Hello** `seq, proto, minor, caps, client str8 0..48, token str8 16..160` (no schema hash).
  **Welcome** `proto, minor, caps, serverTime, userId, username str8 1..24, serverName,
  heartbeatMs, clientPingMs, maxMsgPerSec, msgBurst, activeGame, gestureRate, gestureBurst`.
* **Evolution inside v1:** a minor version only adds message types or appends fields at the end
  of a message; capability bits gate optional features. Clients decode server messages
  leniently (ignore unknown types and trailing bytes; unknown values of open enums map to a
  fallback); the server decodes client messages strictly for the negotiated minor (anti-cheat).
  Reserved ranges: 0x70-0x7F and 0xF0-0xFF experimental, never published.
* **Message set:** the protocol 3 messages and semantics (anti-cheat `posHash`/`ply`/`thinkMs`,
  clocks, gestures relayed by byte copy, snapshots, matchmaking, challenges, private codes,
  rematch, notices), with symmetric bounds (server `move <= 0x7FFF`, `ply <= 1199`; challenge time
  controls 15..10800 s and 0..180 s both ways), `MoveRejected` always followed by `GameSnapshot`,
  exactly one `Ack` or `Error` per lobby request, `gseq` specified as the ordering key between a
  snapshot and events. Never-emitted values are removed from the spec with their numbers reserved.
* **Fingerprint:** first 4 bytes (big-endian) of SHA-256 over the canonical schema JSON;
  informational only (vectors, `/api/v1/info`, logs). `/api/v1/info` keeps the shape
  `protocol: {min: 1, max: 1, schema, subprotocol: "scacelith.rt1"}`, so a protocol-3 client shows
  "update the game" before connecting.

## 8. Interfaces between modules (wave 2)

Written before wave 2 starts, from the APIs that wave 1 delivered: connection task and host
actor messages (`HostMsg`, `Endpoint`), lobby messages, the auth service used by routes and by the
Hello (`validate_token`), the anti-cheat hooks of the host and the router.

## 9. Configuration

* Sources: environment, then `.env` next to the binary's working directory or the file named by
  `SCACELITH_ENV_FILE`; secrets may come from `<KEY>_FILE` (systemd `LoadCredential=`). A
  declarative key table generates `.env.example` and `docs/CONFIG.md` (`scacelith-server
  gen-config-docs`, with a test that the committed files are current).
* `check-config` prints the effective configuration (secrets as `<set>`/`<unset>`) or every error.
* Node-only keys are removed (`SHARD_OVERLOAD_LAG_MS`, `LISTEN_REUSE_PORT`, `UV_THREADPOOL_SIZE`
  and the like): an unknown key in the environment is ignored, an obsolete key in `.env` gets a
  warning, never an error. Per-worker limits become whole-server limits with defaults scaled to
  the machine and documented.

## 10. Operations

* Logs: JSON lines on stdout (or `LOG_FORMAT=pretty`), `<N>` syslog priorities under journald,
  same messages as before. No log files.
* systemd: `Type=notify` (`READY=1` after migrations, journal recovery and listeners;
  `STOPPING=1` at shutdown), `SIGHUP` reloads the TLS certificate, `SIGTERM`/`SIGINT` drain
  (`Notice{ServerShutdown}`, `SHUTDOWN_GRACE_MS`, final commits, journal flush, store close), a
  second signal exits with code 1. Example unit and guide in `deploy/systemd/`.
* Exit codes: 0 success, 1 failure, 2 usage.
* Release build: static `x86_64-unknown-linux-musl` binary, GPL-3.0-or-later, source in this
  repository.
