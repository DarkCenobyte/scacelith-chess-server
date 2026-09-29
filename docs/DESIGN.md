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
| `src/store/*` (+ `migrations/*.sql`), `src/http/routes/players.js` | store | schema, migrations, Store API, game journal and crash recovery, retention jobs, public read API |
| `src/http/server.js`, `src/http/router.js`, `src/http/routes/{auth,account,sso,info}.js`, `src/http/pages/*`, `src/auth/*`, `src/mail/*`, `src/security/*` | auth | HTTPS API, sessions, passwords, MFA, recovery, e-mail, SSO, rate limiting, proof of work |
| `src/anticheat/*`, `src/http/routes/reports.js`, `bin/admin.js` | anticheat | anomaly classification, sanctions, engine analysis, suspicion levels, reports, admin CLI |
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
no `Origin` header (browsers are refused unless configured). Within `WS_HELLO_TIMEOUT_MS` the
client sends `Hello{proto, schema, client, token}`. The shard validates the token (auth
service, cached), checks bans and e-mail verification, then asks the primary
`presence.claim`. The primary kicks an older connection of the same account
(`Error{Replaced}` + close 4007) and returns the active game, if any. The shard answers
`Welcome`, then (if a game is active) attaches the connection to the game, whose host sends a
`GameSnapshot`.

**Matchmaking.** `QueueJoin{category, rated}` -> the shard checks the category and reads the
player's rating (`store.ratings.get`), then `mm.join` to the primary. Each `MATCH_TICK_MS` the
matchmaker pairs players; for each pair the primary allocates the host shard (the shard of the
player who waited longer; any shard when it is overloaded), sends it `game.create`, and sends
`game.attach` to the shards of both connections. The host sends `GameSnapshot` to both.

**A move.** Client A (on shard 2) sends `Move{game, ply, move, posHash, thinkMs}` at the moment
the destination square is chosen (the robot's hand then plays the move and presses the clock:
animation only). Shard 2 sees the game id's shard is 1 and forwards the raw frame on the bus.
The host validates (section 6), updates the canonical state and the clocks, journals the move,
encodes one `MoveMade` and sends it to both players (local socket or bus). Opponent B's client
animates the other robot playing it. A's client treats its own `MoveMade` as the confirmation; a
`MoveRejected` makes it restore the server position from the `GameSnapshot` that follows.

**End of game.** The room decides the result (mate, flag, resignation, agreement, claim,
abandonment, abort...), the host sends `GameEnd` to both, journals it, and queues the game for
the database. Every `DB_COMMIT_MS` the host commits the queued games in one transaction
(`store.games.finishBatch`): game record + both ratings (read and written inside the
transaction) + analysis job (queue policy: section 6.5). The transaction runs on the shard's
store writer thread (`src/store/writer.js`, its own SQLite connection), so the event loop never
waits for the disk or another process's write lock. After the commit it sends `RatingUpdate` and tells the primary
`game.ended`.

**Reconnection.** A lost connection keeps the game running (the player's clock too). The
opponent gets `GameEvent{PlayerDisconnected, arg = grace ms}`. A new connection (any shard)
says `Hello`; the primary knows the active game; the client receives `GameSnapshot` and goes on.
After the grace period without a connection: section 6.4.

## 4. Shared foundations (exist already)

* `loadConfig()` / `testConfig(overrides)` in `src/config.js`: frozen camelCase object
  (`cfg.apiPort`, `cfg.categories` = `[{id:'3+2', baseMs, incMs}]`, secrets are Buffers).
* `logger.child('name')` in `src/log.js`: `.debug/.info/.warn/.error(msg, fields)`,
  `.security(event, fields)`. Redaction is automatic; pass IPs through `ipForLog(ip)`.
* `metrics` in `src/metrics.js`: `counter / gauge / gaugeFn / histogram`, pre-bound `labels()`.
  Metric names are `scacelith_<area>_<what>[_unit][_total]`.
* `GameIdAllocator(shard).next()`, `shardOfGameId(id)` in `src/util/ids.js`.

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
               createdAt, config, rematchOf?, createChessGame /* () => new ChessGame(), injected by the host */ })
