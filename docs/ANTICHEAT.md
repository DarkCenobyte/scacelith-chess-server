# Anti-cheat, sanctions, reports and moderation

This server separates three things that are often mixed up:

| | What | Automatic action |
|---|---|---|
| **Certain protocol cheats** | A client did something an unmodified game client cannot do (forged message type, illegal move or move out of turn in a synchronised position, message for a game it does not play). | Yes: forfeit, disconnection, ban of `BAN_DURATION_HOURS`, integrity level `confirmed`, rating refunds of the player's victims. |
| **Statistical suspicion** | Engine analysis of finished rated games says a player's moves and timing look like engine assistance. | **Never.** The integrity level and the evidence are written for a moderator. No ban, no matchmaking change. A moderator who confirms it bans the player, which refunds the victims too. |
| **Reports** | Players report opponents of their recent games. | **Never.** They raise the review priority, weighted by the reporter's credibility. |

Code: `src/anticheat/` (`index.js` anomalies and sanctions, `sanction.js` the database side of
an automatic ban, `refunds.js` + `refund-notices.js` rating refunds, `analysis/` engine and
features, `scoring.js` + `priors.js` model, `reports.js`, `admin.js`),
`src/http/routes/reports.js`, `bin/analysis-worker.js`, `bin/admin.js`.

## 1. Anomalies (protocol)

The router and the game host report anomalies with `ac.recordAnomaly({ userId, gameId, kind,
detail, posMatched })`. Severities (DESIGN 6.5):

| kind | severity | when | notes |
|---|---|---|---|
| `forged_type` | certain | a server->client message type sent by a client | |
| `foreign_game` | certain | game message for a game the player does not play | |
| `out_of_turn` | certain | move in a synchronised position while it is not the player's turn | downgraded to suspicious if reported with `posMatched: false` |
| `illegal_move` | certain | illegal move in a synchronised position | same |
| `malformed` | suspicious | undecodable frame after Hello (connection closed 4300) | |
| `bad_seq` | suspicious | seq not last+1 | |
| `flood` | suspicious | rate limit exceeded repeatedly (connection closed 4301) | |
| `repeated_desync` | suspicious | 3+ desyncs in one game (the room counts them) | |
| `clock_implausible` | suspicious | `thinkMs` physically impossible | never certain: clock drift, suspended VMs |
| `stale_ply`, `desync`, `nothing_to_claim` | info | honest races and bugs | not security-logged |
| anything else | info | caller bug | recorded as `unknown` with the original kind in the detail |

Storage: certain anomalies are written at once; the others are buffered and written in one
`insertBatch` per second, repeats of the same kind in the same game coalesced into one row with
`count` and `lastAt` (a flooding client cannot flood the database). When the buffer holds a
suspicious anomaly, a shard also writes it right before it commits finished games, so that the
analysis queue policy of the commit sees it. In a shard these writes, like the automatic
sanctions, go through the store writer thread that commits the finished games
(`src/store/writer.js`, its own SQLite connection): the event loop never waits for the disk or
the database lock, and the thread writes its messages in the order they were sent (anomalies
flushed before a commit are written before it, a certain anomaly before the ban it causes). A
batch the thread could not write is counted as lost like any other. The buffer is bounded (5000
rows; info rows are dropped first) and losses are counted in
`scacelith_anticheat_anomalies_dropped_total`. Metrics: `scacelith_anticheat_anomalies_total{kind,severity}`.
Suspicious and certain anomalies are logged with `log.security('anomaly', ...)`.

## 2. Automatic sanctions

With `AUTO_SANCTION_CERTAIN_CHEATS=true` the host ends the game (`Forfeit`) and calls
`ac.sanctionCertain({ userId, gameId, kind })`, which:

* creates a ban of `BAN_DURATION_HOURS` (`source: 'auto'`, reason `certain_cheat:<kind>`, the game id);
* sets the integrity level to `confirmed` and appends `{ kind, gameId, at, banUntil }` to
  `evidence.certain` (existing statistical evidence is kept);
* writes a `sanction_auto` security event and asks the primary to kick the player everywhere
  (`sanction.applied`);
* refunds the player's victims (below).

It is idempotent within a game: several certain anomalies of the same game (even reported by
different shards) produce one ban. Another game is another offence and gets its own ban. In a
shard the database work runs on the store writer thread (section 1).

