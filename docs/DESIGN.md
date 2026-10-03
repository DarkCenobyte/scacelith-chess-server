# Scacelith dedicated server: design

This document describes what the dedicated server does and how its parts work together: the
process, the main flows, the contract of each subsystem, the game policies, what survives a crash
and the security model. It is the reference for behaviour; module documentation cites its sections
("DESIGN 5.3").

| Document | Content |
|---|---|
| [RUST-PORT.md](RUST-PORT.md) | code layout: crates, modules, conventions, interfaces, storage formats |
| [API.md](API.md) | the HTTPS API: every endpoint, its answers, errors and rate limits |
| [PROTOCOL.md](PROTOCOL.md) | the realtime protocol, frozen as version 1 |
| [CONFIG.md](CONFIG.md) | every configuration key (generated from the code) |
| [DEPLOY.md](DEPLOY.md) | installation, systemd, TLS, logs, backups |
| [ANTICHEAT.md](ANTICHEAT.md) | anomalies, the statistical model, moderation |
| [SIZING.md](SIZING.md) | capacity and hosting |

The server is written in Rust (toolchain 1.99.0, pinned by `rust-toolchain.toml`). It replaced a
Node.js server, which stays in the Git history (last in commit 7531830); "the former server" in
the code and the documents refers to that history only.

## 1. Process

One process serves everything:

```
TCP ── TLS gate ── rustls ── HTTP/1.1 (hyper) ─┬─ /api/v1, pages ── auth, store, GIF service
                                                └─ /ws upgrade ── connection tasks (2 per socket)
                                                                     │ lobby          │ game
                                                                     │ requests       │ messages
lobby actor: presence, queues, challenges,  ◄────────────────────────┘                ▼
conduct, game creation, rematches, bans,         host actors, one per shard: rooms, timers,
refund notices                                   journal (an I/O thread per shard), commits
store: writer thread (one FIFO of write jobs) + reader connections ◄── commits, reads
analysis pool (Stockfish child processes, nice 19) · GIF render threads (nice 19) · log writer
```

* **Async runtime.** A tokio multi-thread runtime with `WORKERS` threads named `scacelith-rt`
  (`auto`: one per CPU core, 1 to 16; at most 64). Listeners, TLS handshakes, HTTP handlers,
  connection tasks, the lobby and host actors and the periodic jobs are tasks on it. Blocking work
  runs on tokio's blocking pool (store reads, password hashes, the journal replay) or on dedicated
  threads.
* **Game shards.** `WORKERS` host actors, numbered `SHARD_BASE` to `SHARD_BASE + WORKERS - 1`
  (64 shard numbers at most, checked at start). A game id packs `(ms since 2026-01-01) << 12 |
  shard << 6 | seq` and stays below 2^53; its shard bits name the host, so a message reaches its
  game without a lookup table (`ids`). Section 5.3.
* **Lobby.** One actor owns what must be unique in the server: presence (one live connection per
  account), the matchmaker queues, challenges and private codes, conduct cooldowns, game creation
  and placement, rematches, the ban cache and the rating refund notices. Sections 5.4 and 5.8.
* **Connections.** Each WebSocket has a reader task (Hello, rate buckets, heartbeat, dispatch)
  and a writer task that drains its outbound queue. Section 5.8.
* **Store.** SQLite in WAL mode. One thread (`store-writer`) holds the only writable connection
  and runs write jobs one at a time in submission order; four `query_only` connections serve
  reads on the blocking pool. Section 5.5.
* **Journal.** Each shard's journal has an I/O thread (`journal-<n>`) that writes its batches.
  Section 5.6.
* **Other threads.** `log-writer` (log lines through a bounded queue), `gif-render` threads at
  nice 19 (5.9), and the engine analysis: `ANALYSIS_WORKERS` tasks, each driving a Stockfish child
  process at nice 19 (6.5).

A move never waits for the database or the disk: one host message, one journal append (in memory;
the I/O thread writes it), and the frames queued for two sockets.

**Start-up** (`app`): systemd `STATUS=starting`, then in order: the shard range and
`ABUSE_EXEMPT` checks, the open file limit, the data directory, the store (migrated, server id
read), the lobby's inbox, the mailer and the auth service, the background jobs (retention purge,
analysis gauges, the 10 s sweep of the auth counters, the analysis pool), the GIF service, the
anti-cheat services, the game hosts (`STATUS=recovering N games`: each shard replays its journal
and announces the games it recovered to the lobby), the lobby actor, the realtime layer, the HTTP
API, and the listeners, bound last. `/readyz`, `READY=1` and `STATUS=ready` follow, then the
watchdog keep-alive. A failure stops what was started and exits with 1. SIGHUP reloads the
certificate (`RELOADING=1`, then `READY=1`).

**Shutdown** (SIGTERM or SIGINT; a second signal exits at once with 1): `STOPPING=1`,
`STATUS=draining`; the listeners stop accepting (`/readyz` 503, upgrades 503 `shutting_down`,
requests in progress finish within `SHUTDOWN_GRACE_MS`), the lobby's timers, the analysis and
the retention purge stop, and the connections drain: `Notice{ServerShutdown, arg = grace}`,
`SHUTDOWN_GRACE_MS`, then `Error{ShuttingDown}` and close 4008. The hosts then make their final
commits and close their journals, the lobby ends, the API handlers still running get 5 s, the
buffered anomalies go to the store, the mailer gets 5 s, the GIF threads stop, the auth service
saves its security events, and the store closes last (section 5.5).

## 2. Code map

[RUST-PORT.md](RUST-PORT.md) section 3 lists every crate and module. The sections below map to:

| Section | Code (`crates/server/src` unless stated) |
|---|---|
| 5.1 protocol | `crates/protocol`, `../src/net/protocol_gen.*` (generated) |
| 5.2 chess | `crates/chess` |
| 5.3 rooms and hosts | `game::room`, `game::host`, `game::clock`, `game::timers` |
| 5.4 matching | `matching` (Elo, matchmaker, challenges, conduct), `realtime::lobby` |
| 5.5 store | `store`, `crates/server/migrations` |
| 5.6 journal | `journal` |
| 5.7 network edge | `net` (listeners, TLS, gate, guard, abuse, limits, HTTP/1.1, metrics endpoint) |
| 5.8 realtime | `net::upgrade`, `net::ws`, `realtime` |
| 5.9 HTTP and GIFs | `http`, `gifsvc`, `crates/gif` |
| 6.5, 6.6 anti-cheat, refunds | `anticheat`, `store` |
| 8 security | `auth`, `security`, `mail`, `net::guard`, `net::abuse`, `log` |

## 3. Main flows

**Login (HTTPS).** `POST /api/v1/auth/login` returns a session token `sct_<43 base64url
characters>` (32 random bytes); the server stores its SHA-256 only. With MFA the first step
returns an `mfaToken` (5 minutes, single use) for `POST /api/v1/auth/login/mfa`. API.md has the
whole flow, Google sign-in included.

**Realtime connection.** `wss://host:WS_PORT/ws` with the subprotocol `scacelith.rt1` and no
`Origin` header (browsers are refused unless `WS_ALLOWED_ORIGINS` lists them). The upgrade checks
(5.8) end with the admission counts, and the `101` answer carries `Scacelith-Server-Id` (the
`serverId` of `/api/v1/info`), so that a client which skipped `/info` still checks that its saved
session belongs to this server. Within `WS_HELLO_TIMEOUT_MS` the client sends `Hello{seq, proto,
minor, caps, client, token}`. The connection task validates the token (auth service, 30 s cache),
checks e-mail verification when `REQUIRE_EMAIL_VERIFICATION` is on, reads the stored ban, then
asks the lobby to claim the player's presence. The lobby refuses a newcomer once `MAX_CONNECTIONS`
players are online (`ServerFull`, close 4006) but always admits a player whose game is in
progress; it refuses a banned player (`Notice{Banned}`, `Error{Banned}`, close 4004) and replaces
an older connection of the same account (`Notice{ReplacedByNewConnection}`, `Error{Replaced}`,
close 4007). The task answers `Welcome` (with `activeGame`) and attaches the connection to that
game, whose host sends a `GameSnapshot`.

**Matchmaking.** `QueueJoin{category, rated}`: the connection task checks the category
(`InvalidCategory`), reads the player's rating, stored ban and conduct cooldown, then posts the
request to the lobby. Every `MATCH_TICK_MS` the matchmaker pairs players. For each pair the lobby
reads the stored bans again, creates the game on the host with the fewest games (the lowest shard
on a tie; 5 s timeout, a game created later is cancelled) and tells both connections to attach;
the host sends each a `GameSnapshot`. Challenges and private codes take the same path with the
players' ratings read again; a rematch is created on the shard of the game it follows.

**A move.** The client sends `Move{seq, game, ply, move, posHash, thinkMs, drawOffer}` when its
player chooses the destination square (the robot's hand then plays the move and presses the clock:
animation only), or, in a game without `autoPress` (below), when its player presses the clock. The
connection task posts the decoded message, with the time it read the frame, to the host named by
the game id. The host validates it (6.2), updates the position and the clocks, journals the move
and encodes one `MoveMade` for both players. The opponent's client animates its robot playing it;
the mover's client takes its own `MoveMade` as the confirmation, and a `MoveRejected` makes it
restore the server position from the `GameSnapshot` that follows.

**Clock press.** Whether the robots press the clock by themselves is a setting of each game,
`GameSnapshot.autoPress`. The lobby sets it for every new game (queue, challenge, private code)
from `AUTO_PRESS_CLOCK` (true by default); a rematch keeps the value of the game it follows. The
room writes it in the game's `created` journal record, so a replay, a compaction snapshot and a
restart keep it whatever the configuration says by then. A finished game played without it has
the record flag 8 (manual clock) in its database row (5.5). The clock rules are the same either
way: without `autoPress` the client sends the move when its player presses the clock, so the time
until the press is charged like any thinking time.

