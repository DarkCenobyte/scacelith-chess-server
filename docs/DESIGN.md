# Scacelith dedicated server: design and module contracts

This document is the contract between the modules of `dedicated-server/` and the game client. Each
module owner implements exactly the API written here; anything a module needs from another one
goes through these interfaces. When an interface has to change, the change is made here first.

Runtime: Node.js >= 22.13, **no npm dependency**. Only Node built-ins (`node:tls`, `node:https`,
`node:crypto`, `node:sqlite`, `node:cluster`, `node:net`, `node:worker_threads`, `node:child_process`,
`node:test`...). ES modules (`"type": "module"`), 4-space indent, semicolons, single quotes,
JSDoc on exported functions. Code comments in English.

## 1. Processes

```
                      +------------------------- primary (control plane) --------------------------+
                      | config, migrations, serverId | presence: userId -> (shard, connId)          |
                      | matchmaker (per-category queues) | challenges + private codes | conduct   |
                      | global rate limiter + once-tokens | sanction broadcast | metrics HTTP      |
                      | spawns: N shard workers (cluster), 1 analysis process (anti-cheat)         |
                      +------+------------------------------+-------------------------------+------+
                        IPC (process.send, 'advanced' serialization, request/response ids)
                             |                              |                               |
   +-------------------------v--+        +------------------v---------+     +---------------v------+
   | shard 0 (cluster worker)   |<------>| shard 1                    | ... | analysis process     |
   | HTTPS API (shared port)    |  bus   | HTTPS API                  |     | UCI engine pool,     |
   | WSS server (shared port)   | (unix  | WSS server                 |     | per-game features,   |
   | connections, heartbeats    | socket | GameHost: games whose id   |     | per-player suspicion |
   | GameHost: its games        |  mesh) |   carries shard 1          |     +----------------------+
   | journal-0, SQLite (WAL)    |        | journal-1, SQLite (WAL)    |
   +----------------------------+        +----------------------------+
```

* **Shards** are cluster workers. The kernel/cluster distributes TCP connections among them, so a
  player's connection may live on any shard. A game lives on exactly one shard (its **host**), the
  one whose number is inside the game id (`src/util/ids.js`). When a player's connection is on
  another shard, that shard relays: C2S game messages go to the host over the **shard bus**, and
  the host's S2C frames come back over the bus and are written to the socket unchanged (already
  encoded, no re-serialisation).
* The **primary** is the control plane: nothing per move goes through it. It owns the global
  state that must be unique: presence (one live connection per account), queues, challenges,
  private codes, conduct cooldowns, global rate limits and single-use tokens. It also runs the
  periodic retention purge of the database, in small slices (section 7).
* The **analysis process** runs the engine analysis of finished rated games from a queue table
  in the database (bounded and prioritized, section 6.5); it never touches live games.
* SQLite (`node:sqlite`, WAL mode, `busy_timeout`) is opened by every shard and by the primary.
  Writes are small and batched; the WebSocket hot path never touches the database.

Horizontal scaling later: the bus becomes TCP (mTLS) between machines, the primary's control
plane becomes a service (or Redis), the store gets a PostgreSQL implementation of the same Store
API, and `SHARD_BASE` gives each instance its shard range (section 9).

## 2. Directory map and owners

| Path | Owner (agent) | Content |
|---|---|---|
| `src/config.js`, `src/log.js`, `src/metrics.js`, `src/util/ids.js`, `src/protocol/schema.js`, `docs/DESIGN.md` | orchestrator | shared foundations (agents may append keys to their own section of config.js) |
| `tools/gen-protocol.js`, `src/protocol/codec.gen.js` (a working run-time placeholder exists), `src/protocol/index.js`, `test/fixtures/protocol-vectors.json`, `docs/PROTOCOL.md`, `src/client/*` (Node SDK) | protocol | JS codegen, golden vectors, protocol reference, Node client SDK used by tests and bench |
| `src/chess/*` | chess | rules (move generation, legality, SAN/UCI/FEN, repetition, draws), perft |
| `src/game/*` | game | GameRoom (authoritative game), clocks, lag compensation, disconnection policy, GameHost (rooms of a shard, timer wheel, journal hooks, persistence queue) |
| `src/match/*` | match | Elo (per category), matchmaker, challenges/private codes/rematch, conduct cooldowns |
| `src/net/*`, `src/cluster/*`, `bin/scacelith-server.js` | net | RFC 6455 server, TLS listeners, connections, heartbeat, backpressure, per-connection limits, primary/worker bootstrap, IPC, shard bus, presence, routing, metrics endpoint, graceful shutdown |
| `src/store/*` (+ `migrations/*.sql`), `src/http/routes/players.js`, `src/http/routes/account-games.js` | store | schema, migrations, Store API, game journal and crash recovery, retention jobs, public read API |
| `src/http/server.js`, `src/http/router.js`, `src/http/routes/{auth,account,account-export,sso,info}.js`, `src/http/pages/*`, `src/auth/*`, `src/mail/*`, `src/security/*` | auth | HTTPS API, sessions, passwords, MFA, recovery, e-mail, SSO, rate limiting, proof of work |
| `src/anticheat/*`, `src/http/routes/reports.js`, `bin/admin.js` | anticheat | anomaly classification, sanctions, engine analysis, suspicion levels, reports, admin CLI |
| `src/gif/*`, `assets/*`, `src/http/routes/gif.js`, `src/http/routes/gif-thread.js` | gif | animated GIF of a game (renderer, encoder, pieces, fonts, thread pool), the two GIF endpoints with their cache and render quotas |
| `test/unit/<module>.*.test.js` | each owner | unit tests of the module |
| `test/integration/*` | tests | multiplayer scenarios against a real server |
| `bench/*` | bench | load generator and benchmark report |
| `tools/gen-protocol-cpp.js`, `../src/net/*` (C++, incl. generated `protocol_gen.*`), `../tests/net_tests.cpp`, `../CMakeLists.txt` (net part) | client-net | C++ codegen, transports (WinHTTP / OpenSSL), HTTPS API client, WSS client, credential store, OnlineClient |
| `../src/game/online_*`, `../src/ui/ui_screens_online*`, changes in `game_scene.*`, `ui_*`, `settings.*`, `assets/i18n/en.lang` | client-ui | menus, options, online game mode in the 3D scene, scoresheet header, ping |

## 3. Main flows

**Login (HTTPS).** `POST /api/v1/auth/login` -> session token `sct_<43 base64url chars>` (32
random bytes). The server stores `sha256(token)` only. With MFA: an intermediate `mfaToken`
(5 min, single use) and `POST /api/v1/auth/login/mfa`.

**Realtime connection.** `wss://host:WS_PORT/ws` with `Sec-WebSocket-Protocol: scacelith.v1`,
no `Origin` header (browsers are refused unless configured). The `101` answer carries
`Scacelith-Server-Id` (the `/info` `serverId`), so that a client which skipped `/info` still
checks that its saved session belongs to this server. Within `WS_HELLO_TIMEOUT_MS` the
client sends `Hello{proto, schema, client, token}`. The shard validates the token (auth
service, cached), checks bans and e-mail verification, then asks the primary
`presence.claim`. The primary refuses a new player beyond `MAX_CONNECTIONS` (`ServerFull`,
close 4006), but not a player whose game is in progress: the upgrade lets `max(16, 2 %)`
connections beyond `MAX_CONNECTIONS` through for them, since it cannot tell users apart. It kicks
an older connection of the same account (`Error{Replaced}` + close 4007) and returns the active
game, if any. The shard answers `Welcome`, then (if a game is active) attaches the connection to
the game, whose host sends a `GameSnapshot`.

**Matchmaking.** `QueueJoin{category, rated}` -> the shard checks the category and reads the
player's rating (`store.ratings.get`), then `mm.join` to the primary. Each `MATCH_TICK_MS` the
matchmaker pairs players; for each pair the primary allocates the host shard (the shard of the
player who waited longer; any shard when it is overloaded), sends it `game.create`, and sends
`game.attach` to the shards of both connections. The host sends `GameSnapshot` to both.

**A move.** Client A (on shard 2) sends `Move{game, ply, move, posHash, thinkMs}` at the moment
the destination square is chosen (the robot's hand then plays the move and presses the clock:
animation only), or, in a game without `autoPress` (below), when its player presses the clock.
Shard 2 sees the game id's shard is 1 and forwards the raw frame on the bus.
The host validates (section 6), updates the canonical state and the clocks, journals the move,
encodes one `MoveMade` and sends it to both players (local socket or bus). Opponent B's client
animates the other robot playing it. A's client treats its own `MoveMade` as the confirmation; a
`MoveRejected` makes it restore the server position from the `GameSnapshot` that follows.

**Clock press.** Whether the robots press the clock by themselves is a setting of each game,
`GameSnapshot.autoPress`. The primary decides it in `ControlPlane.createGame` for every new game
(queue, challenge, private code) from `AUTO_PRESS_CLOCK` (true by default); a rematch keeps the
value of the game it follows (`game.rematch` carries it). The host passes it to the room, which
writes it in the game's `created` journal record, so a replay, a compaction snapshot and a
restart keep it whatever the configuration says by then (a `created` record of an older build has
none: true). A finished game played without it has `RecordFlag.ManualPress` (8) in its record's
`flags` (5.5); there is no migration. The server's clock rules are the same either way: without
`autoPress` the client sends the move when its player presses the clock, so the time until the
press is charged like any thinking time.

**Gestures.** The player's head, the piece in hand, where it is aimed and a move placed before the
clock press reach the opponent live, as `Gesture` (C2S 0x28), relayed as the server `Gesture`
(0xA6) and never stored. The router takes a `C_Gesture` before the `WS_MSG_RATE` bucket and
never touches that bucket or answers `RateLimited` for it: the connection has a bucket of its own
(`GESTURE_RATE` per second, `GESTURE_BURST`; both announced in `Welcome`, 0 and 0 when
`GESTURE_RATE=0`), and a gesture beyond it is dropped silently, its seq still counted; more than
max(50, 10 x `GESTURE_BURST`, `GESTURE_RATE` x (`HEARTBEAT_TIMEOUT_MS` + `HEARTBEAT_INTERVAL_MS` +
the 250 ms sweeper tick)) drops in 10 s is a flood (4301). The bucket counts arrivals: after a
stall of the network or of the worker, the gestures of a client pacing them at the rate arrive
together, and the last term is what it sends during the longest silence that a connection lives
through (the heartbeat notices a silence at its next visit), so no such burst is a flood. Both
buckets and their drop windows run on the monotonic clock of `clock.js`: a step of the wall clock
neither empties them nor holds a drop window open. So does the heartbeat sweep (pings, silence
timeout, hello deadline): a step neither closes every connection nor holds the pings back. A
gesture is then decoded and its seq checked like any message (malformed: 4300), and it must name a
game attached to the connection. The local host gets `host.relayGesture(gameId, userId, frame)`;
for another shard the raw frame goes over
the bus as `ToHost` (skipped while that link holds a quarter of its queue), where the router
recognises it by its type byte and does not decode it again. The host finds the room and the
sender's colour (not a player: dropped, no anomaly: gestures are not authoritative), and copies
the frame without its seq into an `S_Gesture` (the two layouts match
byte for byte: `protocol.codec.test.js`) for the opponent's endpoint, through its
`sendDroppable`, which skips it when the connection already has a quarter of
`WS_SEND_BUFFER_LIMIT` unsent or, for another shard, when the bus link has a quarter of its queue
(that shard's router then checks the connection's own backlog). A gesture can therefore never
close a slow client. Nothing is journaled, and the room, its clocks, `gseq` and the anti-cheat
never see it; it is relayed as long as the room exists, the rematch window included. Metrics:
`scacelith_gestures_relayed_total` and `scacelith_gestures_dropped_total{reason}` (`rate`,
`not_attached`, `backlog`, `no_game`, `not_player`, `no_opponent`, `malformed`). Its CPU cost is
in docs/SIZING.md.

**End of game.** The room decides the result (mate, flag, resignation, agreement, claim,
abandonment, abort...), the host sends `GameEnd` to both, journals it, and queues the game for
the database. Every `DB_COMMIT_MS` the host commits the queued games in one transaction
(`store.games.finishBatch`). It first waits until the journal has written every record it holds
(so the database never has a finished game whose `ended` record a crash could still lose, which
would bring the game back running after the restart). When the anti-cheat still buffers an
anomaly that is not `info`, it writes the buffer first, so that the analysis-job policy sees it
(section 6.5); `info` anomalies wait for the anti-cheat's own 1 s timer. The transaction holds
the game record + both ratings (read and written inside the transaction, section 6.6) +
analysis job (queue policy: section 6.5). It runs on the shard's store writer thread
(`src/store/writer.js`, its own SQLite connection), so the event loop never waits for the
transaction, its disk writes or the write lock it takes. The anti-cheat writes through the same
thread (the anomaly rows, the automatic sanction of a certain cheat and its rating refunds), and
the thread handles its messages one at a time in the order they were sent: the anomalies flushed
right before a commit are written before it, without the event loop waiting for either. After
the commit it sends `RatingUpdate` and tells the primary `game.ended`.

When the journal's writes keep failing (a full disk, a read-only or failing `JOURNAL_DIR`
volume, too many open files), waiting for the journal would keep every finished game out of the
database: no rating change, both players still "in game" for the primary (every queue join or
challenge refused with `AlreadyInGame`), and the results lost at the next stop. So the wait is
bounded. A failed journal flush makes the host journal the batch's games again (one snapshot
each) and retry after its backoff; at the third failed flush in a row (about 0.3 s after the
first one with the default `DB_COMMIT_MS`), and at the first one during a shutdown, the batch is
committed without waiting for the journal. Its snapshots and `committed` records are still
appended, in case the journal comes back. The host logs one error for the whole episode and
counts the games committed this way in `scacelith_game_commit_unjournaled_total`. Each such
commit starts a journal flush (one at a time), and the first one that writes without a failure
ends the episode (logged at info level): commits wait for the journal again. The database is
then the only durable copy of these results, and `finishBatch` ignores a game id it already
has, so no later replay can apply a rating twice. One risk remains, the one the wait avoids:
after a crash, or a restart before the journal has written the game's snapshot or `committed`
record, a game whose `ended` record was lost with a failed write comes back running. Its players
get it back as their game in progress, and whatever result it ends with, the database keeps the
first one.

