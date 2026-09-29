# Benchmark

`bench/loadgen.js` measures what one Scacelith server holds: authenticated WebSocket connections,
games at a realistic pace, and the move throughput without think time. It uses Node's built-ins
only (no npm package), starts its own throw-away server or targets an existing test server, and
writes a JSON report of every run. This page explains how to run it, what each scenario measures,
the numbers measured on the development machine, what saturated first, and what they mean for a
dedicated machine.

## Quick start

```sh
cd dedicated-server
npm run bench -- --scenario connect --conns 10000             # same as node bench/loadgen.js ...
node bench/loadgen.js --scenario games --games 5000           # 5,000 games, one move per second each
node bench/loadgen.js --scenario burst --games 500            # no think time: throughput limit
node bench/loadgen.js --help                                  # every option
node bench/table.js bench/results/*.json                      # markdown tables of the reports
```

A run prints one progress line every 2 s (connections, games, moves/s, move round trip, server and
load generator CPU, event-loop lag, memory, CPU used by the other processes of the machine), then
a summary, and writes `bench/results/<scenario>-<time>.json` (ignored by git). A run is bounded
(`--max-run-s`, 900 s by default); at the end, on an error or on Ctrl-C it stops the server and the
load processes and deletes the temporary directory.

## What the tool does

**Its own server** (default). A temporary data directory on the normal disk (the journal fsyncs and
the SQLite commits are real), a self-signed ECDSA P-256 certificate made with the `openssl` command,
free ports on 127.0.0.1, `WORKERS` from `--workers` (auto: the number of cores, more when the
open-file limit requires it, see below), proof of work off, per-IP limits and `MAX_CONNECTIONS`
raised to 1,000,000, mail to the log, `LOG_LEVEL=warn`. The burst scenario also raises
`WS_MSG_RATE`/`WS_MSG_BURST` to 1000/2000 (the defaults, 20/40 messages per second per connection,
would throttle a player who moves without thinking). `--server-env K=V` sets anything else,
`--reuse-port` sets `LISTEN_REUSE_PORT=true`, `--plain` runs without TLS (`TLS_MODE=off`, to measure
what TLS costs). The accounts come from `bin/admin.js bench-accounts` (verified accounts with a live
session each); they are created in a copy of the database on tmpfs and moved into place, because
account creation is fsync-bound on a real disk (about 3.5 ms per account here; 100,000 accounts
take 7 s this way).

**An existing test server.** Never a production server: `bench-accounts` refuses to run without
its explicit flag.

```sh
# on the test server: accounts and their tokens (username<TAB>token per line)
node bin/admin.js bench-accounts --count 20000 --prefix bench --out tokens.tsv --format tsv --i-know-this-is-a-test-server
# the test server needs MAX_CONNECTIONS_PER_IP (16 by default) above the number of clients per
# load machine, and WS_MSG_RATE / WS_MSG_BURST raised for the burst scenario
# on the load machine
node bench/loadgen.js --scenario games --games 10000 --url wss://test.example.org:44664/ws \
    --ca cert.pem --tokens tokens.tsv --metrics http://test.example.org:9464/metrics --metrics-token TOKEN
```

`--metrics` lets the report include the server side (CPU, memory and event-loop lag per shard,
move processing time, store commits); without it only the client side is measured. To load one
server from several machines, give each machine its own slice of the tokens file (an even number
of lines: direct challenges pair consecutive accounts), since an account has one live connection.

**The load processes.** The clients are spread over `--procs` processes (auto: at least 2, one
more per 15,000 clients). Each one speaks RFC 6455 directly over `node:tls` with one shared
`SecureContext` and a precomputed upgrade request, keeps one fixed-shape object per client, encodes
and decodes the hot messages (Move, MoveMade, Ping, Pong) at fixed offsets and everything else
with the generated codec, keeps the think times on one timing wheel, and plays legal random moves
with the correct `posHash` (both players of a direct challenge live in the same process and share
one position). Each process uses its own source address (127.1.0.x) against a local server, so
that 100,000 connections do not run out of ephemeral ports (28,232 per source address here). The
coordinator samples `/metrics` and, for a local server on Linux, the CPU time of every server and
load process from `/proc`; the rest of the machine's CPU time is reported as "other processes".

