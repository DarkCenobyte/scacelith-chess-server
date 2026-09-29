# Anti-cheat, sanctions, reports and moderation

This server separates three things that are often mixed up:

| | What | Automatic action |
|---|---|---|
| **Certain protocol cheats** | A client did something an unmodified game client cannot do (forged message type, illegal move or move out of turn in a synchronised position, message for a game it does not play). | Yes: forfeit, disconnection, ban of `BAN_DURATION_HOURS`, integrity level `confirmed`. |
| **Statistical suspicion** | Engine analysis of finished rated games says a player's moves and timing look like engine assistance. | **Never.** The integrity level and the evidence are written for a moderator. No ban, no matchmaking change. |
| **Reports** | Players report opponents of their recent games. | **Never.** They raise the review priority, weighted by the reporter's credibility. |

Code: `src/anticheat/` (`index.js` anomalies and sanctions, `analysis/` engine and features,
`scoring.js` + `priors.js` model, `reports.js`, `admin.js`), `src/http/routes/reports.js`,
`bin/analysis-worker.js`, `bin/admin.js`.

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
analysis queue policy of the commit sees it. The buffer is bounded (5000 rows; info rows are
dropped first) and losses are counted in
`scacelith_anticheat_anomalies_dropped_total`. Metrics: `scacelith_anticheat_anomalies_total{kind,severity}`.
Suspicious and certain anomalies are logged with `log.security('anomaly', ...)`.

## 2. Automatic sanctions

With `AUTO_SANCTION_CERTAIN_CHEATS=true` the host ends the game (`Forfeit`) and calls
`ac.sanctionCertain({ userId, gameId, kind })`, which:

* creates a ban of `BAN_DURATION_HOURS` (`source: 'auto'`, reason `certain_cheat:<kind>`, the game id);
* sets the integrity level to `confirmed` and appends `{ kind, gameId, at, banUntil }` to
  `evidence.certain` (existing statistical evidence is kept);
* writes a `sanction_auto` security event and asks the primary to kick the player everywhere
  (`sanction.applied`).

It is idempotent within a game: several certain anomalies of the same game (even reported by
different shards) produce one ban. Another game is another offence and gets its own ban.

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
engine build: a moderator can re-run it and get the same numbers.

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

Cost (Stockfish 16, one core of the test container): depth 10 MultiPV 3 about 60 ms per
position, depth 14 about 0.4 s, depth 18 about 2 s. With the defaults (10/18) a 40- to 60-move
game costs 2 to 4 minutes of one core; raise `ANALYSIS_WORKERS` or lower `ANALYSIS_DEPTH_DEEP`
(14 is about 5 times cheaper) if `scacelith_anticheat_analysis_queue_ordinary` stays at
`ANALYSIS_QUEUE_MAX` (ordinary games are then being skipped), or lower `ANALYSIS_SAMPLE_RATE`
to analyse a smaller, steadier share of them. A job whose engine times out
(`ANALYSIS_POSITION_TIMEOUT_MS`) or crashes is marked failed and the engine restarted; an engine
that cannot start makes the worker wait (5 s .. 5 min) without claiming jobs.

## 4. Statistical model (`scoring.js`, `priors.js`)

### Population statistics

For every (category, 100-point rating bucket) the analysis process keeps Welford statistics (n,
mean, M2) of each per-game metric of rated games. Until a bucket has its own data, **priors**
stand in: hard-coded means and per-game standard deviations by rating (accuracy, ACPL, T1 by
rating from published human data: lichess accuracy/ACPL statistics and Regan & Haworth /
Guid & Bratko engine-matching studies; accuracy derived from the ACPL row through the relation
our pipeline measures, accuracy ~ 100 - 0.23 ACPL), adjusted by time class (bullet, blitz,
rapid, classical: faster games are less accurate). The prior counts as 40 games and its standard
deviations are inflated by 25%, so a young server is deliberately cautious; the server's own
data takes over bucket by bucket. Only the games of the random sample feed the population (the
jobs claimed at ordinary priority, drawn with `ANALYSIS_SAMPLE_RATE`): reported, flagged and
moderator-requested games are analysed first and in full, so counting them would shift the
baseline towards the very players it judges. Values entering the population are winsorised at
4 sd, and games of players already `high_confidence` or `confirmed` are left out (cheaters must
not make cheating look normal).

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
  suspected. Full engine users (profile measured with Stockfish): with learned statistics
  flagged after a median of 10 to 19 games from 1000 to 2400 (2400 rapid: not within 30 games);
  with priors only (fresh server) after about 20 games at 1000, 28 at 1500 blitz, and mostly not
  within 30 games above.