**Gestures.** The player's head, the piece in hand, where it is aimed and a move placed before the
clock press reach the opponent live as `Gesture` (C2S `0x28`), relayed as the server `Gesture`
(`0xA6`) and never stored. A gesture never touches the `WS_MSG_RATE` bucket nor gets a
`RateLimited` answer: it has a bucket of its own (`GESTURE_RATE` per second, `GESTURE_BURST`; both
announced in `Welcome`, 0 and 0 when `GESTURE_RATE=0`), and one beyond it is dropped silently with
its seq still counted. More drops in 10 s than max(50, 10 x `GESTURE_BURST`, `GESTURE_RATE` x
(`HEARTBEAT_TIMEOUT_MS` + `HEARTBEAT_INTERVAL_MS` + 250 ms)) is a flood (close 4301): the last term
is what a client pacing its gestures at the rate sends during the longest silence a connection
survives, which may arrive at once. The buckets and their drop windows run on the monotonic clock.
A gesture within the bucket is decoded strictly and its seq checked like any message (malformed:
close 4001), and it must name a game attached to the connection. The host finds the sender's
colour (not a player: dropped, no anomaly; gestures are not authoritative) and copies the frame,
minus its seq, into the server `Gesture` for the opponent (the two layouts match byte for byte:
`ClientGesture::relay_frame`). That frame is queued with `send_droppable`, which skips it while the
opponent's connection holds more than a quarter of `WS_SEND_BUFFER_LIMIT`, so a gesture never
closes a slow client. The room, its clocks, `gseq`, the journal and the anti-cheat never see
gestures; they are relayed as long as the room exists, the rematch window included. Metrics:
`scacelith_gestures_relayed_total` and `scacelith_gestures_dropped_total{reason}` (`rate`,
`not_attached`, `no_game`, `not_player`, `no_opponent`, `backlog`, `malformed`). Cost: SIZING.md.

**End of game.** The room decides the result (mate, flag, resignation, agreement, claim,
abandonment, abort...); the host sends `GameEnd` to both players, journals it and queues the game
for the database. A batch of finished games is committed at most `DB_COMMIT_MS` after the first of
them ended, one batch in flight per host, in one store write job (`finish_batch`): the game rows,
both players' ratings (read and written inside the transaction, 6.6), the rating refunds owed by a
banned cheater and the analysis job (6.5). The host first waits until its journal has written and
fsynced every record appended before the batch was chosen, so the database never holds a finished
game whose `ended` record a crash could still lose (which would bring the game back running after
the restart). The anti-cheat hands the first anomaly of each kind in a game to the store writer at
once, and the writer runs jobs in submission order, so the analysis policy of the commit sees it.
After the commit the host sends `RatingUpdate` to the attached players, appends the game's
`committed` journal record and tells the lobby the game ended. A commit that fails is retried
after a backoff (max(100 ms, `DB_COMMIT_MS`), doubling up to 10 s); a batch that fails because of
one game is committed game by game. `finish_batch` ignores a game id it already holds, so a replay
never applies a rating twice.

When the journal's writes keep failing (a full disk, a read-only or failing `JOURNAL_DIR` volume,
too many open files), waiting for it would keep every finished game out of the database: no rating
change, both players still busy for the lobby (every queue join or challenge refused with
`AlreadyInGame`), and the results lost at the next stop. So the wait is bounded. A failed flush
marks the batch's games to be journaled again (one snapshot each, right before their commit); at
the third failed flush in a row (`JOURNAL_GATE_TRIES`, about 0.3 s after the first with the default
`DB_COMMIT_MS`), and at the first one during a shutdown, the batch is committed without the
journal. Its snapshots and `committed` records are still appended in case the journal comes back.
The host logs one error per episode and counts these games in
`scacelith_game_commit_unjournaled_total`; the first flush that writes without a failure ends the
episode. The database is then the only durable copy of these results. One risk remains, the one the
wait avoids: after a crash before the journal has written the game's snapshot or `committed`
record, a game whose `ended` record was lost comes back running; whatever result it ends with, the
database keeps the first one.

**Reconnection.** A lost connection keeps the game running (the player's clock too, except for the
clock hold of a game restored after a restart: 6.4). The opponent gets
`GameEvent{PlayerDisconnected, arg = grace ms}`. The player's next connection says `Hello`; the
lobby knows the game in progress, `Welcome.activeGame` names it and the host sends a
`GameSnapshot`. After the grace without a connection: 6.4. The game client spreads its
reconnections so that a restart or a full server does not bring every player back at the same
instant: full jitter (a random delay between 0.5 s and min(30 s, 2 s x 2^n)), 60 to 120 s after
`ServerFull` (HTTP 503 at the upgrade or close 4006), a first attempt 5 to 35 s after a shutdown
(`Notice{ServerShutdown}` or close 4008), and no `/api/v1/info` request for 10 minutes after losing
a connection that had reached `Welcome`, so a restart costs one TLS handshake per player (another
server at the same origin is caught by the server id of the `101` answer, before `Hello`). A
player whose game is in progress is the exception: the grace is short (at least
`RECONNECT_GRACE_MIN_MS`, 15 s by default, and `RECOVERY_GRACE_MS`, 90 s by default, for a game
restored after a restart), so their attempts are 8 s apart at most unless the server gave a
`Retry-After`, and the first one after a shutdown comes 1 to 8 s after it.

**Client pings.** Besides answering the server's heartbeat (`HEARTBEAT_INTERVAL_MS`, which also
measures the round trip used for lag compensation), the game client sends its own `Ping` for its
ping indicator and its estimate of the server clock. The server chooses how often:
`Welcome.clientPingMs` = `CLIENT_PING_INTERVAL_MS` (10 s by default, 1 s to 60 s). The client sends
four quick pings after each `Welcome`, then follows that interval. Every ping costs CPU for every
connected player, so a lower value makes the indicator more reactive at a price on a small machine
(SIZING.md). A connection gets at most one `Pong` per 950 ms; other pings are dropped. The client's
liveness check does not depend on that interval: when nothing came for 1.5 heartbeats (at least
7.5 s, at most 90 s) it sends one `Ping` at once, and it calls the connection dead after two
heartbeats (at least 10 s, at most 120 s) with nothing received; it counts `Welcome.heartbeatMs`
as 60 s at most.

## 4. Foundations

* **Configuration** (`config`): one table of keys (`config/keys.rs`) with their types, ranges,
  defaults and descriptions; `Config` is the validated, typed result, shared as `Arc<Config>`.
  `scacelith-server check-config` prints the values in use; `gen-config-docs` writes
  `.env.example` and [CONFIG.md](CONFIG.md) from the table.
* **Time** (`clock`): a monotonic clock in fractional milliseconds, anchored to the Unix epoch at
  start (game clocks, timers, rate buckets, heartbeats, the `serverTime` fields of the protocol),
  and the wall clock in integer milliseconds (stored timestamps, token expiries, bans, retention).
  A step of the wall clock changes no game clock, timer or bucket.
* **Logs** (`log`): JSON lines on stdout (or a readable format), written by the `log-writer`
  thread, with redaction (section 8).
* **Metrics** (`metrics`): a Prometheus registry served by the metrics endpoint (5.7); names are
  `scacelith_<area>_<what>[_unit][_total]`.
* **Cross-module events** (`events`): `HostEvents` (hosts to the lobby: game ended, game recovered,
  rematch, conduct incident), `AnomalySink` (hosts and connections to the anti-cheat),
  `SessionEvents` (auth to realtime: sessions revoked), `SanctionEvents` (anti-cheat to the lobby:
  ban applied, refunds pending). Every method posts and returns; none blocks.

## 5. Subsystems

### 5.1 Realtime protocol v1

The protocol is specified, and frozen as version 1 (minor 0), in [PROTOCOL.md](PROTOCOL.md). Its
single source is `protocol/scacelith-v1.json`; `protogen` generates the Rust codec
(`crates/protocol/src/gen.rs`), the game's C++ codec (`../src/net/protocol_gen.h` and `.cpp`,
namespace `net::proto`), the tables of PROTOCOL.md and the golden vectors
(`test/fixtures/protocol-vectors.json`), read by the Rust tests and by `../tests/net_tests.cpp`.
RUST-PORT.md section 7 describes the generator and the freeze.