**Reconnection.** A lost connection keeps the game running (the player's clock too, except
for the clock hold of a game restored after a restart: section 6.4). The opponent gets
`GameEvent{PlayerDisconnected, arg = grace ms}`. A new connection (any shard)
says `Hello`; the primary knows the active game; the client receives `GameSnapshot` and goes on.
After the grace period without a connection: section 6.4. The game client spreads its
reconnections so that a restart or a full server does not bring every player back at the same
instant: full jitter (a random delay between 0.5 s and min(30 s, 2 s x 2^n)), 60 to 120 s after
`ServerFull` (HTTP 503 at the upgrade or close 4006), a first attempt 5 to 35 s after a shutdown
(`Notice{ServerShutdown}` or close 4008), and no `/api/v1/info` request for 10 minutes after
losing a connection that had reached `Welcome`, after a shutdown too, so a restart costs one TLS
handshake per player (another server at the same origin is caught by the server id of the `101`
answer, before `Hello`; PROTOCOL.md, lifecycle step 6). A player whose game is in progress is
the exception: the grace is short (at least
`RECONNECT_GRACE_MIN_MS`, 15 s by default, and `RECOVERY_GRACE_MS`, 90 s by default, for a game
restored after a restart), so their attempts are 8 s apart at most unless the server gave a
`Retry-After`, and the first one after a shutdown comes 1 to 8 s after it.

**Client pings.** Besides answering the server's heartbeat (`HEARTBEAT_INTERVAL_MS`, which also
measures the round trip used for lag compensation; the router's sweep pings each connection every
half interval to one interval), the game client sends its own `Ping` for its ping indicator and
its estimate of the server clock. The server chooses how often: `Welcome.clientPingMs` =
`CLIENT_PING_INTERVAL_MS` (10 s by default, 1 s to 60 s). The client sends four quick pings after
each `Welcome` and then follows that interval. Every ping costs a TLS record each way for every
connected player, so a lower value makes the indicator more reactive at a measurable price on a
small machine (docs/BENCHMARK.md). The router answers at most one client `Ping` per 950 ms per
connection. The client's liveness check does not depend on that interval: when nothing came for
1.5 heartbeats (at least 7.5 s, at most 90 s) it sends one `Ping` at once, and it calls the
connection dead after two heartbeats (at least 10 s, at most 120 s) with nothing received: it
counts `Welcome.heartbeatMs` as 60 s at most.

## 4. Shared foundations (exist already)

* `loadConfig()` / `testConfig(overrides)` in `src/config.js`: frozen camelCase object
  (`cfg.apiPort`, `cfg.categories` = `[{id:'3+2', baseMs, incMs}]`, secrets are Buffers).
* `logger.child('name')` in `src/log.js`: `.debug/.info/.warn/.error(msg, fields)`,
  `.security(event, fields)`. Redaction is automatic; pass IPs through `ipForLog(ip)`.
* `metrics` in `src/metrics.js`: `counter / gauge / gaugeFn / histogram`, pre-bound `labels()`.
  Metric names are `scacelith_<area>_<what>[_unit][_total]`.
* `GameIdAllocator(shard).next()`, `.seed(id)` (the next ids come after `id`), `shardOfGameId(id)` in
  `src/util/ids.js`.

## 5. Contracts

### 5.1 Protocol codec (generated from `src/protocol/schema.js`)

JavaScript (`src/protocol/index.js` re-exports `codec.gen.js`):

```js
import { MSG, encode, decode, ProtocolError, PROTOCOL_VERSION, PROTOCOL_MIN, SCHEMA_HASH,
         WS_SUBPROTOCOL, enums, MoveFlag, CloseCode, isClientType } from '../protocol/index.js';
MSG.Move === 0x20; MSG.S_Ping === 0x82; MSG.C_Ping === 0x02;   // names shared by both directions get C_/S_ prefixes
const buf = encode.MoveMade({ game, gseq, ply, move, flags, spentMs, whiteMs, blackMs, serverTime, drawOffer, firstMoveMs }); // Buffer, exact size
const msg = decode(buf);   // { type: 0x20, seq, game, ply, ... } ; throws ProtocolError{reason} when malformed
decode(buf, { dir: 'c2s' });  // refuses server->client types (server side uses this)
```
`encode.<Name>` exists for every message; the direction-shared names are `encode.C_Ping`,
`encode.S_Ping`, `encode.C_Pong`, `encode.S_Pong`. Decoded objects use the schema field names;
`enum:` fields are numbers; `struct:` fields are nested objects; lists are arrays. `id53` decodes
to a number. Encoding validates ranges too (a server bug must not produce a frame the client
refuses). Helpers in `src/protocol/index.js` (hand-written, protocol owner): `encodeMove(from,
to, promo)`, `decodeMove(u16) -> {from, to, promo}`, `fnv1a32(string)`.

C++ (`src/net/protocol_gen.h`, namespace `net::proto`): one struct per message with the same
field names (camelCase), types `uint8_t/uint16_t/uint32_t/int32_t/double/uint64_t(id53)/bool/
std::string/std::vector<T>`, enums as `enum class` with the schema values, and:
```cpp
constexpr uint16_t kProtocolVersion = 1; constexpr uint16_t kProtocolMin = 1; constexpr uint32_t kSchemaHash = 0x........;
constexpr const char* kWsSubprotocol = "scacelith.v1";
enum class MsgType : uint8_t { Hello = 0x01, ..., C_Ping = 0x02, S_Ping = 0x82, ... };
void encode(const Move& m, std::vector<uint8_t>& out);          // appends
bool decode(const uint8_t* p, size_t n, MoveMade& out);         // false when malformed
bool peekType(const uint8_t* p, size_t n, MsgType& t);
```
Struct names are the message names, with `C_`/`S_` prefixes for Ping/Pong. `SCHEMA_HASH` is the
first 4 bytes (big-endian u32) of SHA-256 of the canonical JSON of `{version, enums, structs,
messages}` (object keys sorted, `doc` fields excluded): `computeSchemaHash()` in
`src/protocol/schema-hash.js`, used by both generators. Golden vectors
(`test/fixtures/protocol-vectors.json`: message object + hex encoding) are checked by the JS
tests and by `tests/net_tests.cpp`.

### 5.2 Chess rules (`src/chess/index.js`)

Mirrors `src/chess/chess.h` of the game exactly (same automatic endings, same claim rules, same
FEN normalisation of castling rights and en passant). Moves are the protocol u16.

```js
export const WHITE = 0, BLACK = 1;
export const PieceType = { None: 0, Pawn: 1, Knight: 2, Bishop: 3, Rook: 4, Queen: 5, King: 6 };
export class Position {
  static start(); static fromFEN(fen) /* -> Position | null */; clone();
  get side(); get castling(); get epSquare(); get halfmove(); get fullmove();
  legalMoves() /* -> number[] (u16 moves, promotions expanded) */;
  isLegal(move) /* bool; ignores bit patterns that are not a legal move */;
  play(move) /* applies a LEGAL move, returns MoveFlag bits incl. Check/Mate; throws on illegal */;
  inCheck(); hasLegalMove(); isCheckmate(); isStalemate();
  hasInsufficientMaterial(); canColorMate(color);
  fen(); digest() /* u32 posHash, section 5.1 of schema.js */; repetitionKey() /* string */;
  san(move); uci(move); parseUCI(str) /* -> move | -1 */; perft(depth);
}
export class ChessGame {
  constructor(startFen?);
  position;             // current Position
  moves;                // number[] u16
  status; reason;       // enums.GameStatus / enums.EndReason values
  play(move) /* -> { ok:false } if illegal or game over, else { ok:true, flags, status, reason } (updates mate, stalemate, insufficient, 5-fold, 75) */;
  repetitionCount(); canClaimThreefold(); canClaimFiftyMove();
  claimDraw() /* -> bool */; resign(color); agreeDraw(); flagFall(color); /* flag: draw if the opponent cannot mate */
  end(status, reason);  // for online reasons (abandonment, abort, forfeit...)
  sanMoves(); pgn(tags);
}
```
Performance target: `isLegal` + `play` < 20 µs per move, no per-move allocation beyond small
arrays (use typed-array mailbox, 0x88 or 10x12).

### 5.3 GameRoom and GameHost (`src/game/`)

`GameRoom` is deterministic: time is always passed in (`now` in ms, monotonic wall clock from
`src/game/clock.js: now()` = `performance.timeOrigin + performance.now()`), no timers inside.

```js
new GameRoom({ id, category /* '3+2' | 'custom' */, baseMs, incMs, rated,
               white: { userId, name, rating, provisional }, black: {...},
               createdAt, config, rematchOf?, autoPress? /* true */,
               createChessGame /* () => new ChessGame(), injected by the host */ })
// recvAt (optional, default now): the arrival the host credits after a stall of its worker (6.1);
// stalledSince (optional, default Infinity): the start of that stall
room.onMove(color, { seq, ply, move, posHash, thinkMs, drawOffer }, now, recvAt?, stalledSince?)  -> Outcome
room.onResign(color, now, seq?, recvAt?, stalledSince?) / onDrawOffer(...) / onDrawAnswer(color, accept, now, seq?, recvAt?, stalledSince?)
room.onDrawClaim(color, now, seq?, recvAt?, stalledSince?) / onAbort(color, now, seq?, recvAt?, stalledSince?)
room.onRematch(color, accept, now, seq?, recvAt?, stalledSince?)
room.onDisconnect(color, now, recvAt?, stalledSince?) / onReconnect(...) / onResync(color, now, recvAt?, stalledSince?)
room.onRtt(color, rttMs)
room.forfeit(color, now, recvAt?, stalledSince?)  // anti-cheat: loss (EndReason.Forfeit)
room.serverAbort(now)                // EndReason.ServerAborted
room.tick(now, stalledSince?) -> Outcome  // flags, first-move timeouts, grace expiries
room.nextDeadline() -> ms | Infinity
room.snapshot(forColor, now) -> object for encode.GameSnapshot
room.isOver; room.result          // { status, reason, whiteMs, blackMs, endedAt }
room.record() -> finished game record for store.games.finishBatch (section 5.5)
room.journalState() / GameRoom.fromJournal(records)   // see 5.6
```
`Outcome = { broadcast: [Buffer], reply: [Buffer] (to the sender), anomaly: null | { color, kind,
detail, posMatched }, ended: bool, journal: [records], clockStarted: colour | 2 }`. Buffers are
already encoded with the codec; the host sends them as they are. `clockStarted` names a clock held
since a recovery that has just started (6.4): the host then sends the other player a new
`GameSnapshot`, unless the outcome holds a move or the end.

`GameHost` (one per shard):
```js
new GameHost({ shard, config, store, journal, anticheat, bus, primary, log })
host.createGame(spec) -> gameId            // spec from the primary (players, tc, rated, colours, autoPress)
host.attach(gameId, userId, endpoint)      // endpoint: { send(buf), sendDroppable?(buf), connId, shard } ; sends the snapshot
host.detach(gameId, userId, endpoint)      // connection closed
host.onClientMessage(gameId, userId, msg, endpoint) // decoded C2S game message (Move..Rematch)
host.relayGesture(gameId, userId, frame) -> bool    // raw C_Gesture frame -> S_Gesture to the opponent (section 3)
host.heartbeat(t) ; host.stallStart(t) ; host.stallCredit(t) ; host.stallDuring(t0)  // stall detection and credit (6.1)
host.recover() -> count                    // replays the journal at start-up
host.compactJournal(now) -> count          // journal snapshots the journal asks for (5.6), from the interval
host.stats() -> { games, ... }
host.shutdown()                            // flush journal + pending commits
```
The host owns a timer wheel (10 ms slots) driven by one interval; each room's `nextDeadline()`
is (re)scheduled after every outcome. Each run of the interval is a beat of the stall detection
(6.1). It records `anomaly` through `anticheat.recordAnomaly`
and, for a certain cheat with `AUTO_SANCTION_CERTAIN_CHEATS`, calls `room.forfeit` and
`anticheat.sanctionCertain` (section 5.8).

### 5.4 Match (`src/match/`)

```js
// elo.js: pure, mirrors src/game/elo.h of the game (FIDE ratings, section 6.6).
expectedScore(rating, opponent); kFactor(record /* {rating, games, peak, reachedSenior, rated} */, cfg);
applyGame(white /* record */, black, score /* 1, 0.5, 0 from White's side */, cfg)
  -> { white: { before, after, delta, k, expected, games, provisional, record }, black: {...} }
  // records updated (games, w/d/l, peak, reachedSenior, rated and the unrated-phase sums);
  // k: the K factor of the change, 0 when the K formula did not apply (unrated phase, unrated opponent)
// matchmaker.js (primary)
new Matchmaker({ config, now })
mm.join({ userId, username, category, rated, rating, provisional, shard, connId, colorBalance, joinedAt }) -> { ok } | { error: ErrorCode }
mm.leave(userId) -> bool ; mm.has(userId) ; mm.statusOf(userId, now) -> QueueStatus fields
mm.tick(now) -> [{ category, rated, white: entry, black: entry }]
mm.recordPairing(a, b, now)  // repeat limit bookkeeping (done by tick itself)
// challenges.js (primary)
new Challenges({ config, now })
ch.create({ from: {userId, username, rating, provisional, shard, connId}, target /* username | '' */, baseSec, incSec, rated, color }) -> { ok, challenge } | { error }
ch.accept(id, by /* player */) -> { ok, challenge } | { error } ; ch.decline(id, userId) ; ch.cancel(id, userId)
ch.joinCode(code, by) -> { ok, challenge } | { error } ; ch.expire(now) -> [challenge] ; ch.forUser(userId)
// conduct.js (primary; persistent counters in store.conduct)
conduct.record(userId, kind /* 'abandon'|'abort'|'noshow' */, now) ; conduct.cooldownUntil(userId, now) -> ms | 0
```
Official categories come from `cfg.categories`; `categoryOf(baseMs, incMs)` returns the id or
`'custom'`.