room.onMove(color, { seq, ply, move, posHash, thinkMs, drawOffer }, now)  -> Outcome
room.onResign(color, now) / onDrawOffer(color, now) / onDrawAnswer(color, accept, now)
room.onDrawClaim(color, now) / onAbort(color, now) / onRematch(color, accept, now)
room.onDisconnect(color, now) / onReconnect(color, now) / onRtt(color, rttMs)
room.forfeit(color, now)            // anti-cheat: loss (EndReason.Forfeit)
room.serverAbort(now)                // EndReason.ServerAborted
room.tick(now) -> Outcome           // flags, first-move timeouts, grace expiries
room.nextDeadline() -> ms | Infinity
room.snapshot(forColor, now) -> object for encode.GameSnapshot
room.isOver; room.result          // { status, reason, whiteMs, blackMs, endedAt }
room.record() -> finished game record for store.games.finishBatch (section 5.5)
room.journalState() / GameRoom.fromJournal(records)   // see 5.6
```
`Outcome = { broadcast: [Buffer], toWhite: [Buffer], toBlack: [Buffer], reply: [Buffer] (to the
sender), anomaly: null | { color, kind, detail, posMatched }, ended: bool, journal: [records] }`.
Buffers are already encoded with the codec; the host sends them as they are.

`GameHost` (one per shard):
```js
new GameHost({ shard, config, store, journal, anticheat, bus, primary, log })
host.createGame(spec) -> gameId            // spec from the primary (players, tc, rated, colours)
host.attach(gameId, userId, endpoint)      // endpoint: { send(buf), connId, shard } ; sends the snapshot
host.detach(gameId, userId, endpoint)      // connection closed
host.onClientMessage(gameId, userId, msg, endpoint) // decoded C2S game message (Move..Rematch)
host.recover() -> count                    // replays the journal at start-up
host.stats() -> { games, ... }
host.shutdown()                            // flush journal + pending commits
```
The host owns a timer wheel (10 ms slots) driven by one interval; each room's `nextDeadline()`
is (re)scheduled after every outcome. It records `anomaly` through `anticheat.recordAnomaly`
and, for a certain cheat with `AUTO_SANCTION_CERTAIN_CHEATS`, calls `room.forfeit` and
`anticheat.sanctionCertain` (section 5.8).

### 5.4 Match (`src/match/`)

```js
// elo.js: pure, mirrors src/game/elo.h of the game.
expectedScore(rating, opponent); kFactor(record /* {rating, games, reachedSenior} */, cfg);
applyGame(white /* record */, black, score /* 1, 0.5, 0 from White's side */, cfg)
  -> { white: { before, after, record }, black: {...} }   // records updated (games, w/d/l, peak, reachedSenior)
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
store.ratings.get(userId, category) -> { rating, games, wins, draws, losses, peak, reachedSenior }  // defaults when absent
store.ratings.forUser(userId) -> [{ category, ... }] ; leaderboard(category, limit, minGames)
store.games.finishBatch(records) -> [{ gameId, ratings: null | { white: RatingChange, black: RatingChange } }]
  // One transaction for the batch: inserts each game, applies match/elo.applyGame to rated games
  // with the ratings read inside the transaction, queues rated games of >= ANALYSIS_MIN_PLIES for analysis
  // (section 6.5: an ordinary game left out by the policy has analysisSkipped: 'sample' | 'backlog' in its entry).
  // record: { id, category, rated, baseMs, incMs, whiteId, blackId, whiteName, blackName, whiteRating, blackRating,
  //           startedAt, endedAt, status, reason, moves: Uint16Array, spentMs: Uint32Array, clockMs: Uint32Array,
  //           rematchOf, flags }
store.games.byId(id) ; recentForUser(userId, limit, before?) ; countBetween(a, b, since)
store.conduct.record(userId, kind, at) ; store.conduct.countSince(userId, since) -> { abandon, abort, noshow } ; store.conduct.cooldown(userId) / setCooldown(userId, until, level)
store.sanctions.create({ userId, kind /* 'ban'|'mm_block'|'warning' */, reason, source /* 'auto'|'moderator' */, gameId, startsAt, endsAt, createdBy }) -> id
store.sanctions.activeBan(userId, now) -> sanction | null ; list(userId) ; lift(id, by, now)
store.anomalies.insertBatch([{ userId, gameId, kind, severity /* 'info'|'suspicious'|'certain' */, detail, at }]) ; forUser(userId, limit)
store.security.insertBatch([{ kind, userId, ip, detail, at }]) ; purge(now, cfg)
store.analysis.next(limit, workerId, now) -> [job] ; complete(gameId, features) ; fail(gameId, error) ; forUser(userId, limit)
  // next() takes the highest priority first, then the oldest; job: { gameId, priority, attempts, queuedAt, startedAt, worker }
store.analysis.enqueue(gameId, now) ; request(gameId, 'report', now) -> bool ; backlog() -> { ordinary, priority }
  // enqueue: moderator re-analysis (priority manual); request: a reported game (section 6.5)
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
segment is deleted when every game it mentions is ended *and* committed to the database. A torn
last record is ignored on replay.

