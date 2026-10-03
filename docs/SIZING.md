# Sizing and hosting

This page tells an operator how many players a machine can hold, what limits it first, and how to set up a small VPS for the server. The unit costs were measured on 2026-10-03 with `bench/run.sh` ([BENCHMARK.md](BENCHMARK.md#results-of-2026-10-03)): the server at commit b02f03b on a 4-vCPU Intel Xeon @ 2.10 GHz KVM guest (the "test vCPU" below), pinned to 2 vCPUs with `WORKERS=2`, the load generator pinned to the other 2, TLS 1.3 over loopback. Figures for another host are scaled by a speed factor `s`: how much work one of its vCores does compared with the test vCPU. Values marked "(inferred)" are reasoned from the measurements, not measured directly. Every setting is described in [CONFIG.md](CONFIG.md).

Terms used throughout:

- **r**: the gestures per second that a player in a game sends on average ([Gestures](#gestures)). The game client sends one at least every `GESTURE_IDLE_MS` (1 s by default) and at most `GESTURE_RATE` (4) per second; with `GESTURE_RATE=0` it sends none.
- **Calm** players sit still during their games and send only the keepalive gestures: r = 1,000 ÷ `GESTURE_IDLE_MS`, so r = 1 with the defaults. **Moving** players look around and hold pieces all the time: r = `GESTURE_RATE` at worst.
- **Pace**: a game makes one ply every 5 s (each side thinks about 5 s per move, as in a 3+2 game) and lasts 80 plies, about 400 s (inferred).
- **Mix M1**: 60 % of the connected players are in a game, 40 % are in the menus; games in progress are 0.3 × connected players.
- **Comfortable**: 70 % of the server's CPU, where the server still adds only milliseconds to a move (inferred, see [Latency](#latency-and-the-comfortable-point)). **CPU full**: 100 %.
- **Connected player**: anyone signed in. The game keeps its WebSocket open for the whole session, menus included.

## Summary

- One process serves everything: the games, the connections, the API and the database (SQLite in WAL mode, one local file, no database server).
- CPU limits first while the live gestures are on. A relayed gesture costs about 22 µs of the test vCPU, and a calm player sends one per second with the defaults, so these keepalive gestures are about two thirds of the CPU of a calm game. An OVH vCore holds about 5,500 calm games comfortably with the defaults, 9,600 with `GESTURE_IDLE_MS=3000`, 15,400 with `GESTURE_RATE=0` (no live gestures), and 1,900 if every player moved at the default `GESTURE_RATE` of 4 all the time (inferred). See [CPU](#cpu).
- A 2-vCore, 4 GB VPS (OVH VPS-1) comfortably holds about 9,300 calm games with the defaults (28,000 connected players in mix M1), 16,300 with `GESTURE_IDLE_MS=3000`, and 26,000 with `GESTURE_RATE=0` (68,000 connected) (inferred).
- Memory: about 28 KiB per connection. 4 GB holds about 67,000 connections and 8 GB about 163,000 with the default settings and the analysis engines (inferred), so memory limits first only without live gestures. Set `MAX_CONNECTIONS` (200,000 by default) to what the machine really holds. See [Memory](#memory).
- 100,000 simultaneous games (200,000 players) need about 7 OVH vCores with `GESTURE_RATE=0`, 11 with calm players at `GESTURE_IDLE_MS=3000`, 18 with calm players at the defaults, and about 8.5 GiB of RAM (inferred): a 4 GB machine holds about a third of that at best. See [The 100,000-game target](#the-100000-game-target).
- Network bandwidth is not a limit on a VPS. Disk space is: about 1 KB per finished game, 212 MB a day per 1,000 games in progress on average, and nothing archives old games. See [Network](#network) and [Database and disk](#database-and-disk).
- A password login costs one Argon2id hash: about 0.17 s of the test vCPU, 0.31 s of an OVH vCore (inferred). See [Password logins](#password-logins).
- A restart is ready in about 24 ms with an empty journal, and each player's reconnection costs about 0.7 ms of an OVH vCore (inferred). The default `RECOVERY_CLOCK_HOLD_MS` of 20 s covers about 25,000 players on VPS-1; above that set 45000. See [Restarts](#restarts).
- One network address costs at most a few percent of a vCore with the default per-address limits (inferred); floods from many addresses are the provider's job. See [Protection per address](#protection-per-address) and [Floods](#floods).
- Animated GIFs are rendered on threads at the lowest CPU priority, so they take only the CPU the games leave. See [Animated GIFs](#animated-gifs).

## The process, for sizing

[DESIGN.md](DESIGN.md) section 1 describes the process. What matters for sizing:

- One async runtime with `WORKERS` threads (`auto`: one per CPU core, at most 16; at most 64 when set) runs everything that serves the players: the TLS handshakes, the HTTP API, two tasks per WebSocket connection (reader and writer), the lobby actor and one host actor per game shard. No game or connection is bound to a core: any runtime thread serves any task, so the CPU figures below scale with the cores up to `WORKERS`. On more than 16 cores, set `WORKERS` to the number of cores.
- Password hashes run on threads of their own, at most `PASSWORD_HASH_CONCURRENCY` (`WORKERS` by default) at once, at normal priority: a wave of logins takes CPU from the games.
- GIF renders (`GIF_THREADS`, `WORKERS` by default) and the analysis engines (`ANALYSIS_WORKERS` Stockfish child processes) run at nice 19: they only take the CPU the runtime leaves.
- The database has one writer thread, which runs every write job in submission order, and 4 reader connections on the blocking pool. Each shard's journal has an I/O thread, and the logs have a writer thread. None of these was a limit in the runs.
- Every limit of the configuration is a whole-server limit. Several defaults scale with `WORKERS` (`DB_CACHE_MB`, `PASSWORD_HASH_CONCURRENCY`, `GIF_THREADS`, `GIF_CACHE_MB`, `MAX_PENDING_HANDSHAKES` and the queues), so a large machine needs some of them capped ([Memory](#memory)).

## CPU

### Unit costs

Server CPU on the test vCPU, and on an OVH vCore at `s` = 0.55 (the test figure ÷ 0.55, inferred):

| Item | Test vCPU | OVH vCore (inferred) | Source |
|---|---|---|---|
| Relayed gesture (a `Gesture` in from one player and out to the opponent, a TLS record each way) | 22.4 µs | 41 µs | measured: the difference between the runs at 1 and 4 gestures per second per player, at 4,000 and 8,000 games |
| A game, per second, without gestures, at the pace above (its plies, the server's heartbeat and the game client's pings for both players) | about 25 µs | about 45 µs | 20-24 µs measured with the bench clients, which send no ping of their own, plus about 5 µs for the game client's pings (inferred) |
| One ply (the `Move`, the `MoveMade` to both players, the journal record) | about 75-120 µs | about 140-220 µs | inferred from fits of all the game runs |
| Connected player in the menus, per second | 2.5-2.9 µs with the server's heartbeat alone; about 5 µs with the game client's ping every 10 s (inferred) | about 9 µs | measured: 1,000 to 10,000 idle connections |
| One ping exchange (the server's heartbeat or the client's `Ping`) | about 25 µs | about 45 µs | inferred: 2.5 µs per second per connection at one heartbeat per 10 s |
| New connection or reconnection (TCP, TLS 1.3 handshake with an ECDSA P-256 certificate, WebSocket upgrade, `Hello` and `Welcome`) | 0.36-0.40 ms | about 0.7 ms, more without ADX | measured during the connection ramps |
| Password login or registration (one Argon2id hash) | 0.168 s | 0.31 s | measured: 11.7 logins per second on 2 vCPUs |
| GIF of a 40-ply game at the medium size, request included | 9.2 ms | 17 ms | measured: 213 renders per second on 2 vCPUs |
| API request on a kept-alive connection: `GET /api/v1/info`; a game's PGN; a GIF from the cache | 37 µs; 240 µs; 158 µs | 67 µs; 0.44 ms; 0.29 ms | measured with the CPU saturated |
| Fixed cost: the idle process; with games in progress | 0.011 core; about 0.12 core | about 0.02 and 0.22 vCore | measured; the second is the constant of the fit (the cost per message is higher at low load) |

So a game costs B + 2 × r × 22.4 µs of the test vCPU per second, with B about 25 µs at the pace above (inferred): 25 µs without gestures, 70 µs with calm players and the defaults, 204 µs if both players move at the default `GESTURE_RATE` of 4 all the time. A faster game costs more through its plies: one ply every 1.5 s (bullet) adds about 50 µs (inferred).

`CLIENT_PING_INTERVAL_MS` (10 s, 1 to 60 s) sets how often the game client pings for its ping indicator and its estimate of the server clock; the server announces it in `Welcome`. Each exchange costs about 25 µs, so 10 s costs about 2.5 µs per second per connected player and 2 s about 12.5 µs (inferred). For mix M1, 2 s instead of 10 s costs about 30 % of the capacity with calm players at the defaults, and half of it without gestures (inferred). After each connection the client sends four quick pings anyway. The server's own heartbeat (`HEARTBEAT_INTERVAL_MS`, 10 s) pings every connection once per interval, the first half an interval after it opened, and feeds the lag compensation: leave it.

### Speed factor `s`

`s` depends on the clock speed, the work per cycle, the CPU features the hypervisor exposes, and CPU steal. An OVH VPS vCore shows up as QEMU's "Intel Core Processor (Haswell, no TSX)" at 2,400 MHz, without the ADX flag (ring, the cryptography library of rustls, then uses slower code for P-256 and X25519, so TLS handshakes cost more: inferred), and its vCores are shared, with a steal the provider does not publish. The earlier study of the Node.js server put such a vCore at 0.60-0.70 of a 2.8 GHz Cascade Lake vCPU, which ran a single-threaded scrypt reference in 0.50 s against 0.42 s for the test vCPU (0.43 s again on 2026-10-03): `s` ≈ 0.65 × 0.42 ÷ 0.50 ≈ 0.55 (0.50-0.60), so an OVH vCore takes about 1.8 times as long as the test vCPU (inferred). The CPU model name hides the real host CPU: measure `s` on the real machine ([Validating on the real machine](#validating-on-the-real-machine)).

### Latency and the comfortable point

The runs at a realistic pace measured the move relay, from the mover's `Move` to the `MoveMade` read by the opponent, over loopback:

| Server CPU (% of its 2 vCPUs) | Load | Move relay p99 |
|---|---|---|
| 7-32 % | 1,000 to 8,000 games, 1 gesture per second per player | 0.8-1.4 ms |
| 15 % | 1,000 games, 4 gestures per second per player | 1.0 ms |
| 46 % | 4,000 games, the same | 5.1 ms |
| 86 % | 8,000 games, the same (the load generator at 78 % of its own 2 vCPUs) | 114 ms |

The comfortable point is taken at 70 % of the server's CPU (inferred, between the last two rows); the players' internet round trip (20-80 ms, inferred) comes on top. No game ended during these measurement windows, so the cost of the game churn (starts, ends, commits) is not in them: check the point on the real machine.

### Gestures

During a game the client sends its player's live gestures (the head, the piece in hand, where it is aimed, a move placed before the clock press) whenever they change, at most `GESTURE_RATE` per second (4 by default, in bursts of `GESTURE_BURST`, 8), and the server relays each one to the opponent without storing it. While nothing changes, the client still sends one every `GESTURE_IDLE_MS` (1 s by default), for the whole game and on the opponent's turn too: the opponent's client relies on these keepalives to tell a player who sits still from one whose gestures stopped. So r lies between 1,000 ÷ `GESTURE_IDLE_MS` (calm players) and `GESTURE_RATE` (players who move all the time), and is 0 with `GESTURE_RATE=0`. Where it lies depends on how the players play, so it is known only in production: the rate of `scacelith_gestures_relayed_total` divided by the players in a game (twice `scacelith_games_active`).

Three settings, all announced to the game in `Welcome`, so a change applies to the players who connect after the restart (all of them, since a restart reconnects everyone):

- **`GESTURE_IDLE_MS`** (1,000 to 10,000 ms, default 1,000; `gestureIdleMs` in `Welcome`): the keepalive interval. The game client follows the opponent's head for 2.5 intervals after the last gesture and puts a piece held live back after 5, so a longer interval saves CPU and the opponent's robot notices later that the gestures stopped. With calm players a game costs about 70 µs per second of the test vCPU at 1,000 ms, 47 µs at 2,000, 40 µs at 3,000 (43 % less) and 30 µs at 10,000 (58 % less) (inferred). It does not change what moving players send.
- **`GESTURE_RATE`** (0 to 60, default 4): what a moving player may send, so the worst case of r.
- **`GESTURE_RATE=0`: no live gestures.** The server announces 0 in `Welcome` (`gestureRate`, `gestureBurst` and `gestureIdleMs`), and the game client then sends no gesture at all. The server relays nothing: a modified client's gestures are dropped (`scacelith_gestures_dropped_total{reason="rate"}`), and more than 80 drops in 10 s (max(50, 10 × `GESTURE_BURST`) with the default burst) close its connection as a flood. Moves, clocks, draw offers and resignation are shared as always, and the opponent's robot still plays each move on the board; only its head and hand stop following the opponent live. A game then costs about 25 µs per second of the test vCPU, 2.8 times less than a calm game with the defaults (inferred, extrapolated from the runs at 1 and 4 gestures per second).

A gesture is small: about 110 bytes in and 100 bytes out at the IP level, one packet each way ([Network](#network)).

### Capacity per vCore

Games per OVH vCore at the comfortable point (70 %), apart from the fixed cost of about 0.22 vCore per server (inferred):

| r | Settings and players | A game, test vCPU | A game, OVH vCore | Games per OVH vCore |
|---|---|---|---|---|
| 0 | `GESTURE_RATE=0` | 25 µs | 45 µs | about 15,400 |
| 1/3 | calm players, `GESTURE_IDLE_MS=3000` | 40 µs | 73 µs | about 9,600 |
| 1 | calm players, defaults | 70 µs | 127 µs | about 5,500 |
| 2 | players moving all the time, `GESTURE_RATE=2` | 115 µs | 208 µs | about 3,400 |
| 4 | players moving all the time, defaults | 204 µs | 371 µs | about 1,900 |

With the defaults the real r lies between the rows 1 and 4; with `GESTURE_IDLE_MS=3000` and `GESTURE_RATE=2`, between the rows 1/3 and 2. The figures scale with `s`, and move by about ±10 % with the cost of a gesture and, for r = 0, by about ±20 % with B (inferred).

### Worked example: OVH VPS-1 (2 vCores, 4 GB) and VPS-2 (4 vCores, 8 GB)

At `s` = 0.55 the comfortable point leaves 70 % of the vCores minus the fixed 0.22 vCore for the players: 1.18 vCores on VPS-1 and 2.58 on VPS-2 (inferred). Comfortable capacity (inferred):

| r | VPS-1: games, every player in a game | VPS-1: connected players, mix M1 | VPS-1: games when the CPU is full | VPS-2: games | VPS-2: connected players, mix M1 |
|---|---|---|---|---|---|
| 0 (`GESTURE_RATE=0`) | 26,000 | 68,000 | 39,000 | 57,000 | 149,000 |
| 1/3 (calm, `GESTURE_IDLE_MS=3000`) | 16,300 | 46,500 | 24,600 | 35,600 | 102,000 |
| 1 (calm, defaults) | 9,300 | 28,300 | 14,000 | 20,300 | 62,000 |
| 2 (moving, `GESTURE_RATE=2`) | 5,700 | 17,900 | 8,600 | 12,400 | 39,000 |
| 4 (moving, defaults) | 3,200 | 10,300 | 4,800 | 7,000 | 22,400 |

Memory caps VPS-1 at about 67,000-76,000 connections (33,000-38,000 games with every player in one) and VPS-2 at about 163,000-175,000 (inferred, [Memory](#memory)). With live gestures the CPU limits first on both machines. Without them, on VPS-1 in mix M1 the comfortable CPU point and the memory limit come together near 68,000 connected players; with every player in a game the CPU passes 70 % at about 26,000 games, and memory runs out at 33,000-38,000, before the CPU is full (about 39,000).

A 2-vCore VPS is enough to start. Move to 4 vCores when the peak CPU regularly passes 70 %, or when you want engine analysis on top (one engine takes a whole vCore, at low priority).

### The 100,000-game target

100,000 simultaneous games are 200,000 connected players, all in a game. The model gives (inferred; the largest run held 8,000 games):

| | `GESTURE_RATE=0` | calm, `GESTURE_IDLE_MS=3000` | calm, defaults | moving, `GESTURE_RATE=2` | moving, defaults |
|---|---|---|---|---|---|
| OVH vCores at 70 % CPU | about 7 | about 11 | about 18 | about 30 | about 53 |
| Network in / out, without TCP acknowledgements | 45 / 67 Mbit/s, 60 / 80 kpkt/s | 104 / 120 Mbit/s, 127 / 147 kpkt/s | 220 / 228 Mbit/s, 260 / 280 kpkt/s | 394 / 390 Mbit/s, 460 / 480 kpkt/s | 743 / 713 Mbit/s, 860 / 880 kpkt/s |

- Memory: 200,000 connections × 28 KiB = 5.3 GiB, plus about 1.5 GiB of fixed costs with capped settings (`DB_CACHE_MB=256`, `PASSWORD_HASH_CONCURRENCY=4`, `GIF_THREADS=4`, `GIF_CACHE_MB=128`, one engine): 6.8 GiB, about 8.5 GiB with 20 % kept free (inferred). An 8 GB machine fits it with only about 10 % free: plan 12 GB.
- `WORKERS=auto` stops at 16 threads: set the number of cores beyond that. Raise `MAX_CONNECTIONS` above its default of 200,000 if players in the menus should get in too: the default is exactly 100,000 games (players coming back to a game in progress are let in whatever the count).
- Disk: 100,000 games at the peak are about 40,000 on average (inferred: 40 %), about 8.5 GB of finished games a day.
- A restart brings 200,000 players back: about 145 vCore-seconds of handshakes, 21 s on 7 vCores (inferred), so set `RECOVERY_CLOCK_HOLD_MS=45000`.
- Not measured at all: the one database writer and the one lobby actor at about 250 game ends per second (inferred: 100,000 games of about 400 s), and the kernel's network work for 200,000 sockets on a real network interface. Test them before relying on this target.

A 4 GB machine cannot hold this: memory caps it at 33,000-38,000 games, and its 2 vCores carry about 26,000 comfortably without live gestures.

## Memory

Per connection: 21-25 KiB of server RSS (22-23 KiB for each further connection in a game, 20-25 KiB per idle connection), plus about 4 KiB of kernel socket memory (inferred; the earlier study measured 3.7 KB). Budget 28 KiB. Measured: 13 MiB of RSS for the idle server, 258 MiB with 10,000 idle connections, 409 MiB with 16,000 players in 8,000 games.

Fixed costs to subtract from the visible RAM before dividing by 28 KiB, in MiB with the default settings (VPS-1: 3,826 MiB visible; VPS-2: about 7,650, inferred):

| Item | Size | VPS-1 (`WORKERS`=2) | VPS-2 (`WORKERS`=4) |
|---|---|---|---|
| Kept free | 20 % of the visible RAM | 765 | 1,530 |
| OS, sshd, journald (inferred) | about 250 MiB | 250 | 250 |
| Server process without connections (inferred) | about 40 MiB (13 measured at idle) | 40 | 40 |
| SQLite page cache | `DB_CACHE_MB`, by default 64 × (`WORKERS` + 1), shared by the writer and the 4 readers | 192 | 320 |
| Memory-mapped database (page cache, reclaimable) | `DB_MMAP_MB`, 256 | 256 | 256 |
| Password hashes in progress | 64 MiB × `PASSWORD_HASH_CONCURRENCY` (`WORKERS`) | 128 | 256 |
| GIFs, while players make them | `GIF_CACHE_MB` (32 × `WORKERS`) + about 20 MiB per render thread (`GIF_THREADS`, `WORKERS`; inferred) | 104 | 208 |
| Analysis engines, Stockfish 19, network shared | 1 engine: 273 MB; 2 engines: 340 MB ([Anti-cheat engines](#anti-cheat-engines)) | 260 | 324 |
| **Connections** (inferred) | the rest ÷ 28 KiB | **67,000** (76,000 without the engine) | **163,000** (175,000 without the engines) |

- The SQLite cache and the mapped pages fill only as the database is read, and the kernel can drop mapped pages, so counting them in full is cautious. `DB_MMAP_MB` covers only the start of the file; a database of several GB is read mostly through the OS page cache.
- On a larger machine the defaults that scale with `WORKERS` grow with it: with 16 workers the SQLite cache is 1,088 MiB, the hashes 1 GiB and the GIFs about 830 MiB. Cap them explicitly: `DB_CACHE_MB=256`, `PASSWORD_HASH_CONCURRENCY=4`, `GIF_THREADS=4` and `GIF_CACHE_MB=128` keep them under 750 MiB together.

**`MAX_CONNECTIONS`.** Its default of 200,000 means about 5.3 GiB of connections (inferred), more than a 4 GB or an 8 GB machine holds. Set it to what the machine really holds, so that a full server refuses newcomers at login instead of running out of memory: about 60,000 on VPS-1 and 150,000 on VPS-2 (the memory figures above with the engines, minus a margin). That protects memory and file descriptors. To protect the CPU as well, set it to the mix M1 figure of your worst case r in the [worked example](#worked-example-ovh-vps-1-2-vcores-4-gb-and-vps-2-4-vcores-8-gb): for example about 18,000 on VPS-1 with `GESTURE_RATE=2`, if every player in a game might move all the time.

### Anti-cheat engines

Memory of the analysis engines (`ANALYSIS_WORKERS`), measured with the official Stockfish 19 release (its universal binary, which ran its `x86-64-avx512icl` build on the development container) and the default `ANALYSIS_HASH_MB=32`, after each engine had analysed 12 positions at each of the default depths (9 and 15): the proportional set size (PSS, which splits a shared page between the processes that map it), summed over the engines.

| Engines | 1 | 2 | 4 | 8 | Each further engine |
|---|---|---|---|---|---|
| **Stockfish 19, network shared** (the normal case) | 273 MB | 340 MB | 474 MB | 742 MB | 67 MB |
| Stockfish 19, each engine with its own copy | 273 MB | 450 MB | 804 MB | 1,512 MB | 177 MB |
| Stockfish 16 (shares nothing), for comparison | 162 MB | 285 MB | 531 MB | | 123 MB |

- The first Stockfish 19 engine holds 110 MB of network, 67 MB of its own (the 32 MB hash table, search stacks and heap) and 96 MB of pages of the binary (clean pages of the file, which the kernel can drop and read again). Each further engine adds only its own 67 MB while the network is shared, and 177 MB when it has to load a copy.
- Summed RSS counts the shared network once per engine (2,103 MB for 8 sharing engines, against 742 MB of PSS): judge the engines by PSS (`/proc/<pid>/smaps_rollup`) or by the free memory, not by RSS.
- The depths do not change these figures: the hash table is allocated and cleared when the engine starts.
- The unshared line runs each engine in a mount namespace of its own, whose `/tmp/stockfish-0` Stockfish cannot use, so it falls back to a copy of its own as it does when `/tmp` is not writable: the same binary at the same path, only the sharing is off.
- Whether the running engines share is in the log and the metrics ([ANTICHEAT.md](ANTICHEAT.md#engine)).

In connections at 28 KiB each, one engine takes the room of about 9,500 connections, two sharing engines about 11,900 and four about 16,500 (inferred). CPU sets the number of engines, not memory: an engine busy with the backlog takes a whole vCore, at nice 19, on the CPU the server leaves idle. Run one on VPS-1, and two on VPS-2 when the CPU allows. One engine analyses about 870 games a day on an OVH vCore, 540 to 1,260 depending on the length of the games ([ANTICHEAT.md](ANTICHEAT.md#3-engine-analysis), inferred).

## Animated GIFs

Signed-in players can download a game as an animated GIF (`GET /api/v1/games/:id/gif` for a game of the server, `POST /api/v1/gif` for any game sent as PGN: [API.md](API.md#11-games-pgn-and-gif)). The server renders them on threads of their own (`GIF_THREADS`, `WORKERS` by default), never on the runtime threads, at the lowest CPU priority (nice 19, on Linux). The threads start with the first render and stop after a minute without one. Up to `GIF_QUEUE_MAX` (4 × `WORKERS`) renders wait for a free thread, each at most `GIF_QUEUE_TIMEOUT_MS` (10 s); beyond that the request is answered 503 `server_busy` and its quotas are given back. A GIF already made is kept in a cache of `GIF_CACHE_MB` (32 × `WORKERS` MiB) and costs no render; a request for a GIF being rendered joins that render.

### Cost of one GIF

Measured: 9.2 ms of the test vCPU for a 40-ply game at the medium size (424 × 515), the request included (213 renders per second on 2 vCPUs, about 110 KiB each). The renderer draws the same pictures with the same algorithm as the former Node.js renderer, in 0.45 of its time on that size of game in the same run (20.3 ms). Scaling the former renderer's timings by 0.45 gives (inferred; the former renderer's own ratio between game lengths gives up to a third less):

| Game | small (284 × 350) | medium (424 × 515) | large (628 × 762) | OVH vCore (small / medium / large) | File (small / medium / large) |
|---|---|---|---|---|---|
| 40 moves (80 plies) | 14 ms | 21 ms | 44 ms | 25 / 38 / 80 ms | 135 / 206 / 324 KiB |
| 150 moves (300 plies) | 38 ms | 75 ms | 151 ms | 70 / 136 / 275 ms | 506 / 783 / 1,235 KiB |
| 300 moves (600 plies, `GIF_MAX_PLIES`) | 72 ms | 140 ms | 290 ms | 130 / 255 / 530 ms | 980 / 1,514 / 2,384 KiB |

The file sizes are those of the former renderer, which drew the same pictures. Memory: the RSS rose from 44 to 153 MiB while 2 threads rendered and filled the 64 MiB cache, so a render thread takes about 20 MiB (inferred); a large picture of a long game may take more. A GIF being sent stays in memory until it is sent (2.4 MiB at most).

### What the quotas allow at most

All GIF quotas are counted for the whole server. Only a GIF that has to be rendered counts in the render quotas; a request served from the cache counts only in the `gif` limit (30 per minute per account) and in the account's budget. Taking the most expensive GIF, 600 plies at the large size (about 0.53 s of an OVH vCore, inferred):

| Who | Render quotas | CPU at most, one minute | CPU at most, one hour |
|---|---|---|---|
| One account | `GIF_USER_RENDERS_PER_MIN` (4), `GIF_USER_RENDERS_PER_HOUR` (30) | 2.1 s: 3.5 % of one vCore | 16 s: 0.4 % |
| One IPv4 address or IPv6 /64, all its accounts | `GIF_IP_RENDERS_PER_MIN` (12), `GIF_IP_RENDERS_PER_HOUR` (120) | 6.4 s: 11 % | 64 s: 1.8 % |
| One IPv6 /48, all its /64s | 3 times those: 36 and 360 | 19 s: 32 % | 190 s: 5.3 % |

A 40-move game at the medium size costs 14 times less. Many accounts on many addresses together are bounded by the threads, not by a quota: at most `GIF_THREADS` renders at once, which by default is the whole machine, but at the lowest priority.

**Effect on the games.** The kernel gives a nice-19 thread a weight of 15 against 1,024 for the runtime threads, so a render sharing a busy core with a runtime thread gets about 1.4 % of it (inferred from the scheduler's weights; not measured on this server). When the CPU is full the renders wait for the games, and the requests behind them get 503 `server_busy`.

**Budget.** Count `GIF_CACHE_MB` plus about 20 MiB per thread while players make GIFs: 104 MiB on VPS-1 and 208 MiB on VPS-2 with the defaults, as in the [Memory](#memory) table. `GIF_CACHE_MB` can be lowered; `GIF_ENABLED=false` turns the feature off. A GIF is 135 KiB to 2.4 MiB: the `gif` limit lets one account download 30 a minute, cached ones included, about 1 Mbit/s with usual games and 10 Mbit/s with the longest at the large size (inferred).

## Network

Sizes at the IP level (IPv4 and TCP with timestamps, 52 bytes; a TLS 1.3 record, 22 bytes; the WebSocket header): a gesture 109 bytes in and 101 out, a `Move` 106 bytes in, a `MoveMade` 119 bytes out, a ping or a pong 89 bytes, a TCP acknowledgement 52 bytes. Per player, at the pace above, with the server's heartbeat and the game client's ping every 10 s (inferred):

| r | In, per player in a game | Out, per player in a game | 100,000 games, in / out |
|---|---|---|---|
| 0 | 28 B/s, 0.3 packets/s | 42 B/s, 0.4 packets/s | 45 / 67 Mbit/s, 60 / 80 kpkt/s |
| 1/3 | 65 B/s, 0.6 packets/s | 75 B/s, 0.7 packets/s | 104 / 120 Mbit/s, 127 / 147 kpkt/s |
| 1 | 137 B/s, 1.3 packets/s | 143 B/s, 1.4 packets/s | 220 / 228 Mbit/s, 260 / 280 kpkt/s |
| 2 | 246 B/s, 2.3 packets/s | 244 B/s, 2.4 packets/s | 394 / 390 Mbit/s, 460 / 480 kpkt/s |
| 4 | 464 B/s, 4.3 packets/s | 446 B/s, 4.4 packets/s | 743 / 713 Mbit/s, 860 / 880 kpkt/s |

A player in the menus sends and receives about 18 B/s (0.2 packets per second each way). TCP acknowledgements come on top: up to one packet more for each packet received, about +50 % of the bandwidth at worst (inferred). At its comfortable CPU point a VPS-1 moves about 20 Mbit/s each way with calm players and the defaults, and 12-17 Mbit/s without gestures, a few percent of a 500 Mbit/s link; watch the packets per second rather than the bandwidth, since providers rarely publish a limit. At 100,000 games with moving players at the default `GESTURE_RATE`, a 1 Gbit/s link is full once the acknowledgements are counted. A reconnection costs a few KB (the TLS handshake with the certificate chain, the upgrade, `Hello`, `Welcome` and a game snapshot; inferred), so a wave of 2,700 reconnections per second is about 60-130 Mbit/s out (inferred).

## Database and disk

### Storage engine

The server uses the SQLite bundled with rusqlite (SQLite 3.53.2). The database is `DATA_DIR/scacelith.db`, in WAL mode with `synchronous=FULL`, a WAL cut back to 64 MiB after checkpoints (`journal_size_limit`), foreign keys on, a `busy_timeout` of 5 s and `secure_delete` on. One thread holds the only writable connection and runs the write jobs one at a time in submission order, each in its own `BEGIN IMMEDIATE` transaction; four `query_only` connections serve the reads. `DB_CACHE_MB` is shared evenly by these five connections. The schema is one migration, applied by `start` (or `scacelith-server migrate`) and checksummed: the server refuses to start when an applied migration was modified or is unknown to its version.

Games in progress live in memory and in a journal per shard ([DESIGN.md](DESIGN.md) section 5.6): a batch is written `JOURNAL_FLUSH_MS` (50 ms) after its first record, with one `write` and an `fdatasync` (`JOURNAL_FSYNC`), so a crash loses at most the last 50 ms of moves. Finished games are committed in batches, at most `DB_COMMIT_MS` (50 ms) after the first of them ended, one batch in flight per shard, with the rating changes in the same transaction; before a batch is committed, the shard waits until its journal has written and fsynced the records of those games, so the database never holds a finished game whose end the journal could still lose. Journal compaction keeps each shard's journal at about (`JOURNAL_COMPACT_SEGMENTS` + 1) × 16 MiB, 80 MiB with the default 4, however long the games last; plan 100 MB per shard.

When the journal cannot be written (a full disk, a failing volume), after 3 failed journal flushes in a row finished games are committed without it, rating changes included, and `scacelith_game_commit_unjournaled_total` counts them. Alert on any increase of it and of `scacelith_journal_errors_total`, and fix the disk before restarting: a finished game whose end the journal lost would come back as a game in progress after a restart, and the database keeps its first result.

What the database holds, and for how long: [DESIGN.md](DESIGN.md) section 7. Everything sits on one machine; nothing is implemented to spread one server over several.

### Growth

| Item | Size, indexes included |
|---|---|
| Finished game | about 180 B plus 10 B per ply, about 1 KB for 80 plies (inferred from the bench databases: 290 B per game for games of about 11 plies) |
| Session | about 230 B, removed by the retention purge after it expires |
| Rating (one per account and official time control played) | 56 B |
| Security event | about 190 B, removed after `RETENTION_SECURITY_DAYS` (90) |
| Game waiting for engine analysis | about 80 B |

The size of an engine analysis of a game was not measured on this server. At the pace above a game lasts about 400 s, so each game in progress on average is about 216 finished games a day: **about 212 MB a day per 1,000 games in progress on average** (inferred). Accounts are small.

**Retention.** Every `RETENTION_INTERVAL_MS` (one hour; the first run about a minute after the start) the server deletes expired and revoked sessions, expired tokens and pending signups, security events after `RETENTION_SECURITY_DAYS` (90), non-certain anomalies of the same age, conduct events and failed analysis jobs after 30 days, and erases stored IP addresses after `RETENTION_IP_DAYS` (30). Finished games, ratings, analysed games, sanctions and reports are kept. The purge runs in short write jobs, one statement each, whose size adapts between 50 and 1,000 rows so that each takes about 5 ms, and pauses 10 ms after every 10 ms of work, so the game commits keep getting the write lock. The file does not shrink: SQLite reuses the freed pages.

**Analysis queue.** At most `ANALYSIS_QUEUE_MAX` (5,000) ordinary games wait; while the queue is at that cap, newly finished ordinary games are not queued, and `ANALYSIS_SAMPLE_RATE` (1) draws the share of ordinary games that are queued. Games with a report, a suspicion signal or a moderator request are queued anyway and mostly analysed first, but one engine claim in four takes the oldest ordinary game. With one engine and a busy server the ordinary queue stays at its cap, and an ordinary game can wait 16 to 37 days (inferred: 4 × 5,000 ÷ 540-1,260 games a day); lower `ANALYSIS_QUEUE_MAX` if analysed games should be more recent.

Disk life = space for the database ÷ daily growth. Space for the database is the disk minus 20 % kept free, the OS (3 GB), swap, logs (1 GB) and the journal (100 MB per shard): about 26.8 GB on a 40 GB disk and 53.6 GB on a 75 GB disk. Halve it if backups are written to the same disk first.

| Games in progress, on average | Per day | 40 GB disk (26.8 GB for the database; 13.4 GB with local backup copies) | 75 GB disk (53.6 GB; 26.8 GB) |
|---|---|---|---|
| 1,000 | 212 MB | 4 months (2) | 8 months (4) |
| 4,000 | 850 MB | 1 month (2 weeks) | 2 months (1) |
| 10,000 | 2.1 GB | 13 days (6) | 25 days (13) |
| 40,000 | 8.5 GB | 3 days (1.5) | 6 days (3) |

The average is taken as 40 % of the evening peak (inferred): a VPS-1 full of calm games every evening with the defaults (9,300) averages about 3,700, and one full of games without gestures (26,000) about 10,400, which fills the database space of a 40 GB disk in about 12 days. Nothing archives finished games: plan the disk, or move old games out, before it fills.

### Disk I/O

An NVMe disk (about 20,000 writes of 4 KB per second) is not a limit: each shard writes at most about 20 journal batches per second with their `fdatasync` from the 50 ms flushes, plus one before each commit batch, and the database commits at most one batch per shard every `DB_COMMIT_MS` besides the occasional account, session and security writes (inferred).

### Backups

Back up with `scacelith-server admin backup FILE --verify` (README, "Data, backups and upgrades"; a timer and the restore procedure: [DEPLOY.md](DEPLOY.md) section 10). It writes a consistent snapshot with `VACUUM INTO` while the server runs, in one pass, and needs free space for a full copy of the database. Run it at quiet hours: the WAL grows past its 64 MiB limit while the snapshot is open. Do not use the `sqlite3` shell's `.backup` on a busy server: it copies a few pages at a time and starts again whenever another connection writes, so it may never finish. A provider's daily disk image is a complement only: it is taken without telling SQLite, so it is only crash-consistent (inferred).

## Logins, connections and restarts

### Password logins

A password login, a registration, a password reset and an account change that asks for the password each cost one Argon2id hash (64 MiB, 3 passes, 4 lanes computed on one thread), and a password change two; a reconnection with the session token costs no hash. Measured: 0.168 s of CPU per login on the test vCPU, about 0.31 s on an OVH vCore (inferred). Sessions last 30 days idle (`SESSION_IDLE_DAYS`), so password logins are rare in normal operation.

The whole server runs at most `PASSWORD_HASH_CONCURRENCY` hashes at once (`WORKERS` by default, so 2 on VPS-1): at most about 6.5 logins per second on VPS-1 with both vCores doing nothing else, and about 2 per second from the 30 % of CPU left at the comfortable point (inferred). Up to `PASSWORD_HASH_QUEUE_MAX` more wait (32 × `WORKERS`), and all the hashes of one request wait at most `PASSWORD_HASH_QUEUE_TIMEOUT_MS` (10 s) together; a request that finds the queue full or whose wait runs out gets 503 `server_busy` with a `Retry-After` of 5 to 15 s, and nothing changes on the server. Once half of the queue waits, one client (an IPv4 address or an IPv6 /48) may have at most `PASSWORD_HASH_WAITERS_PER_SOURCE` (2 × `WORKERS`) hashes waiting, and its next request gets 429. A wave of logins therefore slows and refuses logins; the games only lose the CPU of the hashes in flight. On a VPS-1 near its CPU capacity, `PASSWORD_HASH_CONCURRENCY=1` keeps a hash wave to one of the two vCores.

A failed login is held until it took as long as the slowest password check of the last 10 to 20 minutes, and at least as long as the check the server timed when it started (2 s at most), so its duration does not tell whether the account exists; that wait takes no hash slot.

The login proof of work turns on at `POW_LOGIN_TRIGGER_PER_MIN` (30) failed logins per minute across the server: about 0.15 OVH vCore of hashes (inferred: 30 × 0.31 s ÷ 60 s), within reach of both VPS. It slows an attacker's clients down (18 bits by default) but a GPU pays almost nothing for it.

### New connections: the TLS gate

With `TLS_MODE=native`, the server screens every new TCP connection before any TLS work ([DESIGN.md](DESIGN.md) section 5.7):

1. The protection per address ([below](#protection-per-address)).
2. A new connection has 3 s to send the first record of its TLS ClientHello and holds no handshake slot meanwhile; a silent or malformed one is closed. At most 16 × `MAX_PENDING_HANDSHAKES` connections wait at once (4,096 on VPS-1), and 4 × `MAX_PENDING_HANDSHAKES_PER_IP` per address group (32 on VPS-1).
3. The connection then needs one of the `MAX_PENDING_HANDSHAKES` handshake slots (128 × `WORKERS`: 256 on VPS-1, 512 on VPS-2) and one of the `MAX_PENDING_HANDSHAKES_PER_IP` slots of its address group, an IPv4 address or an IPv6 /48 (by default `MAX_PENDING_HANDSHAKES` ÷ 32: 8 on VPS-1). A connection beyond either cap is closed with a reset, and the game retries after its backoff. A handshake that fails or passes its 10 s timeout gives its slot back and is closed.

At about 0.7 ms of an OVH vCore per new connection (inferred), 256 handshakes in flight are about 0.2 s of work, so the slots turn over quickly and a reconnection storm is served at the speed of the CPU: about 2,700 connections per second on VPS-1 and 5,500 on VPS-2 with the vCores doing nothing else (inferred; the test machine served 4,250-5,030 per second with 2 vCPUs, limited by the load generator). `scacelith_tls_refused_total{reason}` counts the closed connections.

**Players who share one address.** A school, a company network or a mobile operator's carrier-grade NAT puts many players behind one IPv4 address. In normal play the per-group cap does not matter: a handshake holds its slot for one or two network round trips, so 8 slots serve dozens of handshakes per second from one address (inferred). After a restart, players behind one address come back 8 at a time and later than the others. Other limits matter more for such a group: `MAX_CONNECTIONS_PER_IP` (64 WebSocket connections), the protection per address, `AUTH_RATE_PER_IP` (20 password logins, registrations or resets per 10 minutes) and `PASSWORD_HASH_WAITERS_PER_SOURCE`. For a club or a school that plays over one known address, list it in `ABUSE_EXEMPT`, and raise `MAX_CONNECTIONS_PER_IP`, `AUTH_RATE_PER_IP`, `MAX_PENDING_HANDSHAKES_PER_IP` (below `MAX_PENDING_HANDSHAKES`) and `PASSWORD_HASH_WAITERS_PER_SOURCE` for it (README, "Protection against abuse").

**Load tests from one machine.** Every client of a load machine shares its address, and `scacelith-bench` counts a refused connection as failed. On the test instance, set `ABUSE_EXEMPT` to the load machines' addresses, `MAX_PENDING_HANDSHAKES_PER_IP` to one below `MAX_PENDING_HANDSHAKES` (255 with 2 workers) and `MAX_CONNECTIONS_PER_IP` above the clients per load address, and keep `--inflight` (200 by default) below `MAX_PENDING_HANDSHAKES`, as `bench/run.sh` does. See [Validating on the real machine](#validating-on-the-real-machine).

### Protection per address

Every request and every connection first meets a per-address layer (README, "Protection against abuse"; [DESIGN.md](DESIGN.md) section 8): a request budget (`HTTP_RATE_PER_IP`, 600 per minute with a burst of half a minute), a cap on the requests in progress (`IP_MAX_INFLIGHT`, 32 × `WORKERS`), and with native TLS, before any TLS work, a new-connection rate (`IP_CONN_RATE`, 10 per second with a burst of 4 s) and a cap on open connections (`IP_MAX_CONNECTIONS`, 128). An address is an IPv4 address or an IPv6 /64, and an IPv6 /48 gets 4 times each limit. These are whole-server limits, counted exactly in the one process. An address that keeps going after being refused (`ABUSE_BLOCK_REFUSALS_PER_MIN`, 600 refusals in a minute) is blocked for 1 minute, then 4, 16 and 60 minutes at each new block within 6 hours; one that reaches the threshold within a second is blocked at once.

**What one address can still cost on VPS-1** (default settings, OVH vCore, inferred from the unit costs):

| Resource | Bound for one IPv4 address or IPv6 /64 | Cost at most |
|---|---|---|
| Requests | 600 per minute, a burst of 300 | about 0.4 % of a vCore if every one exports a PGN (0.44 ms), less for the other routes |
| Requests in progress | 64 (a body sent slowly, a wait in the hash queue, a GIF waiting for its render, a data export), each until its answer is sent or its timeout | memory and places in the queues, not CPU |
| New connections | 10 per second, a burst of 40 | about 0.7 % of a vCore if each is a full TLS handshake, upgrade and `Hello` (0.7 ms) |
| Open connections | 128, of which 64 WebSockets (`MAX_CONNECTIONS_PER_IP`) | about 3.5 MiB |
| Password hashes | `AUTH_RATE_PER_IP`, 20 per 10 minutes for sign-in, registration and reset, and as many again for the account changes that ask for the password, where a password change hashes twice: about 60 per 10 minutes | about 3 % of a vCore (0.31 s each) |
| GIF renders | 12 per minute, 120 per hour, at nice 19 | 11 % of a vCore over a minute, 1.8 % over an hour, with the most expensive GIF ([Animated GIFs](#animated-gifs)) |
| Refusals before a block | 600 in a minute | small (below) |

An IPv6 /48 may cost 4 times each figure (5 times for the password hashes, `AUTH_RATE_PER_PREFIX`; 3 times for the GIFs). Many addresses that each stay under these bounds are not stopped here: the capacity limits (the hash queue, the TLS gate, `MAX_CONNECTIONS`) and the provider are what remain.

**Connections refused before TLS.** A connection from a blocked address, or one beyond `IP_CONN_RATE` or `IP_MAX_CONNECTIONS`, is accepted, checked and reset without any TLS work. Its cost was not measured on this server; the Node.js server took 26-30 µs of the test vCPU for the same path, kernel included, so count at most about 55 µs of an OVH vCore (inferred, an upper bound): 1,000 such connections per second take at most about 5 % of a vCore. Only the provider's edge or a filter on the host remove that cost ([Floods](#floods)).

Behind a reverse proxy (`TLS_MODE=proxy`) only the request budget, the requests in progress and the blocks apply, per `X-Forwarded-For` client, and a block answers 429 without closing the proxy's connection. The connection limits are then the proxy's job (nginx: `limit_conn`, `limit_req`), at about the figures above.

### When the server is full

`MAX_CONNECTIONS` counts the signed-in players of the whole server. A newcomer beyond it still completes the TLS handshake and the WebSocket upgrade, is refused at `Hello` with `ServerFull`, and the game waits 60 to 120 s before trying again: 1,000 newcomers waiting for a place cost about 0.02 vCore (inferred: each attempt reads `/api/v1/info` and opens the WebSocket, two TLS connections, about 1.5 ms). `scacelith_ws_hello_total{result="server_full"}` counts these refusals: it is the metric that shows a full server. A player whose game is in progress is still admitted at `Hello`, whatever the count. So that such a player can reach `Hello`, WebSocket upgrades may go max(16, 2 %) beyond `MAX_CONNECTIONS`; beyond that reserve the upgrade gets HTTP 503 (`scacelith_ws_handshakes_rejected_total{reason="server_full"}`), and for up to 5 s after such a refusal, or while the server holds 1.2 times `MAX_CONNECTIONS`, the TLS gate lets only `MAX_PENDING_HANDSHAKES` ÷ 2 new connections per second through (128 on VPS-1) and closes the others before any TLS work (`scacelith_tls_refused_total{reason="server_full"}`). That bounds the TLS work while shedding to about 0.09 vCore on VPS-1 (inferred: 128 × 0.7 ms). On the default shared port the API is slowed down with the upgrades; a separate `WS_PORT` keeps the API outside that limit.

### Restarts

After a restart every client comes back at nearly the same moment. The server applies any new migration, replays the journal and restores the games that were running, then binds its listeners; with an empty journal it was ready in 24 ms (the replay of a full journal, up to about 80 MiB per shard, was not measured). The kernel queue in front of the listener holds `LISTEN_BACKLOG` (2,048) connections, capped by `net.core.somaxconn`. For 10 minutes after losing a connection that had reached `Welcome`, a client reuses the `GET /api/v1/info` answer it was made with, after a crash as after a graceful restart, so each reconnection costs one TLS handshake, the upgrade and `Hello`: about 0.7 ms of an OVH vCore (inferred). The game spreads its attempts:

- after a crash, the first attempt comes within 0.5-2 s;
- after a graceful restart (the server announces it `SHUTDOWN_GRACE_MS`, 3 s, before closing), players with a game in progress try within 1-8 s and the others within 5-35 s;
- later attempts wait a random delay between 0.5 s and min(30 s, 2 s × 2^n) after the n-th failure, at most 8 s for players with a game in progress.

CPU work of the reconnection wave, with every vCore on it (inferred; the clients' backoff adds a few seconds, since a refused attempt waits before the next):

| Clients | VPS-1 (2 vCores) | VPS-2 (4 vCores) |
|---|---|---|
| 10,000 | 3.6 s | 1.8 s |
| 28,000 (VPS-1 at its comfortable point, calm players, defaults, mix M1) | 10 s | 5 s |
| 62,000 (VPS-2, the same) | | 11 s |
| 67,000 (VPS-1 at its memory limit) | 24 s | 12 s |
| 150,000 (VPS-2 at the suggested `MAX_CONNECTIONS`) | | 27 s |
| 200,000 on 7 vCores | 21 s | |

Two settings decide whether the games survive:

- `RECOVERY_GRACE_MS` (90 s): both players of a restored game have this long, from the replay, to come back (or the normal grace when that is longer). The default covers every row above, the clients in the menus included, who retry up to 30 s apart (inferred).
- `RECOVERY_CLOCK_HOLD_MS` (20 s): the clock (or first-move timer) of the side to move stays stopped until that player is back, for this long at most; after it the clock runs and a late player loses the difference. A player in a game may wait up to 8 s between two attempts, so the wave must be done in about 12 s, about 9 s of CPU work once the clients' backoff is counted: the default covers about 25,000 clients on VPS-1 and 50,000 on VPS-2 (inferred). Above that, set 45000, which covers both machines up to their memory limits (inferred: 27 s of work plus 8 s). The value must stay below `RECOVERY_GRACE_MS`, and it is also the free thinking time a player could take by staying away on purpose, so do not raise it further. In a game restored before its second ply, a player who comes back on its turn before its first-move time ran out gets the whole `FIRST_MOVE_TIMEOUT_MS` (30 s) from that reconnection, even after the hold.

Restart at quiet hours, and keep `SHUTDOWN_GRACE_MS` so that the clients receive the notice and put the players with a game in progress first.

## Recommended settings

### Server configuration

Three gesture profiles, by what the operator wants most (capacity at the comfortable point, inferred):

| Profile | Settings | VPS-1: calm / moving players | VPS-2: calm / moving players |
|---|---|---|---|
| Live gestures as designed | defaults (`GESTURE_RATE=4`, `GESTURE_IDLE_MS=1000`) | 9,300 / 3,200 games | 20,300 / 7,000 games |
| Live gestures, lighter | `GESTURE_IDLE_MS=3000`, `GESTURE_RATE=2` | 16,300 / 5,700 games | 35,600 / 12,400 games |
| Most players: no live gestures | `GESTURE_RATE=0` | 26,000 games, 68,000 connected (mix M1), whatever the players do | 57,000 games, 149,000 connected |

Once `scacelith_gestures_relayed_total` shows the real r, use the [capacity table](#worked-example-ovh-vps-1-2-vcores-4-gb-and-vps-2-4-vcores-8-gb) for it.

| Setting | 2 vCores, 4 GB | 4 vCores, 8 GB | Why |
|---|---|---|---|
| `WORKERS` | auto | auto | one runtime thread and one game shard per vCore |
| `GESTURE_RATE`, `GESTURE_IDLE_MS` | one of the profiles above | same | the opponent's live gestures are most of the CPU of a game ([Gestures](#gestures)) |
| `MAX_CONNECTIONS` | 60000, or the CPU figure of your worst case | 150000, or the CPU figure | the default of 200,000 is more than the memory holds ([Memory](#memory)) |
| `CLIENT_PING_INTERVAL_MS` | 10000 (default) | 10000 | 2000 makes the ping display livelier at about 30 % of the capacity |
| `HEARTBEAT_INTERVAL_MS`, `HEARTBEAT_TIMEOUT_MS` | 10000, 30000 (defaults) | same | the heartbeat round trip feeds the lag compensation |
| `RECOVERY_GRACE_MS`, `RECOVERY_CLOCK_HOLD_MS` | 90000 and 20000 (defaults); a hold of 45000 when more than about 25,000 players may be connected | the same; a hold of 45000 above about 50,000 players | the hold must cover the reconnection wave ([Restarts](#restarts)) |
| `LISTEN_BACKLOG` | 2048 (default) | 2048, and 8192 above about 50,000 players | the kernel queue in front of the listener during a reconnection wave ([System](#system)) |
| `DB_CACHE_MB`, `DB_MMAP_MB` | empty (192), 256 (defaults) | empty (320), 256 | ample for these machines; cap `DB_CACHE_MB` at 256 on larger ones |
| `PASSWORD_HASH_CONCURRENCY` | empty (2), or 1 near the CPU capacity | empty (4) | each hash in flight takes a vCore for 0.31 s and 64 MiB |
| `POW_LOGIN_TRIGGER_PER_MIN` | 30 (default) | 30 | about 0.15 vCore of hashes, within reach of both machines |
| `MAX_CONNECTIONS_PER_IP`, `MAX_PENDING_HANDSHAKES_PER_IP`, `PASSWORD_HASH_WAITERS_PER_SOURCE` | 64, empty (8), empty (4) (defaults) | 64, empty (16), empty (8) | raise them for a school or a club that plays behind one address |
| `HTTP_RATE_PER_IP`, `IP_CONN_RATE`, `IP_MAX_CONNECTIONS`, `IP_MAX_INFLIGHT`, `ABUSE_BLOCK_REFUSALS_PER_MIN` | 600, 10, 128, empty (64), 600 (defaults) | same, `IP_MAX_INFLIGHT` 128 | what one address can cost stays a few percent of a vCore ([Protection per address](#protection-per-address)) |
| `ABUSE_EXEMPT` | empty | same | the addresses of your monitoring and load machines, and of a school or club that plays over one address |
| `JOURNAL_FLUSH_MS`, `JOURNAL_FSYNC`, `DB_COMMIT_MS` | 50, true, 50 (defaults) | same | an NVMe disk has plenty of room |
| `ANALYSIS_ENGINE_PATH`, `ANALYSIS_WORKERS` | optional: the official Stockfish 19 binary, 1 | the same, 2 when the CPU allows | one engine takes a whole vCore at low priority and about 270 MB (70 MB more per further engine) |
| `GIF_THREADS`, `GIF_CACHE_MB` | empty (2, 64) (defaults) | empty (4, 128) | low-priority threads, about 104 / 208 MiB while players make GIFs |
| `TLS_MODE`, `TLS_MIN_VERSION` | native, TLSv1.2 (defaults) | same | the TLS gate and the protection per address need native TLS |
| `TLS_CERT_FILE`, `TLS_KEY_FILE` | full chain, ECDSA P-256 key | same | the key type of the measurements; an RSA key makes each handshake cost more |

The rest of a production configuration (data directory, public host name, mail, Google sign-in, secrets) is in [DEPLOY.md](DEPLOY.md).

### System

- **One listening port.** The API and the game WebSocket share `API_PORT` (443 by default), and every player connection is a socket on that one port: inbound connections need no extra ports and no change to the ephemeral port range. The metrics port (9464) listens on 127.0.0.1. A custom `API_PORT` inside the ephemeral range (32768-60999) must be reserved (README, "Kernel settings").
- **Open files.** Each connection takes one file descriptor. The unit of `deploy/systemd/` sets `LimitNOFILE=1048576`, and the server raises its own soft limit to the hard limit at start (the `started` log line shows it as `nofile`; `scacelith_process_max_fds` too).
- **Listen backlog.** The kernel caps `LISTEN_BACKLOG` (2048) at `net.core.somaxconn`, and connections still in their TCP handshake wait in a queue bounded by `net.ipv4.tcp_max_syn_backlog`. The values below hold a reconnection wave; above about 50,000 players use the larger ones together with `LISTEN_BACKLOG=8192` (inferred: a wave then brings tens of thousands of connections within the first two seconds).

```ini
# /etc/sysctl.d/90-scacelith.conf, applied with: sysctl --system
net.core.somaxconn = 4096           # 8192 above about 50,000 players
net.ipv4.tcp_max_syn_backlog = 8192 # 16384 above about 50,000 players
```

- **Connection tracking.** Without a stateful firewall on the machine, there is nothing to do. With one (nftables or iptables rules on `ct state`, ufw, firewalld), every player connection takes an entry of the conntrack table, and closing connections keep theirs for a while; its size, `net.netfilter.nf_conntrack_max`, scales with the RAM (65,536 on many 4 GB machines: read it with `sysctl`), and a full table drops packets. Set it to at least twice the connections you expect (262144 on VPS-1, 524288 on VPS-2 or for 100,000 games), or exempt the game port from tracking with a `notrack` rule. An `inet` table with a `ct state` rule tracks the IPv4 game traffic too: filter IPv6 in an `ip6` table if that is all you need.

Nothing else is needed: the server needs no other kernel setting.

## Provider firewall: the OVH Edge Network Firewall

The OVH Edge Network Firewall is stateless, filters IPv4 only, applies the first matching rule, and allows 20 rules per IP (priorities 0-19). Accept rules alone do nothing: the final Deny rule is required. Being stateless, it must explicitly accept the replies to connections the server opens itself.

| Priority | Action | Protocol | Source | Source port | Destination port | TCP option | Purpose |
|---|---|---|---|---|---|---|---|
| 0 | Accept | TCP | any | | 32768-60999 | established | replies to outgoing connections: SMTP 587/465, Google 443, apt, ACME, DNS over TCP |
| 1 | Accept | TCP | any | | 443 | | API and WSS (`API_PORT`) |
| 2 | Accept | TCP | admin IPv4/32 | | 22 | | SSH |
| 3 | Accept | TCP | second admin /32 (optional) | | 22 | | SSH, or the machine that pulls backups |
| 4 | Accept | ICMP | any | | | | ping, and the path-MTU "fragmentation needed" messages that TLS relies on |
| 5 | Accept | TCP | any | | 80 | | only for certbot HTTP-01; not needed with DNS-01 |
| 6 | Accept | UDP | resolver /32 | 53 | | | only if not using the OVH resolver |
| 7 | Accept | UDP | 1-2 NTP servers /32 | 123 | | | only if not using `ntp.ovh.net`; never without a pinned source (NTP amplification) |
| 19 | Deny | IPv4 | any | | | | everything else, including the metrics port |

What it does not cover:

- IPv6 is not filtered, and recent images configure it by default. The game server binds IPv4 only by default (`BIND_ADDRESS=0.0.0.0`), but sshd listens on `::` unless changed: use keys only, and `ListenAddress 0.0.0.0` (on Ubuntu 22.10 and later sshd is socket-activated: run `systemctl daemon-reload` and `systemctl restart ssh.socket`, then check with `ss -tlnp 'sport = :22'`). Publish no AAAA record.
- Traffic from inside the provider's network is not filtered, other customers included. It also needs no rule: use the provider's NTP server (OVH: `ntp.ovh.net`, in `timesyncd.conf` or chrony; TOTP codes and Google ID tokens need a correct clock) and resolver (OVH: 213.186.33.99), and check them with `timedatectl timesync-status` and `resolvectl status`.
- The metrics port (9464) is bound to 127.0.0.1 by default; read it through an SSH tunnel.
- The firewall does not limit the rate of each source; the server does that itself ([Protection per address](#protection-per-address)).

To avoid locking yourself out: take your IPv4 address from `echo $SSH_CLIENT` inside an `ssh -4` session, keep that session open, test a new `ssh -4` login, and only then add the Deny rule. The rules can also be edited from the provider's control panel, and the KVM console and rescue mode remain available, so give the local console a password even with key-only SSH.

### Floods

The edge firewall cannot tell a flood on the game's own port from the players, and has no rate limit. What it does against floods:

- **Everything that is not the service is dropped at the edge**, before it reaches the VPS link: the table above, with its final Deny. UDP floods, reflection and amplification on any port, and TCP floods on other ports then never reach the machine. The server needs no inbound UDP.
- **TCP fragments**: if the rule form offers the "fragments" option, add a Deny for fragmented TCP to 443 at a priority before the 443 Accept (renumber the rules). Legitimate TLS over TCP is not IP-fragmented.
- **Floods on 443 itself** from many addresses (SYN, ACK or data floods) are the job of OVH's always-on anti-DDoS mitigation, which starts by itself when it detects an attack and keeps the firewall rules during the mitigation (inferred from OVH's documentation; the control panel shows the mitigation).
- **A few persistent sources** that the server blocks again and again (`scacelith_abuse_blocks_total{level="4"}` growing, the same addresses in the `ip blocked` log lines; with `LOG_IP=truncated` they show the /24, the right size for an edge rule): a temporary Deny rule per source /24 or /32, at a priority before the 443 Accept, takes their connections off the machine (each one still costs up to about 55 µs when the server resets it itself, [Protection per address](#protection-per-address)). Keep such rules few, dated, and removed when the attack ends: there are 20 rules in all, and a /24 may hold a school or a carrier's CGNAT pool.
- **IPv6** is not filtered at the edge: keep the server on IPv4 with no AAAA record. If IPv6 is enabled later, an `ip6` nftables table on the host does the edge's job, and the server's /64 and /48 handling applies.

Optionally, the host can drop the connection attempts the server would refuse anyway, before they reach it: a per-source meter of new TCP connections in an `ip` table, without connection tracking. Set it well above what the server itself lets one address open: `IP_CONN_RATE` is 10 per second with a burst of 40, so a meter of 40 per second with a burst of 80 drops only what is far beyond it, and the server's own refusals stay the normal path. Raise it with `IP_CONN_RATE`, and keep a whole CGNAT address in mind. A dropped SYN is sent again by the client after about a second, so a legitimate client over it waits rather than fails. This ruleset passes `nft -c` (a check that applies nothing) with nftables 1.0.9; check it again on the machine's version before loading it (`nft -f`, and for good in `/etc/nftables.conf`):

```
table ip scacelith {
  set syn4 {
    type ipv4_addr
    size 65536
    flags dynamic,timeout
    timeout 60s
  }
  chain input {
    type filter hook input priority -10; policy accept;
    tcp dport 443 tcp flags & (syn | ack) == syn update @syn4 { ip saddr limit rate over 40/second burst 80 packets } counter drop
  }
}
```

It keeps one entry per source address seen in the last minute (at most 65,536), and `nft list set ip scacelith syn4` shows them. The kernel drops a SYN there for less than the server spends to accept and reset a connection.

## Validating on the real machine

1. **CPU steal**: run `vmstat 1 300` when quiet and again at the evening peak, and read the `st` column. Above 5 % sustained, take it off `s`; above 10 %, the figures on this page are optimistic.
2. **Speed of one vCore**: time this single-threaded reference 5 times and keep the smallest CPU time (user + sys):

   ```sh
   for i in 1 2 3 4 5; do time taskset -c 0 sh -c 'dd if=/dev/zero bs=1M count=1024 status=none | b2sum >/dev/null'; done
   ```

   The test vCPU takes 1.69 s (1.69-1.77 over 5 runs), so `s` ≈ 1.69 ÷ your time; expect about 3.1 s at `s` = 0.55 (inferred). `md5sum` in place of `b2sum` is a second reference (2.06 s on the test vCPU). Do not use `sha256sum`: the test vCPU has the SHA extensions, which many hosts (an OVH Haswell vCore among them) lack. Such a reference approximates the server's mix of work; the load test below is the real check.
3. **Real vCores**: run as many copies in parallel as there are vCores, `time (for i in $(seq $(nproc)); do taskset -c $((i-1)) sh -c 'dd if=/dev/zero bs=1M count=1024 status=none | b2sum >/dev/null' & done; wait)`. The wall time should stay close to one run; if it doubles, the vCores are shared or are hyperthreads.
4. **ADX**: `grep -cw adx /proc/cpuinfo` prints 0 with the QEMU Haswell model, which confirms the slower TLS path.
5. **Load test from another machine** (on the same machine the load generator takes as much CPU as the server), against a separate test instance with its own `DATA_DIR`:
   - Accounts: `scacelith-server admin bench-accounts --count 20000 --out tokens.tsv --format tsv --i-know-this-is-a-test-server`. That flag is the only safeguard: the command creates verified accounts with live sessions in whatever database the configuration points to.
   - Server: the settings of `common_env` in `bench/run.sh` (the load machines' addresses in `ABUSE_EXEMPT`, the per-account and per-pair limits raised, `GESTURE_RATE` at least the `--gesture-hz` of the test), `MAX_CONNECTIONS_PER_IP` above the clients per load address (one source address opens at most about 28,000 connections to one port), `MAX_PENDING_HANDSHAKES_PER_IP` one below `MAX_PENDING_HANDSHAKES`, and the metrics through `ssh -L 9464:127.0.0.1:9464`.
   - Load, with `--addr IP:443 --host NAME --tokens tokens.tsv --metrics-addr 127.0.0.1:9464` on each command (`--ca FILE` for a self-signed certificate; without it any certificate is accepted):
     - connections: `scacelith-bench connections --steps 10000,30000 --inflight 200 --out connections.json`;
     - games at the pace of this page: `scacelith-bench games --steps 2000,5000,10000 --move-interval-ms 5000 --gesture-hz 1 --start-rate 100 --warmup-s 30 --duration-s 120 --out games.json`, raising the steps until the CPU passes 70 % or the move relay p99 passes a few tens of milliseconds; `--gesture-hz 0` for the profile without gestures, `0.33` for calm players at `GESTURE_IDLE_MS=3000`, `GESTURE_RATE` for moving players;
     - `scacelith-bench table connections.json games.json` prints the tables.
   - The load machine needs about 0.6 core per 16,000 bots at one gesture per second, 1.6 cores at four, and 2 cores for a ramp of about 4,500 connections per second (measured on the test vCPU). Its own CPU is in each report: keep it below 85 % of its cores.
   - Afterwards, restore `ABUSE_EXEMPT`, `MAX_CONNECTIONS_PER_IP`, `MAX_PENDING_HANDSHAKES_PER_IP` and your `MAX_CONNECTIONS`.
6. **In production**, watch `/metrics`, the size of `DATA_DIR` and `vmstat`:

| Metric | What to look for |
|---|---|
| `scacelith_process_cpu_ratio`, `scacelith_runtime_lateness_p99_ms` | CPU (in cores: 1 = one core) above 70 % of the vCores at the peak; a rising lateness of the runtime's 10 ms timer (ready tasks waiting for a runtime thread) |
| `scacelith_ws_connections`, `scacelith_ws_players`, `scacelith_process_rss_bytes` | connections and memory against the memory figure and `MAX_CONNECTIONS` |
| `scacelith_process_open_fds`, `scacelith_process_max_fds` | file descriptors against their limit |
| `scacelith_games_active`, `scacelith_gestures_relayed_total`, `scacelith_gestures_dropped_total{reason}` | r = the relayed rate ÷ (2 × games in progress) ([Gestures](#gestures)); `backlog` drops mean slow players or an overloaded server, `rate` drops a client beyond `GESTURE_RATE` (every gesture of a modified client with `GESTURE_RATE=0`) |
| `scacelith_game_stall_ms`, `scacelith_game_timer_late_ms` | stalls of a game host (the games do not charge them to the players, up to `GAME_STALL_CREDIT_MAX_MS`); frequent ones of 100 ms or more mean an overloaded machine, CPU steal or a slow disk |
| `scacelith_tls_refused_total{reason}`, `scacelith_tls_hello_waiting`, `scacelith_tls_handshakes_pending` | `handshakes` or `per_ip` refusals outside restarts (not enough CPU for the handshakes, or a shared address), `hello_timeout` and `bad_hello` from scanners |
| `scacelith_http_rate_limited_total{limit}`, `scacelith_tls_refused_total{reason}` (`blocked`, `conn_rate`, `conn_open`), `scacelith_abuse_blocks_total{scope,level}`, `scacelith_abuse_blocked{scope}` | the protection per address: refusals of ordinary players (a school, a carrier's address: `ABUSE_EXEMPT`), blocks at level 4 from the same sources (an edge rule, [Floods](#floods)); each block is logged (`ip blocked`) |
| `scacelith_ws_hello_total{result="server_full"}`, `scacelith_ws_handshakes_rejected_total{reason="server_full"}` | newcomers refused at login by `MAX_CONNECTIONS`, the sign of a full server; upgrades refused (HTTP 503) once its reserve is in use too, after which the TLS gate sheds (`scacelith_tls_refused_total{reason="server_full"}`) |
| `scacelith_password_hash_rejected_total{reason}`, `scacelith_password_hash_queued`, `scacelith_password_hash_wait_ms` | `queue_full` or `timeout` outside an attack: not enough CPU for the logins; `source_limit`: clients held to `PASSWORD_HASH_WAITERS_PER_SOURCE` waiting hashes while the queue was at least half full |
| `scacelith_journal_disk_bytes` (per shard) | more than about (`JOURNAL_COMPACT_SEGMENTS` + 1) × 16 MiB |
| `scacelith_journal_errors_total`, `scacelith_game_commit_unjournaled_total` | any increase: the journal cannot be written, and finished games are committed without it; fix the disk before a restart |
| `scacelith_retention_runs_total{result}`, `scacelith_retention_run_seconds` | a `failed` result, or runs growing longer |
| `scacelith_anticheat_analysis_engines`, `scacelith_anticheat_analysis_engines_shared` | fewer engines than `ANALYSIS_WORKERS` (an engine that does not start), fewer sharing engines than engines (each of those takes 110 MB more; the log says why) |
| `scacelith_anticheat_analysis_queue_ordinary`, `scacelith_anticheat_analysis_queue_priority`, `scacelith_anticheat_analysis_skipped_total{reason}` | the ordinary queue held at `ANALYSIS_QUEUE_MAX`, a growing priority queue |
| `scacelith_gif_renders_total{result}`, `scacelith_gif_render_duration_ms`, `scacelith_gif_queue`, `scacelith_gif_cache_total{result}`, `scacelith_http_rate_limited_total{limit=~"gif.*"}` | `busy` renders (503 `server_busy`) or a rising render time: the CPU is full and the GIFs wait for the games, as intended; a low cache hit rate is normal (each game is asked for once or twice) |

## Risks, largest first

1. **The players' gesture rate.** Between calm players and players who move all the time at the default `GESTURE_RATE`, the capacity changes by a factor of 2.9, and nobody knows r before production. Start with a profile of [Recommended settings](#recommended-settings), watch `scacelith_gestures_relayed_total`, and adjust.
2. **Real vCore speed and steal.** `s` = 0.55 comes from a chain of earlier measurements, not from an OVH machine, and moves every CPU figure by about ±10 %, more with heavy steal. Measure it.
3. **Disk space.** Nothing archives finished games: a VPS-1 full of games without gestures every evening fills the database space of its 40 GB disk in about 12 days, and in about a month with calm players and the defaults (inferred).
4. **What the runs did not cover.** The figures for r = 0 are extrapolated from the runs at 1 and 4 gestures per second; the 70 % comfortable point is inferred from two measured points; no game ended during the measurement windows, so the churn of games (starts, ends, commits, rating updates), the lobby and the database writer at hundreds of game ends per second are not in the model; the cost of a connection reset before TLS, the replay of a full journal and a real reconnection wave were not measured on this server; and loopback charges part of the receiving side's TCP work to the sender. Run the load test on the real machine.
5. **Restarts.** Above about 25,000 players on VPS-1 (50,000 on VPS-2), the players to move keep their whole clock only with `RECOVERY_CLOCK_HOLD_MS=45000` (inferred).
6. **The 100,000-game target** is 12 times the largest run (8,000 games), and rests on one database writer and one lobby actor that were never measured at that rate.
7. **Small items.** The memory of a GIF render thread and the times of long renders are inferred; the size of an engine analysis in the database was not measured.