### 5.5 Store (`src/store/index.js`)

`openStore(config, { readonly = false, applyGame }) -> Store` (`applyGame` is
`match/elo.js`'s, passed in by the bootstrap so the store does not import the match module). Synchronous (`node:sqlite`). WAL,
`synchronous=FULL`, `foreign_keys=ON`, `busy_timeout=5000`. Prepared statements cached. All
times are epoch ms integers. `migrate(store)` applies `migrations/NNN_*.sql` in order inside
transactions, recorded in `schema_migrations`.

```js
store.meta.get(key) / set(key, value)                 // 'server_id' (uuid, created at first start)
store.users.create({ username, email, passwordHash, emailVerified }) -> id   // throws StoreError{code:'username_taken'|'email_taken'}
store.users.byId(id) / byUsername(name) / byEmail(email) / byLogin(usernameOrEmail) -> User | null
  // User: { id, username, email, emailVerified, passwordHash, mfaEnabled, mfaSecretEnc, mfaLastStep,
  //         createdAt, lastLoginAt, status ('active'|'deleted'), acceptChallenges, pendingMfaSecretEnc }
store.users.update(id, fields) ; store.users.anonymize(id)
store.users.advanceMfaStep(id, step) -> bool          // atomic: only when step > mfaLastStep (TOTP replay protection)
store.mfa.replaceRecoveryCodes(userId, hashes) ; store.mfa.consumeRecoveryCode(userId, hash) -> bool ; store.mfa.countRecoveryCodes(userId)
store.sessions.create({ userId, tokenHash, createdAt, expiresAt, idleExpiresAt, clientLabel, ip }) -> id
store.sessions.byTokenHash(hash) -> { id, userId, createdAt, lastSeenAt, expiresAt, idleExpiresAt, revokedAt } | null
store.sessions.touch(id, now, idleExpiresAt) ; revoke(id) ; revokeAllForUser(userId, exceptId?) -> [tokenHash] ; listForUser(userId) ; enforceLimit(userId, max)
store.tokens.create({ kind, tokenHash, userId, data, expiresAt }) ; consume(kind, tokenHash, now) -> row | null (atomic single use) ; get(kind, tokenHash) ; update(kind, tokenHash, data)
store.sso.find(provider, subject) -> { userId } | null ; link(userId, provider, subject, email)
store.ratings.get(userId, category) -> { rating, games, wins, draws, losses, peak, reachedSenior, rated, countedGames, unratedGames,
  unratedOpponents, unratedHalfPoints }  // defaults when absent: unrated at INITIAL_RATING
store.ratings.forUser(userId) -> [{ category, ..., provisional }] ; leaderboard(category, limit, minGames)  // rated, >= minGames counted games
store.refunds.applyForCheater({ cheaterId, since, now, sanctionId, source, by }) -> [refund]   // section 6.6, one transaction
store.refunds.list({ cheaterId, victimId, limit }) ; pendingSince(afterId, limit) ; pendingFor(victimId) ; markNotified(ids, now)
store.games.finishBatch(records) -> [{ gameId, ratings: null | { white: RatingChange, black: RatingChange } }]
  // One transaction for the batch: inserts each game, applies match/elo.applyGame to rated games
  // with the ratings read inside the transaction (and stores the K factor of each side's change),
  // queues rated games of >= ANALYSIS_MIN_PLIES for analysis
  // (section 6.5: a game left out by the policy has analysisSkipped: 'sample' | 'backlog' | 'player' in its entry,
  // and a game that took over waiting jobs past the per-player cap has analysisDisplaced: [gameId]).
  // record: { id, category, rated, baseMs, incMs, whiteId, blackId, whiteName, blackName, whiteRating, blackRating,
  //           startedAt, endedAt, status, reason, moves: Uint16Array, spentMs: Uint32Array, clockMs: Uint32Array,
  //           rematchOf, flags }
  // flags: 1 rated requested, 2 recovered after a restart, 4 forfeit, 8 manual clock press (autoPress
  // false; records of older builds never have it)
store.games.byId(id) ; lastId() ; recentForUser(userId, limit, before?) ; countBetween(a, b, since)
store.conduct.record(userId, kind, at) ; store.conduct.countSince(userId, since) -> { abandon, abort, noshow } ; store.conduct.cooldown(userId) / setCooldown(userId, until, level)
store.sanctions.create({ userId, kind /* 'ban'|'mm_block'|'warning' */, reason, source /* 'auto'|'moderator' */, gameId, startsAt, endsAt, createdBy }) -> id
  // a ban for cheating: source 'auto' and reason 'certain_cheat:<kind>', or source 'moderator' and reason
  // 'confirmed: <reason>' ('confirmed, no refund: <reason>' with --no-refund); finishBatch refunds the games
  // recorded during an 'auto' or a 'confirmed: ' ban only (section 6.6)
store.sanctions.activeBan(userId, now) -> sanction | null ; list(userId) ; lift(id, by, now)
store.anomalies.insertBatch([{ userId, gameId, kind, severity /* 'info'|'suspicious'|'certain' */, detail, at }]) ; forUser(userId, limit)
store.security.insertBatch([{ kind, userId, ip, detail, at }]) ; purge(now, cfg)
store.analysis.next(limit, workerId, now) -> [job] ; complete(gameId, features) ; fail(gameId, error) ; forUser(userId, limit)
  // next() takes the highest priority first, then the oldest, but every fourth claim the oldest ordinary job first
  // (section 6.5); job: { gameId, priority, attempts, queuedAt, startedAt, worker }
store.analysis.enqueue(gameId, now) ; request(gameId, 'report' | 'signal', now) -> bool ; backlog() -> { ordinary, priority }
  // enqueue: moderator re-analysis (priority manual); request: a reported game, 'signal' for a low-credibility
  // report (section 6.5); backlog counts each tier up to 100,000
store.integrity.get(userId) -> { level /* 'none'|'suspected'|'high_confidence'|'confirmed' */, score, evidence, updatedAt, reviewedBy }
store.integrity.set(userId, fields) ; listFlagged(minLevel, limit) ; populationStats(ratingBucket) / updatePopulation(...)
store.reports.create({ reporterId, reportedId, gameId, category, comment, weight, at }) -> id
store.reports.countByReporterSince(reporterId, since) ; exists(reporterId, reportedId, gameId) ; listOpen(limit) ; forReported(userId) ; resolve(id, outcome, by, now)
store.retention.run(now, cfg) -> counts        // expired sessions/tokens, old security events, IP erasure (section 7)
store.retention.runAsync(now, cfg, { sliceMs, signal }) -> Promise<counts>   // the same, returning to the event loop between slices
store.close()
```

### 5.6 Journal (`src/store/journal.js`)

Crash safety of games in progress without a database write per move. One journal per shard:
`JOURNAL_DIR/shard-<n>/segment-<seq>.log`. Records are binary:
`u32 length | u32 crc32c | u8 kind | id53 gameId | f64 at | payload`. Writes are buffered and
flushed (one `write` + optional `fsync`) every `JOURNAL_FLUSH_MS`. Segments rotate at 16 MB; a
segment is deleted when no game needs it any more: every game it mentions is ended *and*
committed to the database, or has a newer snapshot (below). A segment holding a game's
`committed` record outlives every other segment that mentions that game, so a recovery never
sees a committed game without its `committed` record. A torn last record is ignored on replay.

```js
const j = await openJournal({ dir, shard, flushMs, fsync, compactSegments })
j.append(kind, gameId, payloadBuffer, at)   // kinds: 1 created (JSON spec), 2 move, 3 event, 4 ended, 5 committed, 6 snapshot
j.flush() -> Promise ; j.committed(gameId)  // marks it safe to forget
j.hasUnwritten() -> bool ; j.failedWrites    // records not on disk yet ; count of failed writes
j.compactionCandidates(max) -> [gameId]      // games to snapshot now, a failed write's first (at most max, 8 per flush)
j.recover() -> Map<gameId, [{ kind, at, payload }]>  // games not committed, in order, from their latest snapshot
j.stats() ; j.close()
```
The game module decides the payloads (`GameRoom.journalState` / `journalSnapshot` / `fromJournal`).

**Compaction.** Without it, a segment stays as long as the oldest game it mentions runs, so a
shard would keep its write rate times the age of its oldest game on disk: with custom time
controls (up to 3 h + 180 s, games of up to about 66 hours) that is gigabytes, and a restart
replays all of it. A `snapshot` record holds the whole state of one game (`room.journalSnapshot`:
the records of `journalState()`, i.e. the created spec, every move with its clocks, one
checkpoint with the offers, presence, desyncs, flags, turn start and lag quotas, and the result
of a finished game; about 45 bytes per ply, under 60 KB for the longest game) and supersedes
every earlier record of its game. When the journal starts segment N, the games not committed
whose first needed segment (the one of their first record, or of their latest snapshot) is
`N - JOURNAL_COMPACT_SEGMENTS` or older are queued. The host's 10 ms interval asks
`compactionCandidates(max)` and appends a snapshot of each queued game it hosts, running or
ended and waiting for its commit, between two outcomes (so the snapshot includes every record
already appended for that game). It builds at most 2 snapshots per 10 ms tick and a flushed batch
holds at most 8, which keeps the cost off the move path: building one costs about 0.2 µs per ply,
about 0.3 ms with its CRC for the longest game. Once the batch holding a snapshot is written and
fsynced, that segment becomes the game's first needed one and the older segments are deleted by
the rule above. A shard's journal therefore stays around `JOURNAL_COMPACT_SEGMENTS + 1` segments
(80 MB by default) however long its games last, and so does the replay at start-up. Games that
end sooner are never snapshotted (at 17 KB/s per shard, 3 to 4 segments of 16 MB are about 50 to
65 minutes of writes); a game that lasts longer is written again once every
`JOURNAL_COMPACT_SEGMENTS` segments, which adds little unless a shard runs thousands of such games.

Crash safety of the compaction: before the batch holding a snapshot is durable, every older
segment is still on disk, and a torn snapshot is ignored like any torn record (the game replays
from its older records). Once it is durable, recovery starts each game from its latest snapshot
and ignores the records before it, wherever they are, so a crash in the middle of the deletions
is harmless too. A game with records left in an old segment that another game still needs keeps
that segment listed among its own, so its `committed` record still outlives it. With
`JOURNAL_FSYNC=true` this also holds after a power loss: a segment is deleted only once the
`committed` or `snapshot` record that releases it is fsynced; at start-up the journal fsyncs the
segments it read and their directory before it deletes anything (a record it reads may have been
written by a process that died before its fsync); and a segment holding a `committed` record is
deleted only after a directory fsync has made the deletion of that game's older segments durable
(about once per rotation). The metrics `scacelith_journal_snapshots_total`,
`scacelith_journal_segments` and `scacelith_journal_disk_bytes` (per shard) show the compaction at
work. With `JOURNAL_FSYNC=false`, which gives no power-loss guarantee for the last records, a
batch holding a snapshot is still fdatasynced before its bookkeeping deletes anything (after a
directory fsync when its segment was created since the last one), and at start-up the journal
fsyncs the segments holding a snapshot and their directory before it deletes anything: otherwise
a power loss shortly after a compaction could keep the deletions and lose the snapshot, and with
it the whole game. That costs one fdatasync per batch holding snapshots, at most one per game
and rotation. Server versions older than the compaction do not know the `snapshot` record: rolled
back to one, a shard cannot rebuild the games compacted so far and drops or aborts them.

Write failures: after a failed write (a full disk, for example), the next batch goes to a new
segment. The `committed` records of the failed batch are appended again (without them the segments
of those games would stay until the next restart). Every other game of the failed batch lost
records: its `created` record, moves, events, its `ended` record or a snapshot. Its later records
would replay after a gap, and a restart would then restore it at an old ply and abort it
(`ServerAborted`), or drop it when its `created` record was the lost one. So these games go to a
heal queue, which `compactionCandidates` serves before the compaction queue: whatever their age,
even a game the journal does not track yet, never one whose `committed` record it has, and still at
most 8 snapshots per flushed batch, so a large failed batch heals over a few flushes. The host
appends a snapshot of each of them it still hosts and has not committed, running or waiting for its
commit, and that snapshot supersedes the lost records. A crash before the snapshot is written still
loses them. `j.failedWrites` counts the failures. The host commits a finished game to the database
only once `j.hasUnwritten()` is false or a flush has written what it held without a new failure.
After a failure it marks every game waiting for its commit and journals each one again (one
snapshot) right before its own commit, since the failed write may have held its `ended` record; the
synchronous work of one commit attempt is thus bounded by its batch (500 games at most). When the
flushes keep failing, the third failed one in a row (the first one during a shutdown) lets the
batch go to the database without the journal, until a flush writes again (section 3, "End of
game"). `host.shutdown()` logs a failed final flush instead of throwing, and the shard's stop goes
on.

### 5.7 Primary control plane: IPC catalog

Transport: `src/cluster/ipc.js` (net owner) gives both sides `request(type, payload) ->
Promise<reply>` (timeout 5 s) and `on(type, handler)`; handlers return the reply (or a promise).
Buffers inside payloads are sent as they are (`serialization: 'advanced'`).

Shard -> primary:

| type | payload | reply |
|---|---|---|
| `presence.claim` | `{ userId, username, shard, connId, ip }` | `{ ok, activeGame: id or 0, kicked: bool }` or `{ error }` (ServerFull, Banned) |
| `presence.release` | `{ userId, connId }` | - |
| `conn.ipAcquire` / `conn.ipRelease` | `{ ip }` | `{ ok }` or `{ ok: false, reason }` (`per_ip`: `MAX_CONNECTIONS_PER_IP`; `global`: `MAX_CONNECTIONS` plus `max(16, 2 %)`) |
| `mm.join` | `{ userId, username, category, rated, rating, provisional, shard, connId }` | `{ ok }` / `{ error }` (`Banned`: a ban found in the database, enforced as `sanction.applied`, 6.6) |
| `mm.leave` | `{ userId }` | `{ ok }` |
| `challenge.create` / `.accept` / `.decline` / `.cancel` / `.joinCode` | see 5.4 | `{ ok, id?, code? }` / `{ error }` |
| `game.ended` | `{ gameId, whiteId, blackId, status, reason, rated, category, rematchOffer }` | - |
| `game.rematch` | `{ gameId, white, black, category, baseMs, incMs, rated }` | `{ ok, gameId }` / `{ error }` |
| `conduct.record` | `{ userId, kind }` | - |
| `ratelimit.take` | `{ key, limit, windowMs, cost }` | `{ allowed, retryAfterMs, count }` |
| `ratelimit.refund` | `{ key, windowMs, cost, ageMs }` | `{ refunded }` (gives back a take granted `ageMs` ago, in the window that counted it) |
| `once.consume` | `{ key, ttlMs }` | `{ fresh }` |
| `sanction.applied` | `{ userId, until, reason, refunds }` | - (primary kicks the user everywhere; `refunds`: victims refunded, whose notices it looks for at once) |
| `session.revoked` | `{ userId, tokenHashes }` | - (broadcast to every shard's auth cache) |
| `abuse.report` | `{ entries: [[key64, key48 or null, weight]] }` | - (notification: the refusals counted toward a block since the last report, at most one report per second per worker and 512 entries, the largest first; section 8) |

Primary -> shard:

| type | payload | effect |
|---|---|---|
| `game.create` | `{ spec }` | host creates the room, replies `{ ok, gameId }` |
| `game.attach` | `{ gameId, userId, connId }` | the shard binds that connection to the game (local or via bus) |
| `conn.send` | `{ connId, frames: [Buffer] }` | writes encoded S2C frames (QueueStatus, Challenge*, Notice); as a request (refund notices) it replies `{ ok }`, false when the connection is gone or has not had its Welcome yet |
| `conn.kick` | `{ connId, code, closeCode, frames }` | sends then closes |
| `auth.invalidate` | `{ userId, tokenHashes }` | drops cached sessions |
| `metrics.snapshot` | - | replies `registry.snapshot()` |
| `shutdown` | `{ graceMs }` | drain: Notice{ServerShutdown}, stop accepting, flush |
| `abuse.block` | `{ blocks: [[key, ttlMs, level]] }` | the shard blocks these addresses (IPv4, IPv6 /64 or /48) for `ttlMs` on its monotonic clock; sent to every shard with the new blocks of each `abuse.report`, and to one shard at its `shard.ready` with every running block (section 8) |

### 5.8 Shard internals (net owner) and the anti-cheat hooks

```js
// src/net/ws.js
class WsServer { constructor({ maxMessageBytes, subprotocol, allowOrigins, onConnection, guard }) ; handleUpgrade(req, socket, head) }
class WsConnection {
  id; ip; userId; username; state /* 'hello'|'ready'|'closing' */; rttMs; bufferedBytes;
  sendFrame(buf) -> bool   // one binary message; false when closed or over WS_SEND_BUFFER_LIMIT (then closes 4303)
  close(code, reason)
}
// src/cluster/router.js: decodes C2S frames (decode(buf, {dir:'c2s'})), enforces seq, rate limits,
// handles Ping/Pong, sends Queue*/Challenge* to the primary, game messages to the local GameHost or
// to the host shard over the bus (raw frame + userId + connId), and bus deliveries back to sockets.
// C_Gesture has its own bucket and relay path (section 3); endpoints have sendDroppable(buf) for it.
// src/cluster/bus.js: Unix-socket (Windows: named pipe) mesh between shards; frames
// [u32 len][u8 kind][u32 connId][u32 userId][payload]; batched writes (setImmediate).
```
Anti-cheat API used by the router and the host (`src/anticheat/index.js`):
```js
const ac = createAnticheat({ config, store, primary, log })
ac.recordAnomaly({ userId, gameId, kind, detail, posMatched }) -> { severity, certain }  // batched to the store
ac.sanctionCertain({ userId, gameId, kind }) -> { banUntil }  // ban + primary 'sanction.applied'
ac.classify(kind, ctx) -> { severity, certain }
```

Connection storms (`src/net/listeners.js`, `Router.isFull`). Every listener listens with
`LISTEN_BACKLOG` (the kernel caps it at `net.core.somaxconn`; the primary's round-robin handle
honours it too). With native TLS, a gate (`TlsGate`) runs in front of the TLS servers' own
'connection' listener, which is where Node starts the TLS work on a new TCP socket. The gate
closes the sockets it refuses with an RST, before any TLS work, and counts them in
`scacelith_tls_refused_total{reason}`:

* Protection per address first (`src/net/ipguard.js` `IpGuard.connection`, section 8; skipped
  for `ABUSE_EXEMPT`): a blocked IPv4 address, IPv6 /64 or /48 (`blocked`), more than the
  worker's share of `IP_CONN_RATE` new connections per second (`conn_rate`) or of
  `IP_MAX_CONNECTIONS` open ones (`conn_open`) from one address are refused before anything is
  read: two `Map` lookups and a token bucket. A socket let through is counted open until its
  'close', whatever happens to it next. The refusals that say something about one address
  (`conn_rate`, `conn_open`, `per_ip`, `waiting_per_ip`, `bad_hello`, `hello_timeout`, a failed
  handshake) count toward a block of it; the server-wide ones (`handshakes`, `waiting`,
  `server_full`) and the refusals of an address already blocked do not.
* Waiting for the ClientHello. A new socket holds no handshake slot until its first TLS record
  has arrived whole: a handshake record of at most 16 KB whose body starts a ClientHello (a client
  may fragment the ClientHello over several records; only the first one is awaited). It has 3 s
  for that, in as many TCP segments as it likes. The gate copies the bytes as they come and puts
  them back into the raw socket before it hands the socket to Node's listener, which replays them
  into the TLS engine. A socket that stays silent or is too slow (`hello_timeout`), that sends
  anything else (`bad_hello`), or that ends first is closed. Waiting sockets cost no CPU and
  little memory, and they are bounded as well: `16 * MAX_PENDING_HANDSHAKES` per worker
  (`waiting`) and `4 * MAX_PENDING_HANDSHAKES_PER_IP` per address group (`waiting_per_ip`). The
  gauge `scacelith_tls_hello_waiting` counts them.
* Handshake slots. The socket then takes a slot, held until 'secureConnection', 'tlsClientError'
  or its close. Beyond `MAX_PENDING_HANDSHAKES` slots per worker (`handshakes`), or
  `MAX_PENDING_HANDSHAKES_PER_IP` for one address group (`per_ip`), it is closed. The per-group
  cap is separate from `MAX_CONNECTIONS_PER_IP` and small: by default
  `MAX_PENDING_HANDSHAKES / 32` with a floor of 2, so 4 (config.js computes it, so `check-config`
  prints the value in use), and config.js refuses a value that is not below
  `MAX_PENDING_HANDSHAKES`. An address group here is an IPv4 address or an IPv6 /48,
  the usual allocation of one customer (presence.js counts WebSocket connections per /64). A
  worker is one thread: starting 10,000 handshakes together (1-3.5 ms of CPU each) makes all of
  them late, while a bounded number in flight lets the CPU finish them in turn; the refused
  clients retry with their backoff. Players behind one public IPv4 address (NAT, CGNAT), or in
  one IPv6 /48 (some mobile and residential networks give many customers addresses from the same
  /48), share their group's slots; a handshake lasts about one round trip, so 4 per worker still
  serve many of them, and `MAX_PENDING_HANDSHAKES_PER_IP` can be raised where that is not enough.
* Handshake timeout (10 s). Node only reports it ('tlsClientError') and leaves the socket open, so
  the gate destroys every socket whose handshake fails or times out. Without that, a client that
  stopped after its ClientHello kept its socket and a file descriptor forever (on the https
  server even after sending its FIN, in CLOSE_WAIT), and could still complete the handshake
  later, outside the gate's accounting.
* On the listener that carries the WebSocket upgrade, the gate also sheds load while the server is
  full. `Router.isFull()` is true while the primary's last answer to `conn.ipAcquire` was a
  server-full refusal (`global`: `MAX_CONNECTIONS` plus its reserve of max(16, 2 %)) less than 5 s
  ago (an admitted upgrade ends it), or while the worker holds `ceil(MAX_CONNECTIONS * 1.2 /
  WORKERS)` WebSocket connections; its times come from the monotonic clock (`performance.now()`),
  as do the gate's, so a step of the wall clock neither prolongs nor shortens the state. New
  connections then pass at `MAX_PENDING_HANDSHAKES / 2` per second (token bucket, one second of
  burst; `server_full`) and the others are closed before TLS. What passes reaches the API or the
  upgrade, where `conn.ipAcquire` (then the `MAX_CONNECTIONS` check at Hello) stays exact and
  refreshes the signal; a slot freed by a leaving player is found by the next upgrade that passes.
  The HTTP 503 of that check, or `ServerFull` at Hello, is the only way a client learns that the
  server is full: `GET /info` has no such field, and a client closed before TLS sees a network
  error and retries with its short backoff. Only WebSocket connections are counted, never API
  requests.
* Players first at `MAX_CONNECTIONS`. A `ServerFull` at Hello does not feed `isFull()`, and an
  upgrade admitted inside the reserve ends the state like any other. So a server at exactly
  `MAX_CONNECTIONS` does not shed: each newcomer completes the TLS handshake and the upgrade, gets
  `ServerFull` at Hello (close 4006), and the official game then waits 60 to 120 s. Shedding there
  would make a player coming back to a game in progress (admitted at Hello whatever the count)
  compete with the newcomers for the `MAX_PENDING_HANDSHAKES / 2` connections let through per
  second. The price is one handshake, one upgrade and one Hello per attempt of a newcomer, and no
  server-wide rate on the attempts of clients that ignore the backoff (the per-address caps still
  apply) until the connections counted at the upgrade reach the reserve's bound. The metric of a
  full server is therefore `scacelith_ws_hello_total{result="server_full"}`. The 503 of the
  upgrade (`scacelith_ws_handshakes_rejected_total{reason="server_full"}`) only comes once the
  reserve is in use as well, and the shedding (`scacelith_tls_refused_total{reason="server_full"}`)
  only after such a 503 or while a worker holds 1.2 times its share.
  `test/integration/admission.test.js` checks this on a real server.
* Compromise: before TLS the gate cannot tell an API request from an upgrade on the shared port
  (both are inside the TLS stream), so while the gate sheds, new API connections are let
  through at the rate above too, together with the upgrades: the API keeps working for players
  already logged in, more slowly (connections already open are past the gate and not affected).
  With `WS_PORT != API_PORT` the API listener is never shed, the better layout for a server that
  expects to be full.
* What the gate does not stop. Its caps keep a few hosts from blocking everyone; an attacker with
  many address groups can still fill them, on every worker. With the defaults, replaying a
  captured ClientHello and then staying silent holds a slot for the 10 s of the handshake timeout,
  at the cost of one ServerHello for the server (the attacker does no cryptography): 32 address
  groups opening about 13 such connections per second per worker keep every slot full, and every
  new TLS connection to that worker is refused until the attack stops, the API included on the
  shared port. The waiting room needs more (128 groups and about 700 silent connections per
  second per worker). Players already connected are not affected. Raising
  `MAX_PENDING_HANDSHAKES` raises the connection rate such an attack needs in proportion, and a
  lower `MAX_PENDING_HANDSHAKES_PER_IP` raises the number of address groups it needs. Stage 0
  does not change that: every timed-out handshake counts toward a block of its address, but each
  group needs only about 0.4 of them per second per worker (inferred: 128 slots ÷ 10 s ÷ 32
  groups), 50 a minute on two workers, far below `ABUSE_BLOCK_REFUSALS_PER_MIN`; it stops the
  same attack from a few addresses, which their per-address refusals soon block. Stopping it
  belongs in front of the server (a per-source connection rate limit in the firewall, or a
  filtering provider: SIZING, provider firewall).
* `TLS_MODE=proxy` and `off` have no gate: the TLS work (and its limits) belongs to the proxy.
  Behind a proxy the per-address connection limits are the proxy's job too; the request budget,
  the in-flight cap and the blocks still apply per `X-Forwarded-For` client (section 8).

### 5.9 HTTP (`src/http/`)

`createApiHandler({ config, store, auth, primary, anticheat, log, guard }) -> (req, res)`, mounted
by the net owner on the HTTPS server behind `wrapApiHandler` (and handling `upgrade` elsewhere);
`guard` is the shard's `IpGuard` (section 8), the same one the listeners use, which the wrapper
applies to every request before the handler sees it; `handler.close()` (awaited
first by the worker's shutdown) runs what the routes registered through `deps.onClose`, today the
GIF rendering threads. Router:
```js
router.get(path, handler, { auth: 'none'|'optional'|'required', rate: { key, limit, windowMs, shared?, by?: 'user', prefixLimit?, abuseWeight? } | [rates] })
router.post(path, handler, { auth, body: schemaObject, rate, timeoutMs?, bodyLimit?, ownBodyValidation? })   // JSON only, HTTP_BODY_LIMIT
router.page(method, path, handler, opts)                                    // HTML page outside /api
// handler(ctx) -> { status, body, headers } (JSON) | { status, html, headers } (page)
//               | { status, text, contentType, headers } (a text file: the PGN download)
//               | { status, bytes, contentType, headers } (a binary file: the GIF), each with refundRate?: true
// ctx = { req, ip, params, query, body, user, session, config, store, log, primary, takeRates(rates) }
```
Rates: `by: 'user'` counts per account, and per client for a request without a session;
`prefixLimit` also counts each IPv6 /48 as a whole; `shared` adds the primary's sliding window
(`ratelimit.take`, given back by `ratelimit.refund`); `abuseWeight` marks the auth family for the
network shield: a refusal by a rate counted per client (not per account) is reported to the
`guard` (`IpGuard.noteRefusal`) with that weight, or 1, toward a block of the address. The rates of a route are checked in order, and when one refuses, the tokens the
earlier ones took are given back (local bucket and primary). `ctx.takeRates(rates)` takes more
rates from inside a handler (the GIF render quotas, only on a cache miss): a 429 if one refuses,
and they are given back with the route's when the answer carries `refundRate: true`. Before the
route's rates, every request authenticated with a session spends one token of the account's
budget (`USER_RATE_PER_MIN`, per worker: max(1, min(L, ceil(2L / WORKERS))) per minute, a burst
of half a minute, local only; never counted toward a block). A route may raise its body limit
(`bodyLimit`) and its handler timeout (`timeoutMs`: the export 60 s, the GIFs 45 s). The send
deadline is the listener's (section 8): an answer not flushed 60 s after the handler ended it is
destroyed with its socket, and the handler's own time does not count toward it.
The complete reference of every endpoint, with its request, answers, errors, rate limits and curl
examples, is [API.md](API.md); the table below is the contract summary.
Path parameters use `:name`. JSON errors are `{ "error": "<snake_case_code>", "message": "...",
"retryAfter"?: s }` (with a `Retry-After` header when `retryAfter` is set). An endpoint that
hashes or checks a password may answer 503 `server_busy` when the worker's password hash queue
is full, or 429 `rate_limited` when the queue is at least half full and the client already has
`PASSWORD_HASH_WAITERS_PER_SOURCE` hashes waiting there (section 8); that 429 gives the tokens the
request took from the route's rate limits back.
Every response carries `Cache-Control: no-store`, `X-Content-Type-Options: nosniff`,
`Referrer-Policy: no-referrer`, `Strict-Transport-Security` (native TLS), and HTML pages a
strict `Content-Security-Policy`. Route modules export `register(router, deps)`.

Endpoints (prefix `/api/v1`):

| Method and path | Owner | Notes |
|---|---|---|
| `GET /info` | auth | `{ name, serverId, motd, protocol: {min, max, schema, subprotocol}, wsPort, wsPath: '/ws', registration, emailVerification, sso: { google }, mfa: true, pow: { register }, categories: [{id, baseSec, incSec}], limits }` |
| `POST /auth/register` | auth | `{ username, email, password, pow? }` -> 202 `{ status: 'verification_sent' }` (same answer when the e-mail exists); without `REQUIRE_EMAIL_VERIFICATION`: 201 `{ status: 'ready' }`, or 409 `email_taken`. Rates `auth`, then `auth_register` (`AUTH_REGISTER_PER_HOUR` 10 per hour per client, 3x per /48, shared) |
| `POST /auth/login` | auth | `{ login, password, clientLabel?, pow? }` -> `{ token, expiresAt, user }` or `{ mfaRequired: true, mfaToken }` |
| `POST /auth/login/mfa` | auth | `{ mfaToken, code? , recoveryCode? }` (401 `invalid_mfa_token` once the step expired or was used, or when the password was reset or changed since the first step). Every code checked here or in a re-authentication first takes `mfa:u<id>` from the primary (`AUTH_MFA_PER_ACCOUNT` 10 per 15 minutes per account, any address): beyond it 429 `too_many_attempts` before the check, a recovery code not spent |
| `POST /auth/logout`, `POST /auth/logout-all` | auth | bearer |
| `GET /auth/sessions`, `DELETE /auth/sessions/:id` | auth | |
| `POST /auth/verify-email/resend` | auth | `{ email }` -> 202 always. Rates `auth`, `auth_mail` (`AUTH_MAIL_PER_HOUR` 10 per hour per client, 3x per /48, shared) |
| `POST /auth/password/forgot` | auth | `{ email }` -> 202 always (a refusal is a 429, which says nothing about the address). Rates `auth`, `auth_forgot` (`AUTH_FORGOT_PER_HOUR` 3 per hour) and `auth_forgot_day` (`AUTH_FORGOT_PER_DAY` 10 per 24 h), per client, 3x per /48, shared; plus one mail per address every 5 minutes |
| `POST /auth/password/reset` | auth | `{ token, newPassword }` (also the HTML form at `/reset-password`). Rates `auth`, `auth_reset` (`AUTH_RESET_PER_HOUR` 10 per hour per client, 3x per /48, shared, the form included) |
| `POST /auth/sso/google/start` | auth | `{ codeChallenge }` -> `{ attemptId, authUrl, pollMs, expiresIn }`. Rate `sso_start` 30 per 10 min per client, 90 per /48, shared |
| `POST /auth/sso/google/poll` | auth | `{ attemptId, codeVerifier }` -> `{ status: 'pending' }` / login answer / `{ needsUsername, ssoTicket }` |
| `POST /auth/sso/complete` | auth | `{ ssoTicket, username }`. Rate `auth`, its /48 count included |
| `GET /account/me` | auth | `{ user, ratings, sanctions (active), ban }`; `user`: `{ id, username, email, emailVerified, mfaEnabled, googleLinked, hasPassword, acceptChallenges: 'all'\|'none', createdAt, lastLoginAt, pendingEmail }` (`pendingEmail`: the address of an e-mail change waiting for its link, or null; the login answers carry the same `user`); the integrity level is NOT exposed |
| `POST /account/password` | auth | `{ currentPassword, newPassword }`: the current password only, never a second factor (by design, even with MFA on); revokes the other sessions |
| `POST /account/mfa/totp/setup` | auth | `{ password }` -> `{ secret, uri, algorithm, digits, period }` (409 when MFA is already on) |
| `POST /account/mfa/totp/enable` | auth | `{ code }`: a code of the secret from setup only, no password -> `{ status, recoveryCodes }` |
| `POST /account/mfa/totp/disable`, `POST /account/delete` | auth | `{ password, code?, recoveryCode? }`: the password, plus a TOTP code or a recovery code when MFA is on (disable requires MFA on) |
| `POST /account/mfa/recovery-codes` | auth | `{ password, code }`: the password and a TOTP code (a recovery code is refused) -> `{ recoveryCodes }` |
| (every re-authenticated route above, `/account/email`, `/account/export`) | auth | rates `reauth` (per client and /48, shared) and `reauth_user` (`AUTH_REAUTH_PER_USER` 10 per 10 min per account, shared): `reauthRatesOf(config)` of `routes/account.js` |
| `PUT /account/preferences` | auth | `{ acceptChallenges: 'all'\|'none' }`: the session only, no re-authentication |
| `POST /account/email` | auth | `{ newEmail, password, code?, recoveryCode? }`, the re-authentication of delete. With `REQUIRE_EMAIL_VERIFICATION`: 202 `{ status: 'verification_sent' }`, the same answer and the same `pendingEmail` whether or not another account uses the address; a 24 h link to the new address (`/confirm-email-change`), a notice to the current one. Without it: 200 `{ status: 'email_changed', email }` or 409 `email_taken`. 400 `invalid_email` / `same_email` before the password check |
| `POST /account/export` | auth | `{ password, code?, recoveryCode? }` -> 200 JSON attachment `scacelith-account-<username>.json` (`format: 'scacelith-account-export'`, version 1: account, ratings, refunds, sessions, security events, sanctions, conduct, reports filed, every game summary, notes); never a secret, token hash, anti-cheat data, report received or moderator identity. Rate `account_export` (5 per hour per player, shared) then `reauth`, `reauth_user`; 60 s timeout |
| `GET /account/games` | store | `?before=<id>&limit<=50&category=<id>\|custom&rated=true\|false&result=win\|loss\|draw` -> `{ games: [summary + baseMs, incMs, outcome], next, total }`, the player's own games newest first (`outcome` from the player's side; aborted games only without a result filter); rate `account_games` 60 per minute per player |
| `GET /players/:username`, `GET /players/:username/games`, `GET /leaderboard?category=` | store | public data only; the two player routes take an optional session (same answer) so that their `public_read` limit counts per account when there is one; the leaderboard has no limit of its own (the per-address layer only, section 8) |
| `GET /games/:id`, `GET /games/:id/pgn` | store | public data, optional session: a player of the game also gets `you: 'white'\|'black'` and `reportable` (whether `POST /reports` would take a report of the opponent now). The PGN: `application/x-chess-pgn` attachment `scacelith-<id>.pgn`, standard tags plus `ScacelithGameId`, `[%clk]` / `[%emt]` per move. Both share the `public_read` limit (60 per minute, `by: 'user'`) with the two player routes |
| `GET /games/:id/gif?size&orientation&delay&coords` | gif | session required (quotas per account). `small\|medium\|large` (default medium), `white\|black`, delay 100-3000 ms (500), coords `0\|1` (1); else 400 `invalid_option {field}`. The game record's names, ratings, result and ending -> 200 `image/gif` attachment `scacelith-<id>.gif` (binary answer). 400 `invalid_game_id`, 404 `not_found`, 422 `game_too_long` (> `GIF_MAX_PLIES` 600), 429, 503 `server_busy {retryAfter 3-10}` (queue full or wait over; render quotas given back), 500 `render_failed`, 404 `gif_disabled` |
| `POST /gif` | gif | `{ pgn (<= 65536 bytes), size?, orientation?, delayMs? (number), coords? (boolean) }`: the first game of the PGN (`src/chess/pgn.js` `readPgn`), names and ratings from its tags (printable ASCII), result from `Result`, ending from `Termination` unless `normal` -> attachment `scacelith-game.gif`; 400 `invalid_pgn {line, column}`; body limit 2 x 65536 + 4096; other answers as GET |
| (both GIF routes) | gif | `routes/gif.js`: rate `gif` 30 per min per account on every call; on a cache miss only, `ctx.takeRates` of `gif_user_min` / `gif_user_hour` (`GIF_USER_RENDERS_PER_MIN` 4, `_PER_HOUR` 30, per account) and `gif_ip_min` / `gif_ip_hour` (`GIF_IP_RENDERS_PER_MIN` 12, `_PER_HOUR` 120, per client, 3x per /48), all shared. Render on `src/gif/pool.js` threads (`GIF_THREADS` 1 per worker, started from `routes/gif-thread.js` at nice 19 on Linux, created on the first render, `GIF_QUEUE_MAX` 4 waiting up to `GIF_QUEUE_TIMEOUT_MS` 10 s, `GIF_RENDER_TIMEOUT_MS` 30 s, closed by `handler.close()`), never on the event loop; LRU cache per worker (`GIF_CACHE_MB` 32) keyed by a hash of the job (moves, start, names, ratings, result, ending, options), in-flight renders joined; metrics `scacelith_gif_renders_total{result}`, `scacelith_gif_render_duration_ms`, `scacelith_gif_cache_total{result}`, `scacelith_gif_queue`, `scacelith_gif_renders_running`, `scacelith_gif_cache_bytes` |
| `POST /reports` | anticheat | `{ gameId, reported, category: 'cheating'|'abuse'|'other', comment }`. Rate `reports` 30 per hour per account (`by: 'user'`) |
| `GET /healthz`, `GET /readyz` | net | also on the metrics port |

HTML pages outside `/api`: `GET/POST /verify-email?token=`, `GET/POST /reset-password?token=`,
`GET/POST /confirm-email-change?token=` (POST: 200, 400 for an invalid link, 409 when another
account took the address meanwhile), `GET /auth/sso/google/callback` (auth owner). GET only shows
a confirmation button; the state change happens on POST (link scanners must not consume tokens).

## 6. Game policies

### 6.1 Clocks (server authoritative)

* Time is measured on the server only. The client's `thinkMs` never adds time; it only bounds
  lag compensation.
* Plies 0 and 1 (each side's first move) do not run the clock: each player has
  `FIRST_MOVE_TIMEOUT_MS` to make it, otherwise the game is aborted (`NoShow`, unrated). Its
  deadline has the same margin as a flag, `min(quota, rttEma + 50, LAG_COMP_MAX_MS)`, so that a
  first move sent in time over a slow link counts; the `firstMoveMs` sent to the clients has no
  margin. No increment is added for them. The clocks start with White's second move.
* For every later move: `elapsed = recvTime - turnStart` where `turnStart` is when the server
  sent the opponent's `MoveMade`. `lag = elapsed - clamp(thinkMs, 0, elapsed)`.
  `comp = min(lag, rttEma + 50, LAG_COMP_MAX_MS, quota)`; `quota -= comp`, then
  `quota = min(quota + LAG_QUOTA_GAIN_MS, LAG_QUOTA_MAX_MS)`. `charged = elapsed - comp`.
  If `remaining - charged <= 0` the player flagged: the move is refused (`MoveRejected{FlagFell}`)
  and the game ends on time (draw if the opponent cannot mate). Otherwise `remaining -= charged`,
  then `+= incMs`.
* The flag timer fires at `turnStart + remaining + min(quota, rttEma + 50, LAG_COMP_MAX_MS)`, the
  latest moment a move could still arrive in time.
* `thinkMs > elapsed + 100` is physically impossible for an honest client: anomaly
  `clock_implausible` (suspicious, never certain: clock drift and suspended VMs exist). The
  client counts its thinking time from the opponent's `MoveMade`, so after a server restart
  (6.4), which moves `turnStart` later, only a `thinkMs` longer than the time since the previous
  move (plus 100 ms) is reported.
* `rttEma` is the server's own measurement (its Ping / the client's Pong), exponential moving
  average, capped at 2000 ms. A client that delays its Pongs only inflates a value that is
  itself capped by the quota. A Pong whose Ping preceded a stall of the worker (below) is left
  out: the stall delayed its reading, not the network. `Welcome`, `Ping` and `Pong` carry the
  same clock as the games (`clock.js now()`), `Welcome`'s read after the session check.
* Stall credit (`GAME_STALL_MIN_MS`, 30; `GAME_STALL_CREDIT_MAX_MS`, 5000). A worker's event loop
  can stop for a moment (garbage collection, a synchronous SQLite write, CPU steal), and Node runs
  its timers before it reads its sockets: without care, a flag that fell during the stall would
  fire before the move that arrived in time and waited in a socket. Each run of the host's 10 ms
  interval is a beat; a beat more than `10 + GAME_STALL_MIN_MS` ms after the previous one is a
  stall (`scacelith_game_stall_ms`), and the timers, commits and compaction of that beat then run
  from `setImmediate`, after the poll phase has read the sockets. Those timers fire the deadlines
  due by that beat only: the poll phase reads what the sockets held when it began, and when it
  lasts (the backlog of a large stall), what reaches a socket meanwhile waits for the next poll
  phase; a later deadline therefore waits for the next beat, which sees that poll phase as a stall
  of its own. Until the timers ran, and while a beat is that late before the interval noticed it,
  a game request (Move, Resign, DrawOffer, DrawAnswer, DrawClaim, Abort, Resync, Rematch), an
  attach, a detach or a forfeit (`game.forfeit`) counts as arrived when the stall began, at most
  `GAME_STALL_CREDIT_MAX_MS` before it is handled (`scacelith_game_stall_credit_ms_total`); the
  forfeit of a certain cheat (6.5) takes the arrival of the request that revealed it. The room
  checks the deadlines, the flag and the time charged for a move at that arrival (never before the
  latest move, never after now), a resignation, draw, abort or forfeit takes effect at it, and a
  disconnection, reconnection or Resync processes the deadlines due at it only; the next turn starts
  at the real time of the `MoveMade`, so the stall is charged to nobody and uses no quota, and the
  implausible-`thinkMs` test keeps the real time since the previous move. The room also gets the
  stall's start, from the timers that run after the stall (`room.tick`) and with each of those
  requests: a first-move timeout that fell during the stall aborts the game without a `noshow`
  conduct incident, whichever processes it (a request does when the stall lasted longer than the
  credit). Nothing of it is journaled (the journal holds the clock values it produced, and a replay
  uses them), and a player gains at most one stall; no client can cause one.
  `scacelith_game_timer_late_ms` measures how late the timers fire. The credit covers stalls of the
  game's host worker only: a move relayed over the bus from another shard that stalled is timed when
  the host reads it.

### 6.2 Validation of a Move intent (in this order)

1. Not a participant of `game` -> `NotInGame` (certain: `foreign_game`).
2. Game over -> `GameOver` (info).
3. `ply < current` : duplicate of a move already played -> if identical, reply the original
   `MoveMade` again (idempotent), else `StalePly` (info).
4. `posHash != digest(current position)` -> `Desync` + `GameSnapshot` (info; 3+ in one game:
   suspicious `repeated_desync`).
5. `ply > current` with matching hash cannot happen; treat as `Desync`.
6. Not the sender's turn (hash matches, so the client knew) -> `NotYourTurn` (certain:
   `out_of_turn`).
7. Illegal move in the matching position -> `IllegalMove` (certain: `illegal_move`).
8. Clock check (6.1), then play. A pending draw offer from the opponent is declined by the move.

A rejected move is never sent to the opponent. The game client validates moves with the same
rules before sending, so steps 6 and 7 only happen with a modified client.

### 6.3 Draws, resignation, abort, rematch

* One pending draw offer per game; it stands until the opponent answers or moves (decline).
  At most `DRAW_OFFERS_PER_GAME` per player, and not again until 10 plies after a decline
  (`DrawOfferLimit`). An offer can come with a move (`Move.drawOffer`, FIDE 9.1.2) or alone.
* `DrawClaim`: accepted when the current position occurred 3 times or the halfmove clock is at
  least 100; otherwise `NothingToClaim`. Automatic: mate, stalemate, insufficient material,
  fivefold, 75 moves (as in the game).
* `Resign` at any time while the game runs. `Abort` only before the sender's own first move
  (conduct counter `abort`).
* `Rematch` within 60 s after the end: both accept -> the primary creates a new game (colours
  swapped, same time control and rated flag, conduct and bans checked). It expires when either
  player leaves (disconnects or joins a queue).

### 6.4 Disconnections, abandonment, rage quit, server restart

* Grace = clamp(baseMs / 10, RECONNECT_GRACE_MIN_MS, RECONNECT_GRACE_MAX_MS). The disconnected
  player's clock keeps running when it is their turn (they can lose on time before the grace
  ends).
* Grace expired: fewer than 2 plies -> aborted (`NoShow`); otherwise the absent player loses by
  `Abandonment` (`AbandonmentVsInsufficient` draw when the opponent cannot mate). Conduct
  counter `abandon`.
* Both players disconnected: if they left within 5 s of each other (a network or server event),
  the game waits for the longer grace, then is aborted (`BothDisconnected`, unrated); otherwise
  the first to leave is the one who abandoned.
* Leaving a running game from the menu = resignation (as in the offline game).
* Conduct: `CONDUCT_ABANDON_LIMIT` abandon + abort + no-show in 24 h pauses rated matchmaking for
  15 min, then 1 h, then 6 h (level decays after 24 h without incident). Direct challenges stay
  possible.
* Server restart: games are replayed from the journal (section 7) with both players marked
  disconnected; the downtime is not charged to anyone. Both players get the recovery grace, `RECOVERY_GRACE_MS` (90 s,
  or the normal grace when that is longer), instead of the normal grace: the server broke the
  connections, and every client reconnects at the same moment (after a `ServerShutdown` notice
  the clients spread their first attempt over several seconds). The grace starts at the replay,
  before the shard listens, and is written in the `recovered` journal record. The disconnections
  journaled by the drain before a graceful stop are superseded by the recovery, so they do not
  shorten it. A player who comes back and then loses the connection again gets the normal grace;
  the deadlines above use each player's own grace (with equal graces they are the rules above).
  The clock of the side to move (or its first-move timer before the second ply) keeps its
  journaled value and stays stopped until that player is back: it starts at the reconnection, or
  `RECOVERY_CLOCK_HOLD_MS` (20 s, lower than the recovery grace) after the replay when the player
  is still away, so that staying away on purpose gives little free thinking time. Until then the
  snapshots show no running clock; when the clock starts (its player is back or the hold ends), the opponent gets a
  new `GameSnapshot`. Both the hold and its end are journaled (the `recovered` record carries the
  hold, and a checkpoint marks its end), so a later replay rebuilds the same clocks. Once the
  clock runs, a side to move with less time left than its reconnection delay can still lose on
  time. Before the second ply, the first reconnection of a player after the restart also
  restarts that player's first-move timer when it is its turn, even after the hold ended or
  after the opponent's first move: a player who is back late but before its first-move time ran
  out (for example 48 s after the replay, when 2 s of it were left) gets the whole
  `FIRST_MOVE_TIMEOUT_MS` from its reconnection. Only the first reconnection after a restart
  counts, so leaving and coming back again gives no more time. The players not back yet are
  journaled (the `recovered` record asks for this restart, and every checkpoint carries it), so a
  replay, a compaction snapshot and a second restart rebuild it; a journal of an older build,
  which carries neither, replays without the restart. A restored game aborted `NoShow` because
  its side to move never came back before the end of the first-move timer records no `noshow`
  conduct incident (the server broke the connection); it is still aborted, unrated. A player who
  came back and then does not move within that whole first-move time gets the incident as
  usual. A first-move timeout that fell during a stall of the host's worker (6.1) records no
  incident either. Games that cannot be rebuilt end as `ServerAborted` (unrated) and are committed as such.

### 6.5 Anomalies, certain cheats, suspicion

| kind | severity | when |
|---|---|---|
| `malformed` | suspicious | undecodable frame after Hello (connection closed 4300) |
| `forged_type` | certain | a server->client message type sent by a client |
| `bad_seq` | suspicious | seq not last+1 |
| `flood` | suspicious | rate limit exceeded repeatedly (connection closed 4301) |
| `foreign_game` | certain | game message for a game the player does not play |
| `out_of_turn` | certain | move in a synchronised position while it is not the player's turn |
| `illegal_move` | certain | illegal move in a synchronised position |
| `repeated_desync` | suspicious | 3+ desyncs in one game |
| `clock_implausible` | suspicious | thinkMs impossible (6.1) |
| `stale_ply`, `desync`, `nothing_to_claim` | info | honest races and bugs |

Certain cheat with `AUTO_SANCTION_CERTAIN_CHEATS=true`: the game (if any) ends
`Forfeit` (a rated game is rated normally: the opponent wins), `Error{CheatDetected, fatal}`,
close 4302, ban `BAN_DURATION_HOURS` (source `auto`), integrity level `confirmed` with the
evidence, and the rating refunds of the player's victims (section 6.6). Everything else is only
recorded.

Statistical assistance detection never bans automatically. Per player and category the
analysis process accumulates, over rated games of at least `ANALYSIS_MIN_PLIES`: accuracy
(win-probability loss per move, robust to won positions), average centipawn loss (capped),
agreement with the deep and the shallow engine choice (engine correlation at two depths),
agreement restricted to *complex* positions (several plausible moves, no forced reply), think
time against position complexity (humans think longer in hard positions), variance of think
times, and performance against the player's own history (sudden lasting jump). Scores are
z-scores against the server's population at the same rating, shrunk towards 0 for small samples
(Bayesian). Levels: `suspected` (one strong signal over >= 5 games), `high_confidence` (several
independent signals agree over >= 10 games and >= 300 non-trivial moves), `confirmed` (moderator
decision via `bin/admin.js`, or a certain protocol cheat). Reports raise the review priority,
weighted by the reporter's credibility, never the level itself.

**Analysis backlog.** One engine analyses about 540 to 1,260 games a day on a VPS vCore (Stockfish
19 at depths 9/15, MultiPV 3, from ply 16 on), far fewer than a busy server finishes, so the queue
is bounded and prioritized instead of growing without end. Every job has a priority and the engine
takes the highest first, then the oldest:

1. `manual`: a moderator asked for the (re-)analysis of a game (`store.analysis.enqueue`);
2. `report`: a credible player reported the game (category `cheating` or `other`, stored weight
   at least 0.5); the report queues it even if the policy had left it out, and re-queues it if
   its analysis had failed. A report of lower weight (a new account, or one beyond the daily cap
   of report weight a player receives) asks for `signal` priority only, so that such reports
   cannot push the games of statistically suspected players back;
3. `signal`: a suspicion signal at the end of the game: the integrity level of either player is
   above `none`, either player has an open `cheating` or `other` report from the last 30 days
   by a credible reporter (stored weight at least 0.5, so sock puppets never flag anyone), or a
   `suspicious` or `certain` anomaly was recorded in this game;
4. `ordinary`: every other rated game of an official category with at least
   `ANALYSIS_MIN_PLIES` plies.

Games of the first three kinds are queued whatever the backlog, with one bound: at most 20
`signal` jobs of one player wait at a time. The scoring reads a player's 30 latest analysed
games, so these 20 renew most of that window, and one prolific flagged player cannot grow the
tier without end. When one of its players already has 20 waiting (as white or black), a game
flagged only through its players (integrity level or report) is not queued, and a
low-credibility report does not raise a game past that bound either. A game with a
`suspicious` or `certain` anomaly of its own is still queued: in the same transaction it takes
the place of the oldest waiting job of that player whose game has no such anomaly (one for each
capped player, or one job for both when it is one of their games), so the count stays at 20.
It is skipped only when every job the player has waiting carries an anomaly of its own. A
player who fills the bound with quick games (flagged only because the player is) therefore
cannot keep a game with evidence out of the analysis, while the games with evidence are never
displaced by any other game. An ordinary game is queued with probability
`ANALYSIS_SAMPLE_RATE` (default 1), and only while fewer than `ANALYSIS_QUEUE_MAX` (default
5000, at most 100,000) ordinary jobs wait. A game left out gets no job at all and is counted in
`scacelith_anticheat_analysis_skipped_total{reason="sample"|"backlog"|"player"}`, and a waiting
job removed for a game with evidence is counted there with `reason="displaced"`.

The highest priority is not taken every time: every fourth claim of the analysis process takes
the oldest ordinary job first when one waits. Ordinary games, the random sample of the rated
games and the only ones that feed the population statistics the players are compared with
(docs/ANTICHEAT.md 4), therefore keep at least a quarter of the engine time however many
prioritized games arrive. An ordinary game waits at most about `ANALYSIS_QUEUE_MAX` divided by
that quarter of the engine throughput (4 x `ANALYSIS_QUEUE_MAX` / throughput), and a suspicious
game waits behind at most one ordinary game in four. `scacelith_anticheat_analysis_queue_ordinary`
and `scacelith_anticheat_analysis_queue_priority` show the backlog (each counted up to 100,000).
The policy only decides what is analysed and when: it changes no level and no sanction, and
statistics never ban.

### 6.6 Ratings

One Elo rating per player and official category (`3+2`, ...; a custom time control is never
rated), computed as FIDE computes ratings (FIDE Rating Regulations effective from 1 March 2024)
by `src/match/elo.js`, which mirrors the game's offline rating (`src/game/elo.cpp`) to the last
point: both are checked against the same vectors (`test/fixtures/elo-vectors.json`).

* **Expected score**: FIDE's table 8.1.2 as published (`PD` for the rating difference `D`, the
  higher-rated player gets `PD`, the lower `1 - PD`), `D` counted as 400 at most (8.3.1). The
  table is the normal distribution with a standard deviation of 2000/7 rounded to hundredths,
  except at six differences where FIDE's rows differ (54, 343, 344, 358, 392, 620); the
  published table is what FIDE applies, so it is used as published.