```js
const j = await openJournal({ dir, shard, flushMs, fsync })
j.append(kind, gameId, payloadBuffer, at)   // kinds: 1 created (JSON spec), 2 move, 3 event, 4 ended, 5 committed
j.flush() -> Promise ; j.committed(gameId)  // marks it safe to forget
j.recover() -> Map<gameId, [{ kind, at, payload }]>  // games not committed, in order
j.close()
```
The game module decides the payloads (`GameRoom.journalState` / `fromJournal`).

### 5.7 Primary control plane: IPC catalog

Transport: `src/cluster/ipc.js` (net owner) gives both sides `request(type, payload) ->
Promise<reply>` (timeout 5 s) and `on(type, handler)`; handlers return the reply (or a promise).
Buffers inside payloads are sent as they are (`serialization: 'advanced'`).

Shard -> primary:

| type | payload | reply |
|---|---|---|
| `presence.claim` | `{ userId, username, shard, connId, ip }` | `{ ok, activeGame: id or 0, kicked: bool }` or `{ error }` (ServerFull, Banned) |
| `presence.release` | `{ userId, connId }` | - |
| `conn.ipAcquire` / `conn.ipRelease` | `{ ip }` | `{ ok }` (per-IP connection limit) |
| `mm.join` | `{ userId, username, category, rated, rating, provisional, shard, connId }` | `{ ok }` / `{ error }` |
| `mm.leave` | `{ userId }` | `{ ok }` |
| `challenge.create` / `.accept` / `.decline` / `.cancel` / `.joinCode` | see 5.4 | `{ ok, id?, code? }` / `{ error }` |
| `game.ended` | `{ gameId, whiteId, blackId, status, reason, rated, category, rematchOffer }` | - |
| `game.rematch` | `{ gameId, white, black, category, baseMs, incMs, rated }` | `{ ok, gameId }` / `{ error }` |
| `conduct.record` | `{ userId, kind }` | - |
| `ratelimit.take` | `{ key, limit, windowMs, cost }` | `{ allowed, retryAfterMs, count }` |
| `once.consume` | `{ key, ttlMs }` | `{ fresh }` |
| `sanction.applied` | `{ userId, until, reason }` | - (primary kicks the user everywhere) |
| `session.revoked` | `{ userId, tokenHashes }` | - (broadcast to every shard's auth cache) |

Primary -> shard:

| type | payload | effect |
|---|---|---|
| `game.create` | `{ spec }` | host creates the room, replies `{ ok, gameId }` |
| `game.attach` | `{ gameId, userId, connId }` | the shard binds that connection to the game (local or via bus) |
| `conn.send` | `{ connId, frames: [Buffer] }` | writes encoded S2C frames (QueueStatus, Challenge*, Notice) |
| `conn.kick` | `{ connId, code, closeCode, frames }` | sends then closes |
| `auth.invalidate` | `{ userId, tokenHashes }` | drops cached sessions |
| `metrics.snapshot` | - | replies `registry.snapshot()` |
| `shutdown` | `{ graceMs }` | drain: Notice{ServerShutdown}, stop accepting, flush |

### 5.8 Shard internals (net owner) and the anti-cheat hooks

```js
// src/net/ws.js
class WsServer { constructor({ maxMessageBytes, subprotocol, allowOrigins, onConnection }) ; handleUpgrade(req, socket, head) }
class WsConnection {
  id; ip; userId; username; state /* 'hello'|'ready'|'closing' */; rttMs; bufferedBytes;
  sendFrame(buf) -> bool   // one binary message; false when closed or over WS_SEND_BUFFER_LIMIT (then closes 4303)
  close(code, reason)
}
// src/cluster/router.js: decodes C2S frames (decode(buf, {dir:'c2s'})), enforces seq, rate limits,
// handles Ping/Pong, sends Queue*/Challenge* to the primary, game messages to the local GameHost or
// to the host shard over the bus (raw frame + userId + connId), and bus deliveries back to sockets.
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

### 5.9 HTTP (`src/http/`)

`createApiHandler({ config, store, auth, primary, anticheat, log }) -> (req, res)`, mounted by
the net owner on the HTTPS server (and handling `upgrade` elsewhere). Router:
```js
router.get(path, handler, { auth: 'none'|'optional'|'required', rate: { key, limit, windowMs } })
router.post(path, handler, { auth, body: schemaObject, rate })   // JSON only, HTTP_BODY_LIMIT
// handler(ctx) -> { status, body, headers } ; ctx = { req, ip, params, query, body, user, session, config, store, log, primary }
```
Path parameters use `:name`. JSON errors are `{ "error": "<snake_case_code>", "message": "...",
"retryAfter"?: s }`. Every response carries `Cache-Control: no-store`, `X-Content-Type-Options:
nosniff`, `Referrer-Policy: no-referrer`, `Strict-Transport-Security` (native TLS), and HTML
pages a strict `Content-Security-Policy`. Route modules export `register(router, deps)`.

Endpoints (prefix `/api/v1`):

| Method and path | Owner | Notes |
|---|---|---|
| `GET /info` | auth | `{ name, serverId, motd, protocol: {min, max, schema, subprotocol}, wsPort, wsPath: '/ws', registration, emailVerification, sso: { google }, mfa: true, pow: { register }, categories: [{id, baseSec, incSec}], limits }` |
| `POST /auth/register` | auth | `{ username, email, password, pow? }` -> 202 `{ status: 'verification_sent' }` (same answer when the e-mail exists) |
| `POST /auth/login` | auth | `{ login, password, clientLabel?, pow? }` -> `{ token, expiresAt, user }` or `{ mfaRequired: true, mfaToken }` |
| `POST /auth/login/mfa` | auth | `{ mfaToken, code? , recoveryCode? }` |
| `POST /auth/logout`, `POST /auth/logout-all` | auth | bearer |
| `GET /auth/sessions`, `DELETE /auth/sessions/:id` | auth | |
| `POST /auth/verify-email/resend` | auth | `{ email }` -> 202 always |
| `POST /auth/password/forgot` | auth | `{ email }` -> 202 always |
| `POST /auth/password/reset` | auth | `{ token, newPassword }` (also the HTML form at `/reset-password`) |
| `POST /auth/sso/google/start` | auth | `{ codeChallenge }` -> `{ attemptId, authUrl, pollMs, expiresIn }` |
| `POST /auth/sso/google/poll` | auth | `{ attemptId, codeVerifier }` -> `{ status: 'pending' }` / login answer / `{ needsUsername, ssoTicket }` |
| `POST /auth/sso/complete` | auth | `{ ssoTicket, username }` |
| `GET /account/me` | auth | user, ratings, active sanctions, integrity level is NOT exposed |
| `POST /account/password`, `/account/mfa/totp/setup`, `/account/mfa/totp/enable`, `/account/mfa/totp/disable`, `/account/mfa/recovery-codes`, `/account/delete`, `PUT /account/preferences` | auth | re-authentication with password (+ TOTP when enabled) |
| `GET /players/:username`, `GET /players/:username/games`, `GET /games/:id`, `GET /leaderboard?category=` | store | public data only |
| `POST /reports` | anticheat | `{ gameId, reported, category: 'cheating'|'abuse'|'other', comment }` |
| `GET /healthz`, `GET /readyz` | net | also on the metrics port |

HTML pages outside `/api`: `GET/POST /verify-email?token=`, `GET/POST /reset-password?token=`,
`GET /auth/sso/google/callback` (auth owner). GET only shows a confirmation button; the state
change happens on POST (link scanners must not consume tokens).

## 6. Game policies

### 6.1 Clocks (server authoritative)

* Time is measured on the server only. The client's `thinkMs` never adds time; it only bounds
  lag compensation.
* Plies 0 and 1 (each side's first move) do not run the clock: each player has
  `FIRST_MOVE_TIMEOUT_MS` to make it, otherwise the game is aborted (`NoShow`, unrated). No
  increment is added for them. The clocks start with White's second move.
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
  `clock_implausible` (suspicious, never certain: clock drift and suspended VMs exist).
* `rttEma` is the server's own measurement (its Ping / the client's Pong), exponential moving
  average, capped at 2000 ms. A client that delays its Pongs only inflates a value that is
  itself capped by the quota.

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
  disconnected and a fresh grace; the downtime is not charged to anyone (the running clock
  restarts from its journaled value at recovery). Games that cannot be rebuilt end as
  `ServerAborted` (unrated) and are committed as such.

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
evidence. Everything else is only recorded.

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

**Analysis backlog.** One engine analyses about 1,000 to 12,000 games a day (depth 18, MultiPV 3,
from ply 16 on), far fewer than a busy server finishes, so the queue is bounded and prioritized
instead of growing without end. Every job has a priority and the engine takes the highest first,
then the oldest:

1. `manual`: a moderator asked for the (re-)analysis of a game (`store.analysis.enqueue`);
2. `report`: a player reported the game (category `cheating` or `other`); the report queues it
   even if the policy had left it out, and re-queues it if its analysis had failed;
3. `signal`: a suspicion signal at the end of the game: the integrity level of either player is
   above `none`, either player has an open `cheating` or `other` report from the last 30 days
   by a credible reporter (stored weight at least 0.5, so sock puppets never flag anyone), or a
   `suspicious` or `certain` anomaly was recorded in this game;
4. `ordinary`: every other rated game of an official category with at least
   `ANALYSIS_MIN_PLIES` plies.

Games of the first three kinds are always queued. An ordinary game is queued with probability
`ANALYSIS_SAMPLE_RATE` (default 1), and only while fewer than `ANALYSIS_QUEUE_MAX` (default 5000)
ordinary jobs wait; otherwise it gets no job at all and is counted in
`scacelith_anticheat_analysis_skipped_total{reason="sample"|"backlog"}`. An ordinary game
therefore waits at most about `ANALYSIS_QUEUE_MAX` divided by the engine throughput, and a
suspicious game never waits behind ordinary ones. `scacelith_anticheat_analysis_queue_ordinary`
and `scacelith_anticheat_analysis_queue_priority` show the backlog. The policy only decides what
is analysed and when: it changes no level and no sanction, and statistics never ban.

## 7. What lives where, and what survives a crash

| Data | Where | Written | After a crash |
|---|---|---|---|
| Accounts, e-mail, password hashes, MFA secrets (encrypted), recovery-code hashes, SSO links | SQLite | immediately (transaction) | kept |
| Sessions | SQLite (+ 30 s cache per shard) | login/logout immediately; `lastSeen` at most every 5 min | kept (revocations immediate: cache invalidation broadcast) |
| Ratings, finished games, analysis queue | SQLite | batched every `DB_COMMIT_MS`, one transaction per batch | kept once committed; games ended but not committed are in the journal and committed at recovery |
| Games in progress (moves, clocks, offers) | memory of the host shard + journal | journal flushed every `JOURNAL_FLUSH_MS` | replayed; at most `JOURNAL_FLUSH_MS` of moves lost (clients resend: stale ply / resync) |
| Sanctions, anomalies (certain), integrity levels, reports | SQLite | sanctions immediately; anomalies batched (1 s) | kept (a batch in flight may be lost for `info` anomalies) |
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
kept. The primary is the control plane, so the purge must not hold its event loop: every
statement deletes at most 1000 rows in its own short transaction, and the run returns to the
event loop whenever 10 ms of work have been done (measured on the development container: 350,000
rows purged in 1.5 s, the event loop never held for more than about 30 ms). Runs never overlap.
On shutdown the run in progress is aborted between two statements and awaited before the
database is closed. Each run logs its counts at info level (numbers only, no personal data) and
feeds `scacelith_retention_purged_total{kind}`, `scacelith_retention_ip_erased_total`,
`scacelith_retention_runs_total{result}` and `scacelith_retention_run_seconds`.

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
  login -> the same work on a dummy hash (constant-time behaviour).
* **Tokens**: 32 random bytes, only their SHA-256 is stored; e-mail verification (24 h),
  password reset (1 h, revokes all sessions), MFA login challenge (5 min), SSO attempt (10 min),
  all single-use.
* **TOTP**: RFC 6238 (SHA-1, 6 digits, 30 s, +-1 step), secret 20 bytes, AES-256-GCM at rest with
  a key derived from SERVER_SECRET (or MFA_ENCRYPTION_KEY), replay refused (last used step
  stored). 10 recovery codes (`xxxx-xxxx-xx`, 50 bits) stored as HMAC-SHA256 with a derived
  pepper, single use. Password reset never disables MFA; an administrator can
  (`bin/admin.js user reset-mfa`) after verifying the owner by other means.
* **Enumeration**: register / forgot / resend answer the same whatever the e-mail; login errors
  are the same for an unknown account and a wrong password; usernames are public anyway (the
  "taken" answer is rate limited).
* **Brute force and stuffing**: per-IP token buckets (API, auth), per-account failure counter
  with exponential delay, global failure-rate detector that turns on the login proof-of-work,
  proof-of-work on registration (`SHA-256(challenge || nonce)` with N leading zero bits; the
  challenge is HMAC-signed, bound to the IP and the endpoint, single use).
* **WebSocket surface**: one message type table, strict decoding, size limit checked from the
  frame header before buffering, no compression, no fragmentation beyond the size limit,
  Hello timeout, per-connection token bucket, per-IP and global connection limits, slow
  consumers closed, heartbeat timeout, `Origin` refused by default.
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