* Constants: `PROTOCOL_VERSION` 1, `PROTOCOL_MINOR` 0, subprotocol `scacelith.rt1`, `CAPS` 0,
  `FINGERPRINT` (the first 4 bytes, big-endian, of the SHA-256 of the schema's canonical JSON),
  client messages of at most 512 bytes, server messages of at most 65,536, games of at most 1200
  plies. The C++ names are `kProtocolVersion`, `kMinor`, `kWsSubprotocol`, `kFingerprint`, ...
* Rust: one struct per message implementing `Message` (`encode`, `to_bytes`, `decode`,
  `validate`); `ClientMsg::decode` is strict (the server's side), `ServerMsg::decode` is lenient
  (unknown types and trailing bytes of a later minor are skipped). `HelloPrefix` and
  `decode_hello` read the Hello of any minor; `close_code_for` maps a fatal error code to its close
  code. The names used in both directions are `ClientPing`/`ServerPing`, `ClientPong`/`ServerPong`
  and `ClientGesture`/`ServerGesture`.
* C++: one struct per message with the schema's field names, `C_`/`S_` prefixes for Ping, Pong and
  Gesture; `encode(m, out)` appends, `decode(p, n, out)` is strict for client messages and lenient
  for server messages, plus `decodeHello`, `peekType`, `peekSeq` and `errorCodeForClose`.
* Moves are the u16 `from | to << 6 | promo << 12` (squares `file + 8 * rank`, a1 = 0);
  `posHash` is the position digest of the chess crate (5.2).

### 5.2 Chess rules (`crates/chess`)

`scacelith-chess` mirrors the game's `src/chess/chess.h` exactly: the same automatic endings, the
same claim rules and the same FEN normalisation of castling rights and en passant. It has no
dependencies. `Position` (a 168-byte `Copy` board with its Zobrist key) generates and checks legal
moves, plays them, and gives the digest (`posHash`), FEN, SAN and UCI; `ChessGame` adds the move
list, repetition counts, claims and the end of the game; the crate also reads and writes PGN.
`tests/crosscheck.rs` compares it with the C++ rules on `test/fixtures/chess-crosscheck.json`
(written by `tools/gen-chess-crosscheck.sh`), and the perft tests check move generation. The
rooms use it through the `game::rules::Rules` trait, which tests replace with a scripted double.

### 5.3 Game rooms and host actors (`game`)

**Room** (`game::room::GameRoom`): one authoritative game. It is deterministic: time is passed in
(integer milliseconds of the monotonic clock), it owns no timer and does no I/O. Every entry point
(`on_move`, `on_resign`, `on_draw_offer`, `on_draw_answer`, `on_draw_claim`, `on_abort`,
`on_rematch`, `on_disconnect`, `on_reconnect`, `on_resync`, `forfeit`, `server_abort`, `tick`)
first processes the deadlines due at its time (flag, first-move timeout, grace expiry, rematch
window), so simultaneous events resolve by time. Each returns an `Outcome`: frames to broadcast
and to reply to the sender (encoded once), an anomaly, journal records, conduct incidents, a
rematch to create, the end flag, and `clock_started` (a clock held since a recovery that has just
started: the host then sends the other player a new `GameSnapshot` unless the outcome holds a move
or the end). A request carries a `Timing`: real time, credited arrival and stall start (6.1).
`next_deadline`, `snapshot`, `record` (the row for `finish_batch`) and `journal_state`,
`journal_snapshot` and `from_journal` (5.6) complete it.

**Host actor** (`game::host`): one tokio task per shard owns the rooms of its games, their timers
(one deadline per game in an ordered set), the shard's journal and the commit of finished games.
`Hosts::start` opens each journal, replays it on a blocking thread before the shard serves
anything, and spawns the actor. The actor handles its inbox one message at a time (a connection's
messages and its detach stay ordered), and a 10 ms beat (`MissedTickBehavior::Delay`) fires due
deadlines, detects stalls (6.1), starts commits and builds compaction snapshots (at most 2 per
beat). A beat first handles the messages already in the inbox, never those that arrive meanwhile,
so a busy inbox cannot hold the timers back. `HostHandle` is the cloneable way in: `client` (a
strictly decoded game request with its read time), `gesture`, `attach` (binds a connection and
sends it a `GameSnapshot`), `detach`, `rtt`, `forfeit_user`, `decline_rematch`, `create`, `cancel`
(a game whose creation came after the lobby's timeout: `ServerAborted`, no conduct incident),
`load` and `stats`. `Hosts::pick` places a new game (5.4). The host talks back through
`HostEvents` and `AnomalySink`.

A panic in room code is caught: the request gets `Error{Internal}` and the game goes on (a timer
that panicked is retried a second later). A request for a game the sender does not play gets
`Error{NotInGame}` and the anomaly `foreign_game`. Anomalies go to `AnomalySink::record`; the kinds
`foreign_game`, and `out_of_turn` and `illegal_move` in a synchronised position, are certain
cheats: with `AUTO_SANCTION_CERTAIN_CHEATS` the host ends the game as a forfeit at the arrival of
the request, sends the sender a fatal `Error{CheatDetected}` (close 4302) and calls
`AnomalySink::sanction_certain` (6.5).

**Recovery.** At start each shard rebuilds its games from the journal: a running game is restored
with the restart rules of 6.4 and announced to the lobby (`HostEvents::game_recovered`), a game
that ended but was not committed is queued for its commit, a game whose records cannot all be
replayed is rebuilt as far as they allow and ended `ServerAborted`, and one that cannot be rebuilt
at all is dropped (its `committed` record is appended so that its segments go).

### 5.4 Matching (`matching`, `realtime::lobby`)

The `matching` module holds pure state machines owned by the lobby actor:

* `elo`: FIDE ratings per category (6.6), a mirror of the game's `src/game/elo.h` / `elo.cpp`,
  both checked against `test/fixtures/elo-vectors.json`. `Categories` maps a time control to its
  official category id (`3+2`, ...) or `custom`.
* `matchmaker`: per-category queues, a search window that widens with the wait, colour balance,
  `MATCH_REPEAT_LIMIT` bookkeeping (`record_pairing`, `repeat_limited`), and `hold_pair` (two
  players whose game could not be created are not paired together for 5 s). `QueueStatus` is sent
  every 3 s.
* `challenges`: direct challenges and private codes (6 characters of
  `23456789ABCDEFGHJKMNPQRSTUVWXYZ`), at most 3 pending outgoing per player, base time 15 s to 3 h,
  increment at most 180 s, expiry.
* `conduct`: abandon, abort and no-show incidents and the cooldowns of 6.4, recorded in one store
  write job and cached.

**Lobby actor.** It never waits on the database while it holds a decision: the connection tasks
read what a request needs (rating, stored ban, cooldown) before posting it, and work that needs the
store or a host afterwards runs in spawned tasks that report back. Its timers: the match tick
(`MATCH_TICK_MS`), queue status refresh (3 s), challenge expiry (1 s), a 10 s sweep, and the
refund notice poll (5 s). Game creation: a player already busy gets `AlreadyInGame`, a player with
a cached ban `UserUnavailable`; a creation task reads the stored bans (a ban found there is
enforced, not only cached), reads the ratings again for challenges and rematches, and asks
`Hosts::pick` for a host (the rematch's former shard, otherwise the host with the fewest games).
On success the players count as in a game, leave their queues and their live connections attach.
A ban (`SanctionEvents::sanction_applied`, or a stored ban found at a queue join, challenge,
rematch or game creation) kicks the player (`Notice{Banned}`, `Error{Banned}`, close 4004),
forfeits a game in progress, and drops the player's queue entry and challenges; a stored ban found
at Hello refuses that connection the same way. Revoked sessions close their connections
(`Notice{SessionRevoked}`, `Error{Unauthorized}`, close 4003).

### 5.5 Store (`store`)

SQLite through rusqlite (bundled SQLite), opened with WAL, `synchronous=FULL`, `foreign_keys=ON`,
`secure_delete=ON` (section 7), `busy_timeout` 5 s and `journal_size_limit` 64 MiB. All times are
epoch milliseconds. The schema is one migration, `crates/server/migrations/001_initial.sql`,
embedded in the binary and recorded in `schema_migrations`.

* **Writer.** One thread, one connection, one FIFO: a job is queued when it is submitted, and each
  runs in its own `BEGIN IMMEDIATE` transaction. Two jobs submitted one after the other run in that
  order, which the anti-cheat relies on (anomalies before the commit that reads them, a certain
  anomaly before the ban it causes).
* **Readers.** `query_only` connections on the blocking pool; a read started after a write job
  answered sees that write.
* **API.** `Store::read` and `Store::write` run a closure on a `Db`, which has a typed API per
  table: `meta`, `users`, `mfa`, `sessions`, `tokens`, `signups`, `sso`, `ratings`, `games`,
  `conduct`, `sanctions`, `anomalies`, `security`, `analysis`, `integrity`, `reports`, `refunds`;
  `store.users()` and the like are async shortcuts for one call. `cargo doc` documents each method.
* **Commit of finished games** (`finish_batch`, one transaction): inserts each game; for a rated
  game reads both rating records inside the transaction, applies `matching::elo` and stores the K
  factor of each side's change; refunds a game recorded during a cheater's ban (6.6); queues the
  analysis job by the policy of 6.5. Each entry of the answer has the rating changes, whether the
  game was already stored, and the analysis policy's decision (`analysis_skipped`,
  `analysis_displaced`). Record flags: 1 rated requested, 2 recovered after a restart, 4 forfeit,
  8 manual clock (`autoPress` off).
* **Closing.** The store closes last at shutdown. The writer waits up to 7 s for the jobs already
  submitted, longer than `busy_timeout`, so that one wait for another process's lock (the admin
  command) loses nothing; jobs still unanswered then fail with "outcome unknown" (finished games
  stay in the journal).

### 5.6 Journal (`journal`)

Crash safety of games in progress without a database write per move. One journal per shard,
`JOURNAL_DIR/shard-<n>/segment-<seq>.log`. A record is `u32 length | u8 kind | u64 game | f64 at |
payload | u32 crc32c` (little-endian); kinds: 1 created, 2 move, 3 event, 4 ended, 5 committed,
6 snapshot. The room encodes the payloads (a move is 32 bytes). `Journal::append` encodes into an
in-memory buffer; the shard's I/O thread writes a batch `JOURNAL_FLUSH_MS` after its first record,
or at once on `flush()`, with one `write` (+ `fdatasync` when `JOURNAL_FSYNC` is on), one batch in
flight. Segments rotate at 16 MiB; the first batch after a start opens a new segment. The API:
`open`, `append`, `committed`, `flush`, `has_unwritten`, `failed_writes`,
`compaction_candidates`, `recover`, `stats`, `close`. A journal directory belongs to one process.

**Deletion.** A segment is deleted when no game needs it any more: every game it mentions is ended
*and* committed, or has a newer snapshot. A segment holding a game's `committed` record outlives
every other segment that mentions that game, so a recovery never sees a committed game without its
`committed` record.

**Compaction.** Without it a segment stays as long as the oldest game it mentions runs, and with
custom time controls (up to 3 h + 180 s) that is gigabytes, all replayed at a restart. A `snapshot`
record holds the whole state of one game (the created spec, every move with its clocks, one
checkpoint with offers, presence, desyncs, flags, turn start and lag quotas, and the result of a
finished game; about 45 bytes per ply, under 60 KB for the longest game) and supersedes every
earlier record of its game. When the journal starts segment N, the games not committed whose first
needed segment is `N - JOURNAL_COMPACT_SEGMENTS` or older are queued; the host takes at most 2 per
beat and a batch holds at most 8. Once the batch holding a snapshot is written and fsynced, that
segment becomes the game's first needed one and the older segments go by the rule above, so a
shard's journal stays around `JOURNAL_COMPACT_SEGMENTS + 1` segments however long its games last.
Games that end sooner are never snapshotted.

**Crash safety.** Before a snapshot is durable every older segment is still on disk, and a torn
snapshot is ignored like any torn record. After it, recovery starts the game from its latest
snapshot and ignores the records before it, wherever they are. With `JOURNAL_FSYNC=true` this also
holds after a power loss: a segment is deleted only once the record that releases it is fsynced;
at start the journal fsyncs the segments it read and their directory before it deletes anything;
and a segment holding a `committed` record is deleted only after a directory fsync has made the
deletion of that game's older segments durable. With `JOURNAL_FSYNC=false` (no power-loss guarantee
for the last records), a batch holding a snapshot is still fdatasynced before its bookkeeping
deletes anything, and at start the segments holding a snapshot are made durable first: otherwise a
power loss shortly after a compaction could keep the deletions and lose the snapshot. Metrics:
`scacelith_journal_snapshots_total`, and per shard `scacelith_journal_segments` and
`scacelith_journal_disk_bytes`.

**Write failures.** After a failed write the next batch goes to a new segment, and the batch's
`committed` records are appended again. Every other game of the batch lost records, so it goes to
a heal queue that `compaction_candidates` serves first (whatever its age, at most 8 snapshots per
batch): the host appends a snapshot of each such game it still hosts and has not committed. A crash
before that snapshot is written still loses those records. The host commits a finished game only
once the journal has written what it held without a new failure, with the bound of section 3, "End
of game".

**Recovery.** Opening reads the segments in order, stops reading a segment at its first invalid
record (impossible length, cut short, wrong CRC or kind) and goes on with the next segment.
`recover()` gives the games without a `committed` record, each with its records from its latest
snapshot.

### 5.7 Network edge (`net`)

**Topology** (`net::server`). With `TLS_MODE=native` every new TCP connection goes through the TLS
gate, then the rustls handshake, then HTTP/1.1 on hyper. With `WS_PORT == API_PORT` one port
carries the API and the upgrade on `/ws`; with distinct ports the WebSocket port takes only
upgrades. `TLS_MODE=proxy` and `off` serve plain TCP without a gate; in proxy mode the client
address comes from `X-Forwarded-For` when the peer is in `TRUSTED_PROXIES`. `TLS_MODE=off` is
refused unless `ALLOW_INSECURE_DEV`. The metrics endpoint (`METRICS_BIND:METRICS_PORT`, plain HTTP,
0 disables it) serves `/metrics` (with `METRICS_TOKEN`, a bearer compared as SHA-256 digests in
constant time), `/healthz` (the process runs) and `/readyz` (ready and not shutting down).

**Listening sockets.** `SO_REUSEADDR`, backlog `LISTEN_BACKLOG` (the kernel caps it at
`net.core.somaxconn`), dual stack when bound to `::`; accepted sockets get `TCP_NODELAY` and TCP
keep-alive.

**TLS** (`net::tls`). TLS 1.2 and 1.3 (`TLS_MIN_VERSION`), ALPN `http/1.1` only, session
resumption with rustls's rotating ticket keys (random, never derived from `SERVER_SECRET`), and
`Strict-Transport-Security: max-age=31536000` on the HTTP answers. The certificate is reloaded
without a restart on SIGHUP and when a 10 s poll of both files sees a change (reloaded 1 s after
the last change); a failed reload keeps the current certificate, and a reload only affects new
handshakes.

**TLS gate** (`net::gate`, native TLS only). Each accepted socket passes, in order:

1. Stage 0, the protection per address (`IpGuard::connection`, section 8; skipped for
   `ABUSE_EXEMPT`): a blocked address, more than `IP_CONN_RATE` new connections per second, or
   more than `IP_MAX_CONNECTIONS` open ones.
2. The waiting room: the socket waits, without a handshake slot, for the first record of its
   ClientHello, 3 s at most. At most `16 x MAX_PENDING_HANDSHAKES` sockets wait in all and
   `4 x MAX_PENDING_HANDSHAKES_PER_IP` per address group (an IPv4 address or an IPv6 /48).
3. The record must be a TLS handshake record (type 22, version 3.x, 1 to 2^14 bytes) starting a
   ClientHello; only the first record is awaited, so a fragmented ClientHello is fine.
4. A handshake slot: `MAX_PENDING_HANDSHAKES` in all (default 128 x `WORKERS`),
   `MAX_PENDING_HANDSHAKES_PER_IP` per group (default `MAX_PENDING_HANDSHAKES / 32`, at least 2 and
   below `MAX_PENDING_HANDSHAKES`; `check-config` prints the value in use).
5. The handshake, 10 s at most, replays the bytes the gate read; the slot comes back when it
   completes or fails, and a failed or timed-out handshake closes the socket.

A refusal closes the socket with an RST (zero linger) and counts in
`scacelith_tls_refused_total{reason}`. The refusals that say something about one address
(`per_ip`, `waiting_per_ip`, `bad_hello`, `hello_timeout`, a failed handshake) count 1 toward a
block of it; the server-wide ones (`handshakes`, `waiting`, `server_full`) do not. Gauges:
`scacelith_tls_connections_open`, `scacelith_tls_hello_waiting`,
`scacelith_tls_handshakes_pending`. Bounding the handshakes in flight lets the CPU finish them in
turn instead of starting thousands together; the refused clients retry with their backoff. Players
behind one IPv4 address or IPv6 /48 share their group's slots; a handshake lasts about one round
trip, so the default per-group cap still serves many of them.

* **Load shedding while full.** On the listener that carries the upgrades, the gate lets only
  `max(1, ceil(MAX_PENDING_HANDSHAKES / 2))` new connections per second through (token bucket, one
  second of burst; reason `server_full`) while the realtime layer's server-full signal is on
  (5.8). What passes reaches the API or the upgrade, where the exact checks run and refresh the
  signal. The HTTP 503 of the upgrade or `ServerFull` at Hello is the only way a client learns the
  server is full: `/api/v1/info` has no such field, and a client closed before TLS sees a network
  error and retries with its short backoff.
* **Compromise on a shared port.** Before TLS the gate cannot tell an API request from an upgrade,
  so while it sheds, new API connections pass at the same rate as upgrades: the API keeps working
  for players already connected, more slowly. With `WS_PORT != API_PORT` the API port is never
  shed, the better layout for a server that expects to be full.
* **What the gate does not stop.** Its caps keep a few hosts from blocking everyone; an attacker
  with many address groups can still fill them. With the defaults, replaying a captured ClientHello
  and staying silent holds a slot for the 10 s of the handshake timeout at the cost of one
  ServerHello: 32 address groups opening about 12.8 x `WORKERS` such connections per second keep
  every slot full, and every new TLS connection is refused until the attack stops (the API
  included on a shared port). The waiting room needs more (128 groups and about 680 x `WORKERS`
  silent connections per second). Players already connected are not affected. Stage 0 does not
  change that: each group needs only about 0.4 x `WORKERS` timed-out handshakes per second (24 x
  `WORKERS` a minute), below `ABUSE_BLOCK_REFUSALS_PER_MIN` (600) for any `WORKERS` up to 24; it
  stops the same attack from a few addresses. Stopping it belongs in front of the server (a
  per-source connection rate in the firewall, or a filtering provider: SIZING.md).

**HTTP/1.1** (`net::http1`, on hyper). A request head must arrive within 10 s (from the
connection's start, or from the first byte of a request on a kept-alive connection), else a raw
`408`. An idle kept-alive connection closes after 6 s (answers advertise `Keep-Alive: timeout=5,
max=1000`; the 1000th request closes the connection). While an answer is being sent, 30 s without
a byte in or out, or 60 s since the handler produced it, destroy the connection; a request whose
handler is still working has no such timer (the handler's own timeout answers it). Unparsable
requests, heads over 8192 bytes or 100 header lines get raw `400`/`431` answers, count in
`scacelith_http_client_errors_total{reason}` and count 1 toward a block (not from a trusted proxy).
A connection closed after an answer keeps reading for 2 s (1 s after a raw answer) so the client
reads the answer. Then, per request: `CONNECT` closes the connection, an upgrade goes to the
WebSocket endpoint (5.8), a request without `Host` gets 400 and an unknown `Expect` 417, the
protection per address takes a request token and an in-flight slot (section 8), `GET`/`HEAD` of
the health paths are answered at once, and everything else goes to the API (5.9) in its own task,
never cancelled by a disconnect. Pipelined requests are answered in order.

**Limits shared by the whole process** (`net::limits`, `security::ratelimit`): LRU-bounded token
buckets, sliding windows and single-use keys. The former server's primary process answered
`ratelimit.take`, `ratelimit.refund` and `once.consume` over IPC; here they are in-process calls
(`SharedLimits`, `LocalControl`), exact for the whole server.

### 5.8 WebSocket and realtime connections (`net::upgrade`, `net::ws`, `realtime`)

**Upgrade checks**, in order (`WsEndpoint::check`): the protection per address (429
`rate_limited` with `Retry-After` and the JSON body of section 8); 503 `shutting_down` while the
server drains; `GET` (405), HTTP/1.1 (400), path `/ws` (404), `Host` and the `Upgrade`/`Connection`
tokens (400 `bad_upgrade`), `Sec-WebSocket-Version: 13` (426), a valid key (400 `bad_key`), no
`Origin` or `Sec-WebSocket-Origin` unless listed in `WS_ALLOWED_ORIGINS` (403
`origin_forbidden`), the subprotocol `scacelith.rt1` (426 `unsupported_protocol`); then the
admission: `MAX_CONNECTIONS_PER_IP` per IPv4 address or IPv6 /64 (429 `too_many_connections`) and
the server's connections against `MAX_CONNECTIONS` plus a reserve of max(16, 2 %) (503
`server_full`), since the user is not known yet. Refusals count in
`scacelith_ws_handshakes_rejected_total{reason}`. On a dedicated WebSocket port the server reads
the head itself (8 KiB, 64 header lines).

**Codec** (`net::ws`, RFC 6455 server side): masked binary messages only (a text message closes
1003), at most 512 bytes per message checked from the frame header before buffering, at most 64
fragments, no extension (an RSV bit closes 1002), pings answered at 2 per second (burst 5), 2 s to
complete a close.

**Connection task** (`realtime::conn`): no message within `WS_HELLO_TIMEOUT_MS`, or a first
message that is not a `Hello`, is `HelloRequired` (4010); the `Hello` must carry `proto == 1`
(`UnsupportedProtocol`, 4002), decode strictly (`Malformed`, 4001) and carry seq 1
(`ProtocolViolation`, 4300). Then come the token (an invalid one: `Unauthorized`, 4003), e-mail
verification (`EmailUnverified`, 4011), the stored ban and the lobby's claim (section 3), and
`Welcome` with `minor` = min(client, 0), `caps` = client & `CAPS`, the server time, the player,
`heartbeatMs`, `clientPingMs`, `maxMsgPerSec`, `msgBurst`, `activeGame`, `gestureRate` and
`gestureBurst`. Up to 8 messages that arrive during the authentication are kept; a ninth is a
flood. After `Welcome`:

* `WS_MSG_RATE` per second with a burst of `WS_MSG_BURST`: a message beyond it is dropped (its seq
  still counted), with `Error{RateLimited}` at most once per second; more than max(10,
  `WS_MSG_BURST`) drops in 10 s is a flood (anomaly `flood`, close 4301).
* A server message type (0x80 and above) is the certain anomaly `forged_type`: with
  `AUTO_SANCTION_CERTAIN_CHEATS` the player is sanctioned (6.5), forfeits on every shard and gets
  `CheatDetected` (4302), otherwise `ProtocolViolation` (4300). An undecodable message is the
  anomaly `malformed` and closes 4001. A second `Hello` gets a non-fatal `ProtocolViolation`.
* A seq that is not the last plus one is dropped and recorded once as `bad_seq`; the next message
  resynchronises.
* Gestures: section 3. A client `Ping` gets a `Pong` at most once per 950 ms.
* Heartbeat: a server `Ping` every `HEARTBEAT_INTERVAL_MS` (the first after half an interval). The
  round trip feeds an exponential average (weight 0.2 for each new sample, capped at 2 s) passed to
  the rooms for lag compensation; a sample whose `Ping` preceded a stall of a host (6.1) is left
  out. A connection silent for `HEARTBEAT_TIMEOUT_MS` closes (1001).
* Lobby requests (queue, challenges) read the store first, in order; a connection may have 8 in
  flight, and more are answered `RateLimited`. Game messages go to the host named by the game id,
  with the time the frame was read.

**Outbound queue.** Frames are queued with a byte count. A connection whose unsent bytes exceed
`WS_SEND_BUFFER_LIMIT` is a slow consumer: closed 4303 without an `Error` frame (the queue is
full); the game goes on and the player may reconnect. `send_droppable` (gestures) skips a frame
while a quarter of the limit is in use.

**Server-full signal** (`realtime::admission`). The TLS gate sheds (5.7) while the last refusal of
the global check at the upgrade is newer than the last admitted upgrade and less than 5 s old, or
while the server holds 1.2 times `MAX_CONNECTIONS` connections. Players come first at
`MAX_CONNECTIONS`: a `ServerFull` at Hello does not start the signal, and an upgrade admitted inside
the reserve ends it. So a server at exactly `MAX_CONNECTIONS` does not shed: each newcomer completes
the handshake and the upgrade, gets `ServerFull` at Hello (close 4006) and the game client waits 60
to 120 s, while a player coming back to a game in progress (admitted at Hello whatever the count)
does not compete with newcomers for the connections let through. The price is one handshake, one
upgrade and one Hello per attempt of a newcomer until the reserve is in use. The metric of a full
server is therefore `scacelith_ws_hello_total{result="server_full"}`; the 503 of the upgrade comes
only once the reserve is in use, and the shedding (`scacelith_tls_refused_total{reason="server_full"}`)
only after such a 503 or at 1.2 times `MAX_CONNECTIONS`. The tests of `realtime::admission` and
`realtime::lobby` check this.

**Drain.** At shutdown every connection gets `Notice{ServerShutdown, arg = grace}`; a new
connection or a Hello that ends during the grace is refused with `ShuttingDown`; after
`SHUTDOWN_GRACE_MS` every connection gets `Error{ShuttingDown}` and closes 4008.

### 5.9 HTTP API and GIFs (`http`, `gifsvc`)

[API.md](API.md) is the complete reference of every endpoint (requests, answers, errors, rate
limits, examples). The pipeline (`http::api`), after the listener's admission (5.7): request
target checks (at most 4096 bytes), routing, authentication, the account budget, the route's rates,
query and body validation, the handler under its timeout, and the answer with its headers.

* **Router** (`http::router`): routes registered in order; paths relative to `/api/v1` unless they
  start with `/api/`; page routes (`Router::page`) live outside `/api`. `RouteOpts` gives the
  authentication mode (`None`, `Optional`, `Required`: 401 `unauthorized`), the JSON body schema,
  the rates, a body limit and a handler timeout. A handler gets a `Ctx` (client address, headers,
  path parameters, validated query and body, the session when one came, the request time) and
  returns an `Answer` (JSON, HTML page, text or binary file) or an `ApiError`; either can carry
  `refund_rate`. Route modules hold the services they use.
* **Rates** (`http::rates`): a `RateSpec` takes from the bucket of the account (`by_user`, with a
  session) or of the client address (an IPv4 address or an IPv6 /64); `prefix_limit` also counts
  each IPv6 /48 as a whole; `shared` rates also count in the process-wide sliding windows used by
  the authentication limits. A route's rates are taken in order, all or none; an answer with
  `refund_rate` gives them back. A refusal keyed by an address counts toward a block of it with
  the rate's `abuse_weight` (or 1); one keyed by an account never does. `Ctx::take_rates` takes more
  rates inside a handler (the GIF render quotas, on a cache miss only). Before the route's rates,
  every request with a session spends a token of the account's budget (`USER_RATE_PER_MIN`, burst
  half a minute, whole server).
* **Timeouts and bodies.** The handler timeout is 30 s (the export 60 s; a GIF
  `GIF_QUEUE_TIMEOUT_MS + GIF_RENDER_TIMEOUT_MS` + 5 s). A body must arrive within 10 s (408) and
  within its limit (413); either closes the connection.
* **Headers.** Every answer carries `Cache-Control: no-store`, `X-Content-Type-Options: nosniff`,
  `Referrer-Policy: no-referrer`, a `Content-Security-Policy` (strict for HTML pages), and
  `Strict-Transport-Security` with native TLS. JSON errors are `{ "error": "<snake_case_code>",
  "message": "...", "retryAfter"?: s }`, with `Retry-After` when `retryAfter` is set.
* **Route groups** (`http::routes`, registered in this order): `info`, `auth`, `account`, `sso`,
  `players`, `games`, `leaderboard`, `reports`, `account_games`, `account_export`, `gif`, then the
  HTML pages `/verify-email`, `/reset-password` and `/confirm-email-change` (a GET shows a
  confirmation button; only the POST changes state, so link scanners consume no token). Google
  sign-in has no page: Google sends the browser back to the game's 127.0.0.1 listener.

**GIF service** (`gifsvc`, renderer in `crates/gif`). `GET /games/:id/gif` and `POST /gif` render
on `GIF_THREADS` dedicated threads at nice 19, started on the first render and stopped after a
minute without work, behind a FIFO of `GIF_QUEUE_MAX` jobs waiting `GIF_QUEUE_TIMEOUT_MS` at most
(503 `server_busy` beyond, the render quotas given back); a render runs `GIF_RENDER_TIMEOUT_MS` at
most. An LRU cache (`GIF_CACHE_MB`) keyed by a hash of everything the picture shows (never the PGN
text) serves repeated requests, and a request for a GIF being rendered joins that render: neither
costs a render quota. Metrics: `scacelith_gif_renders_total{result}`,
`scacelith_gif_render_duration_ms`, `scacelith_gif_cache_total{result}`, `scacelith_gif_queue`,
`scacelith_gif_renders_running`, `scacelith_gif_cache_bytes`.

## 6. Game policies

### 6.1 Clocks (server authoritative)

* Time is measured on the server only. The client's `thinkMs` never adds time; it only bounds lag
  compensation.
* Plies 0 and 1 (each side's first move) do not run the clock: each player has
  `FIRST_MOVE_TIMEOUT_MS` to make it, otherwise the game is aborted (`NoShow`, unrated). Its
  deadline has the same margin as a flag, `min(quota, rttEma + 50, LAG_COMP_MAX_MS)`, so that a
  first move sent in time over a slow link counts; the `firstMoveMs` sent to the clients has no
  margin. No increment is added for them. The clocks start with White's second move.
* For every later move: `elapsed = recvTime - turnStart`, where `turnStart` is when the server sent
  the opponent's `MoveMade`; `lag = elapsed - clamp(thinkMs, 0, elapsed)`;
  `comp = min(lag, rttEma + 50, LAG_COMP_MAX_MS, quota)`; `quota -= comp`, then
  `quota = min(quota + LAG_QUOTA_GAIN_MS, LAG_QUOTA_MAX_MS)`; `charged = elapsed - comp`. If
  `remaining - charged <= 0` the player flagged: the move is refused (`MoveRejected{FlagFell}`) and
  the game ends on time (a draw if the opponent cannot mate). Otherwise `remaining -= charged`, then
  `+= incMs`.
* The flag timer fires at `turnStart + remaining + min(quota, rttEma + 50, LAG_COMP_MAX_MS)`, the
  latest moment a move could still arrive in time.
* `thinkMs` longer than the real time since the previous move plus 100 ms is physically impossible
  for an honest client: anomaly `clock_implausible` (suspicious, never certain: clock drift and
  suspended virtual machines exist). Since the time since the previous move is used, the later
  `turnStart` of a game restored after a restart (6.4) raises no false report.
* `rttEma` is the room's exponential average (weight 0.25 per value, capped at 2000 ms, 100 ms
  before the first value) of the round trip the player's connection measures with the server's
  `Ping` and the client's `Pong` (5.8). A client that delays its `Pong`s only inflates a value
  that is itself capped by the quota. `Welcome`, `Ping` and `Pong` carry the monotonic clock of the
  games.
* **Stall credit** (`GAME_STALL_MIN_MS`, 30; `GAME_STALL_CREDIT_MAX_MS`, 5000). A host actor can be
  held up for a moment (CPU steal, a long burst of work on its thread). Without care a flag that
  fell meanwhile would fire before a move that arrived in time and waited. Every request carries the
  time its connection read it, and counts as arrived then. Each 10 ms beat of the host measures its
  own delay: a beat more than `10 + GAME_STALL_MIN_MS` ms after the previous one is a stall
  (`scacelith_game_stall_ms`). The host then yields, handles the messages that reached its inbox
  meanwhile, which count as arrived when the stall began (at most `GAME_STALL_CREDIT_MAX_MS`
  earlier; `scacelith_game_stall_credit_ms_total`), and only then fires the deadlines due by that
  beat. Game requests, attaches, detaches, rematch closings and the forfeit of a sanctioned player
  get the credit; the forfeit of a certain cheat takes the arrival of the request that revealed it.
  The room checks the deadlines, the flag and the time charged for a move at the credited arrival
  (never before the latest move, never after now); a resignation, draw, abort or forfeit takes
  effect at it; a disconnection, reconnection or resync processes only the deadlines due at it. The
  next turn starts at the real time of the `MoveMade`, so the stall is charged to nobody and uses no
  quota. A first-move timeout that fell during a stall aborts the game without a `noshow` conduct
  incident. Nothing of it is journaled (the journal holds the clock values it produced), and no
  client can cause a stall. `scacelith_game_timer_late_ms` measures how late the timers fire.

### 6.2 Validation of a Move intent (in this order)

1. Not a participant of `game`: `NotInGame` (certain: `foreign_game`).
2. Game over: `GameOver` (info: `game_over`), or `FlagFell` when the sender's own flag fell at
   that moment.
3. `ply < current`: a duplicate of a move already played. Identical: the original `MoveMade` is
   sent again (idempotent); otherwise `StalePly` (info).
4. `posHash != digest(current position)`: `Desync` and a `GameSnapshot` (info; 3 or more in one
   game: suspicious `repeated_desync`).
5. `ply > current` with a matching hash cannot happen; treated as `Desync`.
6. Not the sender's turn (the hash matches, so the client knew): `NotYourTurn` (certain:
   `out_of_turn`).