* **Change**: `K x (score - PD)`, rounded to the nearest point (halves away from zero),
  computed in whole hundredths. `K` is 40 until the player has `PROVISIONAL_GAMES` (30) counted
  games in the category (below), those of the unrated phase included, then 20, and 10 for good
  once the player has reached 2400.
* **Unrated phase and first rating** (8.2): a new record is unrated, with a working rating of
  `INITIAL_RATING` (1500) that is shown and used for pairing. Its counted games add up the
  opponents' ratings and the score (see the zero score below); after five, `Ru = Ra + dp(p)` with
  `Ra = (sum of the opponents' ratings + 2 x 1800) / (n + 2)` and
  `p = (score + 1) / (n + 2)` rounded to hundredths (two hypothetical draws against
  1800-rated players), `dp` from FIDE's table 8.1.1, rounded to the nearest point and capped at
  2200. The peak becomes `Ru`. Five draws against 1500 give 1586, five wins 1895.
* **Zero score** (8.2.1: FIDE disregards an unrated player's zero score, and their opponents'
  results against them), one game at a time: a game lost by an unrated player who has not scored
  yet in the category (no win, no draw) counts for neither player's rating (it counts in the
  games and wins, draws and losses, not in the five games, the sum or the score of either side).
  A newcomer who only loses stays unrated instead of getting a first rating from the strength of
  the players who beat them, and gives no first rating to anyone: an account that only loses
  cannot rate the accounts that beat it (five wins against it would otherwise give 1895, over
  and over). Once it has scored, its losses count and it is rated within five counted games,
  then loses points like anyone.