### Rating refunds

When a player is banned as a cheater, the rating points their opponents lost to them are given
back (`refunds.js`, the store's `refunds`, table `rating_refunds`):

* **When**: an automatic ban for a certain cheat (also when the player was already banned: the
  refunds are idempotent), and a moderator's `integrity confirm` (unless `--no-refund`). A
  `user ban` is not about cheating and refunds nothing. A game that is not in the database yet
  when the ban is given (still in progress, or ended and waiting for its commit) is refunded in
  the transaction that records it, as long as the player is `confirmed` and banned (the store's
  `games.finishBatch`; not with `RATING_REFUND_DAYS=0`).
* **Which games**: the cheater's rated games that ended within `RATING_REFUND_DAYS` (default 60;
  0 turns the automatic refunds off) before the ban, or since the moderator's `--refund-since`.
* **What**: each opponent who lost points in such a game gets exactly those points back, added
  to their **current** rating in that category; their peak rises with it when it is exceeded.
  Nothing is recomputed: the victim's later games, the cheater's other opponents and the counts
  (games, wins, draws, losses) stand as played. A draw that cost points (against a lower-rated
  cheater) is refunded like a loss; a win is left alone; the cheater's own rating is not touched
  (a `confirmed` player is off the leaderboard). Only a change of the K formula is refunded: the
  game that gave a player their first rating moved them from a working rating, which was no
  rating to lose (the K factor of each game is stored since migration 004; the games finished
  before it were all K-formula changes and are refunded when they cost points).
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
  The primary finds the refunds of an automatic ban at once (`sanction.applied` carries their
  number) and those of an admin command within 5 s. A refund is marked notified only once the
  shard reports the notice written on the victim's connection.
* **An unban takes nothing back**, nor does `integrity clear`. The points were lost in games
  against a player found cheating; lifting the ban (its end, leniency, an appeal) does not make
  those games fair. Taking the points back later would hit the victims, who did nothing wrong,
  on ratings that have moved on through honest games since, and they have already been told.
  A mistaken confirmation costs at most the points its victims had lost in the window, which is
  the lesser harm. `refunds list` shows what was given.

A game that ended on another shard in the last few tens of milliseconds before an automatic ban
may not be in the database yet (its commit waits up to `DB_COMMIT_MS`): it is refunded when it
is recorded, the player being `confirmed` and banned by then.

## 3. Engine analysis

`startAnalysisProcess(config)` (primary) forks `bin/analysis-worker.js` at low CPU priority and
restarts it with exponential backoff (1 s .. 60 s) if it dies. It does nothing when
`ANALYSIS_ENGINE_PATH` is empty or `ANALYSIS_WORKERS=0`; timing and reports still work. The
worker runs `ANALYSIS_WORKERS` engines (one thread each, `ANALYSIS_HASH_MB` of hash, low
priority), claims rated games of at least `ANALYSIS_MIN_PLIES` from the queue, and for each game:

1. **Deep pass**: every position from ply 16 to the end at `ANALYSIS_DEPTH_DEEP`, MultiPV 3.
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

Single thread, fixed depth and controlled hash make the analysis **reproducible** with the same
engine version and network: a moderator can re-run it and get the same numbers. The CPU does not
matter: the sse41, avx2, bmi2, vnni512 and avx512icl builds of Stockfish 19 and its official
binary gave identical features on the same games (another hash size did not).