7. Illegal move in the matching position: `IllegalMove` (certain: `illegal_move`).
8. Clock check (6.1), then the move is played. A pending draw offer of the opponent is declined by
   the move.

Every rejected move gets `MoveRejected` followed by a `GameSnapshot`, and is never sent to the
opponent. The game client checks moves with the same rules before sending, so steps 6 and 7 only
happen with a modified client.

### 6.3 Draws, resignation, abort, rematch

* One pending draw offer per game; it stands until the opponent answers or moves (a decline). At
  most `DRAW_OFFERS_PER_GAME` per player, and not again until 10 plies after a decline
  (`DrawOfferLimit`). An offer can come with a move (`Move.drawOffer`, FIDE 9.1.2) or alone; an
  offer made while the opponent's offer is pending is an agreement.
* `DrawClaim`: accepted when the current position occurred 3 times or the halfmove clock is at
  least 100; otherwise `NothingToClaim`. Automatic endings: mate, stalemate, insufficient material,
  fivefold repetition, 75 moves (as in the game). A game reaching 1200 plies ends `ServerAborted`.
* `Resign` at any time while the game runs. `Abort` only before the sender's own first move
  (conduct incident `abort`).
* `Rematch` within 60 s after the end: when both accept, the host asks the lobby
  (`HostEvents::rematch`), which creates a new game with the colours swapped, the same time control,
  rated flag and `autoPress`, after checking bans, presence, another game and, for a rated game,
  the conduct cooldowns and `MATCH_REPEAT_LIMIT` (`RematchUnavailable` otherwise). The window
  closes when either player leaves (disconnects or joins a queue).