* **Unrated opponent**: a rated player's game against an unrated one leaves the rated player's
  rating unchanged (only games against rated opponents count, 8.3) but counts in their games and
  wins, draws and losses, not in their counted games.
* **Counted games**: the games that entered the rating (FIDE's rated games), those of the
  unrated phase that counted and then those against rated opponents. They set `K` and the
  provisional mark, and a place on the leaderboard needs `PROVISIONAL_GAMES` of them: a rating
  tested against rated players, which games against unrated opponents or zero scores never give.
* **Floor**: a rating never drops below 100 (FIDE's list starts at 1400, which makes no sense
  for players who range from beginners to weak engine presets rated 800).

Departures from FIDE, needed by a game server: games are rated one by one against the ratings
before each game (FIDE rates monthly periods with ratings fixed within the period and `K x
games` capped at 700); a game between two unrated players counts for both, at the other's
working rating (FIDE ignores it, but a new server could then never rate anyone), unless it is a
zero score; FIDE's `K = 40` for players under 18 does not apply (no ages here).

On the wire nothing changed: an unrated record is `provisional` (so is a rated one with fewer
than `PROVISIONAL_GAMES` counted games), its `rating` is the working rating, `games` counts every
rated game played, and the `RatingChange` of a game of the unrated phase has `before = after`,
except the fifth counted game, which goes from the working rating to the first rating. The
records stored before migration 004 (the previous logistic formula, same K factors, start and
floor) that have games stay rated, and a record stored before the counted games existed counts
all its games; a record without games starts unrated. The leaderboard lists rated records with
at least `PROVISIONAL_GAMES` counted games.

**Refunds.** When a player is banned as a cheater (a certain cheat, section 6.5, or a
moderator's `integrity confirm`), every opponent who lost rating points to them in a rated game
that ended within `RATING_REFUND_DAYS` (60) before the ban gets exactly those points back, added
to their current rating in that category (their peak rises with it when it is exceeded).
Nothing is recomputed: wins against the cheater and every other game stand as played; a draw
that cost points is refunded like a loss; only a change of the K formula is refunded (the game
that gave a first rating moved it from a working rating); one refund per game and victim at
most. A game that is not recorded yet when the ban is given (still in progress, or waiting for
its commit) is refunded in the transaction that records it, while the player is `confirmed` and
that ban lasts. A ban for something else (`user ban`) and an `integrity confirm --no-refund`
refund nothing, not even that game: the store tells them by the ban's source and reason
(`certain_cheat:<kind>` from the anti-cheat; `confirmed: <reason>`, or
`confirmed, no refund: <reason>`, from the confirm). A ban given with the admin CLI, which only
writes the database, reaches the running server when the player next connects or tries to start
a game (queue, challenge, private code, rematch): the primary then enforces it as it enforces
`sanction.applied` (kick, queue and challenges dropped), so a banned player starts no game. The
victim gets `Notice{RatingRestored, arg: points}` out of a game only: at once when connected and
idle, otherwise after their current game, otherwise right after `Welcome` at their next
connection (after the game that connection resumes, if any). Moderator options, audit trail and
the reasons an unban takes nothing back: docs/ANTICHEAT.md, rating refunds.

## 7. What lives where, and what survives a crash

| Data | Where | Written | After a crash |
|---|---|---|---|
| Accounts, e-mail, password hashes, MFA secrets (encrypted), recovery-code hashes, SSO links | SQLite | immediately (transaction) | kept |
| Sessions | SQLite (+ 30 s cache per shard) | login/logout immediately; `lastSeen` at most every 5 min | kept (revocations immediate: cache invalidation broadcast) |
| Ratings, finished games, analysis queue | SQLite | batched every `DB_COMMIT_MS`, one transaction per batch | kept once committed; games ended but not committed are in the journal and committed at recovery |
| Games in progress (moves, clocks, offers) | memory of the host shard + journal | journal flushed every `JOURNAL_FLUSH_MS` | replayed; at most `JOURNAL_FLUSH_MS` of moves lost (clients resend: stale ply / resync); both players get `RECOVERY_GRACE_MS` to come back, and the clock of the side to move waits for its player (`RECOVERY_CLOCK_HOLD_MS` at most, 6.4) |
| Sanctions, anomalies (certain), integrity levels, reports | SQLite | sanctions immediately; anomalies batched (1 s) | kept (a batch in flight may be lost for `info` anomalies) |
| Rating refunds (6.6) | SQLite | with the ban that triggers them (their own transaction), or with a game recorded during the ban; `notified_at` once the notice is written | kept; the notices not marked are sent again after a restart |
| Presence, queues, challenges, private codes, rate-limit counters | primary memory | - | lost: clients reconnect and re-queue |
| Security events (failed logins...) | SQLite | batched (1 s) | kept, purged after `RETENTION_SECURITY_DAYS` |

**Retention purge.** Personal data is not kept longer than it is needed. The primary runs
`store.retention.runAsync` every `RETENTION_INTERVAL_MS` (default one hour; the first run about a
minute after the start):

| Data | Deleted or erased |
|---|---|
| Sessions | as soon as they expire (absolute or idle limit); revoked ones a day after the revocation; the IP address of a live session `RETENTION_IP_DAYS` (30) after the login, the row stays |
| Single-use tokens (some carry the e-mail address) | once expired (24 hours at most) |
| Security events | after `RETENTION_SECURITY_DAYS` (90); their IP address after `RETENTION_IP_DAYS`, the row stays |
| Anomalies | `info` and `suspicious` ones after `RETENTION_SECURITY_DAYS`; `certain` ones (the evidence of an automatic sanction) are kept |
| Conduct events | after 30 days (the conduct rules look back 3 days at most) |
| Analysis jobs | failed ones 30 days after the failure; done ones are kept (their features are each player's analysed history, read by the scoring and by `bin/admin.js integrity show` without a time limit); waiting and running jobs are never purged |

Accounts (anonymized when deleted), games, ratings, sanctions, reports and integrity records are
kept. The primary is the control plane, so the purge must not hold its event loop, nor the
database's write lock that the workers need for their own writes (sessions, security events,
reports, finished games). IP addresses are erased first, then the old rows deleted. Every
statement touches at most 1000 rows in its own short transaction: it starts at 200 rows and
adapts so that one statement takes about 5 ms (half of the 10 ms slice, never fewer than 50
rows), and once 10 ms of work have been done the run pauses for 10 ms. Measured on the
development container with a backlog of 100,000 expired sessions and 100,000 old security
events, all with IP addresses: purged in 11 to 12 s with the event loop busy 59% of the time
(delay p99 about 22 ms, at most about 50 ms when a statement ran SQLite's automatic WAL
checkpoint), while a thread committing one row every 20 ms waited about 20 ms at the 99th
percentile. In chunks of 1000 rows without pauses, the same purge took 3 s but kept the event
loop busy all the time and made that thread wait up to 180 to 330 ms. `RETENTION_INTERVAL_MS` is
at most 2147483647 (about 24.8 days), the longest delay of a Node.js timer. Runs never overlap.
On shutdown the run in progress is aborted between two statements and awaited before the
database is closed. Each run logs its counts at info level (numbers only, no personal data) and
feeds `scacelith_retention_purged_total{kind}`, `scacelith_retention_ip_erased_total`,
`scacelith_retention_runs_total{result}` and `scacelith_retention_run_seconds`.

**Erased data does not stay in the file.** Every writable connection runs with
`PRAGMA secure_delete = ON`: SQLite overwrites deleted rows and erased columns with zeros, freed
pages included, so an IP address, an e-mail address or a password hash does not stay readable in
the free space of the database file after the purge or `users.anonymize`. (`FAST` would leave
the content of freed pages, for instance a purge of whole pages of expired sessions.) No cost
was measurable on the purge or on the commits of finished games. Two limits remain. When SQLite
rebalances a table after a deletion it can leave stale copies of the rows it moved in the unused
part of a page, which `secure_delete` does not clear; the purge erases IP addresses before it
deletes rows so that the rows moved around them no longer hold one (a test with 80,000 erased or
deleted addresses found none left in the file). And the WAL file keeps the page images written
before, until it is overwritten as it is reused after each checkpoint (it is truncated to 64 MB,
`journal_size_limit`, and removed when the server stops). A backup made with
`bin/admin.js backup` (`VACUUM INTO`) holds neither.

## 8. Security model

* **TLS everywhere**: HTTPS API and WSS (native TLS or a TLS-terminating proxy with
  `TRUSTED_PROXIES`). HSTS on native TLS. Plain text only with `ALLOW_INSECURE_DEV`.
* **Per-server trust boundary** (client side, section 10): credentials, tokens and pinned
  certificates are stored per server origin (`host:apiPort`), and only ever sent to that
  origin. The WebSocket goes to the same host. Redirects are not followed with credentials.
* **Passwords**: scrypt (N=2^17, r=8, p=1, 64-byte key, 16-byte salt) via `crypto.scrypt`
  (libuv thread pool, never blocks the event loop), stored as a self-describing string
  (`scrypt$17$8$1$<salt>$<hash>`); argon2id is used instead when `crypto.argon2` exists (Node
  >= 24.7) and hashes are upgraded at the next login. Length >= PASSWORD_MIN_LENGTH, <= 256
  bytes, not containing the username, not in the embedded list of common passwords. Unknown
  login -> the same work on a dummy hash (constant-time behaviour); a stored argon2id hash on a
  runtime without argon2 costs the same dummy work. The dummy uses the preferred algorithm, while
  stored hashes keep theirs until the next password login, so every failed login check (known
  account or not) is also padded to the slowest check of the last 10 to 20 minutes of the worker,
  and never less than a baseline the worker measured at start (at most 2 s in all), with a timer
  that runs after the hash slot is released and uses no hash capacity. The start-up warm-up times
  one verification of the preferred algorithm and, when that is argon2id, one with the scrypt
  parameters, the slowest kind of hash the database can still hold; this baseline does not decay,
  so the first failure after a start or a quiet period is padded too (on Node 24.7+ a failed login
  therefore takes about the time of a scrypt check, 0.5 s). The time of a failure does not tell
  which e-mail addresses are registered.
* **Password hash cap**: one hash costs about 0.5-0.6 s of CPU and 64-128 MiB in the libuv thread
  pool, which the journal's fdatasync and the DNS lookups (SMTP, Google sign-in) share. Every
  hash and verification of a worker process (registration, login and its rehash, password change
  and reset, the re-authentication of account changes, the dummy verification of unknown
  accounts) goes through one bounded FIFO queue (`createHashLimiter` in
  `src/security/password.js`): `PASSWORD_HASH_CONCURRENCY` (1) run at once,
  `PASSWORD_HASH_QUEUE_MAX` (32) wait. Each request has one wait budget of
  `PASSWORD_HASH_QUEUE_TIMEOUT_MS` (10 s, at most 13 s so that the answer comes before the game's
  15 s HTTP timeout) for all its hashes together: a password change checks the current password
  and hashes the new one within it. A request beyond the queue, or whose budget ran out, is
  answered HTTP 503 `{ "error": "server_busy", "retryAfter": s }` with a `Retry-After` header (a
  random 5-15 s, so that the refused clients do not come back together) before anything changed:
  no failed login is counted and a reset link stays valid. Once the queue is at least half full,
  one client source (an IPv4 address or an IPv6 /48) may have at most
  `PASSWORD_HASH_WAITERS_PER_SOURCE` (2) hashes waiting; its next request is answered 429
  `rate_limited` the same way and gets back the tokens it took from the auth rate limits (the
  local bucket, and the primary's window through `ratelimit.refund`). So one network cannot hold
  more than half of the queue while staying under the per-address limits, and a classroom behind
  one IPv4 address still uses an idle queue in full. With one hash per worker, each game event loop keeps at least half a core
  during a login burst on a 2-core machine, and the thread pool (`UV_THREADPOOL_SIZE`, 4 by
  default; keep it at 4 or more, the primary warns at start when `PASSWORD_HASH_CONCURRENCY` is
  not below it) keeps free threads for the file system and DNS. Metrics:
  `scacelith_password_hash_in_flight`, `scacelith_password_hash_queued` (per shard),
  `scacelith_password_hash_wait_ms`, `scacelith_password_hash_rejected_total{reason}` (reasons
  `queue_full`, `timeout`, `source_limit`).
* **Password changes under concurrency**: the login upgrades an outdated hash only when a hash
  slot is free at once (it never waits a second time; the next login retries), and every new
  hash computed from a checked password (the rehash, a password change) is written with a
  compare-and-set on the hash that was checked (`svc.setPasswordHashIf` in `src/auth/index.js`,
  one store transaction). After a check, the login and the re-authentication read the account
  again: when the stored hash changed while the check waited or ran, the password is checked
  once more against the new hash. The MFA step of a password login keeps a SHA-256 digest of the
  hash the password matched, and the second step (`POST /auth/login/mfa`) fails with
  `invalid_mfa_token` when the stored hash is no longer that one. So a password reset always wins
  against a login (both of its steps), a rehash or a password change that was in flight, and no
  session is opened with a password the reset has replaced.
* **Tokens**: 32 random bytes, only their SHA-256 is stored; e-mail verification (24 h),
  password reset (1 h, revokes all sessions; it works only while the account still has the
  address it was mailed to), e-mail change (24 h, sent to the new address, at most one link per
  new address every 5 minutes whoever asks; a new request replaces it, a password change or reset
  cancels it, and a request that one of them overtakes gets 403 `invalid_password`: its write is a
  compare-and-set on the password hash it checked), MFA login challenge (5 min), SSO attempt
  (10 min), all single-use. The confirmation of an e-mail change (the link used, the new address,
  the end of the reset and verification links of the former address) and a password reset (the
  link used, the new password, the pending e-mail change cancelled) are each one transaction; a
  store that stays locked answers 503 `server_busy` with `retryAfter: 1`, nothing changed and
  the link still valid.
* **TOTP**: RFC 6238 (SHA-1, 6 digits, 30 s, +-1 step), secret 20 bytes, AES-256-GCM at rest with
  a key derived from SERVER_SECRET (or MFA_ENCRYPTION_KEY), replay refused (last used step
  stored). 10 recovery codes (`xxxx-xxxx-xx`, 50 bits) stored as HMAC-SHA256 with a derived
  pepper, single use. Password reset never disables MFA; an administrator can
  (`bin/admin.js user reset-mfa`) after verifying the owner by other means.
* **Enumeration**: register / forgot / resend answer the same whatever the e-mail; so does an
  e-mail change (`POST /account/email` answers 202 and shows the address as pending whether or not
  another account uses it; the owner of a taken address gets a notice, never a link); login errors
  are the same for an unknown account and a wrong password; usernames are public anyway (the
  "taken" answer is rate limited). Without `REQUIRE_EMAIL_VERIFICATION`, register and the e-mail
  change answer 409 `email_taken` (no link would confirm the address).
* **Account data export** (`POST /account/export`, password and second factor, 5 per hour): the
  player's own data only; never a password hash, TOTP secret, recovery code, token or token hash,
  the anti-cheat's data (integrity level, anomalies, analysis features, report weights), the reports
  made against the player or a moderator's identity, nor anything that tells which opponent was
  sanctioned (rating refunds are summed per UTC day and category, a filed report is only open or
  closed) or another person's IP address (an event keeps its IP only for the kinds the account
  holder does: `IP_KINDS`) (`src/http/routes/account-export.js`).
* **Brute force and stuffing**: per-address token buckets (the background budget of every
  request, below, and the auth family's route limits; IPv6 per /64, and the password endpoints
  also per /48 as a whole with `AUTH_RATE_PER_PREFIX`), whose refusals count 5 toward a block of
  the address; per-account limits whatever the address (`AUTH_MFA_PER_ACCOUNT` second-factor
  codes, `AUTH_REAUTH_PER_USER` re-authentications); per-account failure counter
  with exponential delay, global failure-rate detector that turns on the login proof-of-work
  (`POW_LOGIN_TRIGGER_PER_MIN`, 30 failed logins per minute: each costs a password hash, so a
  small server's hash throughput could never reach a much higher trigger),
  proof-of-work on registration (`SHA-256(challenge || nonce)` with N leading zero bits; the
  challenge is HMAC-signed, bound to the IP and the endpoint, single use).
* **WebSocket surface**: one message type table, strict decoding, size limit checked from the
  frame header before buffering, no compression, no fragmentation beyond the size limit,
  Hello timeout, per-connection token bucket, per-IP and global connection limits, slow
  consumers closed, heartbeat timeout, `Origin` refused by default. Before TLS: blocked addresses
  and the per-address connection rate and open connections (below), handshakes in progress capped
  per worker and per address, and load shed once the upgrades are refused because the server is
  full (5.8).
* **Protection per address (background layer)**: `src/net/ipguard.js` (`IpGuard`, one per
  worker) and `src/cluster/abuse.js` (`AbuseTracker`, in the primary). Every request and every
  connection meets it first, before routing, before authentication and, with native TLS, before
  any TLS work. It only keeps one network address from saturating the server; quotas otherwise
  belong to the signed-in account. An address is an IPv4 address or an IPv6 /64; an IPv6 address
  also counts toward its /48 (one customer's site) with 4 times each limit, so that rotating over
  the 65,536 /64s of a /48 multiplies nothing. A whole-server limit L is enforced in each worker
  as its share `max(1, min(L, ceil(2 L / WORKERS)))` (`workerShare`, `src/security/ratelimit.js`):
  all of it with 1 or 2 workers, half with 4, so a client spread over the workers gets at most
  twice L and no request waits for an IPC.
  * Requests (`admitRequest` in `wrapApiHandler`, `src/net/listeners.js`, and the API handler's
    first statement when it is used alone; `WsServer.handleUpgrade` for upgrades): every HTTP
    request, whatever its path, method or outcome (API, pages, `/healthz`, unknown paths, HEAD,
    OPTIONS, a 414, a WebSocket upgrade), takes one token of `HTTP_RATE_PER_IP` (600 per minute,
    burst half a minute) and, for IPv6, of `HTTP_RATE_PER_PREFIX` (4 times as much per /48; a
    refusal there gives the /64 token back), then one of the worker's `IP_MAX_INFLIGHT` (32)
    places of requests in progress, given back when the response closes, finished or aborted.
    A pipelined response still waiting for its turn on the connection when the connection
    closes (Node never gives it the socket, and it never closes) is given back then, or when its
    handler ends it if the handler is still at work.
    Beyond: 429 `{ "error": "rate_limited", "message": "Too many requests; try again later.",
    "retryAfter": s }` with `Retry-After`, on an upgrade `HTTP/1.1 429` with the same JSON.
  * Connections (TLS gate stage 0, 5.8): `IP_CONN_RATE` new connections per second (burst 4 s)
    and `IP_MAX_CONNECTIONS` open ones (handshakes, API keep-alive and WebSockets together),
    each worker its share; beyond, an RST before any TLS byte. `MAX_CONNECTIONS_PER_IP` (64),
    exact through the primary, still bounds the WebSockets of an address (per /64 only: it has
    no /48 count).
  * Slow clients (`hardenHttp`): `headersTimeout` 10 s and `requestTimeout` 30 s checked every
    second (`connectionsCheckingInterval`; Node's own 30 s let a slowloris client hold a socket 40
    s), `keepAliveTimeout` 5 s, a socket with no byte in or out for 30 s destroyed
    (`server.timeout`; upgraded sockets clear it; a request still in its handler keeps the socket,
    the handler's own timeout answers it), and an answer not flushed to the kernel 60 s
    after the handler ended it destroyed with its socket (a client reading a GIF or a PGN a few
    bytes at a time). Malformed HTTP is answered 400, 408 (header timeout) or 431 (headers too
    large) and counted in `scacelith_http_client_errors_total{reason}`.
  * Blocks. Each worker sums, per address, the refusals that show a client ignoring the limits:
    weight 1 for a 429 of the address budgets or the in-flight cap, a gate refusal of that
    address, a failed TLS handshake, malformed HTTP (not from a trusted proxy). The API's route
    limits report theirs through the same `IpGuard.noteRefusal` (`AUTH_REFUSAL_WEIGHT`, 5, for
    the login, registration, reset and MFA family, whose attempts each cost a password hash).
    Not counted: per-account limits, server-wide capacity refusals (the hash queue, the gate's
    `handshakes`, `waiting` and `server_full`), the refusals of an address already blocked. A
    worker reports the sums at most once a second (`abuse.report`, 5.7, at most 512 keys, the
    largest first); the primary adds every worker up over a sliding minute and blocks an address
    at `ABUSE_BLOCK_REFUSALS_PER_MIN` (600; 4 times that for a /48, which is also blocked once 4
    of its /64s are), for `ABUSE_BLOCK_BASE_SEC` (60 s) times 4 at each new block within 6 hours,
    up to `ABUSE_BLOCK_MAX_SEC` (1 h): 1, 4, 16, then 60 minutes. It sends the new blocks to every
    worker (`abuse.block`), and the running ones to a worker at its `shard.ready`; durations
    travel as durations and run on each process's monotonic clock. A worker that sees an address
    reach the threshold in one second on its own blocks it at once (local fast path), so a flood
    ends within a second on that worker and one or two on the others. A blocked address gets an
    RST before TLS for its new connections, 429 with the time left and `Connection: close` on the
    connections it already has (behind a proxy, 429 only: the connection is the proxy's), and 429
    on upgrades. WebSocket connections already open are never closed by a block, so a player who
    shares the address with an abuser (a school, a mobile operator's CGNAT) keeps the game in
    progress; a player whose connection drops can only come back when the block ends, which is
    why the first one is short. At most 20,000 blocks run at once (the oldest end first); the
    primary logs each one (`ip blocked`, with `ipForLog`, the scope, level, duration and
    refusals).
  * `ABUSE_EXEMPT` (addresses and CIDR subnets: a school or club network, monitoring, a load
    generator) skips all of the above. Login, registration, the other route limits and the
    per-account quotas still apply.
  * Cost (measured on the development container, `test/unit/net.ipguard.test.js`): about 0.15-0.25
    µs per IPv4 request (budget and in-flight place), 0.5-0.7 µs per IPv6 request, 0.02 µs for the
    request of a blocked address, 0.5 µs for a new connection and its close; the keys of an
    address are computed once per connection (0.8 µs for IPv6). Memory is bounded: 50,000 buckets
    per limiter (an evicted bucket comes back full, which only makes the limit more lenient),
    counters for open connections and requests in progress only, 20,000 blocks.
  * Metrics: `scacelith_http_rate_limited_total{limit}` (`ip`, `ip48`, `inflight`, `blocked`),
    `scacelith_tls_refused_total{reason}` (`blocked`, `conn_rate`, `conn_open`),
    `scacelith_tls_connections_open`, `scacelith_http_inflight`, `scacelith_abuse_blocked_keys`
    and `scacelith_abuse_local_blocks_total` per shard, `scacelith_abuse_report_entries_dropped_total`,
    `scacelith_http_client_errors_total{reason}`; in the primary `scacelith_abuse_blocks_total{scope,
    level}`, `scacelith_abuse_blocked{scope}` and `scacelith_abuse_blocks_evicted_total`.
* **Proof of work format**: an endpoint that wants one answers HTTP 428
  `{ "error": "pow_required", "pow": { "challenge": "<opaque ASCII>", "bits": 18, "expiresAt": ms } }`.
  The client finds a nonce, a decimal ASCII string, such that
  `SHA-256(challenge + ":" + nonce)` starts with `bits` zero bits (most significant bit of the
  first byte first), and repeats the same request with `"pow": { "challenge", "nonce" }` added
  to the JSON body. Challenges expire after 2 minutes and are single use.
* **Logs**: no password, token, TOTP secret, recovery code, cookie or e-mail body; IPs truncated
  by default; security events retained `RETENTION_SECURITY_DAYS`, stored IPs erased after
  `RETENTION_IP_DAYS` (by the hourly retention purge, section 7).

## 9. Scaling beyond one machine

* Game ids carry the shard, and shards are numbered per deployment (`SHARD_BASE`), so routing
  stays table-free across instances.
* `src/cluster/bus.js` has a transport interface (`connect(shard) -> link`, `send(shard,
  frame)`); the Unix-socket mesh is the single-machine implementation. A TCP+mTLS
  implementation lets shards of several machines relay to each other.
* The primary's control plane talks to workers through the IPC catalog only; replacing the
  in-process implementation with a network service (or Redis for presence, queues and rate
  limits) does not change the shards.
* The Store API hides SQL: a PostgreSQL implementation of `openStore` (with the same migrations
  translated) is the path to several instances sharing accounts and ratings. SQLite stays the
  simple default for community servers.
* A layer-4 load balancer (e.g. HAProxy) in front of several instances works because any
  connection may land anywhere: presence and the bus find the game.

## 10. Game client integration (C++)

* `net::OnlineClient` (client-net) runs the network on its own thread and exposes a
  non-blocking command/event API to the game thread (`src/net/online_client.h`, written by the
  orchestrator).
* Windows: WinHTTP for HTTPS and WebSocket (TLS by the OS, certificate validation by the OS
  trust store; optional per-server pinned SHA-256 fingerprint for self-signed community
  servers), DPAPI (`CryptProtectData`) for stored tokens, BCrypt for SHA-256 / random / PoW,
  `ShellExecuteW` for the SSO browser. Linux test builds: OpenSSL.
* The client validates moves with `chess::Position` before sending (the same rules as the
  server), sends the intent when the destination is chosen, keeps the robots' physical
  animations, never flies the camera between seats online, shows the ping discreetly at the top
  right, the players' names and ratings in the scoresheet header, and treats the server as the
  only authority on clocks and results.

## 11. Tests and conventions

* `node --test` (built-in runner), files `test/unit/<module>.<topic>.test.js` and
  `test/integration/<scenario>.test.js`. No test touches the network outside localhost.
* Test helpers: `testConfig(overrides)`; `openStore(testConfig({ DB_PATH: ':memory:' }))`;
  integration tests start a real server in a child process with a temporary data dir and a
  self-signed certificate made with the `openssl` command line (skip TLS tests when absent).
* Every module keeps its hot path allocation-light and documents its complexity when it runs
  per message.
