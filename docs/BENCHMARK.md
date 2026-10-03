# Benchmark

This page compares the Rust server with the Node.js server it replaces, on the same machine, with
the same settings and the same load generator: how to run the comparison, what each scenario
measures, the choices made to keep it fair, and the results of 2026-10-03, with two runs of the
Rust server alone at a realistic pace. [SIZING.md](SIZING.md) derives its unit costs from these
results. The load study of the Node.js server alone (2026-09) is in the git history of this file
(for example at commit 08d27f1).

## Running the comparison

### Quick start

```sh
cd dedicated-server
# A checkout of the Node.js server (the last commit before the rewrite), with its dependencies:
git worktree add ../node-ref 7531830 && (cd ../node-ref/dedicated-server && npm ci)

bench/run.sh --node26 /path/to/node26/bin/node --node-tree ../node-ref/dedicated-server
bench/run.sh ... --quick                       # same steps, small sizes: checks the set-up in minutes
bench/run.sh ... --targets rust,node26,node24 --node24 /path/to/node24/bin/node
bench/run.sh --targets rust --scenarios games -- --steps 1000,4000,8000 --move-interval-ms 5000 --gesture-hz 1
bench/run.sh --help                            # every option
```

The default targets are `rust,node26`; Node 24 is still available as `node24`, and `--targets
rust` needs no Node.js at all. Options after `--` go to every `scacelith-bench` command of the
run. `bench/run.sh` builds the release binaries (`cargo build --release -p scacelith-server -p
scacelith-client`, `CARGO_BUILD_JOBS` jobs, 2 by default), then for each target:

1. makes a fresh data directory, runs the migrations and creates the bench accounts with the
   server's own admin command (`scacelith-server admin bench-accounts`, `node bin/admin.js
   bench-accounts`: verified accounts with a live session each, 20,000 of them, 600 with
   `--quick`);
2. for each scenario, starts the server pinned to the server CPUs (`taskset -c 0,1`), waits until
   `/api/v1/readyz` and the metrics listener's `/readyz` answer 200 (except for `idle`, which
   measures that wait), runs `scacelith-bench` pinned to the load CPUs (`taskset -c 2,3`), and
   stops the server (SIGTERM). Every scenario starts on a freshly started server; the steps of a
   scenario run one after the other on the same server.

Results go to `bench/results/<date>/`: one JSON report per target and scenario
(`node26-games.json`), the server and load generator logs, the data directories, and
`summary.md`, the Markdown tables of every report with the machine and the versions. Git keeps
only `summary.md`. A scenario that fails is listed in the summary and the run goes on.

Requirements: Linux (CPU and memory come from `/proc`), 4 CPUs or more (2 for the server, 2 for
the load generator; `--server-cpus` and `--load-cpus` choose them), `taskset`, `openssl`, `curl`,
free ports 18443 and 19464 (`--port`, `--metrics-port`), and an open-file limit (`ulimit -Hn`)
above 20,000 for the 10,000-connection step. The harness raises the soft limit of both processes
to the hard one.

**By hand.** `scacelith-bench` targets any test server (never a production one: it needs bench
accounts and raised limits; [SIZING.md](SIZING.md#validating-on-the-real-machine) lists them):

```sh
scacelith-bench games --target rust --addr 192.0.2.10:443 --host test.example.org \
    --tokens tokens.tsv --steps 200 --warmup-s 10 --duration-s 60 --out games.json
