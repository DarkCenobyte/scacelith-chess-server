# rest-diff: differential test of the HTTPS API

`rest-diff` runs the former Node.js server and the Rust server side by side with equivalent
settings, replays the same scripted scenarios against both, compares every answer and prints a
report of every difference. It checks the compatibility policy of
[RUST-PORT.md](../../docs/RUST-PORT.md) section 1 for the HTTPS API: paths, methods, status codes,
JSON field names, order and number formatting, error codes and messages, the headers clients
read, the rate limits and their thresholds, proof of work, lockouts and address blocks.

## Usage

```sh
cargo build -p scacelith-server -p scacelith-rest-diff      # debug builds are enough
./target/debug/rest-diff                                   # every profile, every scenario (about 15 minutes)
./target/debug/rest-diff --profile main --scenario games   # one scenario
./target/debug/rest-diff --list                            # profiles and their scenarios
./target/debug/rest-diff --report report.txt --keep --work /tmp/rd
```

| Option | Meaning |
|---|---|
| `--node PATH` | node executable (default `$NODE`, else `/opt/nvm/versions/node/v24.21.0/bin/node`) |
| `--node-dir PATH` | the Node server tree, read only (default `$SCACELITH_NODE_DIR`, else `/home/user/rsw/node-ref/dedicated-server`) |
| `--rust PATH` | the `scacelith-server` binary (default `target/debug/scacelith-server` of the workspace) |
| `--profile NAME`, `--scenario NAME` | run only these (repeatable) |
| `--accepted FILE` | the accepted deviations (default `tools/rest-diff/accepted.txt`) |
| `--report FILE` | also write the report there |
| `--work DIR`, `--keep` | where the data directories go; `--keep` keeps them with `node.log` and `rust.log` of each profile |

Exit status: 0 when no difference is open, 1 when some are, 2 for a usage or start-up error.

## How it works

For each profile, both servers start in their own data directory on free ports, with the same
settings: the common ones (`servers.rs`, `base_env`: same `SERVER_NAME`, `SERVER_SECRET`, test
TLS certificate made for the run, `WORKERS=1`, `MAIL_TRANSPORT=log`, `LOG_FORMAT=json`, the same
public ports so that links and `GET /info` match) plus the profile's. The Rust server runs
`scacelith-server migrate` before `start`.

A scenario describes each step once, as a function of the side (its saved tokens, links, game
ids). The step runs on both servers at the same moment, from the same simulated client: every
scenario takes fresh loopback source addresses (`127.0.x.y`), so the servers keep their
production limits and one scenario's buckets never reach another. Requests go over a small
HTTP/1.1 client of its own (keep-alive, raw bytes for malformed requests, detection of a
connection closed after the answer).

- **Mails**: both servers log each mail (`"msg":"mail (log transport)"`) with its recipient,
  subject and text; a step takes the next mail to an address on both sides, compares subject
  and text, and saves the token of its first link (e-mail confirmation, password reset, e-mail
  change).
- **Games**: short scripted games through each server's realtime protocol (Node: protocol 3,
  `scacelith.v1`; Rust: protocol v1 through the SDK), a challenge by name with a fixed colour, UCI
  moves, then a resignation, a checkmate, an agreed draw or an abort. Game ids differ between the
  servers; each one gets a label (`G1`...).
- **Bursts** (`Duo::burst`): a limit is driven to its first 429 on each server; the number of
  requests let through, the statuses before it and the refusal are compared, with a tolerance of
  what the limit refills while the burst runs. The report lists every measured threshold.
- **Two-step verification**: TOTP codes are computed from the secret each server gave (each 30 s
  step works once; the harness waits for a fresh step when needed); proof-of-work challenges are
  solved as a client would.

### What is compared

Status and reason phrase, protocol of the status line, transport failures (no answer, reset),
whether the connection stays open, chunked or not, every header (presence, count, value, and
the case of the name), and the body:

- JSON: compact or not, key order, key sets, array lengths, value types, numbers by their
  lexeme (`1.0` is not `1`), strings with their escapes;
- text (HTML pages, PGN): line by line; binary (GIF): length and SHA-256.

`Content-Length` is compared when the raw bodies are identical (it follows them otherwise).

### Normalisation

Volatile values are replaced by rule, never by ignoring a body; a rule checks the format of its
kind, so that a value of the wrong format stays visible (`normalize.rs`):

| Value | Rule |
|---|---|
| times (`createdAt`, `expiresAt`, `startedAt`, `updatedAt`... epoch ms) | equal within 30 s |
| `retryAfter`, `Retry-After` | within 1 s (bursts: plus the difference of the times both servers took to reach the limit) |
| `spentMs`, `clockMs` | within 5 s |
| game ids (numbers, digit strings, inside text) | `G1`, `G2`... by label |
| `sct_` / `mfa_` / `sso_` tokens, link tokens, long base64url runs in text | masked |
| UUIDs (`serverId`), proof-of-work challenges, TOTP secrets, recovery codes | masked |
| `Date` header | masked |
| PGN `[%clk]`, `[%emt]`, `UTCTime` | masked |
| transport failures | by class: no answer, reset before TLS (on `connect` or the handshake), reset while reading, closed while sending, body cut short |

### The report

- a summary (steps compared, identical, with differences);
- the open differences, step by step: id `profile/scenario/step`, request, statuses, each
  difference with both values;