**Handshakes in flight.** With native TLS every worker performs at most `MAX_PENDING_HANDSHAKES`
(128) handshakes at a time and closes the connections beyond that before any TLS work (counted in
`scacelith_tls_refused_total{reason="handshakes"}` on the server and as failed connections by the
tool, which does not retry). The runs below predate that limit. To measure the raw handshake rate
again, keep `--inflight` times the load processes below `MAX_PENDING_HANDSHAKES` times `WORKERS`,
or raise the key with `--server-env MAX_PENDING_HANDSHAKES=100000`; with the default, a ramp that
opens connections faster than the server completes them measures the gate instead.

**Open files.** The container used here has a hard `nofile` limit of 20,000 per process that
cannot be raised without `CAP_SYS_RESOURCE`, so the tool uses at least `clients / 15000` server
workers and load processes (7 of each for 100,000 connections on 4 cores). On a real server raise
`LimitNOFILE` (systemd) or `ulimit -n` instead; `WORKERS` should then equal the number of cores.

## Scenarios and what they measure

| scenario | load | measures |
|---|---|---|
| `connect` | ramps `--conns` authenticated WSS connections (TCP, TLS 1.3 full handshake, HTTP upgrade, Hello with the session token, Welcome) with `--inflight` handshakes in flight per process, then holds them idle for `--hold-s` with the server heartbeat (every 10 s) and one client Ping per connection every `--ping-interval-ms` | connections/s, failures and their reason, handshake (TCP+TLS+101) and Hello->Welcome latency, server CPU per connection, server memory per connection (RSS, V8 heap and external deltas of every server process, from `/metrics`, before and after, no forced GC), idle CPU, heartbeat round trip (client Ping->Pong, and the server's own RTT histogram), connections dropped during the hold |
| `games` | `--games` pairs start games by direct challenge (ChallengeCreate -> ChallengeAccept) or with `--via queue` through the matchmaking queue, at `--start-rate` games/s, then play legal random moves every `--move-interval-ms` (1000 ms +-50 % by default) per move; a game resigns at `--max-plies` (80) and the pair starts a new one after `--between-games-ms` | move round trip (Move sent -> the mover's own MoveMade) p50/p90/p99/p99.9/max, moves/s, games started and finished, end reasons, rejected moves, errors, game start latency (challenge -> both snapshots), server move processing time (inside the host), CPU and event-loop lag per shard, store commit latency and batch size, relayed frames/s, memory with games |
| `burst` | like `games` with no think time (each player answers the opponent's move at once) | the throughput limit of the server on the machine, and the latency at that limit |

Every report holds the machine (CPU model, cores, memory, Node version, `nofile`, `somaxconn`,
ephemeral port range), the command line, every server setting the tool changed, the per-phase
counters, histograms and CPU, a 2 s timeline, and the list of limits the tool detected (machine
CPU saturated, event-loop lag above 50 ms, other processes busy, memory low).
`--server-cpu-prof DIR` also writes a V8 CPU profile of every server process;
`node bench/profile-summary.js DIR/*.cpuprofile` prints the hot functions by area and the longest
event-loop stalls with their stacks.

## Test machine and conditions

- A 4 vCPU virtual machine (Intel Xeon; the host CPU changed with a container restart: 2.10 GHz for
  the connect 10k/50k/100k runs of 2026-09-28, 2.80 GHz for all the others), 15.7 GB of memory, no
  swap, Linux 6.18, Node 22.22.2, `nofile` 20,000 (hard), `somaxconn` 4096.
- **The load generator runs on the same 4 cores as the server** and used 25 to 50 % of the CPU in
  the saturated runs (TLS handshakes and records cost the client about as much as the server). The server figures are
  what it achieved with the rest.
- **Loopback network**: no latency, no loss, 64 KB MTU. The kernel charges the receiver's TCP
  processing to the sender's `write` on loopback, so both sides' network stack runs on this machine.
- **Shared host**: other jobs (compilations, test suites) ran on the same VM, from 0.1 to 3 cores.
  Each report records their CPU per 2 s window; `--wait-idle-s` waits for a quiet machine before
  starting, and the "quiet" columns below are medians over the windows in which the other
  processes used less than 0.3 core. One whole-VM stall of about 4 s was observed (games 10k).
- Short games: random legal moves, resignation at ply 80, so games end far more often than in real
  play (up to 12,000 per minute in burst). With `--start-rate 1000` all the games start within a
  few seconds, so they also end together: those end-of-game waves (thousands of commits, rating
  updates and new challenges within seconds) are the worst moments of the paced runs.
- One move per second per game is four to five times the pace of a real 3+2 blitz game (80 plies
  in about 6 minutes): 10,000 paced games produce the move rate of about 40,000-45,000 real blitz
  games, with a quarter of their connections.
- No engine analysis (`ANALYSIS_ENGINE_PATH` unset) and no HTTP API load (login, profiles): the
  sessions are created beforehand.

## Results

Every run below was made from `dedicated-server/` with `node bench/loadgen.js` followed by the
options shown (plus `--label` and, for most runs, `--wait-idle-s 120` to `600`: wait for a quiet
machine). The full tables (more columns) come from `node bench/table.js bench/results/*.json`.

### Connections (`connect`)

| run | command | connected (failed) | conn/s | handshake p50 / p99 ms | Hello->Welcome p50 / p99 ms | server CPU per connection | server RSS per connection (V8 heap) | server RSS total | idle server CPU | heartbeat RTT p50 / p99 ms |
|---|---|---|---|---|---|---|---|---|---|---|
| 10k (2.1 GHz) | `--scenario connect --conns 10000 --hold-s 20` | 10,000 (0) | 810 | 299 / 827 | 79 / 266 | 1.8 ms | 61.5 KB (7.7) | 960 MB | 0.15 core | 1.4 / 13.4 |
| 50k (2.1 GHz) | `--scenario connect --conns 50000 --hold-s 30` | 50,000 (0) | 569 | 942 / 3,834 | 73 / 1,655 | 2.6 ms | 53.6 KB (6.2) | 2,978 MB | 0.56 core | 5.3 / 162 |
| 100k (2.1 GHz), 7 workers, 7 load processes | `--scenario connect --conns 100000 --hold-s 30 --max-run-s 950` | 100,000 (0) | 496 | 1,982 / 4,915 | 266 / 2,327 | 3.7 ms | 54.1 KB (5.8) | 5,836 MB | 1.07 cores | 4.7 / 61 |
| 10k | `--scenario connect --conns 10000 --hold-s 20` | 10,000 (0) | 864 | 356 / 582 | 89 / 223 | 2.5 ms | 60.2 KB (8.1) | 947 MB | 0.27 core | 0.6 / 4.8 |
| 10k, `LISTEN_REUSE_PORT` | `... --conns 10000 --hold-s 20 --reuse-port` | 10,000 (0) | 962 | 324 / 520 | 79 / 186 | 2.2 ms | 60.3 KB (7.3) | 947 MB | 0.29 core | 0.6 / 4.4 |
| 10k, no TLS | `... --conns 10000 --hold-s 20 --plain` | 10,000 (0) | 2,320 | 162 / 260 | 6 / 30 | 1.2 ms | 17.8 KB (5.5) | 525 MB | 0.26 core | 0.3 / 2.0 |
| 10k, TLS session resumption (96 % resumed) | `... --conns 10000 --hold-s 20 --tls-resume` | 10,000 (0) | 888 | 324 / 696 | 81 / 260 | 2.4 ms | 57.8 KB (7.5) | 919 MB | 0.29 core | 0.7 / 9.1 |
| 20k (other processes 1.2-1.4 cores) | `... --conns 20000 --hold-s 40` | 20,000 (0) | 365 | 909 / 1,556 | 59 / 827 | 3.7 ms | 56.2 KB (7.5) | 1,453 MB | 0.35 core | 3.0 / 22.3 |
| 20k, server heartbeat only (other processes 1-2.4 cores) | `... --conns 20000 --hold-s 40 --ping-interval-ms 0` | 20,000 (0) | 536 | 422 / 1,589 | 112 / 532 | 2.8 ms | 58.0 KB (7.3) | 1,491 MB | 0.17 core | - |

"Server CPU per connection" is the server's CPU time during the ramp divided by the connections
opened; it includes the heartbeats of the connections already open, which is why it grows with the
count. The heartbeat RTT is measured by the clients and includes the load generator's own
event-loop lag. The rows marked 2.1 GHz ran on the slower host (see above).

- **Connection rate: CPU-bound.** During every ramp the machine was 90-99 % busy: the server used
  1.3-2.2 cores and the load generator 1.0-2.0 cores. A full TLS 1.3 handshake costs the server
  about 1.1-1.3 ms of CPU (2.2-2.5 ms per connection with TLS against 1.2 ms without), and about
  as much on the client side. The primary (control plane) used 0.1-0.35 core during the ramps
  (presence registration, and with `LISTEN_REUSE_PORT=false` the accept and hand-off of every
  connection: 0.18 core without SO_REUSEPORT against 0.11 core with it, at a similar rate).
- **Memory: 54-62 KB of server RSS per idle connection**, of which 6-8 KB is V8 heap; without TLS
  it is 18 KB, so the TLS state (OpenSSL objects and buffers) is about 40 KB per connection. The
  client side costs about the same (7 load processes held 5.8 GB for 100,000 connections), so
  100,000 connections used 11.6 GB of the 15.7 GB; about 120,000-130,000 would be the limit of this
  box with both sides on it.
- **Idle cost: about 11 µs of server CPU per connection per second** at 100,000 connections
  (1.07 cores) with the server heartbeat and one client Ping every 10 s, i.e. four small TLS
  records per connection per 10 s. The client Ping doubles it: 20,000 connections cost 0.35 core
  with it and 0.17 core with the server heartbeat alone (`--ping-interval-ms 0`), so a client that
  only answers the heartbeat costs about 5-6 µs per second. No connection was dropped during any
  hold.
- **TLS session resumption does not lower the cost of a connection** (2.4 ms of server CPU with
  96 % of the sessions resumed, 2.5 ms without): a TLS 1.3 resumption still performs a key
  exchange, so it saves little here.

### Games at a realistic pace (`games`) and throughput (`burst`)

| run | command | moves/s (quiet median) | move RTT p50 / p90 / p99 / max ms | quiet p99 ms | server move processing p50 / p99 µs | commit p99 ms | games ended /min | server / load gen cores | machine busy | shard lag p99 max ms |
|---|---|---|---|---|---|---|---|---|---|---|
| 1k games, 2k connections | `--scenario games --games 1000` | 998 (1,001) | 0.66 / 1.6 / 13.7 / 75 | 7.7 | 54 / 262 | 91 | 19 | 0.57 / 0.22 | 53 % | 17 |
| 5k games, 10k connections | `--scenario games --games 5000 --warmup-s 20 --duration-s 90` | 4,867 (4,966) | 1.2 / 6.0 / 27.9 / 237 | 18.2 | 29 / 166 | 25 | 3,315 | 1.57 / 0.63 | 58 % | 36 |
| 10k games, 20k connections | `--scenario games --games 10000 --warmup-s 20 --duration-s 90` | 9,472 (9,888) | 4.2 / 38.4 / 729 / 1,855 | 57.9 | 21 / 112 | 214 | 6,624 | 2.05 / 1.04 | 81 % | 443 |
| 20k games, 40k connections | `--scenario games --games 20000 --warmup-s 15 --duration-s 60` | 15,551 (15,809) | 199 / 487 / 877 / 1,841 | 680 | 17 / 93 | 24 | 2,128 | 2.29 / 1.62 | 100 % | 406 |
| 10k games, staggered starts (130 game ends/s) | `--scenario games --games 10000 --start-rate 125 --warmup-s 20 --duration-s 60` | 9,174 (9,502) | 22.8 / 118 / 244 / 478 | 128 | 20 / 142 | 24 | 7,809 | 2.14 / 1.15 | 95 % | 111 |
| 10k games via the queue (disturbed: other processes 1.9 cores) | `--scenario games --via queue --games 10000 --warmup-s 20 --duration-s 60` | 6,786 (-) | 122 / 680 / 1,425 / 3,153 | - | 19 / 165 | 73 | 115 | 1.27 / 0.83 | 99 % | 565 |
| burst, 100 games | `--scenario burst --games 100` | 13,088 (13,614) | 3.7 / 11.1 / 24.8 / 626 | 23.3 | 17 / 98 | 45 | 9,905 | 2.06 / 0.98 | 95 % | 31 |
| burst, 100 games, no TLS | `--scenario burst --games 100 --plain` | 17,026 (20,250) | 2.3 / 9.1 / 23.8 / 88 | 19.2 | 15 / 95 | 24 | 12,930 | 2.06 / 0.90 | 93 % | 42 |
| burst, 500 games | `--scenario burst --games 500` | 13,838 (19,443) | 18.2 / 49.7 / 116 / 3,550 | 66.6 | 15 / 87 | 181 | 10,471 | 1.71 / 1.02 | 87 % | 85 |
| burst, 1000 games | `--scenario burst --games 1000` | 16,532 (19,198) | 38.4 / 89.1 / 150 / 299 | 126 | 15 / 89 | 25 | 12,291 | 2.07 / 1.23 | 99 % | 92 |

Paced runs: 3+2 rated games by direct challenge, one move per second per game (+-50 %), started
at 1,000 games/s. "Move RTT" is measured by the moving client from sending Move to receiving its
own MoveMade (validation, clock, journal append, both players' frames); "server move processing"
is the host's own time for one move. No move was rejected and no connection dropped in any run;
the only errors were a few `AlreadyInGame` answers to a ChallengeAccept sent in the few
milliseconds between a game's end and its commit (the load generator retries).

- **10,000 paced games started together**: the server keeps up, 9,500 moves/s with 2 server
  cores, a median round trip of 2-4 ms and a p99 of 15-60 ms in steady state. The long tail of
  that run (p99 729 ms over the whole window) comes from two events visible in its timeline: a
  stall of the whole VM (every process, the load generator's sampling included, stopped for about
  4 s) and the end-of-game wave (10,000 games started within 10 s all resigned 80 s later: about
  1,000 game ends and new challenges per second for 10 s, round trip p50 30 ms, p99 300 ms).
- **Game churn costs CPU too.** With `--start-rate 125` the games start over 80 s, so in the
  measurement window about 130 games end (commit, rating update) and 130 start (challenge through
  the primary, snapshots) every second, like a busy real server. The same 10,000 games then used
  2.14 server cores for 9,200 moves/s and filled the machine (95 % with the load generator and
  0.5 core of other processes): round trip p50 23 ms, p99 244 ms, max 478 ms, without the long
  outliers of the synchronized run. From the difference, a game costs roughly 3 ms of server CPU
  from challenge to commit, besides its moves.
- **20,000 paced games saturate the machine**: 15,500 of the 20,000 intended moves per second, the
  4 cores 100 % busy (server 2.3, load generator 1.6), round trip p50 200 ms, p99 880 ms. The
  server's own move processing stays under 100 µs at p99: the limit is the CPU of the network path,
  not the chess logic.
- **Burst: 13,000-20,000 moves/s** with the server on about 2.1 cores (the rest goes to the load
  generator). Without TLS the same load reaches 20,250 moves/s against 13,600 with TLS in quiet
  windows (17,000 against 13,100 over the whole window): TLS is a quarter to a third of the
  server's CPU per move.
- **Where the shard CPU goes** (V8 profile of one of the 4 shards during the 10k paced run,
  171 s, 52 % busy): 41 % in `writev` (TLS encryption of the outgoing records and the loopback TCP
  send, called when `src/net/ws.js` uncorks a connection), 9 % stream plumbing, 7 % Node internals,
  7 % `src/game`, 5 % `src/cluster` (router and bus), 5 % `src/store` (journal and commits), 4 %
  `src/chess`, 3 % GC. The longest event-loop stalls were a 71 ms `writev` and 40-50 ms GC pauses.
- **Relays**: a game is hosted by the shard of the player who created the challenge (or waited
  longer in the queue), and the opponent's connection is on another shard 3 times out of 4 with 4
  workers, so 35-51 % of the moves crossed the shard bus; 64-67 % in the staggered and 20k runs,
  where the overloaded shards made the primary place new games on a third shard (see the notes
  below).
- **Latency outliers** besides CPU saturation: finished-game commits run synchronously on the
  shard's event loop (`synchronous=FULL`, and a wait on the database write lock held by another
  process, up to `busy_timeout` 5 s), with a p99 of 24-45 ms in most runs and 180-214 ms in two of
  them; GC pauses of 40-50 ms in the profiled run.

## What saturated first

1. **CPU**, in every scenario, with the load generator taking 25-50 % of the same 4 cores: TLS
   handshakes during the connection ramps (500-960 connections/s with TLS, 2,300 without), and the
   TLS record writes of the moves and relays in the game scenarios (about 15,500 moves/s at a paced
   load, 13,000-20,000 in burst).
2. **Memory** next, for connections: about 55-60 KB of server RSS per idle TLS connection and as
   much on the client side; 100,000 connections left 4 GB free on this 15.7 GB machine.
3. The open-file limit (20,000 per process here) and the ephemeral ports (28,000 per source
   address) were worked around by the tool (more workers, more load processes, one source address
   each) and never limited a run. No run had a failed connection, a rejected move or a dropped
   connection.

## Capacity of a dedicated machine

Figures per server core, from the runs above (TLS on, both sides' kernel work on this machine, so
take them as ±30 %):

| cost | measured |
|---|---|
| new connection (TLS 1.3 full handshake, upgrade, Hello) | 2-2.7 ms of CPU (1.2 ms without TLS) |
| idle connection (10 s server heartbeat + a client Ping every 10 s) | about 11 µs of CPU per second (about half without the client Ping) |
| move (validation, clock, journal, frames to both players, relays) | about 120-160 µs of CPU at a high rate (about 100-120 µs without TLS) |
| game start and end (challenge through the primary, snapshots, commit, rating update) | about 3 ms of CPU per game (estimated from the staggered run) |
| memory | 55-60 KB per idle connection, 65-85 KB with a game in progress, plus about 350 MB for the processes |

For a dedicated 4-core, 16 GB machine running only the server (Linux, `nofile` raised,
`WORKERS=4`), keeping the CPU under about 70 % for the latency:

- **About 100,000 connected players with 30,000-40,000 simultaneous blitz games.** 100,000
  connections cost about 1.1 cores for their heartbeats (about 0.6 if the clients send no Ping of
  their own); 35,000 games at a 3+2 pace (one ply every 4-5 s per game) are about 7,500 moves/s,
  i.e. about 1-1.2 cores, and about 90 games end and start per second, about 0.3 core; total about
  2-2.6 cores of 4. Memory: about 7.5 GB.
- **The first hard limits beyond that are memory (about 150,000-180,000 TLS connections in 16 GB)
  and the reconnection storm after a restart**: at 2-2.7 ms per handshake, 4 cores accept about
  1,500-2,000 connections/s, so 100,000 players need about a minute to come back (clients
  reconnect with a random delay). During the storm each worker keeps at most
  `MAX_PENDING_HANDSHAKES` (128) handshakes in flight and closes the extra connections before any
  TLS work, so the handshakes it starts finish in time and the refused clients come back later;
  the kernel queue in front of it is `LISTEN_BACKLOG` (2048), capped by `net.core.somaxconn`.
  Games in progress give both players `RECOVERY_GRACE_MS` (90 s) to come back, instead of the
  normal grace of 15 to 60 s. On a small machine (2 cores at 2.4 GHz, 2.5-3.5 ms per full
  handshake) 10,000 players need about 30 s of CPU, 15 s on its two cores, to reconnect: well
  inside the recovery grace, but without the handshake limit every handshake waited for all the
  others, and a capacity study lost 32 % of a 10,000-client herd to the clients' 10 s deadline.
- The chess logic is not a concern (under 100 µs per move at p99); the primary is not on the move
  path (under 0.3 core in every run, 0.1-0.35 core during the connection ramps).

A machine with more cores scales about linearly with `WORKERS` for moves and idle connections (a
game lives on one shard; relays between shards cost part of the CPU above), with two shared
resources to watch: the primary (presence, challenges, matchmaking) and the single SQLite writer
(every shard commits finished games to the same database file).

## Server behaviours seen in these runs

- **Game placement under overload.** A new game goes to the creator's shard unless that shard
  reports overload (`SHARD_OVERLOAD_LAG_MS`, event-loop p99 above 50 ms); then the primary picks
  the shard with the fewest games in the last `shard.load` report (sent every 2 s), and every game
  created before the next report goes to that same shard. On a busy machine every shard's p99 lag
  hovers around 50 ms (a single 40-50 ms GC pause is enough), so the flag flips from report to
  report: in the burst 1000 run one shard hosted 433 games against 159-199 for the others, in the
  staggered run 3,447 against 1,935-2,270, with `--via queue` 3,414 against 1,831-2,470. The moved
  games usually have no local player, so 64-67 % of the moves crossed the bus instead of 35-41 %,
  and the "overloaded" shard keeps its connections (the TLS and socket work, most of its CPU)
  anyway. With `--server-env SHARD_OVERLOAD_LAG_MS=5000` the same runs placed 210-276 and
  2,322-2,522 games per shard and relayed 39 % and 33 % of the moves (rows below; the machine was
  busier during those two runs, so their latencies are not comparable).
  Since these runs the default threshold is 250 ms, and the primary counts the games it places
  between two reports, so a burst of new games is spread over the shards still eligible (a
  staggered run with that counting placed 2,437 / 2,522 / 2,460 / 2,396 games).

| run | command | games per shard | moves relayed | moves/s | move RTT p50 / p99 ms | other processes |
|---|---|---|---|---|---|---|
| burst 1000 | `--scenario burst --games 1000` | 195 / 199 / 159 / 433 | 41 % | 16,532 | 38 / 150 | 0.67 core |
| burst 1000, threshold 5 s | `... --server-env SHARD_OVERLOAD_LAG_MS=5000` | 240 / 276 / 210 / 253 | 39 % | 10,937 | 58 / 227 | 1.44 cores |
| 10k staggered | `--scenario games --games 10000 --start-rate 125 --warmup-s 20 --duration-s 60` | 2,270 / 3,447 / 1,935 / 2,211 | 64 % | 9,174 | 23 / 244 | 0.5 core |
| 10k staggered, threshold 5 s | `... --server-env SHARD_OVERLOAD_LAG_MS=5000` | 2,522 / 2,456 / 2,322 / 2,506 | 33 % | 7,057 | 182 / 893 | 1.53 cores |

- **Commits on the event loop.** During these runs a shard committed its finished games with a
  synchronous SQLite transaction (`synchronous=FULL`) that may also wait for the write lock of
  another process; the shard's players waited meanwhile (commit p99 up to 214 ms). The commits now
  run on a writer thread per shard (`src/store/writer.js`); a 10k staggered run through it
  committed 15,676 rated games with no error, but its effect on latency was not measured (the host
  was too busy to compare).
- **TLS writes.** A quarter to a third of the CPU per move and 40 of the 55-60 KB per connection are
  TLS; `writev` on uncork (`src/net/ws.js`) is the largest item of the shard profile.

## Going further

- **Give the server the whole machine**: run the load generator on other machines with `--url`,
  `--tokens` (one slice of the accounts per machine) and `--metrics`. On the same machine 25-50 %
  of the CPU went to the clients in the saturated runs.
- **More workers**: `WORKERS` = the number of cores, with `LimitNOFILE` (systemd) or `ulimit -n`
  above the connections per worker (this container's 20,000 forced 7 workers on 4 cores for
  100,000 connections). `MAX_CONNECTIONS` is 200,000 by default.
- **`LISTEN_REUSE_PORT=true`** (Linux): every worker accepts on its own socket and the kernel spreads
  the connections; the primary no longer accepts and hands out every connection (its CPU during
  the 10k ramp went from 0.18 to 0.11 core, 962 against 864 connections/s, within the noise of this
  host). Worth it for high connection rates.
- **TLS offload**: with `TLS_MODE=proxy` a reverse proxy (HAProxy, nginx) terminates TLS. On a
  separate machine it removes a quarter to a third of the server's CPU per move, half of the CPU
  of a new connection and about 40 KB per connection (the `--plain` rows); on the same machine it
  only moves that cost to the proxy (not measured here).
- **`SHARD_OVERLOAD_LAG_MS`** (250 ms by default, 50 ms during the runs above): on a machine that
  runs near its CPU limit, raise it further so that games stay on the creator's shard (see above).
- **Heartbeat**: `HEARTBEAT_INTERVAL_MS` 10 s -> 20 s halves the server's share of the idle cost,
  at the price of a slower detection of dead connections (`HEARTBEAT_TIMEOUT_MS`).
- **Kernel settings** for 100,000+ sockets: `fs.nr_open` and `LimitNOFILE`, `net.core.somaxconn`
  (4096 here; it caps `LISTEN_BACKLOG`) and `net.ipv4.tcp_max_syn_backlog` (8192) for the
  reconnection storms, `net.ipv4.ip_local_reserved_ports=44664` (the server port is inside the
  ephemeral range, and a restart can fail with `EADDRINUSE` otherwise; README, kernel settings),
  `net.ipv4.tcp_mem` and the socket buffer defaults, and on the load machines
  `net.ipv4.ip_local_port_range` or several source addresses.
- **`MAX_PENDING_HANDSHAKES`** (128 per worker): enough to keep a core busy with handshakes (at
  1-3.5 ms each, 128 in flight are 0.1-0.5 s of work) while every started handshake finishes long
  before a client's deadline. A higher value only helps when the clients are far away (each
  handshake then waits for round trips, not for the CPU). While the server is full it also sets
  the rate of new TLS connections let through (half of it per second and per worker).
- **Several machines**: DESIGN.md section 9 (shard ranges with `SHARD_BASE`, a TCP bus between
  machines, the control plane as a service, a PostgreSQL store, a layer-4 load balancer in front).
- **Profiling**: `--server-cpu-prof DIR` and `node bench/profile-summary.js DIR/*.cpuprofile`
  (`--callers FUNCTION` shows who calls a hot function, `--stalls N` the longest event-loop stalls).