scacelith-bench table results/*.json      # Markdown tables
scacelith-bench --help                    # every option
```

`--ca FILE` pins the server's certificate (the harness passes its self-signed one); without it the
tool accepts any certificate, which is only acceptable on a test machine. `--server-pid` (a local
server) adds its CPU and memory to the report.

### One tool, two protocols

The load generator is the Rust client SDK (`crates/client`, `scacelith-bench`): the same TLS
client (rustls, a full handshake per connection unless `--tls-resume`), the same WebSocket client
and the same HTTP/1.1 client for both servers. Only the realtime message codec differs:

- `--target rust`: protocol v1 (`scacelith.rt1`, [PROTOCOL.md](PROTOCOL.md)) through the SDK's
  `Connection`;
- `--target node`: the Node.js server's protocol 3 (`scacelith.v1`) through a small codec of the
  messages the scenarios use, derived from that server's `src/protocol/schema.js` and isolated in
  `crates/client/src/bin/bench/proto3.rs`.

Both protocols carry the same messages with the same layouts apart from `Hello` and `Welcome`, so
both servers receive the same bytes for a move, a gesture or a queue join. The REST calls are
identical.

### Scenarios

Every scenario warms up, then measures over a fixed window (`--warmup-s`, `--duration-s`); the
counters, latency histograms and server resources of the report cover the window only.

| scenario | load | figures |
|---|---|---|
| `idle` | none | time to ready: from the server's start (`--spawned-at-ms`, taken by the harness just before it starts the server) to the first 200 of `/api/v1/readyz` and of the metrics listener's `/readyz`; then idle CPU, RSS and PSS |
| `connections` | authenticated WebSocket connections (TCP, TLS, upgrade, `Hello`/`Welcome` with a session token), `--inflight` (200) handshakes at a time, ramped to each step (1,000, 5,000, 10,000: the connections of a step stay open into the next) and held idle (the connections answer the server's heartbeat) | handshakes per second during the ramp, handshake (TCP + TLS + 101) and `Hello`->`Welcome` latency, failures by class, connections dropped during the hold, idle CPU with the connections, RSS and PSS per connection above the idle footprint, the load generator's CPU during the ramp |
| `games` | N games (100, 500, 1,000), i.e. 2N bots: one challenges the other, both play uniformly random legal moves, the side to move thinking 1 s +-50 % (`--move-interval-ms`), and both send head gestures at 10 per second (`--gesture-hz`); resignation at ply 80, a new game after 1 s; 3+2, rated. Each step stops its games before the next one starts | move relay (Move sent by the mover -> MoveMade read by the opponent), move confirmation (-> MoveMade read by the mover), gesture relay (Gesture sent -> relayed Gesture read by the opponent), moves/s, relayed gestures/s, errors, server CPU and memory |
| `matchmaking` | M players (200, 1,000) connected, then all join the casual 3+2 queue at the same instant; White aborts each game at once; one warm-up burst, then 3 measured bursts | time from `QueueJoin` to `GameSnapshot` per player (p50 ... max), burst makespan (start signal -> last snapshot), unmatched players, server CPU |
| `rest` | 32 keep-alive HTTPS connections sending requests back to back, one window per endpoint: `GET /api/v1/info`, `/leaderboard?category=3+2`, `/games/:id/pgn` (games of 40 plies), `/games/:id/gif` cold (4 connections; a new `delay`/`orientation` per request, so every one is rendered) and cached (8 games with the default options) | requests/s, latency p50/p90/p99/max, errors by status, server CPU and memory |
| `login` | 16 accounts registered through the API, then `POST /api/v1/auth/login` with them in turn at 8 at a time | logins/s and latency: the password hash is the cost, and both servers use the same one (below) |

**How latencies are taken.** The two ends of a game live in one task of the load generator, so
a move's relay time is the difference of two instants of one clock: when the mover's `Move` was
queued for sending, and when the opponent's connection read the `MoveMade` from its socket (the
WebSocket reader stamps every message as it reads it, before any scheduling of the bot). The
gesture relay works the same way, the yaw of each gesture carrying a sequence number. Histograms
are log-linear (exact below 64 µs, then 32 sub-buckets per power of two, about 3 % resolution),
with the bucket math and quantile definition of the former Node.js load generator
(`bench/lib/hist.js`), so the figures compare with its own.

**Server resources.** CPU is the user plus system time of the server's whole process tree read
from `/proc/<pid>/stat` at both ends of the window: the Node.js primary and its two shard workers,
or the single Rust process. Memory is the sum of their RSS, and of their PSS (`smaps_rollup`),
which splits the pages the Node.js processes share (code, the V8 snapshot) between them: RSS
counts those several times, PSS once. "CPU %" is percent of one core (200 = both server cores
busy).

### Fairness choices

- **Same machine, same cores, same order.** The server and the load generator are pinned to
  disjoint CPUs. Each target gets a fresh data directory and every scenario a freshly started
  server, in the same order for every target. The load generator binary is the same for all
  targets, and its random choices are seeded (`--seed 1`).
- **Same TLS.** One self-signed ECDSA P-256 certificate (CA:FALSE) for every server, pinned by the
  client; TLS 1.3 over loopback with a full handshake per connection (no resumption tickets), so
  the handshake cost is that of a player connecting for the first time. The servers use their
  native TLS (`TLS_MODE=native`): OpenSSL in Node.js, rustls in the Rust server.
- **Same size.** `WORKERS=2` on both. The word does not mean the same thing: the Node.js server
  runs 2 shard processes plus its primary, the Rust server 2 game shards and 2 runtime threads in
  one process. The limits that Node.js applies per worker are whole-server limits in the Rust
  server whose defaults are the Node.js default times `WORKERS`, so both keep their defaults and
  have the same totals: 2 password hashes at once, 2 GIF render threads, 8 queued renders, a 64 MiB
  GIF cache, 256 TLS handshake slots. `MAX_PENDING_HANDSHAKES_PER_IP` is set so that the one load
  address may use every slot (127 per Node.js worker, 255 for the Rust server).
- **Same password hash.** Argon2id with 64 MiB, 3 passes and 4 lanes on both (Node.js 24.7 and
  later; the harness checks `crypto.argon2` and records the algorithm in each report's `meta`).
  The implementations differ: OpenSSL's in Node.js, the `argon2` crate in Rust.
- **Limits raised for the bench only.** Proof of work off, no e-mail verification, the load address
  in `ABUSE_EXEMPT`, and these raised: `HTTP_RATE_PER_IP`, `AUTH_RATE_PER_IP`,
  `AUTH_REGISTER_PER_HOUR`, `AUTH_FAILURES_PER_ACCOUNT`, `USER_RATE_PER_MIN`, `MAX_CONNECTIONS`,
  `MAX_CONNECTIONS_PER_IP`, `CHALLENGE_UNPLAYED_PER_MIN`, `MATCH_REPEAT_LIMIT`,
  `CONDUCT_ABANDON_LIMIT`, the four `GIF_*_RENDERS_PER_*`; `WS_MSG_RATE` 100 and `WS_MSG_BURST`
  200; `GESTURE_RATE` 20 and `GESTURE_BURST` 40 (above the bots' 10 per second); no engine
  analysis (`ANALYSIS_WORKERS=0`); `LOG_LEVEL=warn`. The full list is `common_env` in
  `bench/run.sh`.
- **Per-account limits fixed in the code** (`public_read`: 60 requests per minute for the game
  routes; `gif`: 30 per minute) cannot be raised, so the authenticated REST requests rotate over
  the 20,000 accounts. A `429` in a report (`error.429`) means the rotation was too short for the
  server's throughput: create more accounts. The Node.js server keeps these buckets per worker,
  the Rust server once per server, which only matters past that point.
- **Matchmaking** runs in the casual queue (no conduct cooldown after the aborts); both servers
  pair every `MATCH_TICK_MS` (250 ms by default), which dominates the time to a game.

### Caveats

- The load generator runs on the same machine, over loopback: no network latency or loss, and the
  kernel charges the receiving side's TCP work to the sender. The latencies are those of the
  servers' own processing and scheduling, a lower bound of what players see.
- Pinning separates the cores, not the caches, memory bandwidth or the kernel's network work. On
  a shared or virtual machine, other jobs disturb the runs: compare runs made on a quiet machine,
  and repeat a run that looks off.
- The load generator must not saturate its own cores, and against the Rust server it can: its TLS
  handshakes cost about as much as the server's, so the connection ramps of 2026-10-03 ran it at
  up to 189 % of its 200 %. Its own CPU is reported next to the server's ("load CPU %", "ramp load
  CPU %", in % of one core), and it warns on stderr when that passes 85 % of the cores it may use:
  then give it more CPUs (`--load-cpus`), since its queueing shows as server latency and caps the
  rates. The tables below flag the steps it limited.
- RSS overstates the Node.js server (shared pages counted in each process); PSS is the fairer
  memory figure for it. "KiB/conn" divides the growth above the idle footprint by the open
  connections; a garbage-collected heap makes it noisy at small steps.
- The REST and login figures include TLS records but not handshakes (keep-alive connections).
- Bench accounts have no password (they log in only with their tokens), so the login scenario
  registers its own accounts first; their registration time is reported.
- The bench clients answer the server's heartbeat but send no `Ping` of their own, and with the
  sizes of `bench/run.sh` no game ends during a measurement window (a game of 80 plies lasts about
  80 s at the default pace, longer than the warm-up and the window together): the game client's
  pings and the churn of games (starts, ends, commits, rating updates) are not in the game
  figures.

### Reading the results

Each report (`scacelith-bench` prints it on stdout, `--out` writes it) holds `tool`, `version`,
`scenario`, `target`, `label`, `meta` (server version, password hash, CPUs), `settings`, `params`
(the scenario's options) and one entry per step: `headline` (the figures of the Markdown table,
in order) and `detail` (every counter, every latency summary in ms with `n`, `mean`, `p50`, `p90`,
`p99`, `p999` and `max`, the server CPU and memory of the window, the load generator's own CPU
under `loadGenerator`, failures by class).
`scacelith-bench table FILE...` prints one Markdown table per scenario, one row per run and step.

Error counters are named by cause: `fail.<class>` (a connection that did not complete: `timeout`,
`tls`, `upgrade_503`, `refused_<ErrorCode>`...), `drop.<close code>` (a connection the server
closed), `error.<status>` (an HTTP answer), `error.challenge_<code>`, `error.move_rejected_<code>`,
`error.queue_<code>`, `error.server_<code>` (protocol `Error` messages), `error.match_timeout`,
`error.start_timeout`.

Before comparing two figures, check in the same row that the load generator stayed below about
170 % ("load CPU %") and that the server's CPU says which side was busy: a server below its 200 %
with a busy load generator measured the load generator.

### Where the results are

The comparison's results are the `summary.md` files of the runs, committed under
`bench/results/<date>/` with the machine they were measured on. A `--quick` run is only a check
of the set-up (sizes of a few hundred clients, windows of a few seconds, and 600 accounts, too few
for the cached GIF endpoint, which then reports `429`s): do not quote it.

## Results of 2026-10-03

`bench/run.sh` with its defaults (`bench/results/2026-10-03_165735/summary.md`):

- Machine: a KVM guest with 4 vCPUs (Intel Xeon Processor @ 2.10 GHz), 15.7 GiB of RAM, Linux
  6.18.44; the server on CPUs 0 and 1, the load generator on CPUs 2 and 3; `WORKERS=2`; TLS 1.3
  with an ECDSA P-256 certificate, no resumption.
- Rust: `scacelith-server` at commit b02f03b, `PASSWORD_HASH_CONCURRENCY=2` (whole server).
- Node 26: Node.js v26.10.0 running the Node.js server at commit 7531830,
  `PASSWORD_HASH_CONCURRENCY=1` per worker × 2.
- Both: Argon2id with m = 64 MiB, t = 3, p = 4.

In the ratio columns, "×" is how many times better the Rust server does: its throughput over the
Node.js server's, or the Node.js server's cost (CPU, memory, latency) over its own.

### Idle

| | Rust | Node 26 | Ratio |
|---|---:|---:|---:|
| Time to ready | 24 ms | 434 ms | 18× |
| Idle CPU (% of one core) | 1.10 | 2.15 | 2.0× |
| RSS | 12.9 MiB | 242 MiB | 19× |
| PSS | 11.2 MiB | 137 MiB | 12× |
| Processes | 1 | 3 | |

A fresh database and an empty journal on both. The Node.js server starts a primary and two worker
processes, each with its own V8 heap and its own copy of the code; the Rust server is one process.

### Connections

| server | step | open | handshakes/s | handshake p50 ms | handshake p99 ms | hello p99 ms | failed | dropped | idle CPU % | RSS MiB | KiB/conn RSS | KiB/conn PSS | ramp load CPU % |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| rust | 1000 conns | 1000 | 3444 | 36.4 | 95.2 | 59.9 | 0 | 0 | 1.33 | 61.6 | 49.6 | 33.2 | 142 |
| rust | 5000 conns | 5000 | 5031 | 28.9 | 49.7 | 25.3 | 0 | 0 | 2.40 | 159 | 29.9 | 25.0 | 186 |
| rust | 10000 conns | 10000 | 4253 | 33.3 | 68.6 | 32.5 | 0 | 0 | 3.66 | 258 | 25.0 | 22.5 | 189 |
| node26 | 1000 conns | 1000 | 1107 | 162 | 236 | 20.2 | 0 | 0 | 3.06 | 306 | 68.9 | 60.2 | 56.9 |
| node26 | 5000 conns | 5000 | 1467 | 128 | 162 | 16.0 | 0 | 0 | 4.73 | 476 | 48.6 | 46.6 | 70.4 |
| node26 | 10000 conns | 10000 | 1448 | 126 | 186 | 19.2 | 0 | 0 | 6.86 | 643 | 41.4 | 40.4 | 77.8 |

From the ramps in the JSON reports (server CPU time over the connections opened in each ramp):

| step | Rust: server CPU during the ramp | Rust: CPU per new connection | Node 26: server CPU during the ramp | Node 26: CPU per new connection | Ratio |
|---|---:|---:|---:|---:|---:|
| 1,000 | 129 % | 0.38 ms | 190 % | 1.73 ms | 4.6× |
| +4,000 | 178 % | 0.36 ms | 190 % | 1.31 ms | 3.7× |
| +5,000 | 170 % | 0.40 ms | 195 % | 1.35 ms | 3.4× |

- **What limited each step.** The Node.js server was saturated in every ramp (190-195 % of its
  200 %) while its load generator idled (57-78 %). Against the Rust server it was the other way:
  the load generator ran at 186-189 % of its 200 % in the 5,000 and 10,000 steps, with the server
  at 170-178 %, so those handshake rates are the load generator's ceiling, not the server's. The
  1,000 step ramps in 0.29 s, too short for a steady figure. The cost per connection is the fair
  comparison: about 0.36-0.40 ms of server CPU for the Rust server against 1.3-1.7 ms, 3.4 to 4.6
  times less, so the Rust server would take about 5,000-5,500 handshakes per second on these
  2 vCPUs with nothing else to do (inferred).
- **Handshake latency** is queueing at the 200 handshakes the load generator keeps in flight:
  200 ÷ 5,031 per second is 40 ms, 200 ÷ 1,467 is 136 ms, close to the medians.
- **`Hello` latency** is higher for the Rust server (p99 25-60 ms against 16-20 ms): the
  `Welcome` is read by a load generator running near its own limit, whose scheduling delays add
  to it (inferred); in the Node.js runs the slow server kept the load generator idle.
- **Memory per connection** at 10,000 connections: 25.0 against 41.4 KiB of RSS (1.7×), 22.5
  against 40.4 KiB of PSS (1.8×).
- **Idle connections** cost 2.6 µs per second each above the idle process (3.66 % of a core with
  10,000), the server's heartbeat once per 10 s; the Node.js server 4.7 µs (1.8×).
- No connection failed or was dropped.

### Games

| server | step | live games | moves/s | move relay p50 ms | move relay p99 ms | move confirm p99 ms | gestures/s | gesture p50 ms | gesture p99 ms | errors | CPU % | RSS MiB | load CPU % |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| rust | 100 games | 100 | 103 | 0.19 | 0.54 | 0.52 | 2000 | 0.18 | 0.57 | 0 | 11.7 | 19.5 | 11.8 |
| rust | 500 games | 500 | 507 | 0.20 | 0.89 | 0.82 | 9999 | 0.20 | 0.78 | 0 | 32.9 | 53.1 | 36.0 |
| rust | 1000 games | 1000 | 1019 | 0.27 | 1.74 | 1.78 | 19997 | 0.29 | 1.62 | 0 | 55.9 | 82.1 | 59.8 |
| node26 | 100 games | 100 | 102 | 0.37 | 8.32 | 8.13 | 1999 | 0.31 | 9.34 | 0 | 40.9 | 305 | 13.0 |
| node26 | 500 games | 500 | 505 | 0.44 | 15.7 | 17.2 | 9995 | 0.47 | 16.3 | 0 | 90.4 | 374 | 36.2 |
| node26 | 1000 games | 1000 | 1013 | 1.00 | 36.4 | 41.5 | 20006 | 1.10 | 36.4 | 0 | 129 | 508 | 64.6 |

| step | CPU | move relay p99 | gesture relay p99 | RSS |
|---|---:|---:|---:|---:|
| 100 games | 3.5× | 15× | 16× | 16× |
| 500 games | 2.7× | 18× | 21× | 7.0× |
| 1,000 games | 2.3× | 21× | 22× | 6.2× |

- The load is mostly gestures: 10 per second per player, two and a half times the default
  `GESTURE_RATE`, so 20,000 relayed gestures per second at 1,000 games, against one ply per second
  per game. The CPU ratio narrows with the load, as the Node.js server's fixed costs weigh less
  at 1,000 games (inferred).
- **What limited each step.** Neither server was CPU-bound: the Rust server used at most 56 % of
  its 200 %, the Node.js server 129 %, and the load generator at most 65 %. The Node.js server's
  tail latency comes from its shard processes, each a single event loop that a burst of work or a
  garbage collection holds up (inferred); the Rust server's runtime threads serve any connection,
  and its p99 stayed under 2 ms.
- Memory: 82 MiB for 2,000 players in games against 508 MiB.
- No move was rejected and no game failed to start.

### Matchmaking

| server | step | connected | match p50 ms | match p90 ms | match p99 ms | match max ms | makespan ms | unmatched | CPU % |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|
| rust | 200 players | 200 | 240 | 248 | 248 | 249 | 246 | 0 | 5.60 |
| rust | 1000 players | 1000 | 219 | 227 | 231 | 237 | 248 | 0 | 21.9 |
| node26 | 200 players | 200 | 219 | 252 | 252 | 268 | 225 | 0 | 13.0 |
| node26 | 1000 players | 1000 | 207 | 266 | 274 | 296 | 254 | 0 | 29.4 |

Both servers pair a whole burst in one round: every makespan is about one `MATCH_TICK_MS`
(250 ms; 225-254 ms). The time to a game is the wait for the next pairing round, so the differences between
the servers (p50 207-240 ms) are the phase of the tick, not their speed. The Rust server used 2.3
and 1.3 times less CPU for the bursts (5.6 against 13.0 %, 21.9 against 29.4 %).

### REST

| server | step | conns | req/s | p50 ms | p90 ms | p99 ms | max ms | errors | CPU % | RSS MiB | load CPU % |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| rust | info | 32 | 50391 | 0.60 | 0.87 | 1.30 | 24.6 | 0 | 186 | 21.4 | 86.2 |
| rust | leaderboard | 32 | 73675 | 0.41 | 0.60 | 0.89 | 34.2 | 0 | 182 | 22.8 | 115 |
| rust | pgn | 32 | 7410 | 3.49 | 8.83 | 11.6 | 20.8 | 0 | 178 | 44.2 | 18.3 |
| rust | gif-cold | 4 | 213 | 18.2 | 22.3 | 26.4 | 36.9 | 0 | 197 | 153 | 3.33 |
| rust | gif-cached | 32 | 12130 | 2.53 | 3.30 | 4.67 | 22.8 | 0 | 192 | 183 | 86.8 |
| node26 | info | 32 | 38012 | 0.76 | 1.17 | 2.03 | 30.1 | 0 | 197 | 417 | 71.8 |
| node26 | leaderboard | 32 | 39680 | 0.73 | 1.10 | 1.97 | 23.5 | 0 | 196 | 420 | 71.8 |
| node26 | pgn | 32 | 3480 | 5.06 | 20.2 | 53.8 | 210 | 0 | 130 | 477 | 10.2 |
| node26 | gif-cold | 4 | 97.6 | 39.4 | 49.7 | 68.6 | 93.6 | 0 | 198 | 670 | 1.73 |
| node26 | gif-cached | 32 | 9389 | 3.10 | 4.93 | 8.00 | 39.3 | 0 | 197 | 746 | 63.3 |

| endpoint | answer size | requests/s | Rust: CPU per request | Node 26: CPU per request | CPU ratio |
|---|---:|---:|---:|---:|---:|
| `info` | 951 B | 1.33× | 37 µs | 52 µs | 1.4× |
| `leaderboard` | 71 B | 1.86× | 25 µs | 49 µs | 2.0× |
| `pgn` (40 plies) | 2,084 B | 2.13× | 240 µs | 374 µs | 1.6× |
| `gif-cold` (40 plies, medium) | 114 KB | 2.18× | 9.2 ms | 20.3 ms | 2.2× |
| `gif-cached` | 112 KB | 1.29× | 158 µs | 210 µs | 1.3× |

- **What limited each step.** The server's CPU in every window (178-198 % of its 200 %), except
  the Node.js server's `pgn` window, where it used only 130 %: something else held it back, not
  investigated. The load generator stayed at or below 115 %.
- The leaderboard was empty (a 71-byte answer: the bench accounts are all provisional, and the
  leaderboard leaves provisional ratings out), so that row measures the request path, not the
  building of a leaderboard.
- The cold GIFs ran on the 2 render threads of each server; the Rust renderer draws the same
  pictures as the Node.js one, byte for byte, in 0.45 of its CPU time. The Rust server's RSS rose
  from 44 to 153 MiB during that window: 2 render threads and the 64 MiB GIF cache, which the
  cold requests fill.
- The cached GIFs moved about 1.35 GB/s (12,130 per second × 112 KB) over loopback.

### Login

| server | step | conns | req/s | p50 ms | p90 ms | p99 ms | max ms | errors | CPU % | RSS MiB | load CPU % | register s |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| rust | login | 8 | 11.7 | 680 | 713 | 745 | 748 | 0 | 197 | 97.9 | 0.25 | 1.36 |
| node26 | login | 8 | 8.70 | 827 | 1196 | 1327 | 1370 | 0 | 195 | 388 | 0.15 | 2.45 |

Both servers hashed 2 passwords at a time with both cores busy, so the figure is the cost of one
Argon2id hash: 0.168 s of CPU per login for the Rust server (197 % ÷ 11.7 per second), 0.224 s for
the Node.js server, 1.34 times more logins per second. The latency is the queue: 8 logins in
flight for 2 hash slots of about 0.17 s each give the Rust server's 680 ms. The Node.js server's
wider spread (p99 1.33 s) fits its two queues, one per worker, which a login joins by the worker
its connection reached (inferred). Registering the 16 accounts took 1.36 against 2.45 s.

### Games at a realistic pace (Rust)

Two runs of the Rust server alone, after the comparison, on the same machine:

```sh
bench/run.sh --targets rust --scenarios games -- --steps 1000,4000,8000 --move-interval-ms 5000 --gesture-hz 1
bench/run.sh --targets rust --scenarios games -- --steps 1000,4000,8000 --move-interval-ms 5000 --gesture-hz 4
```

The side to move thinks 5 s +-50 %, so a game makes one ply every 5 s (each player moves about
every 10 s), as in a 3+2 blitz game. One gesture per second per player is what calm players send
with the default `GESTURE_IDLE_MS`, four what players who move all the time send at the default
`GESTURE_RATE`. A game of 80 plies lasts about 400 s, so no game ended during a window. Each step
stops its games before the next one starts (`bench/results/2026-10-03_171401_rust-games-3+2-pace-1-gesture-hz/`
and `2026-10-03_171716_rust-games-3+2-pace-4-gesture-hz/`).

| gestures per player per second | step | moves/s | gestures/s | move relay p50 ms | move relay p99 ms | gesture p99 ms | CPU % | RSS MiB | load CPU % |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|
| 1 | 1,000 games | 205 | 2000 | 0.19 | 0.98 | 0.92 | 14.7 | 91.6 | 13.3 |
| 1 | 4,000 games | 808 | 7999 | 0.21 | 0.82 | 0.98 | 38.3 | 230 | 35.8 |
| 1 | 8,000 games | 1610 | 16001 | 0.29 | 1.39 | 1.46 | 64.1 | 409 | 58.7 |
| 4 | 1,000 games | 205 | 8000 | 0.20 | 1.04 | 1.04 | 30.6 | 97.0 | 29.9 |
| 4 | 4,000 games | 808 | 32005 | 0.40 | 5.06 | 5.18 | 91.8 | 231 | 85.6 |
| 4 | 8,000 games | 1608 | 63996 | 1.26 | 114 | 114 | 171 | 408 | 156 |

- **The cost of a gesture.** Between the two runs only the gestures change. At 4,000 games,
  (91.8 − 38.3) % of a core for 24,006 more gestures per second is 22.3 µs per relayed gesture;
  at 8,000 games, 22.4 µs (26.4 µs at 1,000 games, where the fixed costs weigh more).
- **The rest of a game.** A linear fit of the 4,000- and 8,000-game steps of both runs gives
  0.124 core + 19.9 µs per game per second + 22.4 µs per gesture (within 0.2 % of the four
  steps); with the 1,000-game steps too, 0.097 core + 23.7 µs + 22.4 µs. So a game without its
  gestures, at this pace, costs 20-24 µs per second: its plies and the server's heartbeat to both
  players.
- **Memory.** 22.7-23.6 KiB more RSS per extra connection between the steps.
- **Latency.** With one gesture per second the p99 stayed at or below 1.4 ms up to 64 % of one
  core. With four, it was 1.0 ms at 15 % of the 2 vCPUs, 5.1 ms at 46 % and 114 ms at 86 %.
- **What limited each step.** None of the one-gesture steps. At four gestures and 8,000 games the
  server was near saturation (171 % of its 200 %) and the load generator at 156 % (78 % of its
  2 vCPUs, below its warning): part of the 114 ms may be its own queueing (inferred).

### What limited each step

| scenario | step | Rust server | Node 26 server |
|---|---|---|---|
| connections | 1,000 | a ramp of 0.29 s (server 129 %, load generator 142 %) | server CPU (190 %) |
| connections | 5,000, 10,000 | **the load generator** (186-189 % of its 200 %); server 170-178 % | server CPU (190-195 %) |
| games | all | nothing (server at most 56 %, load generator at most 60 %) | no CPU limit (at most 129 %); tail latency from its event loops (inferred) |
| matchmaking | all | `MATCH_TICK_MS` | `MATCH_TICK_MS` |
| rest | all | server CPU (178-197 %) | server CPU (196-198 %), except `pgn` (130 %, cause not identified) |
| login | | the 2 hash slots, server CPU (197 %) | the same (195 %) |
| games at a realistic pace | 8,000 games, 4 gestures per player per second | server CPU near saturation (171 %), load generator at 156 % | |

### Summary

Per unit of server CPU, on these 2 vCPUs, the Rust server relays 2.3 to 3.5 times the game
traffic of the Node.js server on Node 26, opens 3.4 to 4.6 times the connections, serves 1.3 to
2.2 times the API requests and 1.34 times the logins (the hash dominates). It needs 1.7 to 1.8
times less memory per connection, 6 times less with 1,000 games in progress and 19 times less at
idle, and its tail latency on the move and gesture relay is 15 to 22 times lower.
