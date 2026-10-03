# Anti-cheat, sanctions, reports and moderation

This server separates three things that are often mixed up:

| | What | Automatic action |
|---|---|---|
| **Certain protocol cheats** | A client did something an unmodified game client cannot do (forged message type, illegal move or move out of turn in a synchronised position, message for a game it does not play). | Yes: forfeit, disconnection, ban of `BAN_DURATION_HOURS`, integrity level `confirmed`, rating refunds of the player's victims. |
| **Statistical suspicion** | Engine analysis of finished rated games says a player's moves and timing look like engine assistance. | **Never.** The integrity level and the evidence are written for a moderator. No ban, no matchmaking change. A moderator who confirms it bans the player, which refunds the victims too. |
| **Reports** | Players report opponents of their recent games. | **Never.** They raise the review priority, weighted by the reporter's credibility. |

The code is in `crates/server/src/anticheat/` ([section 9](#9-code-map)); moderators use
`scacelith-server admin` ([section 6](#6-for-moderators-scacelith-server-admin)).

## 1. Anomalies (protocol)

The connection tasks and the game hosts report anomalies to the anti-cheat service
(`AnomalySink::record` with an `Anomaly { user, game, kind, detail, pos_matched }`). Severities
(DESIGN 6.5):

| kind | severity | when | notes |
|---|---|---|---|
| `forged_type` | certain | a server->client message type sent by a client | |
| `foreign_game` | certain | game message for a game the player does not play | |
| `out_of_turn` | certain | move in a synchronised position while it is not the player's turn | downgraded to suspicious when reported with `pos_matched: false` (the client's position hash did not match) |
| `illegal_move` | certain | illegal move in a synchronised position | same |
| `malformed` | suspicious | undecodable frame after Hello (connection closed 4001) | |
| `bad_seq` | suspicious | seq not last+1 | |
| `flood` | suspicious | rate limit exceeded repeatedly (connection closed 4301) | |
| `repeated_desync` | suspicious | 3+ desyncs in one game (the room counts them) | |
| `clock_implausible` | suspicious | `thinkMs` physically impossible | never certain: clock drift, suspended VMs |
| `stale_ply`, `desync`, `nothing_to_claim` | info | honest races and bugs | not security-logged |
| anything else | info | caller bug | recorded as `unknown` with the original kind in the detail |

Storage: recording never waits, and every write goes through the store's single writer thread,
which runs its jobs in the order they were submitted. A certain anomaly gets a job of its own.
The first anomaly of a kind by a player in a game is handed to the writer at once, so a commit of
finished games queued after it sees it (the analysis queue policy of the commit reads it), and a
certain anomaly is written before the ban it causes. Its repeats within a second of the last
write of that row are coalesced into that row (`count`, `lastAt`) and written together when the
second ends: a flooding client costs about one row per second for each kind, player and game,
not one per message. The buffer is bounded (5000 distinct rows, after which `info` rows are
dropped; twice as many for the others), losses are counted in
`scacelith_anticheat_anomalies_dropped_total`, and the buffer is written out at the shutdown,
before the store closes. Metrics: `scacelith_anticheat_anomalies_total{kind,severity}`.
Suspicious and certain anomalies are logged as `anomaly` security records.

## 2. Automatic sanctions

With `AUTO_SANCTION_CERTAIN_CHEATS=true` the host ends the game (`Forfeit`) and asks the
anti-cheat service to sanction the player (`AnomalySink::sanction_certain`, which runs
`Anticheat::sanction`: one job on the store writer thread), which:

* creates a ban of `BAN_DURATION_HOURS` (`source: 'auto'`, reason `certain_cheat:<kind>`, the game id),
  unless an active ban for cheating that refunds (automatic, or `integrity confirm` without
  `--no-refund`) is of the same game or lasts as long: a player under a `user ban` or an
  `integrity confirm --no-refund` gets a ban of their own, which refunds (below);
* sets the integrity level to `confirmed` and appends `{ kind, gameId, at, banUntil }` to
  `evidence.certain` (existing statistical evidence is kept);
* writes a `sanction_auto` security event and tells the lobby (`SanctionEvents`), which closes
  the player's connection (`Notice{Banned}`, then a fatal `Error{Banned}`, close 4004), forfeits
  a game they still play and removes them from the queue and their challenges;
* refunds the player's victims (below).

It is idempotent within a game: several certain anomalies of the same game produce one ban.
Another game is another offence and gets its own ban.

The ban holds from the moment the cheat is detected, before it is written: the sanction first
tells the lobby (`SanctionEvents::sanction_pending`, with the end of the ban being written),
which from then on answers the player's `Hello`s, lobby requests and games as for a ban, kicks
their other connections and forfeits their game. The host or the connection that detected the
cheat calls `AnomalySink::sanction_certain` before it sends the forfeit, the
`Error{CheatDetected}` and the 4302 close, so a client that reconnects at once is refused
(`Notice{Banned}`, 4004). The stored ban replaces this hold; a hold that no new ban replaces (the
write failed, or a ban stood already, which the next `Hello` reads) ends after 60 s.

### Rating refunds

When a player is banned as a cheater, the rating points their opponents lost to them are given
back (`anticheat::refunds`, the store's `refunds`, table `rating_refunds`):

* **When**: an automatic ban for a certain cheat (also when the player was already banned: the
  refunds are idempotent), and a moderator's `integrity confirm` (unless `--no-refund`). A game
  that is not in the database yet when such a ban is given (still in progress, or ended and
  waiting for its commit) is refunded in the transaction that records it, as long as the player
  is `confirmed` and that ban lasts (the store's `finish_batch`; not with
  `RATING_REFUND_DAYS=0`). A `user ban` is not about cheating and refunds nothing, even for a
  player confirmed earlier; `--no-refund` refunds no game at all, the one in progress at the
  confirm included.
* **Which bans**: a ban tells by its source and reason what it was given for
  (`refunds::ban_refunds`): an automatic ban has the source `auto` and the reason `certain_cheat:<kind>`;
  `integrity confirm` writes the reason `confirmed: <reason>`, or `confirmed, no refund: <reason>`
  with `--no-refund`; `user ban` refuses a reason that starts like either.
* **Which games**: the cheater's rated games that ended within `RATING_REFUND_DAYS` (default 60;
  0 turns the automatic refunds off) before the ban, or since the moderator's `--refund-since`.
* **What**: each opponent who lost points in such a game gets exactly those points back, added
  to their **current** rating in that category; their peak rises with it when it is exceeded.
  Nothing is recomputed: the victim's later games, the cheater's other opponents and the counts
  (games, wins, draws, losses) stand as played. A draw that cost points (against a lower-rated
  cheater) is refunded like a loss; a win is left alone; the cheater's own rating is not touched
  (a `confirmed` player is off the leaderboard). Only a change of the K formula is refunded: the
  game that gave a player their first rating moved them from a working rating, which was no
  rating to lose (each game stores the K factor of both changes, 0 when the K formula did not
  apply).
* **Once**: one refund per game and victim at most (`UNIQUE (game_id, victim_id)`): a second
  ban, a retry or a moderator's `refunds apply` only gives the refunds still missing.
* **Audit trail**: every refund is a row (game, victim, cheater, category, points, time, the ban
  that triggered it, `auto` or `moderator` and the moderator's name, when the victim was told)
  plus a `rating_refund` security event; a moderator's command adds its `moderator_action` with
  the totals; the log has one `rating.refund` security line per ban or command that refunds.
* **The victims are told** with `Notice{RatingRestored, arg: points}` (the total of their refunds
  not told yet), out of a game only: at once when they are connected and neither playing nor
  starting a game; otherwise right after their current game ends; otherwise at their next
  connection, right after `Welcome`, unless that connection resumes a game (then after it ends).
  The lobby finds the refunds of an automatic ban at once (the sanction event carries their
  number) and those of an administration command, another process, within 5 s (it polls the
  database). A refund is marked notified only once the notice is queued on the victim's
  connection after its `Welcome`.
* **An unban takes nothing back**, nor does `integrity clear`. The points were lost in games
  against a player found cheating; lifting the ban (its end, leniency, an appeal) does not make
  those games fair. Taking the points back later would hit the victims, who did nothing wrong,
  on ratings that have moved on through honest games since, and they have already been told.
  A mistaken confirmation costs at most the points its victims had lost in the window, which is
  the lesser harm. `refunds list` shows what was given.

A game that ended in the last few tens of milliseconds before an automatic ban may not be in
the database yet (its commit waits up to `DB_COMMIT_MS`): it is refunded when it is recorded,
the player being `confirmed` and under the automatic ban by then.

## 3. Engine analysis

The analysis pool (`anticheat::worker`) runs inside the server process: `ANALYSIS_WORKERS`
loops, each driving one engine, a child process started from `ANALYSIS_ENGINE_PATH` (never
through a shell) at nice 19, with one search thread and `ANALYSIS_HASH_MB` of hash. It does
nothing when `ANALYSIS_ENGINE_PATH` is empty or `ANALYSIS_WORKERS=0`; timing and reports still
work. Each loop claims rated games of at least `ANALYSIS_MIN_PLIES` from the queue (an idle loop
looks again every `ANALYSIS_POLL_MS`), and for each game:

1. **Deep pass**: every position from ply 16 to the end at `ANALYSIS_DEPTH_DEEP`, MultiPV 3, in
   game order with the hash kept between positions. A search is stopped at 25 million nodes and
   repeated from an empty hash: a fixed-depth search can blow up on the hash that the earlier
   positions left. With Stockfish 19 that happened in 3 of the 108 calibration games at 9/15 and
   in 4 at 10/18 (section 4; over 200 million nodes, minutes, on positions it searches in half a
   million from an empty hash), and without the limit the position timeout failed those games.
   Normal deep searches at depth 15 took 0.3 million nodes at the median and 9.2 million at most.
2. **Shallow pass**: every scored position at `ANALYSIS_DEPTH_FAST`, MultiPV 1, with the hash
   cleared before each search (a shallow engine's real choice, not a deep result from the hash).
3. **Moves scored**: all except the first 8 moves of each side (opening theory), moves in
   positions already decided (|eval| > 600 cp from the mover's side) and forced moves (a single
   legal move: MultiPV returns one line).
4. **Per move**: win-probability loss and move accuracy (lichess' logistic model), centipawn
   loss capped at 1000, deep best match (T1), shallow best match, top-3 match, complexity (moves
   within 50 cp of the best among the top 3, gap between the two best, whether the shallow and
   deep choices differ), and the clock time the server charged (`spentMs`).
5. **Per player and game**: accuracy (mean and harmonic mean of move accuracies, averaged, as
   lichess), ACPL, T1 deep %, T1 shallow %, top-3 %, T1 % in complex positions (at least two
   moves within 50 cp), Spearman rank correlation between think time and complexity, coefficient
   of variation of think times, number of scored moves, skipped moves by reason, and a compact
   per-move table for moderators.

Single thread, fixed depth, a node limit (not a time) and controlled hash make the analysis
**reproducible** with the same engine version and network: a moderator can re-run it and get the
same numbers. The CPU does not matter: the sse41, avx2, bmi2, vnni512 and avx512icl builds of
Stockfish 19 and its official binary gave identical features on the same games (another hash
size did not).

**Analysis profile.** Each engine learns, when it starts, the engine's `id name` and the
network(s) it reports on a one-ply search (`info string NNUE evaluation using ...`). Every
analysed game records its profile: engine, network, depths, hash and the version of the rules
above, for example `Stockfish 19; nn-1a298aa575a0.nnue; depth 9/15; hash 32; analysis 2`.
Features of different profiles are not comparable (on the same games, Stockfish 19 at 9/15 finds
the human stand-ins' ACPL 26 % higher than Stockfish 16 at 10/18, and scores 16 % fewer moves), so
the statistics never mix them (section 4).

**Queue policy** (docs/DESIGN.md 6.5). The engine takes the highest priority first, then the
oldest job: a moderator request (priority `manual`, `scacelith-server admin analysis queue <gameId>`,
which the store's `analysis().enqueue` writes), then a game a credible player reported (`cheating`
or `other`,
stored weight 0.5 or more), then a game with a suspicion signal at its end (either player's
integrity level above `none`, an open `cheating` or `other` report of weight 0.5 or more against
either player in the last 30 days, a `suspicious` or `certain` anomaly in that game) or a report
of lower weight, then the ordinary games. The first three are queued whatever the backlog, but
at most 20 flagged games of one player wait at a time. Past that bound, a further game flagged
only through its players (integrity level or report) is skipped, while a game with a
`suspicious` or `certain` anomaly of its own replaces the oldest waiting game of that player
without one (in the same transaction, so the bound stays at 20); it is skipped only when every
waiting game of the player has an anomaly of its own. Quick games flagged only because the
player is flagged can therefore never keep a game with evidence out of the analysis, and a game
with evidence is never removed from the queue. An ordinary game is queued with probability
`ANALYSIS_SAMPLE_RATE` (default 1) and only while fewer than `ANALYSIS_QUEUE_MAX` (default 5000)
ordinary games wait; otherwise it is not analysed (`scacelith_anticheat_analysis_skipped_total`,
where `reason="displaced"` counts the waiting games replaced by a game with evidence). A report
on such a game queues it afterwards.
Every fourth claim takes the oldest ordinary game first, so ordinary games keep at least a
quarter of the engine time however many prioritized games arrive. So a busy server never makes
a suspicious game wait weeks behind ordinary ones, and the population statistics keep being fed
by a steady random sample of ordinary games: only games claimed at ordinary priority feed them
(section 4). The policy changes no level and no sanction.

**Cost.** CPU per position analysed (both passes), over the 38 games of the real-engine test
analysed with the `x86-64-bmi2` build of each engine (the one an OVH vCore runs) on one core of
the development container: Stockfish 19 takes 830 ms at 9/15, 1.23 s at 9/16 and about 2.4 s at
10/18 (1.96 times 9/16), Stockfish 16 0.97 s at its former defaults 10/18 (9/14, tried in the
calibration, took half to 60 % of 9/15 with the official binary). On an OVH vCore that is
about 1.5 s per position at 9/15 (inferred: 1.83 times the container, from the speed factors of
[SIZING.md](SIZING.md)), so a game of 80 plies (65 positions analysed) takes about 100 s of one
core: an engine analyses about 870 games a day, 540 to 1,260 for games of 120 to 60 plies
(inferred). A host with AVX-512 runs a faster build (on the container, 9/16 took 0.90 s instead
of 1.23 s). Raise `ANALYSIS_WORKERS` if `scacelith_anticheat_analysis_queue_ordinary` stays at
`ANALYSIS_QUEUE_MAX` (ordinary games are then being skipped), or lower `ANALYSIS_SAMPLE_RATE` to
analyse a smaller, steadier share of them; lower depths are cheaper too, but restart the
statistics (below). A deep search stopped by the node limit (rare: 4 positions, in the 3
calibration games that had failed at 9/15) costs up to 25 million nodes more, about 50 s of an
OVH vCore (inferred). A job whose engine times out (`ANALYSIS_POSITION_TIMEOUT_MS`) or crashes
is marked failed; a search that times out is first asked to stop, and an engine that does not
answer within 2 s is killed and started again for the next job. A loop never claims a job
without a working engine: an engine that cannot start, or dies, is started again after 1 s,
doubling up to 60 s, and back to 1 s once it ran for 60 s.

### Engine

**Use Stockfish 19**, the official release: it is the engine the anti-cheat is calibrated for.
The priors' accuracy relation, the synthetic engine profile of `anticheat/synthetic.rs`, the
calibration of section 4 and the default depths were all measured with it. Its engines share one
copy of their network (below), and at the default depths it analyses a position for less CPU
than Stockfish 16 did at its former defaults (cost, above). Stockfish 16 or any other UCI engine
still works, but it is not calibrated (its statistics are those of its own profile, and the
detection figures of section 4 do not apply to it) and not shared (every engine loads its own
copy of its network).

The release has one file for Linux x86-64, which holds every x86-64 build and runs the best one
for the CPU (on an OVH vCore, a Haswell with AVX2 and BMI2 but no AVX-512, the `x86-64-bmi2`
build); there is no separate file per CPU to choose. It needs glibc 2.35 or later (Ubuntu 22.04,
Debian 12 or later).

```sh
curl -LO https://github.com/official-stockfish/Stockfish/releases/download/sf_19/stockfish-linux-x86-64-universal.tar.gz
sha256sum stockfish-linux-x86-64-universal.tar.gz
# 9defc0d4e55d49c65a6d042f3e571a39fcea499ade6dbe741b53b8c65e03611f (the archive the calibration used)
tar xzf stockfish-linux-x86-64-universal.tar.gz
sudo install -m 755 stockfish/stockfish-linux-x86-64-universal /usr/local/bin/stockfish-19
stockfish-19 compiler | grep architecture      # the build it picked on this CPU
```

Compare the checksum with the digest the release page shows next to the asset too. Then set
`ANALYSIS_ENGINE_PATH=/usr/local/bin/stockfish-19`. Replace the binary with the server stopped:
running engines and new ones find each other by the path of the executable.

**Shared network.** Stockfish 19 keeps its network (109 MiB) in memory that its processes share
when they run the same executable, load the same network and belong to the same user. The first
engine creates it; the others attach through unix sockets in `/tmp/stockfish-<uid>/`
(always `/tmp`: Stockfish does not read `TMPDIR`). The server starts every engine from
`ANALYSIS_ENGINE_PATH` as the service's user, so they all share, including an engine restarted
after a crash or a timeout (checked with the release binary: one engine of three killed with
SIGKILL, and one stopped until its position timed out, came back attached to the same copy; the
socket a killed engine leaves behind is removed by the next engine that finds it dead). An
engine that cannot use `/tmp/stockfish-<uid>` (a read-only `/tmp`, or a directory of that name
owned by another user or open to others) loads a copy of its own and says why. So `/tmp` must be
writable and the same for all the engines: the example unit of `deploy/systemd/` gives the
service a private writable `/tmp` (`PrivateTmp=yes`, which `ProtectSystem=strict` leaves
writable) shared by the server and its engines; with `ProtectSystem=strict` and no `PrivateTmp`,
add `ReadWritePaths=/tmp`. Memory per engine, with
and without sharing: [SIZING.md](SIZING.md) (about 273 MB for the first engine, then 67 MB per
engine shared and 177 MB unshared).

To check it, look at the engine starts in the log (each start and restart is logged once):
`analysis engine started` with `"network": "shared memory"` is an engine that shares (the first
one too: it created the copy the others use). `analysis engine started with its own copy of the
network` is a warning: several engines run and this one could not share; `why` holds
Stockfish's reason (for instance `Shared memory not supported by the OS. Local allocation
fallback.`). With a single engine (`ANALYSIS_WORKERS=1`) a copy of its own costs nothing more
and is logged as `"network": "local memory"`, and an engine that does not say (Stockfish 16,
for instance) as `"network": "not reported"`. In the metrics,
`scacelith_anticheat_analysis_engines_shared` equals `scacelith_anticheat_analysis_engines` when
every running engine shares.

**Default depths.** `ANALYSIS_DEPTH_FAST=9` and `ANALYSIS_DEPTH_DEEP=15` are chosen for Stockfish
19 (section 4, calibration): of the settings tried (9/14, 9/15, 9/16, 10/18) it is the best
overall (the others flag engine users later in most cases), and on an OVH vCore it costs 14 %
less than Stockfish 16 at its former defaults 10/18 (9/16 would cost 27 % more, 10/18 about 2.5
times as much). It flags engine users assisted by Stockfish 19 as early as Stockfish 16 at 10/18
did or earlier from 2000 up, and under a game later below. Against weaker or older assistance (a
depth-12 search, Stockfish 16) Stockfish 16 at 10/18 flagged them up to 5 games earlier, and no
Stockfish 19 setting closes that gap: 9/15 leaves the smallest.

**Changing the engine, its network, a depth or `ANALYSIS_HASH_MB` restarts the statistics**: the
population of the new profile starts from the priors, and every player is scored on their games
of the new profile only, keeping the level they had until five of them are analysed. Plan such a
change; going back to an earlier profile finds its statistics as they were.

## 4. Statistical model (`anticheat::scoring`, `anticheat::priors`)

### Population statistics

For every (analysis profile, category, 100-point rating bucket) the analysis pool keeps
Welford statistics (n, mean, M2) of each per-game metric of rated games. Until a bucket has its
own data, **priors** stand in: hard-coded means and per-game standard deviations by rating
(accuracy, ACPL, T1 by rating from published human data: lichess accuracy/ACPL statistics and
Regan & Haworth / Guid & Bratko engine-matching studies; accuracy derived from the ACPL row
through the relation our pipeline measures, accuracy ~ 100 - 0.28 ACPL up to an ACPL of about
90, flattening above: about 71 at 110 and 65 at 150, the ACPL priors at 1000 and 600; the same
with Stockfish 19 at every depth tried and with Stockfish 16, and a unit test keeps the accuracy
row on it), adjusted by time class (bullet, blitz, rapid, classical: faster games are less
accurate). The prior counts as 40 games and its standard
deviations are inflated by 25%, so a young server is deliberately cautious; the server's own
data takes over bucket by bucket. Only the games of the random sample feed the population (the
jobs claimed at ordinary priority, drawn with `ANALYSIS_SAMPLE_RATE`): reported, flagged and
moderator-requested games are analysed first and in full, so counting them would shift the
baseline towards the very players it judges. Values entering the population are winsorised at
4 sd, and games of players already `high_confidence` or `confirmed` are left out (cheaters must
not make cheating look normal).

**One profile at a time.** A game joins the population of its own analysis profile (section 3),
and a player is judged on their games of the profile of the game just analysed, against that
profile's population: games analysed by another engine, network, depths or hash never enter the
same statistics or the same score. After a change of profile the new population starts from the
priors, and a player whose recent games are of the earlier profile keeps their level until five
games of the new one can be judged (`integrity show` marks the games of another profile). The
statistics of an earlier profile stay in the database, unused.

### Scores

Each metric of a game becomes an oriented z-score (positive = more engine-like) against the
population of its category at the player's rating. The rating is uncertain, so the z-score is
the **smallest over the rating band** (+/-100, +/-400 while the player's rating in the category
rests on fewer than 30 counted games, the games that entered it: none while it is unrated,
however many games were played): smurfs, returning players and fast improvers get the benefit
of the doubt.

Metrics are grouped into signals of different nature:

| signal | content | type |
|---|---|---|
| Q | mean of z(accuracy) and z(-ACPL) | accuracy |
| E | z(T1 in complex positions), games with at least 3 complex positions | accuracy |
| J | recent games vs the player's own earlier games (sudden lasting jump) | accuracy (self-referenced) |
| T | mean of z(-time/complexity correlation) and z(-CV of think times) | timing |

A group's score over a window is the moves-weighted mean z, multiplied by the empirical-Bayes
shrinkage n/(n+k) (n = scored moves, k = 150; for E n = complex positions, k = 50), divided by
tau, the spread between honest players' long-run levels in per-game sd units (Q 0.4, E 0.45,
T 0.5). A score of 3 means "the player's estimated long-run level is 3 between-player standard
deviations above peers of the same rating and time control". Under the null (an ordinary honest
player) its variance is n/(n+k) < 1, so the thresholds are conservative. Windows: the last 30
analysed games and the last 10; a group takes the higher of the two.

J compares the last 10 games' quality with the up to 20 before: t = (recent mean - earlier mean
- 0.75) / se. The 0.75 per-game sd is a plausible honest improvement that is not evidence; the
jump must also last (70% of the recent games above the old level) and land above peers (recent
mean z >= 1).

### Levels

| level | rule |
|---|---|
| `suspected` | >= 5 games and max(Q, E, J) >= 3.5; or J >= 2.5 together with a recent-window Q or E >= 2.5 |
| `high_confidence` | in one window: >= 10 games, >= 300 scored moves, max(Q, E, J) >= 3.0, T >= 1.5 and (A + T)/sqrt(2) >= 3.5 |
| `confirmed` | never automatic: moderators (`integrity confirm`) and certain protocol cheats |

Why: Q, E and J all measure how good the moves are, so they are not independent; timing is.
High confidence therefore needs the move-quality evidence and the timing evidence to agree.
Timing alone never flags anyone (lag, premoves, style). Memory rules: `suspected` stays until
the score falls 0.5 below its threshold (no flapping), `high_confidence` never falls below
`suspected` without a moderator, and a player a moderator cleared is only flagged again on new
evidence (high confidence, or a score 1.0 above the cleared one with at least 5 new games).

The stored evidence (`evidence.statistics`) holds the rule that triggered, the group scores
per window, the jump details and human-readable reasons with the raw numbers (the player's
weighted accuracy, ACPL, T1, timing against what peers of the same rating show);
`evidence.peak` keeps the highest score ever seen. Players with no level and a score under 2
only get a compact summary (scores, games, moves). The per-game numbers stay in the analysis
rows, where `integrity show` reads them.

### Calibration and validation

* **Stockfish 19 against Stockfish 16, per setting.** A synthetic calibration (the model of
  `crates/server/src/anticheat/synthetic.rs`: honest players drawn from the population with personal
  offsets, 1 % strongly underrated, personal timing styles) was made for each analysis setting from
  the same games analysed by each: 16 games of a player assisted by Stockfish 19 at depth 20,
  deeper than any analysis (set C), the same games assisted by Stockfish 16 (set C16), and the 38
  games each of the real-engine test generated by Stockfish 19 (set A) and by Stockfish 16 (set B),
  whose assisted player moves at depth 12 and whose human stand-ins give the honest players. Engine
  users take the per-game values each setting measures on an assisted player; honest players are
  the priors, moved by how each setting measures the stand-ins compared with Stockfish 16 at 10/18
  (at 9/15, over sets A and B, Stockfish 19 finds their ACPL 25 % higher and scores 14 % fewer
  moves: it calls more positions decided); every setting draws the same random numbers; games a
  setting could not analyse are left out of all of them. Mean number of games until an engine user
  assisted by Stockfish 19 (set C) is flagged, 1000 per cell, with learned statistics / with priors
  only (a fresh server); ">30": most are not flagged within 30 games:

  | Rating, time control | Stockfish 16 10/18 | Stockfish 19 9/14 | **9/15** | 9/16 | 10/18 |
  |---|---|---|---|---|---|
  | 1000, 5+0 | 8.6 / 13.8 | 9.7 / 15.3 | 8.9 / 14.1 | 9.2 / 14.9 | 9.1 / 15.0 |
  | 1000, 15+10 | 9.0 / 15.9 | 10.0 / 17.5 | 9.0 / 16.1 | 10.0 / 15.9 | 10.0 / 16.6 |
  | 1500, 5+0 | 11.0 / 22.9 | 12.0 / 24.2 | 11.0 / 21.6 | 12.0 / 20.6 | 12.0 / 22.4 |
  | 1500, 15+10 | 12.7 / 25.9 | 14.1 / 26.0 | 13.4 / 23.0 | 14.0 / 21.8 | 14.0 / 23.8 |
  | 2000, 5+0 | 14.9 / >30 | 15.4 / >30 | 14.1 / 29.1 | 14.1 / 27.3 | 14.6 / >30 |
  | 2000, 15+10 | 16.3 / >30 | 16.5 / >30 | 14.6 / >30 | 14.4 / >30 | 15.4 / >30 |
  | 2400, 5+0 | 18.7 / >30 | 18.7 / >30 | 16.0 / >30 | 15.6 / >30 | 17.0 / >30 |
  | 2400, 15+10 | 20.8 / >30 | 20.4 / >30 | 17.0 / >30 | 17.5 / >30 | 18.6 / >30 |

  With every setting and every kind of engine user: no honest player flagged (3000 with learned
  statistics and 1500 with priors only, each checked at 5, 10, 20 and 30 games), and at most 0.6 %
  of honest players improving by one per-game sd within 10 games suspected. Against Stockfish 19's
  assistance, 9/15 flags engine users from 2000 up as early as Stockfish 16 at 10/18 or earlier
  (2.7 and 3.8 games earlier at 2400), and at 1000 and 1500 under a game later (fewer scored moves
  per game); with priors only it is ahead at 1500 and 0.2 to 0.4 game behind at 1000. Against weaker
  or older assistance Stockfish 16 at 10/18 stays ahead: against the depth-12 assisted player of
  sets A and B, 9/15 needs up to 1.8 more games with learned statistics at 1000 to 2000 (3 to 5
  more with priors only), and against Stockfish 16's own depth-20 assistance 1.7 to 4.8 more (an
  analysis engine likely recognises its own moves best). No Stockfish 19 setting matches Stockfish 16 at
  10/18 in every case, and the deeper ones do worse, not better (against Stockfish 16's assistance
  at 2400 rapid, 9/16 and 10/18 need 6.9 and 10.2 more games), while 9/14 loses the lead against
  Stockfish 19's assistance. 9/15 falls the least behind of them against every kind of engine user,
  and costs less than Stockfish 16 at 10/18 (section 3). A second draw of the random numbers moved
  the difference between 9/15 and Stockfish 16 by up to 1.2 games in a cell (2000 blitz: 0.8 game
  earlier, then 0.4 later).
* Real engine (`anticheat/worker/tests/real_engine.rs` with `SCACELITH_TEST_ENGINE` set to the
  official Stockfish 19 binary or its `x86-64-bmi2` build, which give the same numbers; analysis
  at 6/10): an assisted player (depth 12 best move, relayed with 2-5 s delays) against human
  stand-ins (random plausible moves among the top 4 at low depth, thinking longer on harder
  moves). Assisted: accuracy 98.2, ACPL 7, T1 0.69, time/complexity correlation 0.02, time CV
  0.25; stand-ins: 80.1, 73, 0.28, 0.21, 0.82. With the server's own statistics (the stand-ins'
  games), the assisted player is `none` for the first 11 games and `high_confidence` from the
  12th; the stand-ins are never flagged. On a fresh server (priors only) the same 18 games reach
  `high_confidence` (Q 3.32, T 1.75). With Stockfish 16 (its own games): assisted 98.3, 6, 0.69,
  -0.04, 0.26, stand-ins 80.4, 67, 0.26, 0.19, 0.87; `suspected` at the 5th game and
  `high_confidence` from the 12th with the server's statistics, and `high_confidence` after 18
  games on a fresh server (Q 3.36, T 1.86).
* Production depths (sets A and B, analysed at hash 32 by each setting; set B without its two
  games that blew up a deep search, section 3): with statistics learned from the stand-ins, the
  assisted player reaches `high_confidence` at the 13th game of set A with Stockfish 19 at 9/15,
  9/16 and 10/18 (the 12th at 9/14) and the 14th with Stockfish 16 at 10/18; on set B at the 14th
  at 9/15, the 15th at 9/14, 9/16 and 10/18, and the 13th with Stockfish 16. The stand-ins' scores
  stay under 0.6 with Stockfish 19 (at most 0.95 with Stockfish 16). Effect sizes
  of the assisted player against the stand-ins at 9/15 (accuracy, ACPL, T1, T1 in complex
  positions, time CV): 3.60, -2.74, 3.04, 2.50, -5.06 on set A and 3.79, -4.00, 3.15, 1.86, -4.32
  on set B; with Stockfish 16 at 10/18, 3.83, -2.99, 4.19, 3.81, -4.17 and 3.56, -3.94, 3.51,
  2.79, -5.09. On a fresh server only Stockfish 16 on set A flags the assisted player within its
  18 games (Q 3.31); the others stay under the thresholds (Q 2.76 to 3.20).

Known limits: players using the engine for a minority of their moves (about half or less) look
like strong humans and are not flagged by statistics within 30 games; top players (2400+) have
little room between their own accuracy and an engine's; a fresh server is slow to flag; engine
users assisted by an older or weaker engine than the analysis are flagged a few games later
(above). Reports and moderators' judgement cover these cases.

## 5. Reports

`POST /api/v1/reports` (authenticated) `{ gameId, reported, category: cheating|abuse|other,
comment? }`:

* allowed only for the opponent of one of the reporter's own games that ended within 7 days
  (the opponent is found through the game, never by a username lookup, so nothing can be learnt
  about other accounts); otherwise `403 report_not_allowed`;
* `REPORTS_PER_DAY` per reporter (`429 report_limit`); comments up to 500 characters, control
  characters removed;
* one report per (reporter, reported, game): a duplicate gets the same `202 { status: 'received' }`
  as a new report, and nothing in the answer depends on the reported account;
* a `cheating` or `other` report queues the engine analysis of the reported game ahead of the
  ordinary games (section 3), even when the queue policy had left the game out: at report
  priority when the report is credible (stored weight 0.5 or more), otherwise only at the
  priority of a suspicion signal, so that reports from new accounts cannot push the games of
  statistically suspected players back; this only produces evidence for moderators.

Reporter credibility (0.02 .. 2): `base = 0.1 + 0.9 sqrt(age x games)` with age = min(1, days/30)
and games = min(1, games played/50) (both needed: fresh account farms and idle old accounts
stay low); multiplied by the track record `2 (actioned + 1) / (actioned + dismissed + 2)`
(clamped 0.25 .. 1.75, 1.0 without history); halved for a reporter flagged `high_confidence`, a
fifth for a `confirmed` cheater. The weight stored for a report is capped so that all reports a
player receives in 24 hours sum to at most 2.0, and low-credibility reports (< 0.5) to at most
0.5: ten sock puppets weigh as much as half a credible report. Every report is still kept for
moderators.

Review priority (computed when listing, `anticheat::reports`): level base (suspected 40,
high confidence 70) + min(20, 4 x score) + 15 log2(1 + report weight of the last 30 days).

## 6. For moderators (`scacelith-server admin`)

Run on the server host, as the server's account and with its configuration (docs/DEPLOY.md,
section 3); the commands work on the database directly and may run while the server runs.
`scacelith-server admin --help` lists them. Every action is recorded (`moderator_action`
security event with the moderator's name: `--by NAME`, `SCACELITH_MODERATOR`, `SUDO_USER` or the
OS user; plus `reviewed_by` / `created_by`); `--json` prints machine-readable output.

```
scacelith-server admin integrity list [--level suspected|high_confidence|confirmed] [--limit N]   # by review priority
scacelith-server admin integrity show <name>        # evidence, per-game features, anomalies, reports
scacelith-server admin integrity confirm <name> --reason TEXT [--hours N] [--keep-reports] [--refund-since DATE | --no-refund]
                                                    # confirmed + ban + rating refunds, cheating reports -> actioned
scacelith-server admin integrity clear <name> [--reason TEXT] [--dismiss-reports]
scacelith-server admin refunds apply <name> [--since DATE]   # refunds of a confirmed cheater (window: from the latest ban for cheating)
scacelith-server admin refunds list [<name>] [--victim NAME] [--limit N]
scacelith-server admin analysis queue <gameId>      # analysed before every other game, unless running or done
scacelith-server admin reports list [--limit N] | reports resolve <id> actioned|dismissed
scacelith-server admin anomalies <name> [--limit N] | stats
scacelith-server admin user show|ban|unban|reset-mfa|verify-email|revoke-sessions <name>
```

What to look at in `integrity show`:

1. **Which rule triggered** and whether several independent signals agree (Q/E with T is much
   stronger than Q alone).
2. **The per-game table**: is the high accuracy spread over many games or a few? Do T1 and
   accuracy stay high in complex positions (`cx%`)? Engine users are consistently near-perfect;
   strong humans have bad games too. A game marked `*` was analysed with another profile than
   the evidence (named under it) and is not part of the scores.
3. **Timing**: humans think longer in complex positions (`time~cx` positive) and vary a lot (CV
   around 1); a relay shows no correlation and a low CV. Bullet and bad connections flatten
   timing too, so timing is only corroboration.
4. **History and context**: rating trajectory, account age, a jump that coincides with a new
   account or a long break, reports from credible reporters, protocol anomalies.
5. When in doubt, replay a few games with an engine yourself (the analysis is reproducible with
   the engine, network, depths and hash of its profile) and compare with the player's
   over-the-board style. `clear` records your review: the automatic
   model does not re-flag on the same evidence.

A ban from the command line is written to the database only (the command does not talk to the
running server): the server applies it (disconnection, close 4004) when the player next connects
or tries to start a game (queue, challenge, private code, rematch), so a banned player starts no
game; a game in progress at the ban plays on, and after an `integrity confirm` without
`--no-refund` the points its opponent loses in it are refunded when it is recorded (never after a
`user ban`, which is not about cheating). `--revoke-sessions` also logs them out (the server's
session cache drops the revoked sessions within 30 s). Its rating refunds are in the database at
once; the running server tells the victims within 5 s (or later, out of a game).

Refunds: `integrity confirm` refunds the games of the last `RATING_REFUND_DAYS` by default;
`--refund-since DATE` (`YYYY-MM-DD`, UTC, or an ISO 8601 time with its offset) sets another
start, for a player who cheated longer, among the games already recorded; a game recorded after
the confirm while that ban lasts (one that was in progress) is refunded when it is recorded.
`--no-refund` gives none, that game included (for example when the evidence covers only some
games: refund them later with `refunds apply --since`); the ban's reason records the choice
(`confirmed: <reason>` or `confirmed, no refund: <reason>`), and a `user ban` cannot use either
prefix. `refunds apply` works on a `confirmed` player only, and its default window counts back
from the latest ban for cheating (automatic or `integrity confirm`), not from a later `user ban`.
DATE cannot be in the future. The options are checked before anything is written; the refunds
run in their own transaction after the ban, and if they fail (a database error) the ban stands,
its `moderator_action` records the error, and the command exits with an error that gives the
`refunds apply` to run.

Pending signups: with `REQUIRE_EMAIL_VERIFICATION` a registration has no account until the link
mailed to its address is used (the signup holds its username for 24 h). When that mail never
arrives, `user show <name>` shows the pending signup that holds the name (address, times, whether
it has a link), and `user verify-email <name>` does what the link would: the account is created,
its address verified (`moderator_action` `confirm_signup`). A signup without a link (its address
had an account, whose owner got a notice instead), or one whose username or address another
account has now, is dropped and nothing is created.

## 7. False positives: what the model does about them

* Strong players have high accuracy: z-scores are relative to peers of the same rating and time
  control, never absolute.
* Ratings are uncertain: the most favourable bucket of the rating band is used (wider while
  provisional).
* Short time controls lower human accuracy: statistics per category, priors per time class.
* Small samples: shrinkage by scored moves, minimum 5 games (10 games and 300 moves for high
  confidence), games with fewer than 8 scored moves ignored, per-game z clamped at 6.
* Easy games: opening, forced and decided positions are not scored.
* Improvement: the jump test allows a 0.75 sd improvement and requires it to last.
* One lucky game: windows and "lasting" rules; one game cannot weigh more than its moves.
* Correlated metrics are not counted twice: high confidence needs independent kinds of evidence.
* Contamination: only the random sample of ordinary games feeds the population (flagged and
  reported games, analysed first and in full, never do), players already `high_confidence` or
  `confirmed` are left out of it too, and outliers are winsorised.
* Brigading: capped report weights; reports never change the level.
* Nothing statistical is automatic: a human decides.

## 8. Data retained

| Data | Where | Retention |
|---|---|---|
| Anomalies (kind, severity, game, detail, time) | table `anomalies` | `info` and `suspicious` ones deleted after `RETENTION_SECURITY_DAYS` by the retention purge (hourly by default); `certain` ones kept |
| Sanctions (ban, reason, source, game, moderator, lift) | table `sanctions` | kept |
| Integrity level, score, evidence (statistics, peak, certain cheats, reviews) | table `player_integrity` | kept while the account exists |
| Per-game features of analysed games (numbers, compact per-move table; no positions beyond the game's own moves) | table `analysis_jobs` | kept with the game (the scoring and `integrity show` read a player's latest analysed games, however old) |
| Failed analysis jobs (no features, an error message) | table `analysis_jobs` | deleted 30 days after the failure by the retention purge |
| Population statistics (n, mean, M2 per metric, analysis profile, category and rating bucket; no personal data) | table `population_stats` | kept, those of an earlier analysis profile too (unused) |
| Reports (reporter, reported, game, category, comment, weight, outcome, moderator) | table `reports` | kept; comments are only shown to moderators |
| Moderator actions | table `security_events` (`moderator_action`) | security retention |
| Rating refunds (game, victim, cheater, category, points, time, ban or moderator, notified) | table `rating_refunds` | kept |
| Refund events | table `security_events` (`rating_refund`) | security retention |

No IP address is stored by this module (reports and moderator events carry `ip: null`).

## 9. Code map

| Module (`crates/server/src/`) | Content |
|---|---|
| `anticheat/service.rs` | anomaly classification and recording, the automatic sanction of a certain cheat (`Anticheat`, the `AnomalySink` of the hosts and the connections) |
| `anticheat/sanction.rs` | the database side of an automatic ban (one store write job) |
| `anticheat/refunds.rs`, `anticheat/notices.rs` | rating refunds, and their `Notice{RatingRestored}` sent by the lobby |
| `anticheat/analysis/` | the UCI engine driver (`engine.rs`) and the analysis of one game into per-player features (`analyzer.rs`) |
| `anticheat/worker.rs` | the analysis pool: one loop per engine, claims, results, integrity and population updates |
| `anticheat/priors.rs`, `anticheat/scoring.rs`, `anticheat/integrity.rs`, `anticheat/players.rs` | the statistical model of section 4 and the memory rules of the levels |
| `anticheat/reports.rs`, `http/routes/reports.rs` | reports: eligibility, credibility, weight caps, review priority, and `POST /api/v1/reports` |
| `anticheat/admin/` | `scacelith-server admin` |
| `store/analysis.rs`, `store/integrity.rs`, `store/reports.rs`, `store/refunds.rs`, `store/moderation.rs` | the tables of this module and their queries (the analysis queue policy runs in the game commit, `store/commit.rs`) |

The pure parts (engine driver, analyzer, priors, scoring, integrity rules) hold no store and no
socket; the store-side services run them inside their store jobs, so a read-modify-write of an
integrity record is one transaction on the writer thread.