- the accepted differences grouped by reason;
- the measured limits;
- harness warnings: a status both servers gave that the scenario did not expect, a missing mail,
  a failed game. They are not differences, but they say that a step may not test what it means
  to;
- the coverage of the endpoints of docs/API.md section 2.

### Accepted deviations

`accepted.txt` lists the documented intended deviations, one per line:
`step glob | aspect prefix (or *) | reason`. The glob matches the step id (`*` for any run of
characters), the aspect prefix the difference (`body $.protocol.`, `header name case`,
`status`...). A matched difference is reported as accepted with its reason, not as open.
Each reason starts with `intended:` (a deviation by design, with the place that documents it)
or `Node bug:` (a Node behaviour no client can rely on, which the Rust server does not copy).
Keep the globs narrow: a rule that matches more steps than the deviation it describes would
hide a regression.

## Profiles

| Profile | Settings | Scenarios |
|---|---|---|
| `main` | production limits, e-mail verification; registrations without proof of work, no sign-in wave, `PROVISIONAL_GAMES=2` | `basics`, `http-edge`, `signup`, `login`, `sessions`, `password`, `email-change`, `account`, `mfa`, `games`, `pages`, `limits` |
| `pow` | `POW_REGISTER_BITS=10`, `POW_LOGIN_BITS=8`, `POW_LOGIN_TRIGGER_PER_MIN=3` | `pow` |
| `open` | `REQUIRE_EMAIL_VERIFICATION=false` | `open` |
| `closed` | `REGISTRATION=closed`, a message of the day | `closed` |
| `custom` | GIFs off, no custom time controls, `RATED_CATEGORIES=3+2,10+0`, user names 4-16, passwords from 12, `HTTP_BODY_LIMIT=2048` | `custom` |
| `proxy` | `TLS_MODE=proxy`, `TRUSTED_PROXIES=127.0.0.1` (plain HTTP, `X-Forwarded-For`) | `proxy` |
| `abuse` | `HTTP_RATE_PER_IP=60`, `ABUSE_BLOCK_REFUSALS_PER_MIN=10`, `ABUSE_BLOCK_BASE_SEC=3`, `IP_MAX_INFLIGHT=2` | `abuse` |

## Coverage

- `basics`: `GET /info`, HEAD and OPTIONS, 404 and 405 with their `Allow` lists, the health
  endpoints, path parameters and their decoding, request-target length (4096/4097), target forms.
- `http-edge`: content types and charsets, malformed JSON, schemas of every endpoint (missing,
  extra, wrong-typed, too long fields), the body limit at its boundary, chunked bodies, the body
  timeout, raw requests (HTTP/1.0 and 0.9, no `Host`, unknown methods, bad `Content-Length` and
  `Transfer-Encoding`, `Expect`, upgrades, pipelining, fragments, quotes, UTF-8, NUL), header
  sizes and counts at their boundaries, every form of `Authorization`.
- `signup`, `login`, `sessions`, `password`: registration and its errors, confirmation links and
  their pages, resend, the existing-address notice, sign-in by name and address, the failure
  delay, bans, sessions and their cap, logout, logout-all, password change and reset with their
  mails.
- `email-change`, `account`: the address change with its link, page, notices, cancellation and
  races; the account view, preferences, export and deletion with its effects.
- `mfa`: setup, enable, sign-in with authenticator and recovery codes, re-authentication with a
  second factor, new recovery codes, disable, the per-account code cap, the failure delay.
- `games`: seven games, then `GET /account/games` with every filter, cursor and limit form,
  records, PGN files, GIFs with every option and the render limit, `POST /gif` and its errors,
  profiles and their games, the leaderboard, reports and the daily quota, the export with
  ratings, a deleted player in the records.
- `pages`: `/verify-email` and `/reset-password` (form errors, escaping, body types), the 409 of a
  signup whose address an account took.
- `limits`: the threshold of every limit of docs/API.md section 1.5 (`auth`, `auth_register`,
  `auth_mail`, `auth_forgot`, `auth_reset`, `reauth`, `account_export`, `reports`, `account`,
  `account_games`, `sessions`, `public_read`, `page`, `gif`, the account budget, the per-address
  layer).
- `pow`: each refusal reason, the order of the checks, the sign-in wave.
- `open`, `closed`, `custom`, `proxy`, `abuse`: the behaviours that depend on those settings,
  `X-Forwarded-For`, the address block and the requests in progress.

Not covered: Google sign-in (it needs Google's servers), the hash-queue and locked-store answers
(503 `server_busy`, 503 `busy`), 503 `server_busy` of the GIF queue, `auth_forgot_day` (it needs
more than the hourly limit allows), IPv6 /48 counts (loopback IPv4 only), and the realtime
protocol itself (new by design).

## Adding a scenario

Write an async function over `&mut Duo` in `src/scenarios/` and add it to a profile in
`scenarios/mod.rs`. A step is `d.step(id, ip, expected_status, |side| Req::...)`; save values
with `d.save` (a JSON path of the answer) and read them with `side.v(key)`; take mails with
`d.mail` / `d.mail_count`; play games with `d.connect` and `d.play`; drive a limit with
`d.burst`. Use fresh addresses (`fresh_ip()`) and fresh accounts so that the limits of one part
never decide the answers of another.