* Real engine (`test/unit/anticheat.engine.test.js`, Stockfish 16): an assisted player (depth 12
  best move, relayed with 2-5 s delays) against human stand-ins (random plausible moves among
  the top 4 at low depth, thinking longer on harder moves). Assisted: accuracy 98.3, ACPL 6,
  T1 0.69, time/complexity correlation -0.04, time CV 0.26; stand-ins: 80.4, 67, 0.26, 0.19,
  0.87. With the server's own statistics (the stand-ins' games), the assisted player is `none`
  for the first 4 games, `suspected` around the 5th and `high_confidence` from the 12th; the
  stand-ins are never flagged. On a fresh server (priors only) the same 18 games stay just
  under the thresholds (Q 3.08, T 1.86): the intended caution of the inflated priors.

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
scacelith-admin integrity confirm <name> --reason TEXT [--hours N]   # confirmed + ban, cheating reports -> actioned
scacelith-admin integrity clear <name> [--reason TEXT] [--dismiss-reports]
scacelith-admin reports list | reports resolve <id> actioned|dismissed
scacelith-admin anomalies <name> | user show|ban|unban|reset-mfa|verify-email|revoke-sessions <name> | stats
```

What to look at in `integrity show`:

1. **Which rule triggered** and whether several independent signals agree (Q/E with T is much
   stronger than Q alone).
2. **The per-game table**: is the high accuracy spread over many games or a few? Do T1 and
   accuracy stay high in complex positions (`cx%`)? Engine users are consistently near-perfect;
   strong humans have bad games too.
3. **Timing**: humans think longer in complex positions (`time~cx` positive) and vary a lot (CV
   around 1); a relay shows no correlation and a low CV. Bullet and bad connections flatten
   timing too, so timing is only corroboration.
4. **History and context**: rating trajectory, account age, a jump that coincides with a new
   account or a long break, reports from credible reporters, protocol anomalies.
5. When in doubt, replay a few games with an engine yourself (the analysis is reproducible) and
   compare with the player's over-the-board style. `clear` records your review: the automatic
   model does not re-flag on the same evidence.

A ban from the CLI applies at the player's next connection (the CLI has no network access to
the running server); `--revoke-sessions` also logs them out (shards drop cached sessions within
30 s).

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
| Population statistics (n, mean, M2 per metric, category and rating bucket; no personal data) | `store.integrity` population | kept |
| Reports (reporter, reported, game, category, comment, weight, outcome, moderator) | `store.reports` | kept; comments are only shown to moderators |
| Moderator actions | `store.security` (`moderator_action`) | security retention |

No IP address is stored by this module (reports and moderator events carry `ip: null`).

## 9. Store contract assumptions

DESIGN.md names some Store methods without their exact arguments; this module assumes (see the
header of `src/anticheat/index.js`):

* `integrity.populationStats('<category>|<bucket>')` returns `{ <metric>: { n, mean, m2 } }`,
  and `integrity.updatePopulation([{ key: '<category>|<bucket>|<metric>', value }], now)` sends
  one observation per metric and game, which the store merges (Welford); the analysis process
  is the only writer;
* `analysis.forUser(userId, limit)` returns the player's completed analyses, newest first, each
  row with the `features` given to `complete()`;
* `reports.forReported(userId)` rows carry `weight` and `at`; the optional
  `reports.forReporter(userId)` (rows with `outcome`) feeds the reporter's track record;
* the optional `analysis.request(gameId, 'report' | 'signal', now)` queues a reported game,
  at `report` priority for a credible report and `signal` for a low-credibility one (without it
  a report does not touch the analysis queue);
* `analysis.next()` returns each job with its `priority` (0 for the ordinary sample, the only
  jobs that feed the population);
* free-form values are passed as objects and retried as JSON text if the store refuses them.
