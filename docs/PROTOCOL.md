# Scacelith realtime protocol, version 1

This is the specification of the realtime protocol that the Scacelith game client and a Scacelith
dedicated server speak over a WebSocket: enough to write a compatible client or server from it
alone. Version 1 is **frozen**: what this document describes never changes; later minor versions
only add to it, under the rules of [Versions and evolution](#versions-and-evolution).

The wire format has a single source, `protocol/scacelith-v1.json`. The `protogen` tool
(`cargo run -p scacelith-protocol --features gen --bin protogen` in `dedicated-server/`) checks it
and generates the server's Rust codec, the game's C++ codec, the golden vectors and the tables of
this document (between `protogen` markers; the prose is written by hand). The HTTPS account API is
described in `docs/API.md`.

<!-- protogen:begin summary -->

| | |
|---|---|
| Protocol version (`proto`) | 1 |
| Minor version (`minor`) | 0 |
| Capability bits (`caps`) | none defined |
| WebSocket subprotocol | `scacelith.rt1` |
| Schema fingerprint | `0x05d4f428` (informational) |
| `MaxClientMessage` | 512 |
| `MaxServerMessage` | 65536 |
| `MaxPlies` | 1200 |
| Messages | 19 client to server, 16 server to client |

<!-- protogen:end summary -->

Contents: [Conventions](#conventions) · [Transport](#transport) · [Encoding](#encoding) ·
[Versions](#versions-and-evolution) · [Connection](#connection-lifecycle) ·
[Requests and answers](#requests-and-answers) · [Games](#games) · [Clocks](#clocks) ·
[Gestures](#gesture-relay) · [Rate limits](#rate-limits) · [Errors and closing](#errors-and-close-codes) ·
[Moves and positions](#moves-and-positions) · [Messages](#messages) · [Structs](#structs) ·
[Enums](#enums) · [Flags](#flags) · [Golden vectors](#golden-vectors)

## Conventions

* **Must**, **must not**, **should** and **may** have their usual specification meaning (RFC 2119).
* *Client* is the game, *server* the dedicated server. *C2S* messages go from the client to the
  server, *S2C* messages from the server to the client.
* Times named `...Ms` are durations in milliseconds; `serverTime`, `startedAt` and the `arg` of
  some notices are instants of the server clock, in milliseconds since the Unix epoch (an `f64`,
  with a fraction).
* Names in `UPPER_CASE` are settings of the reference server, given with their default value.
  Another server may choose other values; what a client must know is announced in `Welcome`.

## Transport

* **Discovery.** Before connecting, the client reads `GET /api/v1/info` (`docs/API.md`). Its
  `protocol` object is `{ "min": 1, "max": 1, "schema": <fingerprint>, "subprotocol":
  "scacelith.rt1" }`, `wsPath` is the path of the WebSocket (`/ws` by default) and `wsPort` the
  port it listens on, for information. A client of protocol 1 is compatible when
  `min <= 1 <= max` and `subprotocol` is `scacelith.rt1`. `schema` is informational and is never
  compared. An incompatible server is not contacted: the client tells the player to update the
  game (or the server) and does not retry.
* **WebSocket.** `wss://<host>:<port><wsPath>` (RFC 6455) with `Sec-WebSocket-Protocol:
  scacelith.rt1`, on the host and port of the API unless the player's server address names
  another WebSocket port (the reference client never takes the port from `wsPort`). A server that
  does not select the subprotocol is not a protocol 1 server; the reference server answers an
  upgrade without it with HTTP 426 `{"error": "unsupported_protocol", "supported":
  ["scacelith.rt1"]}`. The client sends no `Origin` header
  (browsers are refused unless the server allows their origin). TLS certificates are validated
  with the operating system's store, or against a certificate the player pinned for a community
  server. No extension is negotiated: frames are never compressed.
* **Server identity.** The `101` answer carries `Scacelith-Server-Id`, the `serverId` of
  `/api/v1/info`. A client whose saved session for that address belongs to another server id
  drops it instead of sending it in `Hello`.
* **Frames.** One protocol message per WebSocket **binary** message. A text message closes the
  connection (1003). A client message is at most `MaxClientMessage` (512) bytes: the server
  refuses a larger one from its frame header, before reading it (1009). A server message is at
  most `MaxServerMessage` (65,536) bytes.

## Encoding

A message is a type byte followed by its fields in schema order, **little-endian, without padding
or alignment**, and nothing else:

```
u8 type | field 1 | field 2 | ...
```

| Type | Wire | Values |
|---|---|---|
| `u8` `u16` `u32` `u64` `i32` | 1, 2, 4, 8, 4 bytes | integers (`i32` two's complement), with optional bounds |
| `f64` | 8 bytes | IEEE 754 binary64, finite (no NaN, no infinity); encoders write -0 as +0 |
| `id53` | 8 bytes (u64) | an identifier below 2^53 (exact in a JSON number and a double); 0 = none |
| `bool` | 1 byte | 0 or 1 |
| `str8` | u8 byte length, then the bytes | strict UTF-8 without NUL, at most 255 bytes, with bounds on the byte length |
| enum | 1 byte (u8) | a value of the enum ([open enums](#versions-and-evolution) excepted) |
| struct | its fields, inline | |
| `list16` | u16 item count, then the items | at most the field's maximum count |

Rules every receiver applies (the first one broken refuses the message):

* The type byte is assigned: `0x01`-`0x7F` are C2S types, `0x80`-`0xFF` S2C types, `0x00` is
  never used. Names used in both directions (`Ping`, `Pong`, `Gesture`) are separate messages
  with separate types; the tables call them `C_Ping` and `S_Ping`, and so on.
* Every field is present: a message cut short is **truncated**.
* Integers lie within their bounds; a bool is 0 or 1; an f64 is finite; an id53 is below 2^53.
* A string's byte length lies within its bounds (checked before its bytes are looked for), and
  its bytes are strict UTF-8 (no overlong form, no encoded surrogate, nothing above U+10FFFF; a
  leading byte order mark is an ordinary U+FEFF) without a NUL byte.
* A list's count is at most its maximum, and is checked against the bytes left before anything
  is allocated.
* An enum value is a value of the enum.
* Nothing follows the last field.

The last two rules are where receivers differ, on purpose:

* The **server decodes strictly**: every rule applies, and a message that breaks one is
  [malformed](#errors-and-close-codes).
* A **client decodes leniently**: it ignores a message whose type it does not know as an S2C type
  (a later minor's message), ignores the bytes that follow the last field it knows (fields a later
  minor appended), and keeps the values of [open enums](#enums) that it does not know, applying
  the fallback the enum describes. Every other rule applies: a client refuses a truncated message,
  an out-of-bounds value or a bad string.

Encoders validate the same rules before writing anything, so a correct peer never sends a frame
the other refuses. Encodings are canonical: one value has one encoding.

The type bytes are divided into ranges by area. A minor version assigns new types inside the
published ranges; the experimental ranges are never published, and a published peer refuses
(server) or ignores (client) their types.

<!-- protogen:begin ranges -->

| Type bytes | Direction | Area | Status |
|---|---|---|---|
| `0x01`-`0x0F` | c2s | connection | 3 assigned |
| `0x10`-`0x1F` | c2s | lobby | 7 assigned |
| `0x20`-`0x3F` | c2s | game | 9 assigned |
| `0x40`-`0x6F` | c2s | future | reserved for later minors |
| `0x70`-`0x7F` | c2s | experimental | never published: private experiments, refused by every published peer |
| `0x80`-`0x8F` | s2c | connection | 6 assigned |
| `0x90`-`0x9F` | s2c | lobby | 3 assigned |
| `0xA0`-`0xBF` | s2c | game | 7 assigned |
| `0xC0`-`0xEF` | s2c | future | reserved for later minors |
| `0xF0`-`0xFF` | s2c | experimental | never published: private experiments, refused by every published peer |

<!-- protogen:end ranges -->

## Versions and evolution

* **`proto`** is the major version: 1 for the whole life of this protocol. A change that breaks
  compatibility would be a new protocol with a new subprotocol token, not a new `proto` value on
  this one.
* **`minor`** numbers the additions to version 1; this document describes minor 0. The client
  sends the highest minor it speaks in `Hello.minor`; the server answers with the negotiated minor
  `Welcome.minor = min(Hello.minor, server minor)`. Both sides then use only what the negotiated
  minor defines: a client sends the fields and messages of that minor and no more.
* **`caps`** is a set of capability bits (u64) for optional features. The client sends the bits
  it supports in `Hello.caps`; `Welcome.caps` is the bitwise AND with the server's. Minor 0 defines
  no bit: a client sends 0 and ignores bits it does not know.
* **Frozen anchors.** Whatever the minor, these never change, so that any client and any server
  can always understand each other's first words:
  1. the Hello prefix: `0x01 | seq u32 | proto u16 | minor u16 | caps u64` (17 bytes);
  2. the beginning of `Welcome`: `proto u16 | minor u16 | caps u64`;
  3. the whole `Error` layout, `0x81 | ref u32 | code u8 | fatal u8 | game id53`, the meaning of
     every error code and close code, and the [close-code rule](#errors-and-close-codes);
  4. the `Ping` and `Pong` layouts of both directions.
* **What a minor may do**, and nothing else: add message types inside the published ranges;
  append fields at the end of an existing S2C message; add values to open enums; add flag bits,
  capability bits and close codes; widen the bounds of a C2S field; narrow the bounds of an S2C
  field. A C2S message never gains fields, Hello included: the server decodes strictly, and the
  frames of older clients would lack them. What clients send in addition is a new C2S message
  type, gated by a capability bit when it is optional. Nothing is removed, renumbered, renamed,
  retyped or moved; structs, closed enums, constants and ranges never change; a retired enum value
  stays reserved forever. Every released minor is frozen as `protocol/frozen/v1.<minor>.json`, and
  `protogen` refuses a schema that breaks these rules.
* **Reading a Hello of any minor.** The server reads the 17-byte prefix first: a `proto` it does
  not support is refused with `UnsupportedProtocol` whatever follows. A Hello whose `minor` is
  higher than the server's is read the same way, except that bytes after the fields the server
  knows are ignored (this is the only place where the server accepts trailing bytes).
* **Fingerprint.** The schema fingerprint is the first four bytes (big-endian) of SHA-256 of the
  canonical schema (prose left out, keys sorted, no whitespace). It appears in `/api/v1/info`, the
  vectors and the logs, and is never a reason to refuse a peer.

## Connection lifecycle

1. **Connect** as described in [Transport](#transport).
2. **Hello.** The client sends `Hello{seq: 1, proto: 1, minor, caps, client, token}` first.
   `token` is the session token of this server's HTTPS login (16 to 160 bytes); `client` names the
   client and its version for the logs (`Scacelith/1.5.0 win64`). The server checks, in order:
   * nothing arrived within `WS_HELLO_TIMEOUT_MS` (10 s), or the first message is not a Hello:
     `HelloRequired`;
   * the message is shorter than the 17-byte prefix: `Malformed`;
   * `proto` is not 1: `UnsupportedProtocol`;
   * the Hello does not decode (as above for a later minor): `Malformed`;
   * `seq` is not 1: `ProtocolViolation`;
   * the token is unknown, expired or revoked: `Unauthorized`; the account's e-mail address is not
     verified while the server requires it: `EmailUnverified`; the account is banned: `Banned`
     (with `Notice{Banned, arg = end of the ban}` before the `Error`); the server already has
     `MAX_CONNECTIONS` players and this one has no game in progress: `ServerFull`; the server is
     shutting down: `ShuttingDown`.

   Each of these is a fatal `Error` followed by its close code. A client should send nothing else
   before `Welcome`; the reference server keeps at most 8 messages received between `Hello` and
   `Welcome` and handles them after it (more is a `Flood`).
3. **Welcome.** The server answers `Welcome`: the negotiated `minor` and `caps`, `serverTime` (a
   first estimate of the server clock), the account (`userId`, `username`), `serverName`, the
   heartbeat and client ping intervals, the rate limits (`maxMsgPerSec`, `msgBurst`,
   `gestureRate`, `gestureBurst`), the gesture keepalive (`gestureIdleMs`) and `activeGame`. An
   older connection of the same account is closed at that moment with
   `Notice{ReplacedByNewConnection}` and a fatal `Error{Replaced}`.
4. **Game in progress.** When `activeGame` is not 0, a `GameSnapshot` of that game follows
   `Welcome`; the client rebuilds the board and the clocks from it.
5. **Heartbeats.** The server sends `Ping{nonce, serverTime}` about every `Welcome.heartbeatMs`
   (`HEARTBEAT_INTERVAL_MS`, 10 s); the client answers `Pong{seq, nonce}` **at once**: the server
   measures each player's round trip with it, for lag compensation. The server closes a
   connection from which nothing arrived for `HEARTBEAT_TIMEOUT_MS` (30 s) with 1001.
6. **Client pings.** The client measures its own round trip and its clock offset with
   `Ping{seq, nonce}`, answered by `Pong{nonce, serverTime}`: `offset = serverTime - (sentAt +
   rtt / 2)`. It sends one right after `Welcome` and three more about a second apart (so the ping
   indicator and the offset settle quickly), then one every `Welcome.clientPingMs` (0 means the
   client's default of 10 s). The server answers at most one per 950 ms.
7. **Liveness.** Anything received counts as a sign of life, an unknown message included. A client
   sends a `Ping` of its own after 1.5 times `heartbeatMs` (7.5 s at least) without receiving
   anything, and considers the connection dead after twice `heartbeatMs` (10 s at least).
8. **Reconnection.** A lost connection does not stop a game: the player's clock keeps running
   and the opponent receives `GameEvent{PlayerDisconnected, arg = grace}`. The client reconnects
   with a new `Hello` (`seq` starts again at 1) and receives `Welcome` then `GameSnapshot`. It
   never replays move intents of the old connection blindly: the snapshot says which moves the
   server accepted. The reference client's policy:
   * exponential backoff with full jitter: attempt *n* waits a uniform random time between 0.5 s
     and min(30 s, 2 s x 2^n); *n* returns to 0 only after a connection that stayed up 60 s after
     its `Welcome`;
   * a full server (HTTP 503 at the upgrade, `ServerFull`) is retried after 60 s to 120 s;
   * after a shutdown (`Notice{ServerShutdown}`, `ShuttingDown`), the first attempt waits 5 s to
     35 s whatever *n*, which spreads the reconnection wave of a restart;
   * a `Retry-After` (of an `/api/v1/info` answer, or of an HTTP 429 or 503 at the upgrade) makes
     the next attempt wait at least that long, and up to half more, 10 minutes at most;
   * a player with a game in progress only has the reconnection grace to come back: attempts are
     at most 8 s apart (unless a `Retry-After` asks for longer), and the first one after a
     shutdown waits 1 s to 8 s;
   * for 10 minutes after losing a connection that reached `Welcome`, automatic attempts reuse
     the `/api/v1/info` answer of that connection; a 4xx answer other than 429 at the upgrade
     makes the next attempt read it again;
   * no automatic reconnection after the close codes that say so ([table](#errors-and-close-codes)).
9. **Restarts.** `Notice{ServerShutdown, arg = ms}` announces a restart. Games in progress
   survive it: their players then have `RECOVERY_GRACE_MS` (90 s) to come back, and the clock of
   the side to move stays stopped until its player is back, for `RECOVERY_CLOCK_HOLD_MS` (20 s)
   at most. While it is held, the game's snapshots have `running = None`; when it starts (its
   player is back, or the hold is over), the opponent receives a `GameSnapshot` it did not ask
   for, with that clock running.
10. **Closing.** The client closes with 1000 when the player logs out or quits. The server closes
    with the codes of [Errors and close codes](#errors-and-close-codes).

## Requests and answers

* **seq.** Every C2S message starts with `seq` (u32): 1 for the Hello, then one more for each
  message sent on the connection, whatever its type (pongs and gestures included). The server
  ignores a message whose `seq` is not the previous one plus one, without answering it: a replayed
  or reordered frame has no effect, and after a gap the next message continues from the skipped
  number. A new connection starts again at 1.
* **Error.ref.** The `ref` of an `Error` is the `seq` of the C2S message that caused it. For a
  message that does not decode, it is read from bytes 1 to 4 when the message has at least five
  bytes and a C2S type byte; otherwise, and for an error that no message caused (`Replaced`,
  `ShuttingDown`, a timeout), `ref` is 0.
* **Lobby requests** (`QueueJoin` to `ChallengeJoinCode`) get exactly one answer each: `Ack{ref}`
  or `Error{ref}`. What they cause comes as separate messages (`QueueStatus`, `ChallengeStatus`,
  `ChallengeReceived`, `GameSnapshot`).
* **Game requests** get their effect, or an `Error` with `ref` and `game`:

| Request | Success | Refusal |
|---|---|---|
| `Move` | `MoveMade` to both players | `MoveRejected`, then `GameSnapshot` (mover only) |
| `Resign` | `GameEnd` to both | `Error` |
| `DrawOffer` | `GameEvent{DrawOffered}` to the opponent | `Error` (`DrawOfferLimit`...) |
| `DrawAnswer` | `GameEnd` (accepted) or `GameEvent{DrawDeclined}` to both | `Error` (`NoPendingOffer`...) |
| `DrawClaim` | `GameEnd` to both | `Error{NothingToClaim}` |
| `Abort` | `GameEnd` to both | `Error{AbortNotAllowed}` |
| `Resync` | `GameSnapshot` | `Error{NotInGame}` |
| `Rematch` | `GameEvent{RematchOffered / RematchDeclined}`, or the new game's `GameSnapshot` | `Error{RematchUnavailable}` |
| `Gesture` | relayed to the opponent; never answered | dropped silently ([Gesture relay](#gesture-relay)) |

* `Ping` is answered by `Pong`; `Pong` and `Gesture` are never answered.
* A second `Hello` on a connection gets a non-fatal `Error{ProtocolViolation}`.

## Games

* **Intents, not commands.** Every game message of a client is a request; the server decides. A
  client never shows a move, a result or a clock that the server did not confirm, except the
  local animation of its own move.
* **Start.** A game starts with a `GameSnapshot` sent to both players when the matchmaker, a
  challenge or a rematch creates it. `you` is the receiver's colour (White or Black),
  `firstMoveMs` the time left for the next first move, `autoPress` who presses the clock (below),
  fixed when the game is created (a rematch keeps the value of its game).
* **Moves.** The client checks the move with the same chess rules as the server, then sends
  `Move{seq, game, ply, move, posHash, thinkMs, drawOffer}`. `ply` is the index of the move in the
  game (0 = White's first move), `posHash` the [digest](#moves-and-positions) of the position the
  move is played in, `thinkMs` the time the client measured since its turn began. With
  `autoPress` the move is sent as soon as its destination is chosen (the robot then plays it and
  presses the clock: animation only). Without it, a move placed on the board is sent when the
  player presses the clock (`Gesture.placed` shows it to the opponent meanwhile), so the clock and
  `thinkMs` run until the press, as over the board.
* **Confirmation.** The server validates the move, updates the position and the clocks and sends
  one `MoveMade` to **both** players: the mover's confirmation, the opponent's move to animate. A
  pending draw offer of the opponent is declined by the move (`GameEvent{DrawDeclined}`).
* **Validation order** of a move: not a player of the game, `NotInGame`; game over, `GameOver`;
  `ply` already played, the original `MoveMade` again (byte for byte) when the move is the same,
  else `StalePly`; `posHash` different from the position of the server, `Desync`; `ply` ahead of
  the game, `Desync`; not the sender's turn, `NotYourTurn`; illegal, `IllegalMove`; the sender's
  flag fell, `FlagFell`. Then the move is played.
* **Rejection.** `MoveRejected{game, ply, move, code}` goes to the mover only (the opponent never
  sees a rejected move) and is **always** followed by a `GameSnapshot`, from which the client
  restores the position and the clocks. `FlagFell` is also followed by `GameEnd`.
* **Ordering: gseq.** Every game event (`MoveMade`, `GameEvent`, `GameEnd`) carries `gseq`, the
  game's event number, one more than the previous event's; a `GameSnapshot` carries the `gseq` of
  the last event it includes. A client applies an event only when its `gseq` is greater than the
  `gseq` of the state it shows, and ignores it otherwise (an event the snapshot already contains).
  When an event's `gseq` is more than one above that state, an event was missed: the client sends
  `Resync`.
* **Draws.** `DrawOffer` (or a `Move` with `drawOffer`) sends `GameEvent{DrawOffered}` to the
  opponent, who answers with `DrawAnswer` (or declines by moving). A player may offer at most
  `DRAW_OFFERS_PER_GAME` draws per game, and none within 10 plies after a decline
  (`DrawOfferLimit`). `DrawClaim` succeeds on a threefold repetition or after 100 halfmoves
  without capture or pawn move, else `NothingToClaim`. Checkmate, stalemate, insufficient
  material, fivefold repetition and the 75-move rule end the game by themselves.
* **Resign, abort.** `Resign` at any time while the game runs (leaving a running game from the
  menu resigns it). `Abort` only before one's own first move (`AbortNotAllowed` otherwise); an
  aborted game is unrated.
* **End.** `GameEnd{status, reason, whiteMs, blackMs, serverTime}` goes to both players. For a
  rated game, `RatingUpdate` follows once the result is committed. A game ends after
  `MaxPlies` (1200) plies at the latest (`ServerAborted`).
* **Rematch.** Within 60 s of the end, `Rematch{accept: true}` offers a rematch (the opponent
  receives `GameEvent{RematchOffered}`) or accepts the opponent's offer: the server then creates
  the new game, colours swapped, and sends its `GameSnapshot` to both. `Rematch{accept: false}`
  declines or withdraws (`GameEvent{RematchDeclined}`); the offer also lapses when a player leaves
  (`RematchDeclined` with color None).
* **Disconnection.** The opponent receives `GameEvent{PlayerDisconnected, arg = grace}` and then
  `PlayerReconnected`. The grace is clamp(base time / 10, `RECONNECT_GRACE_MIN_MS` (15 s),
  `RECONNECT_GRACE_MAX_MS`); after it the absent player loses by `Abandonment` (a draw,
  `AbandonmentVsInsufficient`, when the opponent cannot mate; the game is aborted before the
  second ply).

## Clocks

* The server is the only authority on time. The client shows the clocks of the last
  `GameSnapshot`, `MoveMade` or `GameEnd` with its estimate of the server clock (`serverNow =
  localNow + offset`, the offset from `Welcome.serverTime` and the ping exchanges): the running
  side shows `xMs - (serverNow - serverTime)`, the other side `xMs` as sent.
* `whiteMs` and `blackMs` are the remaining times **at `serverTime`**. Every `serverTime` of every
  message comes from the same monotonic epoch clock of the server, so an offset measured with one
  message applies to all.
* `running` (snapshots) is the side whose clock runs from `serverTime`: None before the clocks
  start, after the end, while a first move is awaited, and while the clock of a restored game is
  held. In `MoveMade`, the clock of the side to move runs from `serverTime` unless
  `firstMoveMs > 0`.
* **First moves.** Plies 0 and 1 run no clock: each player has `firstMoveMs`
  (`FIRST_MOVE_TIMEOUT_MS`) to make their first move, otherwise the game is aborted (`NoShow`,
  unrated). They get no increment. The clocks start with White's second move.
* **Charge.** For every later move the server charges the time from the moment it sent the
  opponent's `MoveMade` to the moment it received the move, minus a lag compensation bounded by
  the client's `thinkMs`, the measured round trip, `LAG_COMP_MAX_MS` and a per-player quota.
  `thinkMs` never adds time. The increment is added after the move: `MoveMade.spentMs` is the time
  charged, `MoveRec.clockMs` the mover's remaining time after the increment.
* **Flags** fall on the server: `GameEnd{Timeout}`, or `TimeoutVsInsufficient` (a draw) when the
  opponent cannot mate. A move arriving after that is refused with `FlagFell`. A client never ends
  a game on its own clock.

## Gesture relay

* **What.** `Gesture` carries a player's live, cosmetic state to the opponent, whose robot mirrors
  it: the head (`yaw` and `pitch` of the look in milliradians, seat-relative, 0 straight ahead and
  level, `yaw` > 0 to the left, `pitch` < 0 down; `lean` towards the board in percent; the
  `GestureFlag` bits for a glance at one's scoresheet and a look at the table beside the board),
  the piece in hand (`touch`) and where it is aimed (`aim`), and the move placed on the board
  before the clock press (`placed`, games without `autoPress`). It is never authoritative: only
  `Move` plays a move.
* **Sending.** A client sends `Gesture{seq, game, ply, ...}` for its game in progress when its
  state changes, and at least once every `Welcome.gestureIdleMs` even when nothing changed, on
  both players' turns, as long as the game exists (the rematch window included). It always sends
  the whole state (a lost gesture heals with the next one), at most `Welcome.gestureRate` per
  second with bursts of `Welcome.gestureBurst` (`GESTURE_RATE` 4 and `GESTURE_BURST` 8). When
  `gestureRate` is 0 the server relays nothing, `gestureIdleMs` is 0 and the client sends none. A
  gesture takes the next `seq` like any message.
* **Keepalive.** `gestureIdleMs` is the server's `GESTURE_IDLE_MS` (1000 ms by default, 1000 to
  10000). A client clamps it to 1000 .. 10000 ms, so that both players of a game derive the same
  interval from it. The keepalives of players who sit still are most of the relay's work in a
  calm game: a longer interval saves server CPU, and receivers then take longer to notice that
  the gestures stopped (below).
* **ply** is the number of plies played when the current state of the hand (`touch`, `aim`,
  `placed`, `Promoting`) began: while a move is being prepared, the ply of that move. A change of
  the head alone keeps it.
* **Relay.** The server sends the opponent the S2C `Gesture`: the C2S message without its `seq`,
  copied byte for byte after checking that it decodes. It never goes back to the sender and is
  never stored, timed or looked at otherwise.
* **Receiving.** The latest gesture wins. Its head (`yaw`, `pitch`, `lean`, `Glance`, `Side`)
  always applies. Its hand (`touch`, `aim`, `placed`, `Promoting`) applies only while the
  receiver's game has exactly `ply` plies and it is the sender's turn: an earlier ply is stale,
  a later one is ahead of a `MoveMade` not received yet. Gestures come from the other client: a
  receiver checks the hand against its own position before showing it.
* **Silence.** The keepalive tells a receiver an opponent who sits still from one whose gestures
  stopped coming. The game client follows the head of the latest gesture for 2.5 x
  `gestureIdleMs` (the clamped interval; 2.5 s at the default) and puts a piece held live back
  after 5 x `gestureIdleMs` without a gesture; a move placed on the board waits 5 x
  `gestureIdleMs` for its `MoveMade` once the gestures show something else, then is taken back.
* **Silent drops.** Gestures have a token bucket of their own, apart from the message rate limit.
  A gesture beyond `gestureRate` is dropped without an `Error` (its `seq` still counts). Only a
  gross excess closes the connection (`Flood`): more than max(50, 10 x `gestureBurst`,
  `gestureRate` x (heartbeat timeout + `heartbeatMs` + 0.25 s)) drops in 10 s. A gesture for
  another game, for an absent opponent, or towards a connection that already has a backlog is
  dropped as well: a slow link loses gestures first.

## Rate limits

* **Messages.** Each connection has a token bucket of `Welcome.maxMsgPerSec` messages per second
  (`WS_MSG_RATE`, 20) holding `Welcome.msgBurst` tokens (`WS_MSG_BURST`, 40). A message beyond it
  is dropped and answered with `Error{RateLimited}` (at most one such error per second); repeated
  excess is a fatal `Flood` (the reference server: more than max(10, `msgBurst`) messages dropped
  within 10 s). Gestures use their own bucket (above).
* **Client pings** beyond one per 950 ms get no `Pong`.
* **Connections.** The server limits simultaneous WebSocket connections per address
  (`MAX_CONNECTIONS_PER_IP`, HTTP 429 `too_many_connections` at the upgrade) and players on the
  whole server (`MAX_CONNECTIONS`: `ServerFull` at `Hello`, except for a player whose game is in
  progress; upgrades may go max(16, 2 %) beyond it for such players, and beyond that reserve an
  upgrade gets HTTP 503 `server_full`).
* **Waiting after a refused upgrade.** Every HTTP 429 or 503 answer to an upgrade carries
  `Retry-After` (seconds) and the same value as `retryAfter` in its JSON body. For 429
  `rate_limited` (the per-address limit of every HTTP request, upgrades included: `docs/API.md`)
  it is the time until the address may send a request again; for the others
  (`too_many_connections`, `server_full`, `shutting_down` while the server drains) a random 2 s to
  5 s. The client waits at least that long before its next attempt
  ([Reconnection](#connection-lifecycle)).
* **Slow consumers.** A client that does not read its messages (more than `WS_SEND_BUFFER_LIMIT`
  bytes queued) is closed with 4303 and no `Error`; it reconnects and resynchronises.
* **Lobby limits** have their own errors: `ChallengeLimit`, `RateLimited` for wrong private codes,
  `MatchmakingCooldown` (with `Notice{MatchmakingCooldown, arg = end}`), `RatedRepeatLimit`.

## Errors and close codes

* `Error{ref, code, fatal, game}`: `ref` as in [Requests and answers](#requests-and-answers),
  `game` the game concerned (0 = none). Codes 1 to 99 concern the connection, 100 to 199 games,
  200 to 239 the lobby, 240 to 255 violations.
* **The close-code rule.** A fatal `Error` is the server's last message: it closes the connection
  right after it with close code `4000 + code` for a code from 1 to 99, and `4300 + (code - 240)`
  for a code from 240 to 255. Codes from 100 to 239 are never fatal. The only private close code
  without an `Error` is 4303 (`SlowConsumer`, whose error code 243 is reserved). A client that
  receives a close code of the rule without the `Error` acts as if it had received it.
* **Malformed messages.** A C2S message that does not decode is `Error{Malformed, fatal}` (4001).
  A C2S message with an S2C type byte is a forgery: `CheatDetected` (4302), or `ProtocolViolation`
  (4300) when the server does not sanction it automatically. A game error is never fatal by itself,
  except when the anti-cheat sanctions a certain cheat (`CheatDetected`).
* **A client that cannot decode** a server message (one that breaks a rule a lenient decoder
  applies) ignores it and logs it; it never acts on part of a message. What it missed shows later
  (a `MoveMade` beyond the next ply, an event `gseq` that skips one), and a `Resync` repairs it.
* **Unknown error codes** (a later minor) are a generic refusal of the request quoted by `ref`;
  when `fatal`, the connection closes as the rule says.

<!-- protogen:begin close-codes -->

| Code | Name | After | Meaning |
|---|---|---|---|
| 1000 | `Normal` | WebSocket | normal closure (a client that logs out or quits) |
| 1001 | `GoingAway` | WebSocket | heartbeat timeout: nothing received from the client for the server's HEARTBEAT_TIMEOUT_MS |
| 1002 | `ProtocolError` | WebSocket | WebSocket protocol error (unmasked client frame, reserved bits or opcode, bad control frame) |
| 1003 | `Unsupported` | WebSocket | a text frame |
| 1009 | `TooBig` | WebSocket | a client message larger than MaxClientMessage |
| 4001 | `Malformed` | fatal `Error` Malformed (1) | a client message that does not decode; reconnect with backoff |
| 4002 | `UnsupportedProtocol` | fatal `Error` UnsupportedProtocol (2) | no automatic retry: update the game |
| 4003 | `Unauthorized` | fatal `Error` Unauthorized (3) | no automatic retry: log in again |
| 4004 | `Banned` | fatal `Error` Banned (4) | no automatic retry |
| 4006 | `ServerFull` | fatal `Error` ServerFull (6) | retried after 60 s to 120 s |
| 4007 | `Replaced` | fatal `Error` Replaced (7) | no automatic retry |
| 4008 | `ShuttingDown` | fatal `Error` ShuttingDown (8) | reconnect later (restart backoff) |
| 4009 | `Internal` | fatal `Error` Internal (9) | an unexpected server error; reconnect with backoff |
| 4010 | `HelloRequired` | fatal `Error` HelloRequired (10) | no Hello within WS_HELLO_TIMEOUT_MS, or another message first |
| 4011 | `EmailUnverified` | fatal `Error` EmailUnverified (11) | no automatic retry: verify the e-mail address |
| 4300 | `ProtocolViolation` | fatal `Error` ProtocolViolation (240) | a client message out of place (a Hello whose seq is not 1, a forged type the server does not sanction) |
| 4301 | `Flood` | fatal `Error` Flood (241) | rate limits exceeded again and again |
| 4302 | `CheatDetected` | fatal `Error` CheatDetected (242) | no automatic retry |
| 4303 | `SlowConsumer` | no `Error` | the client does not read its messages (more than WS_SEND_BUFFER_LIMIT bytes queued): no Error precedes it; 4300 + (243 - 240), 243 being the reserved code SlowConsumer |

<!-- protogen:end close-codes -->

## Moves and positions

* **Moves** are a u16 `from | to << 6 | promo << 12`. Squares are numbered `file + 8 * rank`
  (a1 = 0, b1 = 1, ..., h8 = 63); `promo` is 0 (none), 2 (knight), 3 (bishop), 4 (rook) or
  5 (queen); castling is the king's two-square move (e1g1, e1c1, e8g8, e8c8); bit 15 is always 0.
  Examples: e2e4 = 12 | 28 << 6 = `0x070C` (1804); e7e8=Q = 52 | 60 << 6 | 5 << 12 = `0x5F34`;
  e1g1 = 4 | 6 << 6 = `0x0184`. `Gesture.placed` 0 (a1a1) means no move. `MoveMade.flags` is a set
  of [`MoveFlag`](#flags) bits describing the move as played.
* **posHash** is the FNV-1a 32-bit hash of the ASCII text of the first four FEN fields
  (placement, side to move, castling rights in `KQkq` order or `-`, en passant square) separated by
  single spaces. The en passant square is written **only when an en passant capture is legal**,
  `-` otherwise. FNV-1a 32: start with `0x811C9DC5`; for each byte, `h = (h XOR byte) x
  0x01000193 mod 2^32`. The start position `rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq -`
  hashes to `0x3706291C`; the vectors list more.
* **Identifiers.** Game ids are id53 values (the reference server builds them from the creation
  time); user ids and challenge ids are u32. 0 means none.

## Messages

Every message of both directions, then the fields of each one with their offsets (while they are
fixed) and bounds. Sizes include the type byte.

<!-- protogen:begin messages -->

| Type | Message | Dir | Size (bytes) | Summary |
|---|---|---|---|---|
| `0x01` | [Hello](#01-hello) | c2s | 35..227 | First message of a connection. |
| `0x02` | [C_Ping](#02-c-ping) | c2s | 9 | Round-trip and clock-offset probe of the client, answered by the server Pong. |
| `0x03` | [C_Pong](#03-c-pong) | c2s | 9 | Answer to the server Ping, sent at once (the server measures the latency with it). |
| `0x10` | [QueueJoin](#10-queuejoin) | c2s | 10..14 | Join the matchmaking queue of an official category. |
| `0x11` | [QueueLeave](#11-queueleave) | c2s | 5 | Leave the matchmaking queue. |
| `0x12` | [ChallengeCreate](#12-challengecreate) | c2s | 11..35 | Challenge a player by name, or (empty target) create a private game joined with a code. |
| `0x13` | [ChallengeAccept](#13-challengeaccept) | c2s | 9 | Accept a challenge received. |
| `0x14` | [ChallengeDecline](#14-challengedecline) | c2s | 9 | Decline a challenge received. |
| `0x15` | [ChallengeCancel](#15-challengecancel) | c2s | 9 | Withdraw one's own challenge or private game. |
| `0x16` | [ChallengeJoinCode](#16-challengejoincode) | c2s | 10..18 | Join a private game by its code. |
| `0x20` | [Move](#20-move) | c2s | 26 | Move intent for ply `ply` of game `game`. |
| `0x21` | [Resign](#21-resign) | c2s | 13 | Resign a running game. |
| `0x22` | [DrawOffer](#22-drawoffer) | c2s | 13 | Offer a draw. |
| `0x23` | [DrawAnswer](#23-drawanswer) | c2s | 14 | Answer the opponent's draw offer: GameEnd (accepted) or GameEvent DrawDeclined follows, or Error. |
| `0x24` | [DrawClaim](#24-drawclaim) | c2s | 13 | Claim a draw by threefold repetition or the fifty-move rule in the current position. |
| `0x25` | [Abort](#25-abort) | c2s | 13 | Abort the game before one's own first move (unrated). |
| `0x26` | [Resync](#26-resync) | c2s | 13 | Ask for the game's GameSnapshot. |
| `0x27` | [Rematch](#27-rematch) | c2s | 14 | After the end: accept = true offers (or accepts) a rematch with colours swapped, accept = false declines or withdraws. |
| `0x28` | [C_Gesture](#28-c-gesture) | c2s | 29 | The player's live, cosmetic state in game `game`, relayed to the opponent byte for byte (server Gesture) and never answered, stored or looked at beyond decoding. |
| `0x80` | [Welcome](#80-welcome) | s2c | 54..141 | Hello accepted. |
| `0x81` | [Error](#81-error) | s2c | 15 | A request was refused. |
| `0x82` | [S_Ping](#82-s-ping) | s2c | 13 | Heartbeat, about every Welcome.heartbeatMs: answer with the client Pong at once. |
| `0x83` | [S_Pong](#83-s-pong) | s2c | 13 | Answer to the client Ping. |
| `0x84` | [Ack](#84-ack) | s2c | 5 | A lobby request (QueueJoin ... |
| `0x85` | [Notice](#85-notice) | s2c | 10 | An announcement from the server (NoticeCode). |
| `0x90` | [QueueStatus](#90-queuestatus) | s2c | 14..21 | The player's state in a matchmaking queue: on join, every few seconds while searching, when matched and when left. |
| `0x91` | [ChallengeReceived](#91-challengereceived) | s2c | 23..46 | A player challenges the receiver. |
| `0x92` | [ChallengeStatus](#92-challengestatus) | s2c | 12..48 | State of a challenge for its creator (and for its receiver when it is cancelled or expires). |
| `0xA0` | [GameSnapshot](#a0-gamesnapshot) | s2c | 85..12137 | Complete authoritative state of a game: when it starts (both players), after Welcome when activeGame != 0, on Resync, after every MoveRejected, and unsolicited to the opponent when the held clock of a game restored after a restart starts. |
| `0xA1` | [MoveMade](#a1-movemade) | s2c | 43 | A move accepted by the server, to both players (for the mover it is the confirmation). |
| `0xA2` | [MoveRejected](#a2-moverejected) | s2c | 14 | The move intent was refused (to the mover only; never shown to the opponent). |
| `0xA3` | [GameEvent](#a3-gameevent) | s2c | 19 | Something happened in a game (GameEventKind). |
| `0xA4` | [GameEnd](#a4-gameend) | s2c | 31 | Final result, to both players. |
| `0xA5` | [RatingUpdate](#a5-ratingupdate) | s2c | 29..35 | Rating changes of a finished rated game, once the result is committed. |
| `0xA6` | [S_Gesture](#a6-s-gesture) | s2c | 25 | The opponent's gestures: the client Gesture without its seq, byte for byte. |

<!-- protogen:end messages -->

<!-- protogen:begin fields -->

<a id="01-hello"></a>
#### `0x01` Hello (c2s, 35 to 227 bytes)

First message of a connection. Its first five fields (the Hello prefix: type, seq, proto, minor, caps) are frozen for every version, so that any server can read them and answer any client.

| Offset | Field | Type | Bounds | Meaning |
|---|---|---|---|---|
| 1 | `seq` | `u32` |  | 1 |
| 5 | `proto` | `u16` |  | major protocol version: 1 |
| 7 | `minor` | `u16` |  | highest minor version the client speaks |
| 9 | `caps` | `u64` |  | capabilities the client supports (bit set) |
| 17 | `client` | `str8` | 0..48 bytes | client name and version, for the logs ("Scacelith/1.5.0 win64") |
| ... | `token` | `str8` | 16..160 bytes | session token from the HTTPS login of this server |

<a id="02-c-ping"></a>
#### `0x02` C_Ping (c2s, 9 bytes)

Round-trip and clock-offset probe of the client, answered by the server Pong. At most one is answered per 950 ms.

| Offset | Field | Type | Bounds | Meaning |
|---|---|---|---|---|
| 1 | `seq` | `u32` |  | number of the message on its connection: 1 for the Hello, then one more per message |
| 5 | `nonce` | `u32` |  |  |

<a id="03-c-pong"></a>
#### `0x03` C_Pong (c2s, 9 bytes)

Answer to the server Ping, sent at once (the server measures the latency with it).

| Offset | Field | Type | Bounds | Meaning |
|---|---|---|---|---|
| 1 | `seq` | `u32` |  | number of the message on its connection: 1 for the Hello, then one more per message |
| 5 | `nonce` | `u32` |  | nonce of the server Ping |

<a id="10-queuejoin"></a>
#### `0x10` QueueJoin (c2s, 10 to 14 bytes)

Join the matchmaking queue of an official category. Answered by Ack or Error; QueueStatus messages follow.

| Offset | Field | Type | Bounds | Meaning |
|---|---|---|---|---|
| 1 | `seq` | `u32` |  | number of the message on its connection: 1 for the Hello, then one more per message |
| 5 | `category` | `str8` | 3..7 bytes | category id ("3+2") |
| ... | `rated` | `bool` |  | false: casual queue |

<a id="11-queueleave"></a>
#### `0x11` QueueLeave (c2s, 5 bytes)

Leave the matchmaking queue. Answered by Ack or Error; QueueStatus Left follows.

| Offset | Field | Type | Bounds | Meaning |
|---|---|---|---|---|
| 1 | `seq` | `u32` |  | number of the message on its connection: 1 for the Hello, then one more per message |

<a id="12-challengecreate"></a>
#### `0x12` ChallengeCreate (c2s, 11 to 35 bytes)

Challenge a player by name, or (empty target) create a private game joined with a code. Rated only with an official time control. Answered by Ack or Error; ChallengeStatus follows (with the code of a private game).

| Offset | Field | Type | Bounds | Meaning |
|---|---|---|---|---|
| 1 | `seq` | `u32` |  | number of the message on its connection: 1 for the Hello, then one more per message |
| 5 | `target` | `str8` | 0..24 bytes | username, or empty for a private game |
| ... | `baseSec` | `u16` | 15..10800 |  |
| ... | `incSec` | `u8` | 0..180 |  |
| ... | `rated` | `bool` |  |  |
| ... | `color` | enum [ColorPref](#colorpref) |  | colour asked for by the challenger |

<a id="13-challengeaccept"></a>
#### `0x13` ChallengeAccept (c2s, 9 bytes)

Accept a challenge received. Answered by Ack or Error; the game's GameSnapshot follows.

| Offset | Field | Type | Bounds | Meaning |
|---|---|---|---|---|
| 1 | `seq` | `u32` |  | number of the message on its connection: 1 for the Hello, then one more per message |
| 5 | `id` | `u32` |  | challenge id (ChallengeReceived.id) |

<a id="14-challengedecline"></a>
#### `0x14` ChallengeDecline (c2s, 9 bytes)

Decline a challenge received. Answered by Ack or Error.

| Offset | Field | Type | Bounds | Meaning |
|---|---|---|---|---|
| 1 | `seq` | `u32` |  | number of the message on its connection: 1 for the Hello, then one more per message |
| 5 | `id` | `u32` |  |  |

<a id="15-challengecancel"></a>
#### `0x15` ChallengeCancel (c2s, 9 bytes)

Withdraw one's own challenge or private game. Answered by Ack or Error.

| Offset | Field | Type | Bounds | Meaning |
|---|---|---|---|---|
| 1 | `seq` | `u32` |  | number of the message on its connection: 1 for the Hello, then one more per message |
| 5 | `id` | `u32` |  |  |

<a id="16-challengejoincode"></a>
#### `0x16` ChallengeJoinCode (c2s, 10 to 18 bytes)

Join a private game by its code. Answered by Ack or Error; the game's GameSnapshot follows.

| Offset | Field | Type | Bounds | Meaning |
|---|---|---|---|---|
| 1 | `seq` | `u32` |  | number of the message on its connection: 1 for the Hello, then one more per message |
| 5 | `code` | `str8` | 4..12 bytes | code as shown ("K7QX-9M2P"); the server ignores dashes, spaces and case |

<a id="20-move"></a>
#### `0x20` Move (c2s, 26 bytes)

Move intent for ply `ply` of game `game`. Answered by MoveMade (to both players), or by MoveRejected followed by a GameSnapshot (to the mover only).

| Offset | Field | Type | Bounds | Meaning |
|---|---|---|---|---|
| 1 | `seq` | `u32` |  | number of the message on its connection: 1 for the Hello, then one more per message |
| 5 | `game` | `id53` | < 2^53 |  |
| 13 | `ply` | `u16` | 0..1199 | index of the move in the game (0 = White's first move) |
| 15 | `move` | `u16` | 0..32767 | packed move (Move encoding) |
| 17 | `posHash` | `u32` |  | digest of the position the move is played in |
| 21 | `thinkMs` | `u32` |  | client-measured time since the turn began (bounds the lag compensation only) |
| 25 | `drawOffer` | `bool` |  | the move comes with a draw offer |

<a id="21-resign"></a>
#### `0x21` Resign (c2s, 13 bytes)

Resign a running game. GameEnd follows, or Error.

| Offset | Field | Type | Bounds | Meaning |
|---|---|---|---|---|
| 1 | `seq` | `u32` |  | number of the message on its connection: 1 for the Hello, then one more per message |
| 5 | `game` | `id53` | < 2^53 |  |

<a id="22-drawoffer"></a>
#### `0x22` DrawOffer (c2s, 13 bytes)

Offer a draw. The opponent receives GameEvent DrawOffered; a refusal is an Error.

| Offset | Field | Type | Bounds | Meaning |
|---|---|---|---|---|
| 1 | `seq` | `u32` |  | number of the message on its connection: 1 for the Hello, then one more per message |
| 5 | `game` | `id53` | < 2^53 |  |

<a id="23-drawanswer"></a>
#### `0x23` DrawAnswer (c2s, 14 bytes)

Answer the opponent's draw offer: GameEnd (accepted) or GameEvent DrawDeclined follows, or Error.

| Offset | Field | Type | Bounds | Meaning |
|---|---|---|---|---|
| 1 | `seq` | `u32` |  | number of the message on its connection: 1 for the Hello, then one more per message |
| 5 | `game` | `id53` | < 2^53 |  |
| 13 | `accept` | `bool` |  |  |

<a id="24-drawclaim"></a>
#### `0x24` DrawClaim (c2s, 13 bytes)

Claim a draw by threefold repetition or the fifty-move rule in the current position. GameEnd follows, or Error NothingToClaim.

| Offset | Field | Type | Bounds | Meaning |
|---|---|---|---|---|
| 1 | `seq` | `u32` |  | number of the message on its connection: 1 for the Hello, then one more per message |
| 5 | `game` | `id53` | < 2^53 |  |

<a id="25-abort"></a>
#### `0x25` Abort (c2s, 13 bytes)

Abort the game before one's own first move (unrated). GameEnd follows, or Error AbortNotAllowed.

| Offset | Field | Type | Bounds | Meaning |
|---|---|---|---|---|
| 1 | `seq` | `u32` |  | number of the message on its connection: 1 for the Hello, then one more per message |
| 5 | `game` | `id53` | < 2^53 |  |

<a id="26-resync"></a>
#### `0x26` Resync (c2s, 13 bytes)

Ask for the game's GameSnapshot.

| Offset | Field | Type | Bounds | Meaning |
|---|---|---|---|---|
| 1 | `seq` | `u32` |  | number of the message on its connection: 1 for the Hello, then one more per message |
| 5 | `game` | `id53` | < 2^53 |  |

<a id="27-rematch"></a>
#### `0x27` Rematch (c2s, 14 bytes)

After the end: accept = true offers (or accepts) a rematch with colours swapped, accept = false declines or withdraws. GameEvent or the new game's GameSnapshot follows, or Error RematchUnavailable.

| Offset | Field | Type | Bounds | Meaning |
|---|---|---|---|---|
| 1 | `seq` | `u32` |  | number of the message on its connection: 1 for the Hello, then one more per message |
| 5 | `game` | `id53` | < 2^53 |  |
| 13 | `accept` | `bool` |  |  |

<a id="28-c-gesture"></a>
#### `0x28` C_Gesture (c2s, 29 bytes)

The player's live, cosmetic state in game `game`, relayed to the opponent byte for byte (server Gesture) and never answered, stored or looked at beyond decoding. Sent when it changes and at least every Welcome.gestureIdleMs, paced by Welcome.gestureRate and gestureBurst.

| Offset | Field | Type | Bounds | Meaning |
|---|---|---|---|---|
| 1 | `seq` | `u32` |  | number of the message on its connection: 1 for the Hello, then one more per message |
| 5 | `game` | `id53` | < 2^53 |  |
| 13 | `ply` | `u16` | 0..1199 | plies played when the current state of the hand (touch, aim, placed, Promoting) began |
| 15 | `touch` | `u8` | 0..64 | square of the piece in hand (64 = none) |
| 16 | `aim` | `u8` | 0..64 | square the piece is aimed at (64 = none) |
| 17 | `placed` | `u16` | 0..32767 | move placed on the board before the clock press, packed (0 = none) |
| 19 | `flags` | `u8` | 0..7 | GestureFlag bits |
| 20 | `yaw` | `i32` | -3142..3142 | look, milliradians, seat-relative, > 0 to the left |
| 24 | `pitch` | `i32` | -1571..1571 | look, milliradians, < 0 down |
| 28 | `lean` | `u8` | 0..100 | lean towards the board, percent |

<a id="80-welcome"></a>
#### `0x80` Welcome (s2c, 54 to 141 bytes)

Hello accepted. Its first three fields (proto, minor, caps) are frozen for every version. When activeGame != 0 a GameSnapshot of that game follows.

| Offset | Field | Type | Bounds | Meaning |
|---|---|---|---|---|
| 1 | `proto` | `u16` |  | 1 |
| 3 | `minor` | `u16` |  | negotiated minor: min(Hello.minor, server minor) |
| 5 | `caps` | `u64` |  | negotiated capabilities: Hello.caps AND server capabilities |
| 13 | `serverTime` | `f64` | finite | server clock (epoch ms) when the session was accepted |
| 21 | `userId` | `u32` |  |  |
| 25 | `username` | `str8` | 1..24 bytes |  |
| ... | `serverName` | `str8` | 0..64 bytes |  |
| ... | `heartbeatMs` | `u32` |  | interval of the server Ping (HEARTBEAT_INTERVAL_MS) |
| ... | `clientPingMs` | `u32` |  | interval of the client Ping the server wants (0 = the client's default, 10 s) |
| ... | `maxMsgPerSec` | `u16` |  | messages per second a client may send, sustained (WS_MSG_RATE) |
| ... | `msgBurst` | `u16` |  | message burst a client may send above it (WS_MSG_BURST) |
| ... | `activeGame` | `id53` | < 2^53 | the player's game in progress (0 = none) |
| ... | `gestureRate` | `u16` | 0..60 | gestures per second the server relays, sustained (0 = no relay: send none) |
| ... | `gestureBurst` | `u16` | 0..120 | gesture burst above gestureRate (0 when gestureRate is 0) |
| ... | `gestureIdleMs` | `u16` |  | longest time a client lets pass without sending a gesture for its game in progress while nothing changes, ms (GESTURE_IDLE_MS; 0 when gestureRate is 0; a client clamps it to 1000..10000) |

<a id="81-error"></a>
#### `0x81` Error (s2c, 15 bytes)

A request was refused. Frozen layout for every version. fatal: the server closes the connection right after it (close code 4000 + code or 4300 + (code - 240)).

| Offset | Field | Type | Bounds | Meaning |
|---|---|---|---|---|
| 1 | `ref` | `u32` |  | seq of the message refused (0 when no message caused it) |
| 5 | `code` | enum [ErrorCode](#errorcode) |  |  |
| 6 | `fatal` | `bool` |  |  |
| 7 | `game` | `id53` | < 2^53 | game concerned (0 = none) |

<a id="82-s-ping"></a>
#### `0x82` S_Ping (s2c, 13 bytes)

Heartbeat, about every Welcome.heartbeatMs: answer with the client Pong at once.

| Offset | Field | Type | Bounds | Meaning |
|---|---|---|---|---|
| 1 | `nonce` | `u32` |  |  |
| 5 | `serverTime` | `f64` | finite | server clock (epoch ms) when sent |

<a id="83-s-pong"></a>
#### `0x83` S_Pong (s2c, 13 bytes)

Answer to the client Ping.

| Offset | Field | Type | Bounds | Meaning |
|---|---|---|---|---|
| 1 | `nonce` | `u32` |  | nonce of the client Ping |
| 5 | `serverTime` | `f64` | finite | server clock (epoch ms) when sent |

<a id="84-ack"></a>
#### `0x84` Ack (s2c, 5 bytes)

A lobby request (QueueJoin ... ChallengeJoinCode) succeeded: each one gets exactly one Ack or one Error.

| Offset | Field | Type | Bounds | Meaning |
|---|---|---|---|---|
| 1 | `ref` | `u32` |  | seq of the request |

<a id="85-notice"></a>
#### `0x85` Notice (s2c, 10 bytes)

An announcement from the server (NoticeCode).

| Offset | Field | Type | Bounds | Meaning |
|---|---|---|---|---|
| 1 | `code` | enum [NoticeCode](#noticecode) |  |  |
| 2 | `arg` | `f64` | finite | see NoticeCode (0 when unused) |

<a id="90-queuestatus"></a>
#### `0x90` QueueStatus (s2c, 14 to 21 bytes)

The player's state in a matchmaking queue: on join, every few seconds while searching, when matched and when left.

| Offset | Field | Type | Bounds | Meaning |
|---|---|---|---|---|
| 1 | `category` | `str8` | 0..7 bytes | category id (may be empty for Left) |
| ... | `rated` | `bool` |  |  |
| ... | `state` | enum [QueueState](#queuestate) |  |  |
| ... | `waitMs` | `u32` |  | time spent in the queue |
| ... | `window` | `u16` |  | current rating window (Searching; 0 otherwise) |
| ... | `queued` | `u32` |  | players in this queue (Searching; 0 otherwise) |

<a id="91-challengereceived"></a>
#### `0x91` ChallengeReceived (s2c, 23 to 46 bytes)

A player challenges the receiver.

| Offset | Field | Type | Bounds | Meaning |
|---|---|---|---|---|
| 1 | `id` | `u32` |  | challenge id |
| 5 | `from` | [PlayerInfo](#playerinfo) |  |  |
| ... | `baseSec` | `u16` | 15..10800 |  |
| ... | `incSec` | `u8` | 0..180 |  |
| ... | `rated` | `bool` |  |  |
| ... | `yourColor` | enum [ColorPref](#colorpref) |  | colour offered to the receiver |
| ... | `expiresMs` | `u32` |  | time left to accept |

<a id="92-challengestatus"></a>
#### `0x92` ChallengeStatus (s2c, 12 to 48 bytes)

State of a challenge for its creator (and for its receiver when it is cancelled or expires).

| Offset | Field | Type | Bounds | Meaning |
|---|---|---|---|---|
| 1 | `id` | `u32` |  |  |
| 5 | `state` | enum [ChallengeState](#challengestate) |  |  |
| 6 | `target` | `str8` | 0..24 bytes | username challenged (empty for a private game) |
| ... | `code` | `str8` | 0..12 bytes | code of a private game (empty otherwise) |
| ... | `baseSec` | `u16` | 15..10800 |  |
| ... | `incSec` | `u8` | 0..180 |  |
| ... | `rated` | `bool` |  |  |

<a id="a0-gamesnapshot"></a>
#### `0xA0` GameSnapshot (s2c, 85 to 12137 bytes)

Complete authoritative state of a game: when it starts (both players), after Welcome when activeGame != 0, on Resync, after every MoveRejected, and unsolicited to the opponent when the held clock of a game restored after a restart starts. Clocks are the remaining times at serverTime; the `running` side counts down from there.

| Offset | Field | Type | Bounds | Meaning |
|---|---|---|---|---|
| 1 | `game` | `id53` | < 2^53 |  |
| 9 | `gseq` | `u32` |  | number of the game's last event (MoveMade, GameEvent, GameEnd) included in this state |
| 13 | `category` | `str8` | 1..7 bytes | official category id, or "custom" |
| ... | `baseMs` | `u32` |  |  |
| ... | `incMs` | `u32` |  |  |
| ... | `rated` | `bool` |  |  |
| ... | `white` | [PlayerInfo](#playerinfo) |  |  |
| ... | `black` | [PlayerInfo](#playerinfo) |  |  |
| ... | `you` | enum [Color](#color) |  | the receiver's colour (White or Black) |
| ... | `moves` | list16 of [MoveRec](#moverec) | at most 1200 items |  |
| ... | `running` | enum [Color](#color) |  | side whose clock runs from serverTime (None: no clock runs) |
| ... | `whiteMs` | `u32` |  |  |
| ... | `blackMs` | `u32` |  |  |
| ... | `serverTime` | `f64` | finite |  |
| ... | `drawOffer` | enum [Color](#color) |  | side with a pending draw offer (None: none) |
| ... | `status` | enum [GameStatus](#gamestatus) |  |  |
| ... | `reason` | enum [EndReason](#endreason) |  |  |
| ... | `whiteConnected` | `bool` |  |  |
| ... | `blackConnected` | `bool` |  |  |
| ... | `graceMs` | `u32` |  | reconnection grace of this game |
| ... | `firstMoveMs` | `u32` |  | time left for the next player's first move (plies 0 and 1; 0 otherwise) |
| ... | `startedAt` | `f64` | finite | server clock (epoch ms) when the game was created |
| ... | `rematch` | enum [Color](#color) |  | side with a pending rematch offer (None: none) |
| ... | `autoPress` | `bool` |  | the robots press the clock by themselves (fixed when the game was created) |

<a id="a1-movemade"></a>
#### `0xA1` MoveMade (s2c, 43 bytes)

A move accepted by the server, to both players (for the mover it is the confirmation). Re-sent byte for byte when the mover repeats the same move for the same ply.

| Offset | Field | Type | Bounds | Meaning |
|---|---|---|---|---|
| 1 | `game` | `id53` | < 2^53 |  |
| 9 | `gseq` | `u32` |  |  |
| 13 | `ply` | `u16` | 0..1199 |  |
| 15 | `move` | `u16` | 0..32767 |  |
| 17 | `flags` | `u8` |  | MoveFlag bits |
| 18 | `spentMs` | `u32` |  | clock time charged for the move |
| 22 | `whiteMs` | `u32` |  |  |
| 26 | `blackMs` | `u32` |  |  |
| 30 | `serverTime` | `f64` | finite |  |
| 38 | `drawOffer` | `bool` |  | the move came with a draw offer |
| 39 | `firstMoveMs` | `u32` |  | time the next player has for their first move (0 when not applicable: the side to move's clock runs from serverTime) |

<a id="a2-moverejected"></a>
#### `0xA2` MoveRejected (s2c, 14 bytes)

The move intent was refused (to the mover only; never shown to the opponent). A GameSnapshot always follows; FlagFell is also followed by GameEnd.

| Offset | Field | Type | Bounds | Meaning |
|---|---|---|---|---|
| 1 | `game` | `id53` | < 2^53 |  |
| 9 | `ply` | `u16` | 0..1199 |  |
| 11 | `move` | `u16` | 0..32767 |  |
| 13 | `code` | enum [ErrorCode](#errorcode) |  |  |

<a id="a3-gameevent"></a>
#### `0xA3` GameEvent (s2c, 19 bytes)

Something happened in a game (GameEventKind).

| Offset | Field | Type | Bounds | Meaning |
|---|---|---|---|---|
| 1 | `game` | `id53` | < 2^53 |  |
| 9 | `gseq` | `u32` |  |  |
| 13 | `kind` | enum [GameEventKind](#gameeventkind) |  |  |
| 14 | `color` | enum [Color](#color) |  |  |
| 15 | `arg` | `u32` |  | see GameEventKind (0 when unused) |

<a id="a4-gameend"></a>
#### `0xA4` GameEnd (s2c, 31 bytes)

Final result, to both players. Rated games: RatingUpdate follows once the result is committed.

| Offset | Field | Type | Bounds | Meaning |
|---|---|---|---|---|
| 1 | `game` | `id53` | < 2^53 |  |
| 9 | `gseq` | `u32` |  |  |
| 13 | `status` | enum [GameStatus](#gamestatus) |  |  |
| 14 | `reason` | enum [EndReason](#endreason) |  |  |
| 15 | `whiteMs` | `u32` |  |  |
| 19 | `blackMs` | `u32` |  |  |
| 23 | `serverTime` | `f64` | finite |  |

<a id="a5-ratingupdate"></a>
#### `0xA5` RatingUpdate (s2c, 29 to 35 bytes)

Rating changes of a finished rated game, once the result is committed.

| Offset | Field | Type | Bounds | Meaning |
|---|---|---|---|---|
| 1 | `game` | `id53` | < 2^53 |  |
| 9 | `category` | `str8` | 1..7 bytes |  |
| ... | `white` | [RatingChange](#ratingchange) |  |  |
| ... | `black` | [RatingChange](#ratingchange) |  |  |

<a id="a6-s-gesture"></a>
#### `0xA6` S_Gesture (s2c, 25 bytes)

The opponent's gestures: the client Gesture without its seq, byte for byte. Cosmetic, never authoritative.

The relay of the client `Gesture`: the same fields without `seq`, copied byte for byte.

| Offset | Field | Type | Bounds | Meaning |
|---|---|---|---|---|
| 1 | `game` | `id53` | < 2^53 |  |
| 9 | `ply` | `u16` | 0..1199 |  |
| 11 | `touch` | `u8` | 0..64 |  |
| 12 | `aim` | `u8` | 0..64 |  |
| 13 | `placed` | `u16` | 0..32767 |  |
| 15 | `flags` | `u8` | 0..7 |  |
| 16 | `yaw` | `i32` | -3142..3142 |  |
| 20 | `pitch` | `i32` | -1571..1571 |  |
| 24 | `lean` | `u8` | 0..100 |  |

<!-- protogen:end fields -->

## Structs

<!-- protogen:begin structs -->

#### PlayerInfo

A player as shown to others (9 to 32 bytes, inline.)

| Offset | Field | Type | Bounds | Meaning |
|---|---|---|---|---|
| 0 | `userId` | `u32` |  |  |
| 4 | `name` | `str8` | 1..24 bytes |  |
| ... | `rating` | `u16` |  | rating in the game's category |
| ... | `provisional` | `bool` |  | the rating is still provisional |

#### MoveRec

A move of a game record (10 bytes, inline.)

| Offset | Field | Type | Bounds | Meaning |
|---|---|---|---|---|
| 0 | `move` | `u16` | 0..32767 | packed move (Move encoding) |
| 2 | `spentMs` | `u32` |  | clock time charged for the move after lag compensation (0 for plies 0 and 1) |
| 6 | `clockMs` | `u32` |  | mover's remaining time after the move, increment included |

#### RatingChange

Rating of one player before and after a rated game (9 bytes, inline.)

| Offset | Field | Type | Bounds | Meaning |
|---|---|---|---|---|
| 0 | `before` | `u16` |  |  |
| 2 | `after` | `u16` |  |  |
| 4 | `games` | `u32` |  | rated games in the category, this one included |
| 8 | `provisional` | `bool` |  | the new rating is still provisional |

<!-- protogen:end structs -->

## Enums

An enum is one byte. A **closed** enum never gains values: any other value is malformed. An
**open** enum may gain values in a later minor: the server never sends a value the negotiated
minor does not define, and a client keeps an unknown value and applies the fallback below.
Struck-out values are retired: they are never sent and their numbers are never reused.

<!-- protogen:begin enums -->

#### Color

A side of the board. None: no side (no clock running, no offer pending, a rematch window without an offer). Closed: the values never change within protocol 1.

| Value | Name | Meaning |
|---|---|---|
| 0 | `White` |  |
| 1 | `Black` |  |
| 2 | `None` |  |

#### ColorPref

Colour asked for in a challenge, or offered to the player who receives it. Closed: the values never change within protocol 1.

| Value | Name | Meaning |
|---|---|---|
| 0 | `Random` |  |
| 1 | `White` |  |
| 2 | `Black` |  |

#### GameStatus

Result of a game. The same numbers are the `status` of the game records of the HTTPS API. Closed: the values never change within protocol 1.

| Value | Name | Meaning |
|---|---|---|
| 0 | `Ongoing` |  |
| 1 | `WhiteWins` |  |
| 2 | `BlackWins` |  |
| 3 | `Draw` |  |
| 4 | `Aborted` | no result, unrated |

#### EndReason

Why a game ended. 0..13 are the game's chess::GameEndReason values (14..19 are kept for new ones), 20 and above happen online only. The same numbers are the `reason` of the game records of the HTTPS API. Open: a later minor may add values. A client keeps an unknown value and show a generic end of game: `status` gives the result

| Value | Name | Meaning |
|---|---|---|
| 0 | `None` | the game is not over |
| 1 | `Checkmate` |  |
| 2 | `Resignation` |  |
| 3 | `Timeout` |  |
| 4 | `IllegalMoves` | offline games only |
| 5 | `Stalemate` |  |
| 6 | `InsufficientMaterial` |  |
| 7 | `TimeoutVsInsufficient` | flag fell but the opponent cannot mate: draw |
| 8 | `FivefoldRepetition` |  |
| 9 | `SeventyFiveMoves` |  |
| 10 | `ThreefoldClaim` |  |
| 11 | `FiftyMoveClaim` |  |
| 12 | `Agreement` |  |
| 13 | `IllegalMovesVsInsufficient` | offline games only |
| 20 | `Abandonment` | disconnected longer than the reconnection grace: loss |
| 21 | `AbandonmentVsInsufficient` | abandoned, but the opponent cannot mate: draw |
| 22 | `Aborted` | aborted by a player before their first move (unrated) |
| 23 | `NoShow` | a first move not made in time (aborted, unrated) |
| 24 | `Forfeit` | certain cheat: loss |
| 25 | `ServerAborted` | the server could not continue the game, or it reached MaxPlies (unrated) |
| 26 | `BothDisconnected` | both players vanished at the same time (aborted, unrated) |

#### GameEventKind

What a GameEvent reports. 0 is never assigned. Open: a later minor may add values. A client keeps an unknown value and ignore the event

| Value | Name | Meaning |
|---|---|---|
| 1 | `DrawOffered` | color = the player who offers |
| 2 | `DrawDeclined` | color = the player who declines (also when they move instead of answering) |
| 3 | `PlayerDisconnected` | color = who; arg = their reconnection grace in ms |
| 4 | `PlayerReconnected` | color = who |
| 5 | `RematchOffered` | color = the player who offers (after the end) |
| 6 | `RematchDeclined` | color = the player who declines, or None when the offer expired |
| 7 | ~~`AbortAvailable`~~ | Reserved: retired (never sent) |

#### QueueState

State of the player in a matchmaking queue. Open: a later minor may add values. A client keeps an unknown value and ignore the message

| Value | Name | Meaning |
|---|---|---|
| 0 | `Left` | not in the queue (left, or never joined) |
| 1 | `Searching` |  |
| 2 | `Matched` | a game was found: its GameSnapshot follows |

#### ChallengeState

State of a challenge or of a private game waiting for its code. Open: a later minor may add values. A client keeps an unknown value and ignore the message

| Value | Name | Meaning |
|---|---|---|
| 0 | `Pending` |  |
| 1 | `Accepted` | the game starts: its GameSnapshot follows |
| 2 | `Declined` |  |
| 3 | `Cancelled` |  |
| 4 | `Expired` |  |
| 5 | `Unavailable` | the other player cannot play it any more (offline, in a game, refusing challenges) |

#### NoticeCode

What a Notice announces. 0 is never assigned. Open: a later minor may add values. A client keeps an unknown value and ignore the notice

| Value | Name | Meaning |
|---|---|---|
| 1 | `ServerShutdown` | arg = ms before the shutdown |
| 2 | `Banned` | arg = end of the ban (epoch ms); the connection closes (4004) |
| 3 | `SessionRevoked` | the session token was revoked; the connection closes (4003) |
| 4 | `MatchmakingCooldown` | arg = end of the cooldown (epoch ms) |
| 5 | `ReplacedByNewConnection` | another connection of the account took over; the connection closes (4007) |
| 6 | ~~`Motd`~~ | Reserved: retired (never sent: the message of the day comes from /api/v1/info) |
| 7 | `RatingRestored` | arg = rating points given back: an opponent of your rated games was banned for cheating |

#### ErrorCode

Why a request was refused (Error.code, MoveRejected.code). 1..99: connection, 100..199: game, 200..239: lobby, 240..255: violations. A fatal Error carries a connection or violation code, and the close code that follows is 4000 + code (1..99) or 4300 + (code - 240) (240..255). 0 is never assigned. Open: a later minor may add values. A client keeps an unknown value and a generic refusal of the request quoted by `ref` (when `fatal`, the connection closes)

| Value | Name | Meaning |
|---|---|---|
| 1 | `Malformed` | a message that does not decode (fatal) |
| 2 | `UnsupportedProtocol` | Hello.proto is not 1 (fatal): update the game |
| 3 | `Unauthorized` | session token refused (fatal): log in again |
| 4 | `Banned` | account banned (fatal, after Notice Banned) |
| 5 | `RateLimited` | too many messages: this one was dropped (never fatal) |
| 6 | `ServerFull` | no room for a new player (fatal) |
| 7 | `Replaced` | another connection of the account took over (fatal) |
| 8 | `ShuttingDown` | the server is shutting down (fatal) |
| 9 | `Internal` | unexpected server failure (fatal at Hello, otherwise the request only) |
| 10 | `HelloRequired` | the first message was not a Hello, or none came in time (fatal) |
| 11 | `EmailUnverified` | the account's e-mail address is not verified (fatal) |
| 100 | `NotInGame` | the game does not exist or the player does not play it |
| 101 | `NotYourTurn` |  |
| 102 | `IllegalMove` |  |
| 103 | `StalePly` | a different move for a ply already played |
| 104 | `Desync` | ply ahead of the game, or posHash of another position |
| 105 | `GameOver` |  |
| 106 | `AlreadyInGame` |  |
| 107 | `InvalidCategory` | not an official category of this server |
| 108 | `DrawOfferLimit` |  |
| 109 | `NothingToClaim` |  |
| 110 | `AbortNotAllowed` |  |
| 111 | `NoPendingOffer` |  |
| 112 | `FlagFell` | the mover's flag fell before the move arrived (GameEnd follows) |
| 200 | `QueueNotAllowed` |  |
| 201 | `ChallengeNotFound` |  |
| 202 | `UserUnavailable` |  |
| 203 | `ChallengeLimit` |  |
| 204 | `CannotChallengeSelf` |  |
| 205 | `CodeInvalid` |  |
| 206 | `RatedRequiresOfficialTc` |  |
| 207 | `MatchmakingCooldown` |  |
| 208 | `InvalidTimeControl` |  |
| 209 | `RematchUnavailable` |  |
| 210 | `RatedRepeatLimit` | rated game refused: MATCH_REPEAT_LIMIT reached with this player |
| 240 | `ProtocolViolation` | a message that decodes but is not allowed here (fatal unless stated) |
| 241 | `Flood` | message or gesture flood (fatal) |
| 242 | `CheatDetected` | certain cheat (fatal) |
| 243 | ~~`SlowConsumer`~~ | Reserved: never sent in an Error: a client that does not read gets close 4303 alone |

<!-- protogen:end enums -->

## Flags

<!-- protogen:begin flags -->

#### MoveFlag

MoveMade.flags: the move as played.

| Bit | Name | Meaning |
|---|---|---|
| `0x01` | `Capture` |  |
| `0x02` | `EnPassant` |  |
| `0x04` | `CastleKing` |  |
| `0x08` | `CastleQueen` |  |
| `0x10` | `DoublePush` |  |
| `0x20` | `Promotion` |  |
| `0x40` | `Check` |  |
| `0x80` | `Mate` |  |

#### GestureFlag

Gesture.flags.

| Bit | Name | Meaning |
|---|---|---|
| `0x01` | `Glance` | looking at one's own scoresheet |
| `0x02` | `Promoting` | the promotion picker is open |
| `0x04` | `Side` | the look falls on the table beside the board (clock, captured pieces, scoresheet); each client puts the clock at its own player's right, so the receiver mirrors the look (negates its yaw) |

<!-- protogen:end flags -->

## Golden vectors

`test/fixtures/protocol-vectors.json` (generated by `protogen`, independently of the codecs it
tests) is the conformance suite of this document. The Rust codec, the server and the game's C++
tests all check it:

* `valid[]`: field values and their exact encoding (`hex`), at least one per message, with the
  extremes of every bound, multi-byte and 4-byte UTF-8, f64 extremes and the largest snapshot.
* `malformed[]`: bytes that every receiver in direction `dir` refuses, strict or lenient, with
  exactly one defect each and the reason of the Rust codec (`"ply above max"`).
* `lenient[]`: bytes that a strict decoder refuses (`strictReason`) and a lenient receiver
  accepts as `fields` or ignores (`fields` null): S2C messages of a later minor (unknown types,
  appended fields, unknown open enum values) and a Hello of a later minor read by a server.
* `fnv1a32[]` and `moves[]`: position digests and move packing.

A new implementation should pass all of them before it talks to a peer.