### 6.4 Disconnections, abandonment, server restart

* Grace = clamp(baseMs / 10, `RECONNECT_GRACE_MIN_MS`, `RECONNECT_GRACE_MAX_MS`). The disconnected
  player's clock keeps running when it is their turn (they can lose on time before the grace ends).
* Grace expired: fewer than 2 plies, the game is aborted (`NoShow`); otherwise the absent player
  loses by `Abandonment` (`AbandonmentVsInsufficient`, a draw, when the opponent cannot mate).
  Conduct incident `abandon`.
* Both players disconnected: if they left within 5 s of each other (a network or server event), the
  game waits for the longer grace, then is aborted (`BothDisconnected`, unrated); otherwise the first
  to leave is the one who abandoned.
* Leaving a running game from the menu is a resignation (as in the offline game).
* Conduct: `CONDUCT_ABANDON_LIMIT` abandons, aborts and no-shows within 24 h pause rated matchmaking
  for 15 min, then 1 h, then 6 h; the level decays by one for every 24 h without an incident.
  Casual games and direct challenges stay possible; the lobby checks the cooldown before a rated
  queue join and a rated rematch. The player gets `Notice{MatchmakingCooldown}`.
* **Server restart.** Games are replayed from the journal (5.3) with both players away; the downtime
  is charged to nobody. Both players get the recovery grace, `RECOVERY_GRACE_MS` (90 s, or the
  normal grace when that is longer), instead of the normal grace: the server broke the connections,
  and every client reconnects at about the same time (after a `ServerShutdown` notice the clients
  spread their first attempt over several seconds). The grace starts at the replay, before the
  listeners are bound, and is written in the `recovered` journal record; the disconnections
  journaled by the drain before a graceful stop do not shorten it. A player who comes back and then
  loses the connection again gets the normal grace; the deadlines above use each player's own
  grace. The clock of the side to move (or its first-move timer before the second ply) keeps its
  journaled value and stays stopped until that player is back: it starts at the reconnection, or
  `RECOVERY_CLOCK_HOLD_MS` (20 s, lower than the recovery grace) after the replay when the player is
  still away, so that staying away on purpose gives little free thinking time. Until then the
  snapshots show no running clock; when the clock starts, the opponent gets a new `GameSnapshot`.
  The hold and its end are journaled (the `recovered` record carries the hold, a checkpoint marks
  its end), so a later replay rebuilds the same clocks. Once the clock runs, a side to move with
  less time left than its reconnection delay can still lose on time. Before the second ply, the
  first reconnection of each player after the restart restarts that player's first-move timer when
  it is its turn, even after the hold ended: a player who comes back late but before its first-move
  time ran out gets the whole `FIRST_MOVE_TIMEOUT_MS` from the reconnection. Only the first
  reconnection after a restart counts. A restored game aborted `NoShow` because its side to move
  never came back records no `noshow` incident (the server broke the connection); a player who came
  back and then does not move within the first-move time gets the incident as usual. Games that
  cannot be rebuilt end `ServerAborted` (unrated) and are committed as such.