**Analysis profile.** Each engine learns, when it starts, the engine's `id name` and the
network(s) it reports on a one-ply search (`info string NNUE evaluation using ...`). Every
analysed game records its profile: engine, network, depths, hash and the version of the rules
above, for example `Stockfish 19; nn-1a298aa575a0.nnue; depth 9/15; hash 32; analysis 1`.
Features of different profiles are not comparable (on the same games, Stockfish 19 at 9/15 finds
the human stand-ins' ACPL 26 % higher than Stockfish 16 at 10/18, and scores 16 % fewer moves), so
the statistics never mix them (section 4).

**Queue policy** (docs/DESIGN.md 6.5). The engine takes the highest priority first, then the
oldest job: a moderator request, then a game a credible player reported (`cheating` or `other`,
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
10/18 (1.96 times 9/16), Stockfish 16 0.97 s at its former defaults 10/18. On an OVH vCore that is
about 1.5 s per position at 9/15 (inferred: 1.83 times the container, from the speed factors of
[SIZING.md](SIZING.md)), so a game of 80 plies (65 positions analysed) takes about 100 s of one
core: an engine analyses about 870 games a day, 540 to 1,260 for games of 120 to 60 plies
(inferred). A host with AVX-512 runs a faster build (on the container, 9/16 took 0.90 s instead
of 1.23 s). Raise `ANALYSIS_WORKERS` if `scacelith_anticheat_analysis_queue_ordinary` stays at
`ANALYSIS_QUEUE_MAX` (ordinary games are then being skipped), or lower `ANALYSIS_SAMPLE_RATE` to
analyse a smaller, steadier share of them; lower depths are cheaper too, but restart the
statistics (below). A job whose engine times out (`ANALYSIS_POSITION_TIMEOUT_MS`) or crashes is
marked failed and the engine restarted; an engine that cannot start makes the worker wait
(5 s .. 5 min) without claiming jobs.

### Engine

Install the official **Stockfish 19** release for Linux x86-64: one file that picks the best
build for the CPU when it starts (on an OVH vCore, a Haswell without AVX-512, the `x86-64-bmi2`
build) and needs glibc 2.35 or later (Ubuntu 22.04, Debian 12 or later).

```sh
curl -LO https://github.com/official-stockfish/Stockfish/releases/download/sf_19/stockfish-linux-x86-64-universal.tar.gz
sha256sum stockfish-linux-x86-64-universal.tar.gz
# 9defc0d4e55d49c65a6d042f3e571a39fcea499ade6dbe741b53b8c65e03611f (the archive the calibration used)
tar xzf stockfish-linux-x86-64-universal.tar.gz
sudo install -m 755 stockfish/stockfish-linux-x86-64-universal /usr/local/bin/stockfish-19
stockfish-19 compiler | grep architecture      # the build it picked on this CPU
```

Compare the checksum with the digest the release page shows next to the asset too. Then set
`ANALYSIS_ENGINE_PATH=/usr/local/bin/stockfish-19`. An engine takes about 270 MB, 110 MB more
than Stockfish 16 (its network loaded is 109 MiB); a second one (`ANALYSIS_WORKERS=2`) adds about
70 MB, because the engines of one user share a single copy of the network through a socket under
`/tmp` (the unit's `PrivateTmp` keeps it private to the service). The defaults
(`ANALYSIS_DEPTH_FAST=9`, `ANALYSIS_DEPTH_DEEP=15`) are chosen for it: on an OVH vCore they cost
14 % less than Stockfish 16 at the former defaults 10/18 (9/16 would cost 27 % more), and they
separate the assisted player of the real-engine test from the human stand-ins as well as 9/16
and 10/18 do (section 4, calibration). Stockfish 16 or any other UCI engine still works; the
statistics are then those of its own profile.

**Changing the engine, its network, a depth or `ANALYSIS_HASH_MB` restarts the statistics**: the
population of the new profile starts from the priors, and every player is scored on their games
of the new profile only, keeping the level they had until five of them are analysed. Plan such a
change; going back to an earlier profile finds its statistics as they were.

## 4. Statistical model (`scoring.js`, `priors.js`)

### Population statistics

For every (analysis profile, category, 100-point rating bucket) the analysis process keeps
Welford statistics (n, mean, M2) of each per-game metric of rated games. Until a bucket has its
own data, **priors** stand in: hard-coded means and per-game standard deviations by rating
(accuracy, ACPL, T1 by rating from published human data: lichess accuracy/ACPL statistics and
Regan & Haworth / Guid & Bratko engine-matching studies; accuracy derived from the ACPL row
through the relation our pipeline measures, accuracy ~ 100 - 0.275 ACPL up to an ACPL of about
90, flattening above: about 74 at 110, 70 at 150), adjusted by time class (bullet, blitz, rapid,
classical: faster games are less accurate). The prior counts as 40 games and its standard
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
statistics of an earlier profile stay in the database, unused; the migration that introduced
the profiles (`analysis_profiles`) deleted those written before them, since nothing tells which
engine produced them.

### Scores

Each metric of a game becomes an oriented z-score (positive = more engine-like) against the
population of its category at the player's rating. The rating is uncertain, so the z-score is
the **smallest over the rating band** (+/-100, +/-400 while the player has fewer than 30 games in
the category): smurfs, returning players and fast improvers get the benefit of the doubt.

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

* Synthetic populations (`src/anticheat/testing/synthetic.js`, honest players drawn from the
  population with personal offsets, 1% strongly underrated, personal timing styles): no honest
  player flagged (3000 players with learned statistics, 1500 with priors only, each checked at
  5, 10, 20 and 30 games); honest players improving by one per-game sd within 10 games: about 1%
  suspected. Full engine users (profile measured with Stockfish 19): with learned statistics
  flagged after a median of 11 to 13 games at 1000 and 1500, 15 (blitz) to 19 (rapid) at 2000
  and 24 at 2400 blitz (2400 rapid: 5 % within 30 games); with priors only (fresh server) after
  18 games at 1000, 24 to 26 at 1500, and mostly not within 30 games above.
* Real engine (`test/unit/anticheat.engine.test.js` with `SCACELITH_TEST_ENGINE` set to the
  official Stockfish 19 binary, analysis at 6/10): an assisted player (depth 12 best move,
  relayed with 2-5 s delays) against human stand-ins (random plausible moves among the top 4 at
  low depth, thinking longer on harder moves). Assisted: accuracy 98.2, ACPL 7, T1 0.69,
  time/complexity correlation 0.02, time CV 0.25; stand-ins: 80.1, 73, 0.28, 0.21, 0.82. With
  the server's own statistics (the stand-ins' games), the assisted player is `none` for the
  first 11 games and `high_confidence` from the 12th; the stand-ins are never flagged. On a
  fresh server (priors only) the same 18 games stay just under the thresholds (Q 3.17, T 1.75):
  the intended caution of the inflated priors. With Stockfish 16 (its own games): assisted 98.3,
  6, 0.69, -0.04, 0.26, stand-ins 80.4, 67, 0.26, 0.19, 0.87; `suspected` at the 5th game and
  `high_confidence` from the 12th with the server's statistics, and on a fresh server
  `high_confidence` at the 18th (Q 3.21, T 1.86).
* Production depths (the real-engine test's 38 games, generated by Stockfish 19, analysed at hash
  32 by each profile): effect sizes of the assisted player against the stand-ins (accuracy, ACPL,
  T1, T1 in complex positions, time CV) 3.60, -2.74, 3.04, 2.50, -5.06 for Stockfish 19 at 9/15;
  3.78, -2.71, 3.06, 2.07, -5.10 at 9/16; 3.85, -2.82, 3.25, 2.24, -4.79 at 10/18; and 3.83,
  -2.99, 4.19, 3.81, -4.17 for Stockfish 16 at 10/18. With statistics learned from the stand-ins,
  the assisted player reaches `high_confidence` at the 14th game at 9/15, the 13th at 9/16 and
  10/18, and the 14th with Stockfish 16; the stand-ins' scores stay under 0.45 with Stockfish 19
  (under 0.9 with Stockfish 16); on a fresh server all four stay under the thresholds after 18
  games (Q 2.97 to 3.16). The differences between the depths are within the noise of 18 games,
  while 9/15 costs a third less than 9/16 (Cost, section 3).

Known limits: players using the engine for a minority of their moves (about half or less) look
like strong humans and are not flagged by statistics within 30 games; top players (2400+) have
little room between their own accuracy and an engine's; a fresh server is slow to flag. Reports
and moderators' judgement cover these cases.

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

Review priority (computed when listing, `reports.js: reviewPriority`): level base (suspected 40,
high confidence 70) + min(20, 4 x score) + 15 log2(1 + report weight of the last 30 days).

## 6. For moderators (`bin/admin.js`)

Run on the server host (same `.env`); every action is recorded (`moderator_action` security
event with the moderator's name: `--by NAME`, `SCACELITH_MODERATOR`, `SUDO_USER` or the OS user;
plus `reviewed_by` / `created_by`).

```
scacelith-admin integrity list [--level suspected|high_confidence|confirmed]   # by review priority
scacelith-admin integrity show <name>        # evidence, per-game features, anomalies, reports
scacelith-admin integrity confirm <name> --reason TEXT [--hours N] [--refund-since DATE | --no-refund]
                                             # confirmed + ban + rating refunds, cheating reports -> actioned
scacelith-admin integrity clear <name> [--reason TEXT] [--dismiss-reports]
scacelith-admin refunds apply <name> [--since DATE]   # refunds of a confirmed cheater (window: from the latest ban)
scacelith-admin refunds list [<name>] [--victim NAME] [--limit N]
scacelith-admin reports list | reports resolve <id> actioned|dismissed
scacelith-admin anomalies <name> | user show|ban|unban|reset-mfa|verify-email|revoke-sessions <name> | stats
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

A ban from the CLI is written to the database only (the CLI has no network access to the
running server): the server applies it (disconnection, close 4004) when the player next connects
or tries to start a game (queue, challenge, private code, rematch), so a banned player starts no
game; a game in progress at the ban plays on, and after an `integrity confirm` the points its
opponent loses in it are refunded when it is recorded. `--revoke-sessions` also logs them out
(shards drop cached sessions within 30 s). Its rating refunds are in the database at once; the
running server tells the victims within 5 s (or later, out of a game).

Refunds: `integrity confirm` refunds the games of the last `RATING_REFUND_DAYS` by default;
`--refund-since DATE` (`YYYY-MM-DD`, UTC, or an ISO 8601 time with its offset) sets another
start, for a player who cheated longer; `--no-refund` gives none (for example when the evidence
covers only some games: refund them later with `refunds apply --since`). Both options choose
among the games already recorded: a game recorded after the confirm while the player is banned
(one that was in progress) is refunded when it is recorded. `refunds apply` works on a
`confirmed` player only. DATE cannot be in the future. The options are checked before anything
is written; the refunds run in their own transaction after the ban, and if they fail
(a database error) the ban stands, its `moderator_action` records the error, and the command
exits with an error that gives the `refunds apply` to run.

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
| Anomalies (kind, severity, game, detail, time) | `store.anomalies` | `info` and `suspicious` ones deleted after `RETENTION_SECURITY_DAYS` by the hourly retention purge; `certain` ones kept |
| Sanctions (ban, reason, source, game, moderator, lift) | `store.sanctions` | kept |
| Integrity level, score, evidence (statistics, peak, certain cheats, reviews) | `store.integrity` | kept while the account exists |
| Per-game features of analysed games (numbers, compact per-move table; no positions beyond the game's own moves) | `store.analysis` | kept with the game (the scoring and `integrity show` read a player's latest analysed games, however old) |
| Failed analysis jobs (no features, an error message) | `store.analysis` | deleted 30 days after the failure by the retention purge |
| Population statistics (n, mean, M2 per metric, analysis profile, category and rating bucket; no personal data) | `store.integrity` population | kept, those of an earlier analysis profile too (unused) |
| Reports (reporter, reported, game, category, comment, weight, outcome, moderator) | `store.reports` | kept; comments are only shown to moderators |
| Moderator actions | `store.security` (`moderator_action`) | security retention |
| Rating refunds (game, victim, cheater, category, points, time, ban or moderator, notified) | `rating_refunds` | kept |
| Refund events | `store.security` (`rating_refund`) | security retention |

No IP address is stored by this module (reports and moderator events carry `ip: null`).

## 9. Store contract assumptions

DESIGN.md names some Store methods without their exact arguments; this module assumes (see the
header of `src/anticheat/index.js`):

* `integrity.populationStats('<profile>|<category>|<bucket>')` returns `{ <metric>: { n, mean,
  m2 } }`, and `integrity.updatePopulation([{ key: '<profile>|<category>|<bucket>|<metric>',
  value }], now)` sends one observation per metric and game, which the store merges (Welford);
  the analysis process is the only writer;
* `analysis.forUser(userId, limit)` returns the player's completed analyses, newest first, each
  row with the `features` given to `complete()`;
* `reports.forReported(userId)` rows carry `weight` and `at`; the optional
  `reports.forReporter(userId)` (rows with `outcome`) feeds the reporter's track record;
* the optional `analysis.request(gameId, 'report' | 'signal', now)` queues a reported game,
  at `report` priority for a credible report and `signal` for a low-credibility one (without it
  a report does not touch the analysis queue);
* `analysis.next()` returns each job with its `priority` (0 for the ordinary sample, the only
  jobs that feed the population);
* free-form values are passed as objects and retried as JSON text if the store refuses them;
* the rating refunds use `refunds` when the store has it (a partial store gives none).
