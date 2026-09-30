# Sizing and hosting

This page tells an operator how many players a machine can hold, what limits it, and how to set up a small VPS. The unit costs were measured on a 2.8 GHz Intel Xeon (Cascade Lake) KVM guest, with the server pinned to 2 vCPUs, the load generator pinned to 2 others, Node 22 and TLS 1.3 over loopback. Figures for another host are scaled by a speed factor `s` (how much work one of its vCores does compared with the test vCPU). Values marked "(inferred)" are reasoned from the measurements, not measured directly. The measurements predate the server's admission limits (the TLS gate, the password-hash cap, the upgrade reserve) and journal compaction, so what these change is inferred as well. Raw benchmark results are in [BENCHMARK.md](BENCHMARK.md), and every setting is described in [CONFIG.md](CONFIG.md).

Terms used throughout:

- **Mix M1**: 60 % of connected players are in a game at a 3+2 pace, 40 % are idle in menus. Games in progress are 0.3 × connected players.
- **Comfortable**: 55 % server CPU. **p99 limit**: move round trip about 250 ms at the 99th percentile, reached near 64 % CPU. **Hard**: CPU full.
- **Connected player**: anyone signed in. The game keeps its WebSocket open for the whole session, including while the player is in menus.

## Summary

- Storage is SQLite in WAL mode through Node's built-in `node:sqlite`: one file on local disk, no database server.
- CPU limits capacity first: the per-shard event loop. With the default client ping of 10 s (`CLIENT_PING_INTERVAL_MS`), a vCore with `s` = 0.65 holds about 7,000 connected players comfortably (mix M1). With a 2 s ping it holds about 3,750.
- Gestures come on top: the live head and hand movements that the server relays between the two players of a game (`GESTURE_RATE`, at most 4 per player per second by default) cost about 100 µs of an OVH vCore each (inferred from a measurement), so a player in a game who keeps moving costs more than their moves. The game client sends at least one gesture per second for the whole game, even while its player sits still. With the 10 s ping, capacity is divided by 1 + r, where r is the average number of gestures a player in a game sends per second, and r is at least 1 whenever the relay is on: the default configuration holds at most half the figure above, about 3,500 connected players per vCore comfortably at `s` = 0.65, and 1,400 if every player in a game sent the full default rate all the time. Only `GESTURE_RATE=0` (no relay) gives the full 7,000. See [Gestures](#gestures).
- Memory comes next, near 70 KB per connection: about 24,000-29,000 connections on 4 GB and 51,000-62,000 on 8 GB with the settings below (inferred), at or just after the p99 limit of the CPU with a 10 s ping.
- Network bandwidth and disk I/O are not limits on a typical VPS. Disk space is: finished games take about 85 MB per day per 1,000 average connected players, and nothing archives old games.
- Restarts need one setting near the comfortable load: after a crash or a graceful restart alike, every player is back within about 60 s with the 10 s ping, inside the default `RECOVERY_GRACE_MS` of 90 s, but the last players with a game in progress may need about 30 s, more than the default `RECOVERY_CLOCK_HOLD_MS` of 20 s (inferred). See [Restarts](#restarts).

## CPU

### Unit costs

Server CPU on the test vCPU. Divide by `s` for another host.

| Item | Cost |
|---|---|
| Connected player, client ping every 10 s (default) | about 9 µs per second |
| Connected player, client ping every 2 s | 34 µs per second |
| Move | 190-420 µs, higher at low move rates per shard (moves are then handled one by one); TLS is 23-34 % of it |
| Game (start, end, commit, rating update) | about 3 ms |
| Reconnection after a crash or a graceful restart (TLS handshake, WebSocket upgrade, Hello; the client reuses its `GET /info` answer) | about 2.3-2.5 ms (inferred) |
| First connection of a session, or a newcomer's attempt at a full server (the client reads `GET /info` first, which can take a TLS connection of its own) | about 4 ms (inferred: 2.3 ms for the WebSocket, 1.5-2 ms for `/info`) |
| Password login or registration | 0.50-0.55 s with scrypt (Node 22), 0.35 s with Argon2id (Node 24.7 or later) |
| Fixed cost per shard with traffic, per process at idle | about 0.05 core, about 0.013 core |
| Relayed gesture (a `Gesture` in from one player and out to the opponent, TLS included; with 2 workers half of them also cross the bus between the workers) | about 65 µs (52-77; inferred: measured on the development container and scaled, see [Gestures](#gestures)) |

Where the CPU goes at the comfortable point with a 10 s ping (inferred from the model): moves about half, client pings about 20 %, fixed process costs about 23 %, game ends and reconnections the rest. With a 2 s ping, pings and heartbeats take 43 % (the client ping alone about 33 %).

`CLIENT_PING_INTERVAL_MS` sets how often the client pings. The server announces it to the game at connection (in `Welcome`); it accepts 1,000 to 60,000 ms and defaults to 10,000. Lowering it makes the ping indicator more reactive at a CPU cost: at 2 s, capacity falls by a factor of 1.87 for mix M1.

The server's own heartbeat is part of these costs. Each worker walks its connections in slices and pings a connection when its last ping is at least half of `HEARTBEAT_INTERVAL_MS` (10 s) old. Once a worker holds a few hundred connections, a lap over them takes about one interval (from 41 to about 200 connections it takes between half an interval and one), so a loaded worker sends one ping per connection per interval, as in the measurements; a nearly idle worker pings up to twice as often. The game sends one probe Ping when nothing came from the server for 1.5 intervals (7.5 s at least, 90 s at most) and drops the connection after 2 intervals (10 s at least, 120 s at most).

### Speed factor `s`

`s` depends on clock speed, work per cycle, the CPU features the hypervisor exposes, and CPU steal. As an example, an OVH VPS vCore shows up as QEMU's "Intel Core Processor (Haswell, no TSX)" at 2,400 MHz:

- 14 % less clock than the 2.8 GHz test vCPU;
- about 10-15 % less work per cycle for a Haswell-class core;
- no ADX flag, so OpenSSL cannot use its fastest P-256 code and TLS handshakes are somewhat slower (inferred);
- shared vCores and steal, which the provider does not publish.

That gives `s` = 0.60-0.70, with 0.65 as the central value. The CPU model name hides the real host CPU, so measure `s` on the real machine (see [Validating on the real machine](#validating-on-the-real-machine)).

### Latency

At 55 % CPU the loopback move round trip has a p99 of about 70-140 ms, driven by short event-loop stalls. At 60 % expect 150-185 ms. The players' internet round trip (20-80 ms, inferred) comes on top. The knee was measured with little game churn (about 1/40 of a full server's rate of game ends), so treat it as ±10 CPU points and check it on the real machine.

### Capacity per vCore

For mix M1 at `s` = 0.65, the comfortable point is about 3,750 connected players per vCore with a 2 s ping and about 7,000 with a 10 s ping. It scales roughly with `s`, slightly faster than linearly because the fixed costs do not shrink. Other mixes (inferred by scaling the model):

| Mix | Comfortable players per vCore, 2 s ping | Effect of a 10 s ping |
|---|---|---|
| M1: 60 % in game at 3+2 | about 3,750 | ×1.87 |
| M2: 80 % in game, half 1+0 and half 3+0 | about 2,000 | about ×1.35 (moves dominate) |
| M3: 30 % in game at 10+0 | about 5,600 | about ×2.6 (pings dominate), then memory limits |

### Worked example: OVH VPS-1 (2 vCores, 4 GB) and VPS-2 (4 vCores, 8 GB)

Connected players, mix M1, comfortable / p99 limit / hard:

| | Client ping 2 s | Client ping 10 s (default) |
|---|---|---|
| VPS-1, s = 0.60 | 6,700 / 10,300 / 17,300 | 12,300 / 19,600 / 34,300 |
| **VPS-1, s = 0.65** | **7,500 / 11,400 / 19,100** | **14,000 / 22,000 / 38,300** |
| VPS-1, s = 0.70 | 8,400 / 12,600 / 21,000 | 15,700 / 24,500 / 42,300 |
| VPS-2, s = 0.60 | 13,600 / 20,700 / 34,800 | 25,000 / 39,600 / 69,200 |
| **VPS-2, s = 0.65** | **15,300 / 23,100 / 38,500** | **28,400 / 44,500 / 77,100** |
| VPS-2, s = 0.70 | 17,000 / 25,500 / 42,200 | 31,900 / 49,500 / 85,200 |

Games in progress at the comfortable point (`s` = 0.65): 2,250 with a 2 s ping and 4,200 with a 10 s ping on VPS-1, 4,600 and 8,500 on VPS-2.

With a 10 s ping, memory caps VPS-1 at 24,000-29,000 connections and VPS-2 at 51,000-62,000 (inferred, see [Memory](#memory)). On both machines memory comes at or just after the p99 limit (22,000 and 44,500 at `s` = 0.65), and the hard CPU limit cannot be reached. The VPS-2 figures lean about 5-10 % pessimistic, because the fixed cost per shard measured on 4 cores is lower than the model's (inferred).

A 2-vCore VPS is enough to start. Move to 4 vCores when the peak regularly exceeds about 10,000 connected players, when peak CPU passes 55 %, when the database approaches 10 GB, or when you want engine-based anti-cheat analysis (one engine takes a whole vCore).

These figures leave out the gestures, which the next section adds.

### Gestures

During a game the client sends its player's live gestures (the head, the piece in hand and where it is aimed, a move placed before the clock press) whenever they change, at most `GESTURE_RATE` per second (4 by default, in bursts of `GESTURE_BURST`, 8), and the server relays each one to the opponent without storing it. The client also sends one at least once a second when nothing changed, for the whole game and on the opponent's turn too: the opponent's client relies on it (it stops following the head 2.5 s after the last gesture, and puts back a piece it mirrors after 5 s). A player in a game who sits still therefore sends about one gesture per second, and one who looks around or moves a piece up to `GESTURE_RATE`. Call r the average number of gestures a player in a game sends per second: with the game client it is 0 only with `GESTURE_RATE=0`, and otherwise between about 1 and `GESTURE_RATE`. Where it lies depends on how the players play, so it is known only in production: `scacelith_gestures_relayed_total` per second divided by the players in a game (twice `scacelith_games_active`).

**Cost of one gesture.** The relay was measured on the development container, a 4-vCPU Intel Xeon at 2.10 GHz shared with other jobs, with the load generator on the same machine: the same 1,000 games (2,000 players, one move per 5 s per game, that is 5 s of thinking per move and 200 moves/s, no game ending) on 2 workers, with every player sending 0, 2 or 4 gestures per second, used 0.13-0.15, 0.35-0.41 and 0.49-0.55 server cores ([BENCHMARK.md](BENCHMARK.md#gesture-relay)). That is 44-65 µs per relayed gesture, about 55 µs in the middle: the TLS record in and the one out, the router and the host, and for half of the gestures the hop between the workers over the bus (with 4 workers, where three quarters of them cross it, one run on a busy machine gave about the same, 57 µs). That container's vCPU did the scrypt reference of [Validating on the real machine](#validating-on-the-real-machine) in 0.42 s (0.41-0.43) against 0.50 s for the test vCPU, so a gesture costs about 65 µs (52-77) of the test vCPU and about 100 µs (80-120) of an OVH vCore at `s` = 0.65 (inferred). That is about as much as a client ping exchange (62 µs of the test vCPU, from the unit costs above) and a sixth of a move.

**Capacity.** For mix M1 at the comfortable point, a connected player costs about 39 µs per second of the test vCPU with the 10 s ping, and about 73 µs with the 2 s ping, besides the fixed process costs (inferred from the model above). Gestures add 0.6 × r × 65 = 39 × r µs per second, so the comfortable capacity is multiplied by 1 / (1 + r) with the 10 s ping and by 73 / (73 + 39 × r) with the 2 s ping (inferred; the uncertainty of the cost of a gesture moves these factors by about ±10 % at r = 1 and ±15 % at r = 4). Connected players at the comfortable point, mix M1, `s` = 0.65:

| r (gestures per second per player in a game) | Factor, 10 s ping (default) | VPS-1 | VPS-2 | Factor, 2 s ping | VPS-1 | VPS-2 |
|---|---|---|---|---|---|---|
| 0 (`GESTURE_RATE=0` only: the relay off) | 1 | 14,000 | 28,400 | 1 | 7,500 | 15,300 |
| 1 (every player in a game sitting still, at any `GESTURE_RATE`; or anyone at `GESTURE_RATE=1`) | 0.50 | 7,000 | 14,200 | 0.65 | 4,900 | 9,900 |
| 2 (every player in a game moving all the time at `GESTURE_RATE=2`) | 0.33 | 4,700 | 9,500 | 0.48 | 3,600 | 7,300 |
| 4 (the same at the default rate) | 0.20 | 2,800 | 5,700 | 0.32 | 2,400 | 4,900 |

With the game client, r lies between about 1 and `GESTURE_RATE` whenever the relay is on: the row of the configured rate is the worst case and the row r = 1 the best, and no value between 0 and 1 occurs (inferred). The default configuration therefore at least halves the capacity of the model without gestures. A gesture is small, about 110 bytes in and 100 bytes out at the IP level, one packet each way: in the worst case above a VPS-1 relays about 6,700 gestures per second, about 6 Mbit/s each way, so the network stays out of the way. `GESTURE_RATE` is announced in `Welcome`, so a change applies to the players who connect after the restart; 0 turns the relay off.

## Memory

Per connection: 55-62 KB of server RSS when idle, 58-84 KB in a game, plus about 3.7 KB of kernel socket memory. Budget about 70 KB.

Fixed costs to subtract from the visible RAM before dividing by the cost of a connection, with the OVH example in MiB (VPS-1: 3,826 MiB visible; VPS-2: about 7,650, inferred):

| Item | Size | VPS-1 | VPS-2 |
|---|---|---|---|
| Kept free | 20 % of visible RAM | 765 | 1,530 |
| OS, sshd, journald (inferred) | about 250 MiB | 250 | 250 |
| Node processes (primary + one per worker) | about 70 MB each, plus up to 60 MB of primary growth | 270 | 410 |
| SQLite page cache | `DB_CACHE_MB` × (1 + 2 × workers) connections | 80 | 288 |
| Memory-mapped database pages (reclaimable) | `DB_MMAP_MB` | 256 | 512 |
| Password-hash peak | 128 MiB per scrypt hash in progress (64 MiB with Argon2id), `PASSWORD_HASH_CONCURRENCY` (1) per worker | 256 | 512 |
| Left for connections (inferred) | | about 1,950 | about 4,150 |
| **Connections** (inferred) | the rest ÷ 69-83 KB | **24,000-29,000** | **51,000-62,000** |

- Measured: 20,000 connections in games used 1.41-1.49 GB of RSS. At the comfortable point, VPS-1 uses about 1.9 GiB with a 2 s ping and about 2.3 GiB with a 10 s ping, VPS-2 about 3.6 and 4.4 GiB (inferred).
- The hash peak assumes the default `PASSWORD_HASH_CONCURRENCY=1`: one hash or verification per worker at a time, the dummy check of an unknown account included. Each step higher adds 128 MiB per worker with scrypt. Argon2id halves the peak, to 128 and 256 MiB, except while scrypt hashes are still checked: those of accounts created before, and one at each worker's start to time the padding of failed logins.
- With the default `DB_CACHE_MB=64` (and `DB_MMAP_MB=256`), the budget gives about 21,000-25,000 connections on VPS-1 and 51,000-61,000 on VPS-2 (inferred: 240 MiB more SQLite cache on VPS-1; on VPS-2, 288 MiB more cache and 256 MiB less mapped memory nearly cancel out).
- The engine analysis process, when enabled, adds 180-240 MB (a Node process, its SQLite connection, the engine and its hash table): VPS-2 then holds 48,000-59,000 connections (inferred).
- `DB_MMAP_MB` only covers the start of the file. A database of several GB is read mostly through the OS page cache, and the per-connection cache only keeps the hot pages, so 16 or 32 MB is enough (inferred).

## Network

Bandwidth is not a limit, gestures included (see [Gestures](#gestures)). With a 2 s ping a connected player sends about 85 B/s and receives about 59 B/s at the IP level (1.2 and 0.7 packets per second); a move is about 210 B in and 238 B out. At the comfortable point of a 2-vCore VPS with a 2 s ping that is about 6 Mbit/s in, 4.5 Mbit/s out and 10,000 packets per second in; at its hard CPU limit about 15 Mbit/s in and 11 Mbit/s out, 26,000 packets per second in: 3 % of a 500 Mbit/s link (inferred by scaling the model). A 10 s ping reduces the traffic per player further, and a 4-vCore VPS does twice as much on a 1 Gbit/s link. Watch packets per second rather than bandwidth: providers rarely publish a limit, and a virtio-net interface handles about 100,000 packets per second (inferred). A reconnection costs 4-7.5 KB per client, which is 10-60 Mbit/s at 300-1,000 reconnections per second, after a crash or a graceful restart alike, since the client reuses its `/info` answer in both cases.

## Database and disk

### Storage engine

The server uses the SQLite bundled with Node (3.51 in Node 22.22) through `node:sqlite`, hence Node 22.13 or later. The database is `DATA_DIR/scacelith.db`, in WAL mode with `synchronous=FULL`, a WAL cut back to 64 MB after checkpoints, foreign keys on, a `busy_timeout` of 5 s and `secure_delete` on. Every read followed by a write goes through `BEGIN IMMEDIATE`. Each process has its own connections: 1 for the primary, 2 per worker (its own and its writer thread's), and 1 for the analysis process when it is enabled. That makes 5 connections with 2 workers and 9 with 4, each with its own `DB_CACHE_MB` cache.

Migrations are numbered SQL files (001 to 003 today). `start` applies the missing ones in order before the workers accept players, and `node bin/scacelith-server.js migrate` does the same and exits. Each runs in its own transaction and is recorded with a SHA-256 checksum; the server refuses to start when an applied migration was modified or is unknown to its version (a downgrade).

PostgreSQL is planned but not written. The store's interface hides the SQL, so a PostgreSQL store with translated migrations would be the way to several machines sharing accounts and ratings. A machine of this size does not need it: one SQLite writer commits a batch of 25 games in 4.9 ms, while a full VPS ends about 12 to 23 games per second (inferred: 71,000 games a day per 1,000 connected players, at the comfortable load with the 10 s ping).

### What is stored

Accounts (password hash, encrypted TOTP secret, HMAC'd recovery codes), sessions and single-use tokens (SHA-256 of the token only), one rating per official time control, finished games (moves, times and clocks as BLOBs), sanctions, conduct, anomalies, security events, the analysis queue and reports. Deleted accounts are anonymized, not removed.

Games in progress live in memory and in a per-shard append-only journal, flushed every `JOURNAL_FLUSH_MS` (50 ms) with `fdatasync`. Finished games are committed in batches of up to 500 by a writer thread every `DB_COMMIT_MS` (50 ms), with the rating updates in the same transaction. Before a batch is committed, the shard flushes the journal if records are waiting, so in normal operation the database never holds a finished game whose end the journal has not recorded; with `JOURNAL_FSYNC=true` that is one more `fdatasync` per commit batch. A batch starts `DB_COMMIT_MS` after the first game of an empty queue ended, or at once when games ended during the previous commit, so there is at most one batch per finished game. A crash loses at most the last 50 ms of moves.

When the journal cannot be written (a full disk, a failing volume), that wait is bounded: after 3 failed journal flushes in a row, about 0.3 s after the first one with the default `DB_COMMIT_MS` (inferred: 100 ms then 200 ms of backoff), or at the first one during a shutdown, finished games are committed without it, rating changes included, and `scacelith_game_commit_unjournaled_total` counts them. Alert on any increase of it and of `scacelith_journal_errors_total`. One risk remains in that state: after a crash or a restart, a finished game whose end the journal lost comes back as a game in progress, and the database keeps its first result. Fix the disk before restarting.

Journal compaction keeps the journal small however long the games last. Segments are 16 MB. Once a worker's journal has moved `JOURNAL_COMPACT_SEGMENTS` (4) segments past the oldest record a game still needs, that game is written again as one snapshot record and the older segments are deleted. A worker's journal then stays at about (4 + 1) × 16 MB = 80 MB; the README advises planning 100 MB per worker. Disabling custom time controls to protect the disk is no longer needed.

Passwords use scrypt (N = 2^17, r = 8, p = 1, 128 MiB) on Node 22, and Argon2id (m = 64 MiB, t = 3, p = 4) when Node provides `crypto.argon2` (24.7 or later); old hashes are upgraded at a later login.

### Growth

| Item | Size, indexes included |
|---|---|
| Finished game of 80 plies | 1,193 B |
| Engine analysis of a game | +2,033 B |
| Typical account (sessions, events, ratings) | about 1.9 KB |
| Login (session and security event) | about 400 B, removed by the retention purge |

Mix M1 plays about 71,000 games a day per 1,000 average connected players: **about 85 MB per day**. Accounts are small (100,000 accounts are about 0.19 GB). One analysis engine at depth 18 keeps up with only 1,000-4,000 games a day on these vCores (inferred), so analysis adds little space.

**Retention.** The primary runs the retention purge every `RETENTION_INTERVAL_MS` (one hour; the first run about a minute after the start). It deletes expired sessions and tokens (revoked sessions a day after the revocation), security events after `RETENTION_SECURITY_DAYS` (90), non-certain anomalies of the same age, conduct events and failed analysis jobs after 30 days, and erases stored IP addresses after `RETENTION_IP_DAYS` (30). Finished games, ratings, analysed games, sanctions and reports are kept. The purge works in short transactions: each statement starts at 200 rows and adapts, between 50 and 1,000 rows, so that it takes about 5 ms, and the run pauses 10 ms after every 10 ms of work, so the workers keep getting the write lock. Measured on the development container, a backlog of 100,000 expired sessions and 100,000 old security events, all with IP addresses, took 11-12 s with the primary's event loop busy 59 % of the time (delay p99 about 22 ms), while a thread committing a row every 20 ms waited about 20 ms at the 99th percentile. An hourly run only has the rows that expired in the last hour. With `secure_delete` on, deleted and erased data is overwritten with zeros in the file; no cost was measurable on the purge or on game commits. The file does not shrink: SQLite reuses the freed pages.

**Analysis queue.** The queue is bounded. At most `ANALYSIS_QUEUE_MAX` (5,000, at most 100,000) ordinary games wait; while the queue is at that cap, newly finished ordinary games are not queued at all, and `ANALYSIS_SAMPLE_RATE` (1) draws the share of ordinary games that are queued. Games with a report, a suspicion signal (at most 20 waiting per player; past that, a game with a suspicious (not `info`) anomaly of its own takes the place of the player's oldest waiting game that has none, counted as `reason="displaced"` in `scacelith_anticheat_analysis_skipped_total`) or a moderator request are queued anyway and mostly analysed first, but one engine claim in four takes the oldest ordinary game. With one engine and a busy server the ordinary queue stays at its cap, and while prioritized games keep coming an ordinary game can wait up to 5 to 20 days (inferred: 4 × 5,000 ÷ 1,000-4,000 games a day); lower `ANALYSIS_QUEUE_MAX` if analysed games should be more recent.

Disk life = space for the database ÷ daily growth. Space for the database is the disk minus 20 % kept free, the OS (3 GB), swap, logs (1 GB) and a journal reserve (0.5 GB on 2 workers, 0.75 GB on 4), which is more than the compacted journal needs (about 0.16 and 0.32 GB, inferred: 80 MB per worker). If backups are first written to the same disk, halve it.

| Average connected players | Per day | 40 GB disk (26.5 GB for the database; 13 GB with local backup copies) | 75 GB disk (53 GB; 26 GB) |
|---|---|---|---|
| 1,000 | 85 MB | 10 months (5) | 20 months (10) |
| 3,000 | 255 MB | 3.5 months (1.7) | 7 months (3.4) |
| 5,600 | 476 MB | 2 months (1) | 3.7 months (1.8) |
| 11,400 | 970 MB | beyond a 2-vCore VPS | 1.8 months (0.9) |

Average load is taken as 40 % of the peak (inferred), so 5,600 average players is a 2-vCore VPS at its comfortable peak every day with a 10 s ping, and 11,400 a 4-vCore VPS.

### Disk I/O

An NVMe disk (about 20,000 writes of 4 KB per second) is not a limit: at most about 30 journal `fdatasync` per second per worker (inferred: 20 from the 50 ms flushes, plus one before each commit batch, which needs at least one finished game: about 6 per second per worker at the comfortable load with the 10 s ping, about 12 at the memory limit), plus 5 to 20 database commits per second.

### Backups

- Do not use the `sqlite3` shell's `.backup` on a busy server. It copies 100 pages at a time and restarts whenever another connection writes; with a commit every 5 ms it did not finish after 20 s on a 35 MB database.
- Use the admin command, which writes a consistent snapshot with `VACUUM INTO` in one pass (0.18 s for 35 MB under load; a few minutes for tens of GB, inferred), creates the file with mode 600, and with `--verify` runs `PRAGMA quick_check` on the copy:

```sh
cd /opt/scacelith/dedicated-server
sudo -u scacelith mkdir -p /var/lib/scacelith/backup
sudo -u scacelith env SCACELITH_ENV_FILE=/etc/scacelith/scacelith.env \
  node bin/admin.js backup /var/lib/scacelith/backup/scacelith-$(date +%F).db --verify
```

- The command needs the server's configuration: here the environment file of the README's systemd unit, which the service user must be able to read (leave `SCACELITH_ENV_FILE` out when the configuration is a `.env` next to `package.json`). It refuses to run when `DB_PATH` is not an existing Scacelith database, so a scheduled backup started from the wrong place fails instead of copying an empty file. The target file must not exist. Run it as the service user, so that no root-owned `-wal` or `-shm` file is left behind, and at quiet hours: the WAL grows past its 64 MB limit while the snapshot is open.
- Encrypt the copy, move it off the machine, then delete the local copy. It contains e-mail addresses and the IP addresses of the last 30 days.
- Back up `SERVER_SECRET` and `MFA_ENCRYPTION_KEY` separately, for example in a password manager, never next to the database copy: together they decrypt the players' TOTP secrets. Without them a restored database has broken recovery codes and authenticator enrolments.
- A provider's daily disk image is a complement: it is taken without telling SQLite, so it is only crash-consistent (inferred).

## Logins, connections and restarts

### Password logins

A password login, a registration, a password reset and an account change that asks for the password each cost one hash, and a password change two (the check of the current password and the new hash); a token reconnection costs only a SHA-256 and a database read. Sessions last 30 days idle (`SESSION_IDLE_DAYS`), so password logins are rare in normal operation. At `s` = 0.65 a hash takes about 0.8 s of CPU and 128 MiB with scrypt, about 0.55 s and 64 MiB with Argon2id (inferred).

| `s` = 0.65, scrypt | 2 vCores | 4 vCores |
|---|---|---|
| Continuously, from the spare CPU | about 10 per minute (16 with Argon2id) | about 20 per minute (32) |
| In bursts, games staying under the p99 limit | about 20-25 per minute | about 45-50 per minute |
| CPU saturated, games degraded | about 60 per minute | about 120 per minute |

Each worker runs at most `PASSWORD_HASH_CONCURRENCY` (1) hash at a time; up to `PASSWORD_HASH_QUEUE_MAX` (32) more wait, and all the hashes of one request wait at most `PASSWORD_HASH_QUEUE_TIMEOUT_MS` (10 s, 13 s at most) together. A request that finds the queue full, or whose wait runs out, gets HTTP 503 `server_busy` with a `Retry-After` of 5 to 15 s, and nothing changes on the server. At 0.8 s per hash a worker gets through about 12 waiting hashes within the timeout (inferred: 10 s ÷ 0.8 s). A login wave therefore slows and refuses logins, not games: each worker keeps its event loop beside at most one hash thread.

Once at least half of a worker's queue waits (16 of 32 by default), one client source, an IPv4 address or an IPv6 /48, may have at most `PASSWORD_HASH_WAITERS_PER_SOURCE` (2) hashes waiting in that worker; its next request gets 429 `rate_limited` with the same kind of `Retry-After`, and gets back the `AUTH_RATE_PER_IP` attempt it had used (and its /48 one). Below half, one source may queue more, so a class behind one address logs in on a quiet worker, and, as long as `PASSWORD_HASH_WAITERS_PER_SOURCE` is at most half the queue (16), one source never holds more than half of it. The password endpoints are also limited to `AUTH_RATE_PER_IP` (20) attempts per 10 minutes per IPv4 address or IPv6 /64, and to `AUTH_RATE_PER_PREFIX` (0, which means 5 × `AUTH_RATE_PER_IP`, so 100) per IPv6 /48. A failed login is held, after its hash slot is freed, until it took as long as the slowest password check of the last 10 to 20 minutes, and never less than the slowest kind of check the worker timed at start, a floor that does not decay (2 s at most in all), so its duration does not tell whether the account exists; that wait uses no hash capacity. That floor is one scrypt check on Node 22 and on Node 24.7 or later alike (older accounts keep their scrypt hash), about 0.5 s on the test vCPU and 0.8 s at `s` = 0.65 (inferred: 0.50-0.55 s ÷ 0.65), so every failed login takes at least that long. Timing it costs each worker one hash slot at start: one scrypt hash on Node 22, about 0.8 s of one vCore at `s` = 0.65, and one Argon2id hash plus one scrypt check on Node 24.7 or later, about 1.35 s (inferred: 0.55 s + 0.8 s).

Leave `UV_THREADPOOL_SIZE` at its default of 4: journal writes and DNS lookups for SMTP and Google share that pool and need free threads. The server logs a warning at start, and `check-config` prints it on stderr, when `PASSWORD_HASH_CONCURRENCY` is not below the pool size. If you ever set the variable, put it in the process environment (the systemd unit or its `EnvironmentFile`): libuv does not read the server's `.env`. An empty or non-numeric value (a bare `UV_THREADPOOL_SIZE=` line) or 0 is read by libuv as a pool of 1 thread, and the warning says so.

The login proof of work turns on at `POW_LOGIN_TRIGGER_PER_MIN` (30) failed logins per minute across the server. That is about 0.4 vCore of scrypt at `s` = 0.65 (inferred: 30 × 0.8 s ÷ 60 s), a fifth of a 2-vCore VPS, so it can trigger on both machines; about 10 per vCore (20 on 2 vCores) triggers it sooner where that fifth matters. It slows attackers down (18 bits cost a client 150-180 ms) but a GPU pays almost nothing.

Moving from Node 22 to Node 24.7 or later makes each player's first password login do one extra hash to upgrade the stored hash (inferred). The login only does it when a hash slot is free at once; otherwise a later login does it.

### New connections: the TLS gate

With `TLS_MODE=native`, each worker screens new TCP connections before any TLS work:

1. A new connection has 3 s to send the first record of its TLS ClientHello and holds no handshake slot meanwhile; a silent or malformed one is closed. At most 16 × `MAX_PENDING_HANDSHAKES` (2,048) connections per worker, and 4 × `MAX_PENDING_HANDSHAKES_PER_IP` (16) per address group, may wait at once.
2. The connection then needs one of the worker's `MAX_PENDING_HANDSHAKES` (128) handshake slots and one of the `MAX_PENDING_HANDSHAKES_PER_IP` slots of its address group, an IPv4 address or an IPv6 /48. That key is empty by default, which means 4 (`MAX_PENDING_HANDSHAKES` ÷ 32, at least 2; `check-config` prints the value in use). A connection beyond either cap is closed with a reset, and the game retries after its backoff. A handshake that fails or passes its 10 s timeout gives its slot back and is closed.

At about 3.7 ms of CPU per new connection on these vCores (TLS handshake, upgrade and Hello, the handshake being about half of it; inferred: 2.4 ms ÷ 0.65), 128 handshakes in flight are about half a second of work, so the handshakes a worker starts finish long before the game's 10 s deadline, and a reconnection storm is served in turn. `scacelith_tls_refused_total{reason}` counts the closed connections.

**Players who share one address.** A school, a company network or a mobile operator's carrier-grade NAT puts many players behind one IPv4 address. In normal play the per-group cap does not matter: a handshake holds its slot for one or two network round trips, so 4 slots per worker serve dozens of handshakes per second from one address (inferred). After a restart, when they all reconnect at once, players behind one address are served 4 at a time per worker and come back later than the others. Other limits matter more for such a group: `MAX_CONNECTIONS_PER_IP` (16 connections per IPv4 address), `AUTH_RATE_PER_IP` (20 password logins, registrations or resets per 10 minutes), and, while a worker's hash queue is at least half full, the `PASSWORD_HASH_WAITERS_PER_SOURCE` (2) password hashes one address may have waiting in it (the next concurrent login gets 429, and the game shows the player how long to wait before trying again). For a club or a school that plays over one address, raise `MAX_CONNECTIONS_PER_IP` and `AUTH_RATE_PER_IP` above the size of the group, `MAX_PENDING_HANDSHAKES_PER_IP` to about 16 (it must stay below `MAX_PENDING_HANDSHAKES`), and `PASSWORD_HASH_WAITERS_PER_SOURCE` if its players log in together while the server is busy.

**Load tests from one machine.** Every client of a load machine shares its address, and the load generator does not retry a refused connection. On the test instance, set `MAX_PENDING_HANDSHAKES_PER_IP=127` (one below `MAX_PENDING_HANDSHAKES`) and `MAX_CONNECTIONS_PER_IP` above the clients per load address, as the tool does for a server it starts itself. See [Validating on the real machine](#validating-on-the-real-machine).

### When the server is full

`MAX_CONNECTIONS` counts the signed-in players of the whole server. A server at `MAX_CONNECTIONS` does not shed load before TLS: a newcomer beyond it still completes the TLS handshake and the WebSocket upgrade, is refused at Hello with `ServerFull`, and the game then waits 60 to 120 s before trying again, so 1,000 newcomers waiting for a place cost about 0.07 vCore (inferred: 1,000 ÷ 90 s × 6 ms for an attempt, which reads `/info` and opens the WebSocket over up to two TLS connections: 4 ms ÷ 0.65, an upper bound). `scacelith_ws_hello_total{result="server_full"}` counts these refusals: it is the metric that shows a full server. A player whose game is in progress is still admitted at Hello, whatever the count, so a full server does not make a game end by abandonment, and since a server at `MAX_CONNECTIONS` does not shed, that player does not compete there with the newcomers for a shedding rate. So that such a player can reach Hello, WebSocket upgrades may go max(16, 2 %) beyond `MAX_CONNECTIONS` (200 above 10,000, 400 above 20,000); beyond that reserve the upgrade gets HTTP 503 (`scacelith_ws_handshakes_rejected_total{reason="server_full"}`). Only after such a refusal (for up to 5 s: an admitted upgrade ends it sooner), or while it holds 1.2 times its share of `MAX_CONNECTIONS`, does a worker shed: its TLS gate lets only `MAX_PENDING_HANDSHAKES` / 2 (64) new TLS connections per second through and closes the others before any TLS work (`scacelith_tls_refused_total{reason="server_full"}`). That bounds the TLS work spent while shedding at about a quarter of each vCore (inferred: 64 per second × 3.7 ms), and the connections closed before TLS cost almost nothing. While a worker sheds on the default shared port, the API is let through at the same rate; setting `WS_PORT` to another port keeps the API outside that limit.

`MAX_CONNECTIONS` protects memory and file descriptors, not CPU: set it from the memory figure, and let the CPU figures tell you when to move to a larger machine.

### Restarts

After a restart every client comes back at nearly the same moment. The server applies any new migration, replays the journal and restores the games that were running, then the TLS gate serves the reconnections in turn. The kernel queue in front of it holds `LISTEN_BACKLOG` (2,048) connections per listening socket (one per worker with `LISTEN_REUSE_PORT`), capped by `net.core.somaxconn`. After a crash as after a graceful restart, the client reuses the `GET /api/v1/info` answer its last connection reached `Welcome` with (for 10 minutes after losing it), so each reconnection costs one TLS handshake, the upgrade and Hello, about 2.4 ms of CPU; the server id in the answer to the WebSocket upgrade still tells the client, before it sends its session, that a restart brought another server (a reinstall). The game spreads its attempts:

- after a crash, the first attempt comes within 0.5-2 s;
- after a graceful restart (the server announces it `SHUTDOWN_GRACE_MS`, 3 s, before closing), players with a game in progress try within 1-8 s and the others within 5-35 s;
- later attempts use a random delay, at most 8 s apart for players with a game in progress and up to 30 s apart for the others.

Time until every client is back (inferred):

| Load | Clients on 2 vCores / 4 vCores | After a crash or a graceful restart |
|---|---|---|
| Comfortable, 2 s ping | 7,500 / 15,300 | about 35-40 s |
| Above comfortable, 2 s ping | 10,000 / 20,000 | about 50-57 s |
| Comfortable, 10 s ping (default) | 14,000 / 28,400 | about 60 s |

These times come from a simulation of the client's backoff after a crash, reviewed during the capacity study (57-60 s at the comfortable load and 81-84 s for 10,000 clients on 2 vCores at `s` = 0.70, with 4 ms per reconnection), extrapolated to `s` = 0.65 and brought down to 2.3-2.5 ms per reconnection. A graceful restart costs the same one TLS connection per client, so the same times apply (inferred): only its first attempts are spread differently, which puts the players with a game in progress ahead of the others without changing the total work. Players with a game in progress retry more often than the others, so most games are back before the last client; the table is the cautious case. For comparison, the older server gave restored bullet and blitz games only 15-18 s: about 37 % of the clients came back within it, so only about 14 % of the games in progress kept both players (0.37 × 0.37).

Two settings decide whether the games survive:

- `RECOVERY_GRACE_MS` (90 s): both players of a restored game have this long, from the moment the server restores it, to come back (the normal grace when that is longer). The default covers every row of the table on both VPS, after a crash or a graceful restart (inferred: about 60 s at most, plus half again as a margin, is 90 s). If you raise `MAX_CONNECTIONS` above the comfortable load with the 10 s ping (toward 20000 / 40000), set 130000 (inferred: about 60 s × 20,000 ÷ 14,000 ≈ 86 s, plus half again).
- `RECOVERY_CLOCK_HOLD_MS` (20 s): the clock (or first-move timer) of the side to move stays stopped until that player is back, for this long at most; after it, the clock runs and a late player loses the difference. At the comfortable load with a 10 s ping, reconnecting only the players with a game in progress takes at least about 16 s on either VPS (inferred: 0.6 × 14,000 clients × 2.4 ms ÷ (2 vCores × 0.65 × 0.95)), plus up to 2 s (crash) or 8 s (graceful restart) before the first attempt. They share the handshake slots with the others, who retry at the same time after a crash (all within 0.5-2 s) and start from 5 s after a graceful restart, so in both cases the last of them may need up to about 30 s (inferred: after a crash, 14,000 clients × 2.4 ms ÷ (2 vCores × 0.65 × 0.95) ≈ 27 s, plus up to 2 s; after a graceful restart, the 16 s of the players in a game plus the others' share from 5 s on, whose 11 s of work is spread over 30 s, ≈ 24 s, plus up to 8 s before a refused player's next attempt). The default is therefore about enough up to 10,000 clients on VPS-1 and 20,000 on VPS-2, the initial `MAX_CONNECTIONS` below (inferred: 10,000 × 2.4 ms ÷ (2 × 0.65 × 0.95) ≈ 19 s), as the model of the wave in [BENCHMARK.md](BENCHMARK.md) finds for 10,000 players on 2 cores. At the comfortable load with the 10 s ping it is not: the last players to move would lose up to about 10 s of clock (inferred: 30 s minus the 20 s hold). Set 45000 on both VPS if a crash or a graceful restart may happen near the comfortable load (inferred: 30 s plus half again as a margin, as for the grace). With `MAX_CONNECTIONS` raised toward 20000 / 40000, 45000 still covers them, without the margin (inferred: 20,000 × 2.4 ms ÷ (2 × 0.65 × 0.95) ≈ 39 s, plus up to 2 s). A value you set must stay below `RECOVERY_GRACE_MS` (left empty, the hold is 20000, or `RECOVERY_GRACE_MS` minus 1 when the grace is 20 s or less), and it is also the free thinking time a player could take by staying away on purpose, so do not raise it further. In a game restored before its second ply, the side to move also gets its whole first-move time (`FIRST_MOVE_TIMEOUT_MS`, 30 s) from its first reconnection, even after the hold: its first move may then come until about 80 s after the restore with the defaults (inferred: back just before 20 s + 30 s, then 30 s more), or about 105 s with a hold of 45 s (inferred: 45 s + 30 s + 30 s).

Restart at quiet hours, keep `SHUTDOWN_GRACE_MS` so clients receive the notice, and enable `LISTEN_REUSE_PORT`. Keep `RECONNECT_GRACE_MIN_MS` at 15000 for ordinary disconnections: raising it just before a restart is not needed, and the configuration is only read at start.

## Recommended settings

### Server configuration

| Setting | 2 vCores, 4 GB | 4 vCores, 8 GB | Why |
|---|---|---|---|
| `CLIENT_PING_INTERVAL_MS` | 10000 (default) | 10000 | 2000 gives a livelier ping display but costs a factor of 1.87 in capacity |
| `GESTURE_RATE` | 4 (default) while the peak stays under about 2,800 connected players, then 2 (up to about 4,700), then 1 (up to about 7,000), then 0 (the relay off, up to about 14,000) | 4 up to about 5,700, then 2 (up to about 9,500), then 1 (up to about 14,000), then 0 (up to about 28,400) | the opponent's live gestures cost CPU for every player in a game, at least one gesture per second each while the relay is on; these limits hold even if every player in a game moved all the time (inferred, [Gestures](#gestures)); once `scacelith_gestures_relayed_total` shows the real rate r, use the row of r instead |
| `WORKERS` | auto | auto | one shard per vCore |
| `LISTEN_REUSE_PORT` | true | true | each worker accepts its own connections; halves the primary's cost per connection |
| `SHARD_OVERLOAD_LAG_MS` | 500 | 500 | the default 250 moves games for lag spikes already seen at 63-77 % CPU |
| `DB_CACHE_MB` | 16 | 32 | the default 64 is per connection, 5 or 9 connections |
| `DB_MMAP_MB` | 256 (default) | 512 | shared, reclaimable OS page cache |
| `MAX_CONNECTIONS` | 10000 at first | 20000 at first | a memory guard (default 200000); raise it toward 20000 / 40000 once `s` is measured, never above the memory figure |
| `MAX_CONNECTIONS_PER_IP`, `MAX_PENDING_HANDSHAKES_PER_IP`, `PASSWORD_HASH_WAITERS_PER_SOURCE` | 16, empty (4), 2 (defaults) | same | raise them when many players share one address (a school, a company); the first two on a load-test instance too |
| `PASSWORD_HASH_CONCURRENCY` | 1 (default) | 1 | the memory budget counts one 128 MiB hash per worker, and each hash takes a vCore for 0.8 s |
| `POW_LOGIN_TRIGGER_PER_MIN` | 20 | 30 (default) | the default is about a fifth of a 2-vCore VPS in scrypt (inferred) |
| `RECOVERY_GRACE_MS`, `RECOVERY_CLOCK_HOLD_MS` | 90000 (default), 45000 | same | after a crash or a graceful restart near the comfortable load, the default grace brings every player back, and 45000 keeps the clock of the side to move stopped until the last of them is back (inferred); the default hold of 20000 covers about 10,000 / 20,000 clients; raise the grace to 130000 if `MAX_CONNECTIONS` goes above the comfortable load |
| `HEARTBEAT_INTERVAL_MS`, `HEARTBEAT_TIMEOUT_MS` | 10000, 30000 (default) | same | the heartbeat round trip feeds lag compensation, and the game drops a connection after two silent intervals |
| `JOURNAL_FLUSH_MS`, `JOURNAL_FSYNC`, `DB_COMMIT_MS` | 50, true, 50 (default) | same | NVMe has plenty of room |
| `ANALYSIS_ENGINE_PATH` | empty | optional, `ANALYSIS_WORKERS=1` | one engine takes a whole vCore |
| `TLS_MODE`, `TLS_MIN_VERSION` | native, TLSv1.2 | same | the TLS gate needs native TLS, and a proxy on the same machine only moves the cost; Windows 10 WinHTTP lacks TLS 1.3 (inferred) |
| `TLS_CERT_FILE`, `TLS_KEY_FILE` | full chain, ECDSA P-256 key | same | the key type all measurements used; RSA adds about 1 ms per handshake (inferred) |
| `DATA_DIR` | /var/lib/scacelith | same | as in the README's systemd unit and the backup command above |
| `SERVER_PUBLIC_HOST` | the public DNS name (A record only) | same | the default `localhost` breaks e-mail links and the Google redirect URI |
| `MAIL_TRANSPORT` | smtp | smtp | the default `log` sends nothing, and `REQUIRE_EMAIL_VERIFICATION=true` then blocks new accounts |
| `SMTP_HOST`, `SMTP_USER`, `SMTP_PASSWORD_FILE` | your relay | same | |
| `SMTP_PORT`, `SMTP_SECURITY` | 587 and starttls, or 465 and tls | same | many providers, OVH included, block outbound port 25 |
| `MAIL_FROM` | an address on a domain with SPF and DKIM at that relay | same | the default `no-reply@localhost` is rejected or marked as spam |
| `SSO_GOOGLE_ENABLED`, `GOOGLE_CLIENT_ID`, `GOOGLE_CLIENT_SECRET_FILE` | if Google sign-in is used | same | register the `googleRedirectUri` that `check-config` prints |
| `SERVER_SECRET_FILE`, `MFA_ENCRYPTION_KEY_FILE` | required, recommended | same | back them up separately from the database |

### System

- Node 24 LTS, 24.7 or later: Argon2id costs about 40 % less CPU and half the memory of scrypt per password hash. A certificate from certbot 2 or later has an ECDSA P-256 key by default.
- systemd unit as in the [README](../README.md#running-as-a-service-systemd-example), with `LimitNOFILE` above `MAX_CONNECTIONS` and `RestartSec=2`. With the default 100 ms, repeated start failures hit systemd's limit of 5 starts in 10 s and the service stays down.
- sysctl, for example in `/etc/sysctl.d/90-scacelith.conf`:

```ini
# The API port lies inside the ephemeral range (32768-60999). Without a reservation an outgoing
# connection (apt, certbot) can hold it during a restart and the server's bind() fails (EADDRINUSE).
net.ipv4.ip_local_reserved_ports = 44664
# The kernel caps LISTEN_BACKLOG (2048 by default) at somaxconn: 4096 is the default since
# Linux 5.4 and 128 before. Raise it too if you raise LISTEN_BACKLOG.
net.core.somaxconn = 4096
net.ipv4.tcp_max_syn_backlog = 8192
net.ipv4.tcp_syncookies = 1
net.core.netdev_max_backlog = 8192
vm.swappiness = 10
```

- A swap file of 1 GB (4 GB RAM) or 2 GB (8 GB RAM) as a safety net for hash and garbage-collection peaks, and journald `SystemMaxUse=1G`.
- If you filter IPv6 with nftables on the machine, use an `ip6` table only, not `inet`. A `ct state` rule in an `inet` table turns on connection tracking for all game traffic, and on a 4 GB machine the conntrack table (65,536 entries) can fill during a reconnection wave, which drops packets. Otherwise add `notrack` for the API port or raise `net.netfilter.nf_conntrack_max` to 262144.
- Time: TOTP and Google ID tokens need a correct clock. Prefer the provider's NTP server (OVH: `ntp.ovh.net` in `timesyncd.conf` or chrony), and the provider's DNS resolver (OVH: 213.186.33.99); traffic from inside the provider's network needs no firewall rule. Check with `timedatectl timesync-status`, `chronyc sources` and `resolvectl status`.
- SSH: keys only, and IPv4 only (`ListenAddress 0.0.0.0`) if the provider firewall does not filter IPv6. On Ubuntu 22.10 and later sshd is socket-activated: run `systemctl daemon-reload` and `systemctl restart ssh.socket`, then check with `ss -tlnp 'sport = :22'`.

## Provider firewall: the OVH Edge Network Firewall

The OVH Edge Network Firewall is stateless, filters IPv4 only, applies the first matching rule, and allows 20 rules per IP (priorities 0-19). Accept rules alone do nothing: the final Deny rule is required. Being stateless, it must explicitly accept the replies to connections the server opens itself.

| Priority | Action | Protocol | Source | Source port | Destination port | TCP option | Purpose |
|---|---|---|---|---|---|---|---|
| 0 | Accept | TCP | any | | 32768-60999 | established | replies to outgoing connections: SMTP 587/465, Google 443, apt, ACME, DNS over TCP |
| 1 | Accept | TCP | any | | 44664 | | API and WSS |
| 2 | Accept | TCP | admin IPv4/32 | | 22 | | SSH |
| 3 | Accept | TCP | second admin /32 (optional) | | 22 | | SSH, or the machine that pulls backups |
| 4 | Accept | ICMP | any | | | | ping, and path-MTU "fragmentation needed" messages that TLS relies on |
| 5 | Accept | TCP | any | | 80 | | only for certbot HTTP-01; not needed with DNS-01 |
| 6 | Accept | UDP | resolver /32 | 53 | | | only if not using the OVH resolver |
| 7 | Accept | UDP | 1-2 NTP servers /32 | 123 | | | only if not using `ntp.ovh.net`; never without a pinned source (NTP amplification) |
| 19 | Deny | IPv4 | any | | | | everything else, including the metrics port |

What it does not cover:

- IPv6 is not filtered, and recent images configure it by default. The game server binds IPv4 only (`BIND_ADDRESS=0.0.0.0`), but sshd listens on `::` unless changed. Publish no AAAA record.
- Traffic from inside the provider's network is not filtered, other customers included: use key-only SSH.
- The metrics port (9464) is bound to 127.0.0.1 by default; read it through an SSH tunnel.
- There is no per-IP rate limiting; the server does its own (`MAX_CONNECTIONS_PER_IP`, `MAX_PENDING_HANDSHAKES_PER_IP`, `HTTP_RATE_PER_IP`, `AUTH_RATE_PER_IP`).

To avoid locking yourself out: take your IPv4 address from `echo $SSH_CLIENT` inside an `ssh -4` session, keep that session open, test a new `ssh -4` login, and only then add the Deny rule. The rules can also be edited from the provider's control panel, and the KVM console and rescue mode remain available, so give the local console a password even with key-only SSH.

## Validating on the real machine

1. **CPU steal**: run `vmstat 1 300` when quiet and again at the evening peak, and read the `st` column. Above 5 % sustained, take it off `s`; above 10 %, the figures on this page are optimistic.
2. **Speed of one vCore**: run this 5 times and keep the median.

```sh
for i in 1 2 3 4 5; do node -e "const c=require('crypto');const t=process.cpuUsage();c.scryptSync('x','salt',64,{N:2**17,r:8,p:1,maxmem:256*2**20});const u=process.cpuUsage(t);console.log((u.user+u.system)/1e6)"; done
```

   The quiet reference on the test vCPU is about 0.50 s (0.49-0.53), so `s` ≈ 0.50 ÷ your median; expect about 0.77 s at `s` = 0.65. scrypt is memory-bound, so this is an approximation.
3. **Real vCores**: run as many copies in parallel as there are vCores, `time (for i in $(seq $(nproc)); do node -e "<same code>" & done; wait)`. The wall time should stay close to one run; if it doubles, the vCores are shared or are hyperthreads.
4. **ADX**: `grep -cw adx /proc/cpuinfo` prints 0 with the QEMU Haswell model, which confirms the slower TLS path.
5. **Load test from another machine** (on the same machine the load generator takes 25-50 % of the CPU), against a separate test instance with its own `DATA_DIR`:
   - Accounts: `node bin/admin.js bench-accounts --count 20000 --out tokens.tsv --format tsv --i-know-this-is-a-test-server`. That flag is the only safeguard: the command creates verified accounts with live sessions in whatever database the configuration points to.
   - Server: `MAX_CONNECTIONS_PER_IP` above the clients per load address (one source address gives about 28,000 ports), `MAX_PENDING_HANDSHAKES_PER_IP=127`, and `/metrics` through `ssh -L 9464:127.0.0.1:9464`.
   - Add `--url wss://HOST:44664/ws --tokens tokens.tsv --metrics http://127.0.0.1:9464/metrics` to each command (and `--ca` for a self-signed certificate), and set `--ping-interval-ms` to the server's `CLIENT_PING_INTERVAL_MS`. The tool counts a connection the TLS gate refuses as failed, so keep `--inflight` times the load processes (2 up to 30,000 clients) below `MAX_PENDING_HANDSHAKES` × `WORKERS`, for example `--inflight 100` on 2 workers:
     - idle connections: `node bench/loadgen.js --scenario connect --conns 10000 --inflight 100 --ping-interval-ms 10000 --hold-s 60`;
     - games with realistic churn: `--scenario games --games N --inflight 100 --move-interval-ms 5000 --ping-interval-ms 10000 --start-rate 25 --warmup-s 400 --duration-s 300`, raising N until the p99 passes 100-150 ms;
     - reconnection wave: `--scenario connect --conns N --inflight 5000 --connect-timeout-ms 10000`, ideally from several machines. Here refusals are expected: `scacelith_tls_refused_total{reason="handshakes"}` counts the attempts the game would repeat.
   - The load machine needs about 0.7 core per 20,000 idle clients at a 2 s ping, 0.95 core per 20,000 clients in games and 1.8 cores for a ramp of 500 connections per second.
   - Afterwards, restore `MAX_CONNECTIONS_PER_IP=16`, an empty `MAX_PENDING_HANDSHAKES_PER_IP` and your `MAX_CONNECTIONS` cap.
6. **In production**, watch `/metrics`, the size of `DATA_DIR` and `vmstat`:

| Metric | What to look for |
|---|---|
| `scacelith_process_cpu_ratio`, `scacelith_process_event_loop_delay_p99_ms` (per shard) | CPU above 55 % at the peak, a rising p99 |
| `scacelith_ws_connections`, `scacelith_process_rss_bytes` | connections and memory against the memory figure |
| `scacelith_tls_refused_total{reason}`, `scacelith_tls_hello_waiting`, `scacelith_tls_handshakes_pending` | `handshakes` or `per_ip` refusals outside restarts (not enough CPU for handshakes, or a shared address), `hello_timeout` and `bad_hello` from scanners |
| `scacelith_ws_hello_total{result="server_full"}`, `scacelith_ws_handshakes_rejected_total{reason="server_full"}` | newcomers refused at login by `MAX_CONNECTIONS`, the sign of a full server; upgrades refused (HTTP 503) once its reserve is in use too, after which the TLS gate sheds (`scacelith_tls_refused_total{reason="server_full"}`) |
| `scacelith_password_hash_rejected_total{reason}`, `scacelith_password_hash_queued` | `queue_full` or `timeout` outside an attack: not enough CPU for the logins; `source_limit`: clients held to `PASSWORD_HASH_WAITERS_PER_SOURCE` (2) waiting hashes while the queue was at least half full |
| `scacelith_retention_runs_total{result}`, `scacelith_retention_run_seconds` | a `failed` result, or runs growing longer |
| `scacelith_anticheat_analysis_queue_ordinary`, `scacelith_anticheat_analysis_queue_priority`, `scacelith_anticheat_analysis_skipped_total{reason}` | the ordinary queue held at `ANALYSIS_QUEUE_MAX`, a growing priority queue; `displaced` counts waiting games replaced by a game with a suspicious (not `info`) anomaly of its own |
| `scacelith_journal_disk_bytes` (per shard) | more than about (`JOURNAL_COMPACT_SEGMENTS` + 1) × 16 MB |
| `scacelith_journal_errors_total`, `scacelith_game_commit_unjournaled_total` | any increase: the journal cannot be written, and finished games are committed without it; fix the disk before a restart |
| `scacelith_gestures_relayed_total`, `scacelith_gestures_dropped_total{reason}` | the relayed rate divided by the players in a game is r of [Gestures](#gestures); `backlog` drops mean slow players or an overloaded worker, `rate` drops a client that exceeds `GESTURE_RATE` |
| `scacelith_game_stall_ms`, `scacelith_game_timer_late_ms` | stalls of a worker's event loop (the games do not charge them to the players, up to `GAME_STALL_CREDIT_MAX_MS`); frequent ones of 100 ms or more mean an overloaded machine, CPU steal or a slow disk |

## Risks, largest first

1. **Real vCore speed and steal.** `s` moves every figure by about ±10 %, more with heavy steal, and providers rarely publish either. Measure it.
2. **The latency knee.** It was measured over loopback (kernel network cost ±30 %) and with little game churn, so the 55 % comfortable point may be a few points optimistic.
3. **Disk space.** Nothing archives finished games: a 2-vCore VPS that is full every evening fills a 40 GB disk in about 2 months (inferred), in 1 month if backup copies are written locally.
4. **Restarts.** Near the comfortable load with the 10 s ping, a crash or a graceful restart takes about 60 s to bring every player back (inferred), inside the default recovery grace, but the players to move keep their whole clock only with the `RECOVERY_CLOCK_HOLD_MS` setting above. This has not been tested with a real multi-machine reconnection wave.
5. **Admission limits and journal compaction are tested, not measured under load.** The TLS gate, the password-hash cap, the upgrade reserve and compaction came after the load measurements, and only the retention purge has a measured cost. Run the load tests above again on the real machine.
6. **Player mix.** Between a bullet-heavy mix and a slow one, capacity changes by more than a factor of two, and between players who sit still and players who keep looking around (the gesture rate r, from about 1 to `GESTURE_RATE` while the relay is on) by more than a factor of two again. Production metrics will tell which one your players resemble.
7. **Small items.** One SQLite writer for about 12 to 23 game ends per second is ample, the primary stays under 0.25 core, and one analysis engine follows only a small share of the games at full load.