### 6.5 Anomalies, certain cheats, suspicion

| kind | severity | when |
|---|---|---|
| `malformed` | suspicious | undecodable message after Hello (connection closed 4001) |
| `forged_type` | certain | a server message type sent by a client |
| `bad_seq` | suspicious | seq not last + 1 |
| `flood` | suspicious | rate limit exceeded repeatedly (connection closed 4301) |
| `foreign_game` | certain | game message for a game the player does not play |
| `out_of_turn` | certain | move in a synchronised position while it is not the player's turn |
| `illegal_move` | certain | illegal move in a synchronised position |
| `repeated_desync` | suspicious | 3 or more desyncs in one game |
| `clock_implausible` | suspicious | `thinkMs` impossible (6.1) |
| `stale_ply`, `desync`, `nothing_to_claim` | info | honest races and bugs |

Any other kind (`game_over`, a move for a finished game) is recorded as info. Recording never
waits: the first anomaly of a kind by a player in a game goes to the store writer at once, its
repeats within 1 s are coalesced into that row (`count`, `lastAt`), and a certain anomaly gets a
job of its own (`anticheat::service`).

Certain cheat with `AUTO_SANCTION_CERTAIN_CHEATS=true`: the game in progress ends `Forfeit` (a rated
game is rated normally: the opponent wins), the player gets `Error{CheatDetected}` (fatal, close
4302), and one store write job bans the player for `BAN_DURATION_HOURS` (source `auto`, reason
`certain_cheat:<kind>`), sets the integrity level `confirmed` with the evidence and refunds the
player's victims (6.6). Without it, `forged_type` closes the connection with `ProtocolViolation`
(4300) and the other kinds are only recorded.

Statistical assistance detection never bans automatically. The analysis pool replays finished rated
games with an engine and accumulates, per player and category: accuracy, average centipawn loss,
agreement with a deep and a shallow engine choice, agreement in complex positions, think time
against complexity and its variance, and performance against the player's own history, as z-scores
against the server's population at the same rating, shrunk for small samples. Levels: `suspected`
(a strong signal over at least 5 games), `high_confidence` (several independent signals over at
least 10 games and 300 non-trivial moves), `confirmed` (a moderator decision through
`scacelith-server admin integrity confirm`, or a certain protocol cheat). Reports raise the review
priority, weighted by the reporter's credibility, never the level itself. ANTICHEAT.md has the
model.

**Analysis backlog.** One engine analyses far fewer games than a busy server finishes (ANTICHEAT.md,
engine analysis), so the queue is bounded and prioritized. Every job has a priority, and the engine
takes the highest first, then the oldest:

1. `manual`: a moderator asked for the (re-)analysis of a game (`scacelith-server admin analysis
   queue <gameId>`, which leaves a game being or already analysed alone);
2. `report`: a credible player reported the game (category `cheating` or `other`, stored weight at
   least 0.5); the report queues it even if the policy had left it out, and queues it again if its
   analysis had failed. A report of lower weight asks for `signal` priority only, so that such
   reports cannot push back the games of statistically suspected players;
3. `signal`: at the end of the game the integrity level of either player is above `none`, either
   player has an open `cheating` or `other` report from the last 30 days by a credible reporter, or a
   `suspicious` or `certain` anomaly was recorded in this game;
4. `ordinary`: every other rated game of an official category with at least `ANALYSIS_MIN_PLIES`
   plies.

Games of the first three kinds are queued whatever the backlog, with one bound: at most 20 `signal`
jobs of one player wait at a time (the scoring reads a player's 30 latest analysed games). When one
of its players already has 20 waiting, a game flagged only through its players is not queued, and a
low-credibility report does not raise a game past that bound. A game with a `suspicious` or
`certain` anomaly of its own is still queued: in the same transaction it takes the place of that
player's oldest waiting job whose game has no such anomaly, so the count stays at 20; it is skipped
only when every waiting job of the player has an anomaly of its own. An ordinary game is queued
with probability `ANALYSIS_SAMPLE_RATE` (default 1), and only while fewer than `ANALYSIS_QUEUE_MAX`
(default 5000, at most 100,000) ordinary jobs wait. A game left out gets no job and counts in
`scacelith_anticheat_analysis_skipped_total{reason="sample"|"backlog"|"player"}`, and a job displaced
by a game with evidence counts there with `reason="displaced"`.

Every fourth claim of the analysis pool takes the oldest ordinary job first when one waits. Ordinary
games, the random sample that feeds the population statistics the players are compared with, thus
keep at least a quarter of the engine time however many prioritized games arrive, and a suspicious
game waits behind at most one ordinary game in four. `scacelith_anticheat_analysis_queue_ordinary`
and `scacelith_anticheat_analysis_queue_priority` show the backlog (each counted up to 100,000). The
policy only decides what is analysed and when: it changes no level and no sanction.

### 6.6 Ratings

One Elo rating per player and official category (`3+2`, ...; a custom time control is never rated),
computed as FIDE computes ratings (FIDE Rating Regulations effective from 1 March 2024) by
`matching::elo`, which mirrors the game's offline rating (`src/game/elo.cpp`) to the last point:
both are checked against `test/fixtures/elo-vectors.json`.

* **Expected score**: FIDE's table 8.1.2 as published (`PD` for the rating difference `D`; the
  higher-rated player gets `PD`, the lower `1 - PD`), `D` counted as 400 at most (8.3.1). The table
  is the normal distribution with a standard deviation of 2000/7 rounded to hundredths, except at
  six differences where FIDE's rows differ (54, 343, 344, 358, 392, 620); FIDE applies the table, so
  it is used as published.
* **Change**: `K x (score - PD)`, rounded to the nearest point (halves away from zero), computed in
  whole hundredths. `K` is 40 until the player has `PROVISIONAL_GAMES` (30) counted games in the
  category, those of the unrated phase included, then 20, and 10 for good once the player has
  reached 2400.
* **Unrated phase and first rating** (8.2): a new record is unrated, with a working rating of
  `INITIAL_RATING` (1500) that is shown and used for pairing. Its counted games add up the opponents'
  ratings and the score; after five, `Ru = Ra + dp(p)` with `Ra = (sum of the opponents' ratings +
  2 x 1800) / (n + 2)` and `p = (score + 1) / (n + 2)` rounded to hundredths (two hypothetical
  draws against 1800-rated players), `dp` from FIDE's table 8.1.1, rounded to the nearest point and
  capped at 2200. The peak becomes `Ru`. Five draws against 1500 give 1586, five wins 1895.
* **Zero score** (8.2.1, one game at a time): a game lost by an unrated player who has not scored
  yet in the category (no win, no draw) counts for neither player's rating (only in the games and
  the wins, draws and losses). A newcomer who only loses stays unrated, and an account that only
  loses cannot give a first rating to the accounts that beat it.
* **Unrated opponent**: a rated player's game against an unrated one leaves the rated player's
  rating unchanged (8.3) and is not a counted game.
* **Counted games**: the games that entered the rating. They set `K` and the provisional mark, and
  a place on the leaderboard needs `PROVISIONAL_GAMES` of them.
* **Floor**: a rating never drops below 100.

Departures from FIDE, needed by a game server: games are rated one by one against the ratings
before each game (FIDE rates monthly periods); a game between two unrated players counts for both,
at the other's working rating, unless it is a zero score; FIDE's `K = 40` for players under 18 does
not apply. On the wire an unrated record is `provisional` (so is a rated one with fewer than
`PROVISIONAL_GAMES` counted games), its `rating` is the working rating, and the `RatingChange` of a
game of the unrated phase has `before = after`, except the fifth counted game, which goes from the
working rating to the first rating.

**Refunds.** When a player is banned as a cheater (a certain cheat, 6.5, or a moderator's
`integrity confirm`), every opponent who lost rating points to them in a rated game that ended
within `RATING_REFUND_DAYS` (60) before the ban gets exactly those points back, added to their
current rating in that category (the peak rises with it). Nothing is recomputed: wins against the
cheater and every other game stand; a draw that cost points is refunded like a loss; only a change
of the K formula is refunded; one refund per game and victim at most. A game not recorded yet when
the ban is given (in progress, or waiting for its commit) is refunded in the transaction that
records it, while the ban lasts. A ban for something else (`user ban`) and `integrity confirm
--no-refund` refund nothing: the store tells them apart by the ban's source and reason
(`certain_cheat:<kind>` from the anti-cheat; `confirmed: <reason>` or `confirmed, no refund:
<reason>` from the confirm). A ban given with `scacelith-server admin`, which only writes the
database, reaches the running server when the player next connects (the connection is refused) or
tries to start a game (queue, challenge, private code, rematch): the lobby then enforces it like a
ban of the anti-cheat (kick, game forfeited, queue and challenges dropped), so a banned player
starts no game. The victim gets `Notice{RatingRestored, arg: points}` out of a game only: at once
when connected and idle, otherwise after their current game, otherwise right after `Welcome` at
their next connection (after the game that connection resumes, if any). Moderator options and the
audit trail: ANTICHEAT.md.

## 7. What lives where, and what survives a crash

| Data | Where | Written | After a crash |
|---|---|---|---|
| Accounts, e-mail, password hashes, MFA secrets (encrypted), recovery-code hashes, SSO links | SQLite | at once (one write job) | kept |
| Sessions | SQLite (+ a 30 s cache in memory) | login and logout at once; last use at most every 5 min | kept; revocations through the API apply at once, those of the admin command within 30 s |
| Ratings, finished games, analysis queue | SQLite | in batches, at most `DB_COMMIT_MS` after the end | kept once committed; games ended but not committed are in the journal and committed at recovery |
| Games in progress (moves, clocks, offers) | host memory + journal | journal batch at most `JOURNAL_FLUSH_MS` after each record | replayed; at most `JOURNAL_FLUSH_MS` of moves lost (clients resend: stale ply or resync); recovery grace and clock hold (6.4) |
| Sanctions, integrity levels, reports | SQLite | at once | kept |
| Anomalies | SQLite | the first of each kind at once, repeats coalesced over 1 s | the repeats of the last second may be lost |
| Rating refunds (6.6) | SQLite | with the ban that triggers them, or with a game recorded during the ban; marked notified once the notice is queued | kept; notices not marked are sent after a restart |
| Presence, queues, challenges, private codes, rate-limit counters, repeat-limit counts | lobby and process memory | - | lost: clients reconnect and queue again |
| Security events (failed logins...) | SQLite | batched (1 s) | the last second may be lost; purged after `RETENTION_SECURITY_DAYS` |

**Retention purge** (`store::retention`). Personal data is not kept longer than needed. The purge
runs about 60 s after the start, then every `RETENTION_INTERVAL_MS` (one hour by default, at most
2147483647 ms); runs never overlap.

| Data | Deleted or erased |
|---|---|
| Sessions | once expired (absolute or idle limit); revoked ones a day after the revocation; the IP address of a live session `RETENTION_IP_DAYS` (30) after the login, the row stays |
| Single-use tokens (some carry an e-mail address), pending signups | once expired |
| Security events | after `RETENTION_SECURITY_DAYS` (90); their IP address after `RETENTION_IP_DAYS`, the row stays |
| Anomalies | `info` and `suspicious` ones after `RETENTION_SECURITY_DAYS`; `certain` ones (the evidence of an automatic sanction) are kept |
| Conduct events | after 30 days |
| Analysis jobs | failed ones 30 days after the failure; done ones are kept (their features are each player's analysed history); waiting and running jobs are never purged |

Accounts (anonymized when deleted), games, ratings, sanctions, reports and integrity records are
kept. IP addresses are erased first, then the old rows deleted. The purge must not hold up the
other writes (sessions, security events, finished games): every chunk is one statement in its own
writer job, at most 1000 rows; it starts at 200 rows and adapts so that one statement takes about
half of a 10 ms slice (never fewer than 50 rows), and after a slice of work the run pauses for a
slice. At shutdown the run in progress stops between two statements. Each run logs its
counts (numbers only) and feeds `scacelith_retention_purged_total{kind}`,
`scacelith_retention_ip_erased_total`, `scacelith_retention_runs_total{result}` and
`scacelith_retention_run_seconds`.

**Erased data does not stay in the file.** The writer runs with `PRAGMA secure_delete = ON`: SQLite
overwrites deleted rows and erased columns with zeros, freed pages included, so an IP address, an
e-mail address or a password hash does not stay readable in the free space of the database file
after the purge or an account deletion (`users().anonymize`). Two limits remain. When SQLite
rebalances a table after a deletion it can leave stale copies of the rows it moved in the unused
part of a page, which `secure_delete` does not clear; the purge erases IP addresses before it
deletes rows so that the rows moved around them no longer hold one (a test erases or deletes 1,250
addresses and an anonymized e-mail address, and finds none of them in the file afterwards). And the
WAL file keeps the page images written before until it is overwritten as it is reused after each
checkpoint (it is truncated to 64 MiB and removed when the server stops). A backup made with
`scacelith-server admin backup` (`VACUUM INTO`) holds neither.

## 8. Security model

* **TLS everywhere**: the HTTPS API and WSS, with native TLS or behind a TLS-terminating proxy
  (`TRUSTED_PROXIES`). HSTS with native TLS. Plain text only with `ALLOW_INSECURE_DEV`.
* **Per-server trust boundary** (client side, section 10): credentials, tokens and pinned
  certificates are stored per server origin (`host:apiPort`) and only sent to that origin. The
  WebSocket goes to the same host. Redirects are not followed with credentials.
* **Keys** (`security::keys`): every purpose (proof-of-work challenges, recovery-code pepper, TOTP
  encryption, OAuth state...) has its own key derived from `SERVER_SECRET` with HKDF-SHA256;
  `MFA_ENCRYPTION_KEY`, when set, encrypts the TOTP secrets instead.
* **Passwords** (`security::password`): Argon2id (64 MiB, 3 passes, 4 lanes, 16-byte salt, 32-byte
  tag) in a PHC string; a hash with other parameters is upgraded at the next login. Passwords are
  NFC-normalised, between `PASSWORD_MIN_LENGTH` characters and 256 bytes, must not contain the
  username or the e-mail's local part, and must not be in the embedded list of common passwords. An
  unknown login costs the same work on a dummy hash. Every failed login check (known account or
  not) is padded to the slowest check of the last 10 to 20 minutes, and never less than a baseline
  measured at start by one verification (at most 2 s in all), with a timer that runs after the hash
  slot is released. The time of a failure does not tell which e-mail addresses are registered.
* **Password hash cap** (`HashLimiter`): every hash and verification of the server (registration,
  login and its rehash, password change and reset, the re-authentication of account changes, the
  dummy verification) goes through one bounded FIFO: `PASSWORD_HASH_CONCURRENCY` run at once on the
  blocking pool (default `WORKERS`), `PASSWORD_HASH_QUEUE_MAX` wait (default 32 x `WORKERS`). Each
  request has one wait budget of `PASSWORD_HASH_QUEUE_TIMEOUT_MS` (10 s, at most 13 s so that the
  answer comes before the game's 15 s HTTP timeout) for all its hashes together. A request beyond
  the queue, or whose budget ran out, gets 503 `server_busy` with a random `Retry-After` of 5 to
  15 s, before anything changed: no failed login is counted (except at the password step of a
  Google link, whose ticket try and failure are taken before the hash) and a reset link stays
  valid. Once the queue is at least half full, one client source (an IPv4 address or an IPv6 /48)
  may have at most `PASSWORD_HASH_WAITERS_PER_SOURCE` hashes waiting; its next request gets 429
  `rate_limited` the same way and its route rates back. So one network cannot hold more than half of
  the queue, and a classroom behind one address still uses an idle queue in full. The rehash at
  login only runs when a slot is free at once. Metrics: `scacelith_password_hash_in_flight`,
  `scacelith_password_hash_queued`, `scacelith_password_hash_wait_ms`,
  `scacelith_password_hash_rejected_total{reason}` (`queue_full`, `timeout`, `source_limit`).
* **Password changes under concurrency**: every new hash computed from a checked password (a
  rehash, a password change) is written with a compare-and-set on the hash that was checked, in one
  store job. After a check, the login and the re-authentication read the account again, and check
  the password once more when the stored hash changed meanwhile. The MFA step of a password login
  keeps a SHA-256 digest of the hash the password matched, and `POST /auth/login/mfa` fails with
  `invalid_mfa_token` when the stored hash is no longer that one. So a password reset always wins
  against a login, a rehash or a password change in flight, and no session is opened with a password
  the reset replaced.
* **Sessions and tokens**: session tokens and every single-use token are 32 random bytes, stored as
  SHA-256 only. Session lookups are cached 30 s (positive and negative); the API drops revoked
  sessions from the cache at once. Single-use tokens: e-mail verification (24 h), password reset
  (1 h; revokes every session; works only while the account still has the address it was mailed to;
  a reset or a password change ends the account's other reset links), e-mail change (24 h, sent to
  the new address; a new request replaces it, a password change or reset cancels it), MFA login step
  (5 minutes), SSO attempt, ticket and link ticket (10 minutes each; a link ticket allows 5 password
  tries). Confirming an e-mail change and a password reset are each one transaction; a store that
  stays locked answers 503 `server_busy` with `retryAfter: 1`, nothing changed and the link still
  valid.
* **TOTP**: RFC 6238 (HMAC-SHA1, 6 digits, 30 s, one step either way), 20-byte secrets sealed with
  AES-256-GCM (`v1.` format, bound to the account), replay refused (the last step used is stored).
  10 recovery codes (`xxxx-xxxx-xx`, 50 bits) stored as HMAC-SHA256 under a derived pepper, single
  use. A password reset never disables MFA; an administrator can (`scacelith-server admin user
  reset-mfa`) after verifying the owner by other means.
* **Enumeration**: register, forgot and resend answer the same whatever the e-mail; so does an
  e-mail change (202, the address shown as pending whether or not another account uses it; the owner
  of a taken address gets a notice, never a link); login errors are the same for an unknown account
  and a wrong password. With `REQUIRE_EMAIL_VERIFICATION`, registration creates no account before
  its link is used: the signup waits in `pending_signups` for the 24 h of the link and holds its
  username whether or not the address has an account (then without a link; the owner gets a
  notice), so that a second signup with the username (409 `username_taken`), a sign-in with it (401
  `invalid_credentials`), the public profile (404) and every other answer are the same in both cases,
  and the request does the same work in both (one password hash, one transaction). A resend gives the
  signup its 24 h again in both cases. Using the link creates the account, its address confirmed,
  and drops the signup in one transaction; an expired signup frees its username at once. Google
  sign-in names an existing account (`needsPassword`) only to whoever proved its address to Google,
  and links Google to it only after its password (and second factor), never by the address alone.
  Without `REQUIRE_EMAIL_VERIFICATION` there is no link: the account is created at once with its
  address counted as confirmed, and register and the e-mail change answer 409 `email_taken`. With
  `REQUIRE_EMAIL_VERIFICATION`, an account stored with an unconfirmed address gets 403
  `email_unverified` at sign-in, only after its password matched.
* **Account data export** (`POST /account/export`, password and second factor, 5 per hour): the
  player's own data only; never a password hash, TOTP secret, recovery code, token or token hash, the
  anti-cheat's data, the reports made against the player or a moderator's identity, nor anything
  that tells which opponent was sanctioned (refunds are summed per UTC day and category) or another
  person's IP address (an event keeps its IP only for the kinds of `IP_KINDS`;
  `http::routes::account_export`).
* **Brute force and credential stuffing**: per-address token buckets (the request budget below and
  the auth family's route limits; IPv6 per /64, and per /48 as a whole with `AUTH_RATE_PER_PREFIX`),
  whose refusals count 5 toward a block of the address; per-account limits whatever the address
  (`AUTH_MFA_PER_ACCOUNT` second-factor codes, `AUTH_REAUTH_PER_USER` re-authentications); a failure
  counter per login with an exponential delay (2 s up to 15 minutes, 429 `too_many_attempts`) after
  `AUTH_FAILURES_PER_ACCOUNT` failures; a server-wide failure rate above `POW_LOGIN_TRIGGER_PER_MIN`
  turns on the login proof of work for at least 5 minutes; proof of work on registration
  (`POW_REGISTER_BITS`).
* **WebSocket surface**: one message table, strict decoding, the size limit checked from the frame
  header, no compression, Hello timeout, per-connection token buckets, per-address and global
  connection limits, slow consumers closed, heartbeat timeout, `Origin` refused unless listed (5.8).
  Before TLS: blocked addresses, the per-address connection rate and open connections, bounded
  handshakes, and load shedding while the server is full (5.7).
* **Protection per address** (`net::guard`, `net::abuse`). Every request and every connection meets
  it first, before routing, before authentication and, with native TLS, before any TLS work. It only
  keeps one network address from saturating the server; quotas otherwise belong to the signed-in
  account. An address is an IPv4 address or an IPv6 /64; an IPv6 address also counts toward its /48
  with 4 times each limit, so rotating over the /64s of a /48 multiplies nothing. One process
  enforces the whole-server limits exactly.
  * Requests: every HTTP request whatever its path, method or outcome (API, pages, health checks,
    unknown paths, WebSocket upgrades) takes one token of `HTTP_RATE_PER_IP` (600 per minute, burst
    half a minute) and, for IPv6, of `HTTP_RATE_PER_PREFIX` (a refusal there gives the /64 token
    back), then one of the address's `IP_MAX_INFLIGHT` places of requests in progress (default 32 x
    `WORKERS`), given back when the answer is sent or the connection closes. Beyond: 429 `{ "error":
    "rate_limited", "message": "Too many requests; try again later.", "retryAfter": s }` with
    `Retry-After`, the same on an upgrade.
  * Connections (TLS gate stage 0, 5.7): `IP_CONN_RATE` new connections per second (burst 4 s) and
    `IP_MAX_CONNECTIONS` open ones (handshakes, keep-alive connections and WebSockets together);
    beyond, an RST before any TLS byte. `MAX_CONNECTIONS_PER_IP` still bounds the WebSockets of an
    address (per /64 only).
  * Slow clients: the HTTP/1.1 timers of 5.7.
  * Blocks. The guard sums, per address, the refusals that show a client ignoring the limits:
    weight 1 for a 429 of the address budgets or the in-flight cap, a gate refusal of that address,
    a failed TLS handshake, malformed HTTP (not from a trusted proxy); `AUTH_REFUSAL_WEIGHT` (5) for
    the refusals of the auth family's route limits, whose attempts each cost a password hash. Not
    counted: per-account limits, server-wide capacity refusals (the hash queue, the gate's
    `handshakes`, `waiting` and `server_full`), the refusals of an address already blocked. Once a
    second the guard hands the sums to the abuse tracker (at most 512 keys, the largest first),
    which adds them up over a sliding minute and blocks an address at `ABUSE_BLOCK_REFUSALS_PER_MIN`
    (600; 4 times that for a /48, which is also blocked once 4 of its /64s are), for
    `ABUSE_BLOCK_BASE_SEC` (60 s) times 4 at each new block within 6 hours, up to
    `ABUSE_BLOCK_MAX_SEC` (1 h): 1, 4, 16, then 60 minutes. An address that reaches the threshold
    within one second is blocked at once (fast path). A blocked address gets an RST before TLS for
    its new connections and 429 with the time left for its requests and upgrades (with `Connection:
    close`, except behind a proxy, whose connection it is). WebSocket connections already open are
    never closed by a block, so a player who shares the address with an abuser keeps the game in
    progress; a player whose connection drops comes back when the block ends, which is why the first
    one is short. At most 20,000 blocks run at once (the oldest end first); each one is logged.
    `ABUSE_BLOCK_REFUSALS_PER_MIN=0` turns blocking off.
  * `ABUSE_EXEMPT` (addresses and CIDR subnets: a school or club network, monitoring, a load
    generator) skips all of the above. Login, registration, the other route limits and the
    per-account quotas still apply.
  * Memory is bounded: 50,000 buckets per limiter (an evicted bucket comes back full, which only
    makes a limit more lenient), counters for open connections and requests in progress only,
    20,000 blocks.
  * Metrics: `scacelith_http_rate_limited_total{limit}` (`ip`, `ip48`, `inflight`, `blocked`, and
    the route and `user` limits), `scacelith_tls_refused_total{reason}`,
    `scacelith_tls_connections_open`, `scacelith_http_inflight`, `scacelith_abuse_blocked_keys`,
    `scacelith_abuse_local_blocks_total`, `scacelith_abuse_report_entries_dropped_total`,
    `scacelith_abuse_blocks_total`, `scacelith_abuse_blocked`,
    `scacelith_abuse_blocks_evicted_total`, `scacelith_http_client_errors_total{reason}`.
* **Proof of work** (`security::pow`): an endpoint that wants one answers HTTP 428 `{ "error":
  "pow_required", "pow": { "challenge": "<opaque ASCII>", "bits": 18, "expiresAt": ms } }`. The
  client finds a nonce, a decimal ASCII string, such that `SHA-256(challenge + ":" + nonce)` starts
  with `bits` zero bits (most significant bit of the first byte first), and repeats the same request
  with `"pow": { "challenge", "nonce" }` added to the JSON body. A challenge is HMAC-signed, bound
  to the client's network (a keyed hash of its IPv4 address or IPv6 /64) and to the endpoint,
  expires after 2 minutes and is single use.
* **Logs** (`log`): no password, token, TOTP secret, recovery code, cookie or e-mail body; fields
  with such names are redacted and token prefixes (`sct_`, ...) masked. Client addresses follow
  `LOG_IP` (truncated to /24 or /48 by default, full, or hashed with a key rotated daily). Security
  events are kept `RETENTION_SECURITY_DAYS`, and their IP addresses erased after `RETENTION_IP_DAYS`
  (section 7).

## 9. Scaling

One process serves one machine: `WORKERS` shards share one runtime, and every limit of the
configuration is a whole-server limit. Several instances are independent servers, each with its
own accounts, database, journal and ports (DEPLOY.md, section 11). `SHARD_BASE` gives an instance
its own range of shard numbers, so that the game ids of instances given distinct ranges never
collide. Nothing more is implemented for several machines: presence, queues and limits live in one
process, the store is a local SQLite file, and no channel connects the hosts of different
instances. Capacity on one machine: SIZING.md.

## 10. Game client integration (C++)

* `net::OnlineClient` runs the network on its own thread and exposes a non-blocking command and
  event API to the game thread (`src/net/online_client.h`).
* Windows: WinHTTP for HTTPS and the WebSocket (TLS by the OS, certificate validation by the OS
  trust store; an optional per-server pinned SHA-256 fingerprint for self-signed community
  servers), DPAPI for stored tokens, BCrypt for SHA-256, random numbers and the proof of work,
  `ShellExecuteW` for the Google sign-in page (Google returns to the game's 127.0.0.1 listener).
  Linux test builds use OpenSSL.
* The client checks moves with `chess::Position` before sending (the same rules as the server),
  sends the intent when the destination is chosen, keeps the robots' physical animations, shows the
  ping discreetly, the players' names and ratings in the scoresheet header, and treats the server as
  the only authority on clocks and results.

## 11. Tests

* `cargo test --workspace` runs everything; nothing touches the network outside the loopback
  interface.
* Unit tests live next to the code (a `tests` module, or a `tests.rs` beside the module), black-box
  tests in `crates/*/tests` (chess rules, perft, PGN, the C++ cross-check).
* `realtime::e2e` starts a whole server (store, hosts and journals, lobby, auth, anti-cheat, API,
  listeners) on a loopback port and drives it with minimal WebSocket clients: pairing and a game to
  its rating update, reconnection, replacement, revoked sessions, bans and certain cheats, slow
  consumers, the drain and a restart that gives the game back.
* TLS tests use self-signed certificates made with `rcgen`; store tests use temporary files or
  in-memory databases; time-dependent components take a manual clock.
* Shared vectors in `test/fixtures` (protocol, Elo, chess cross-check) are read by the Rust tests
  and by the game's C++ tests.
* The real-engine analysis tests run when Stockfish is installed (or named by
  `SCACELITH_TEST_ENGINE`) and return at once otherwise.
* Every module keeps its per-message path allocation-light and documents its complexity where it
  runs per message.
