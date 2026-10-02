# Scacelith server: HTTP API reference

Every server, the official `caissa.scacelith.com` and any community server, answers this HTTPS
API. The game uses it for everything outside a game itself: sign-up and sign-in (two-step
verification and Google included), the account page, game history, PGN downloads and animated
GIFs of games, signed-in devices, the data download and the deletion of the account, and
reports. Live play (matchmaking, challenges, moves, clocks) goes through the WebSocket of the
same server, opened with a session token of this API: see [PROTOCOL.md](PROTOCOL.md). Module
contracts are in
[DESIGN.md](DESIGN.md) section 5.9, the security model in section 8, every setting named here in
[CONFIG.md](CONFIG.md). The game's side of these calls is described in
[docs/ONLINE_CLIENT.md](../../docs/ONLINE_CLIENT.md).

This reference describes what the code does (`src/http/server.js`, `src/http/router.js`,
`src/http/routes/*.js`, `src/auth/*.js`). Defaults are those of a server with an untouched
configuration; a community server may change them.

## Contents

1. [Conventions](#1-conventions): base URL, requests, answers and errors, authentication, rate
   limits, proof of work, re-authentication
2. [Endpoint summary](#2-endpoint-summary)
3. [Server info](#3-server-info)
4. [Registration and sign-in](#4-registration-and-sign-in) (two-step verification and Google
   included)
5. [Sessions](#5-sessions)
6. [Account](#6-account): the account view, preferences, password, two-step verification
7. [E-mail address change](#7-e-mail-address-change)
8. [Data export](#8-data-export)
9. [Account deletion](#9-account-deletion)
10. [Game history](#10-game-history)
11. [Games, PGN and GIF](#11-games-pgn-and-gif)
12. [Players and leaderboard](#12-players-and-leaderboard)
13. [Reports](#13-reports)
14. [HTML pages outside /api](#14-html-pages-outside-api)
15. [Health endpoints](#15-health-endpoints)

## 1. Conventions

### 1.1 Base URL, port and transport

- Official server: `https://caissa.scacelith.com/api/v1`, TCP port **443**.
- A community server: `https://<SERVER_PUBLIC_HOST>[:<port>]/api/v1`. `API_PORT` is 443 by
  default; behind a NAT or a proxy, the port players use is `PUBLIC_API_PORT`.
- The game's WebSocket is on the same port by default (`wss://<host>/ws`; `WS_PORT` moves it, and
  `GET /info` tells the client where it is).
- HTTP/1.1 over TLS. With `TLS_MODE=proxy`, a reverse proxy terminates TLS in front of the server.
  `TLS_MODE=off` (plain HTTP) is for local development only.
- A few HTML pages live outside `/api`, for the links of e-mails and the Google sign-in
  ([section 14](#14-html-pages-outside-api)). The health endpoints answer both inside and outside
  `/api/v1` ([section 15](#15-health-endpoints)).
- A trailing slash is ignored (`/api/v1/info/` is `/api/v1/info`), and path parameters are
  URL-decoded.
- The Node SDK in `src/client/` (`ApiClient`) wraps these calls for tests, bots and tools. It also
  solves the proof of work.

The curl examples below use two shell variables:

```sh
API=https://caissa.scacelith.com/api/v1
TOKEN=sct_...            # a session token from POST /auth/login (section 4)
```

For a community server with a self-signed certificate, add `--cacert server.crt` to each command.
To keep a password out of the shell history, read it with `read -rs PASSWORD` and build the body
with `jq`, for example `-d "$(jq -n --arg p "$PASSWORD" '{password: $p}')"`. The examples write the
password inline only to stay short.

### 1.2 Requests

- **JSON bodies.** `POST`, `PUT` and `DELETE` requests carry a JSON object with
  `Content-Type: application/json`. A `charset` parameter other than UTF-8 is refused, and so is
  any other content type (415 `unsupported_media_type`). An empty body counts as `{}`, which is
  what the endpoints without parameters take (`POST /auth/logout`, `DELETE /auth/sessions/:id`).
  The body is limited to `HTTP_BODY_LIMIT` bytes (16384 by default; `POST /gif` has a limit of its
  own, 135,168 bytes, see [section 11](#post-gif); 413 `payload_too_large`) and must arrive within
  10 seconds (408 `request_timeout`). After either error the server closes the connection.
- **Strict schemas.** A field that the endpoint does not know is refused, a field without "optional"
  in this reference is required, and types and lengths are checked. Any of these failures answers
  400 `invalid_request`, with `field` naming the field. Strings may not contain control
  characters. Lengths are counted in characters. Invalid JSON answers 400 `invalid_json`.
  `POST /reports` checks its body itself ([section 13](#13-reports)).
- **Query strings.** The first occurrence of a parameter counts, and unknown parameters are
  ignored. A `+` decodes to a space: the endpoints that take a time-control category accept both
  `3%2B2` and `3+2`.
- **Methods.** `HEAD` is `GET` without the body. `OPTIONS` on an existing path answers 204 with an
  `Allow` header. Any other method that the path does not have answers 405 `method_not_allowed`
  with `Allow`.
- The request target may not exceed 4096 characters (414 `uri_too_long`).
- **No CORS.** The API serves the game, not web pages. No `Access-Control-*` header is ever sent,
  so a web page cannot read an answer. Because only JSON bodies are taken, a cross-site write
  would need a preflight, and that preflight fails.

### 1.3 Answers and errors

- Answers are JSON in UTF-8, except the PGN download (`application/x-chess-pgn`), the animated
  GIFs (`image/gif`, section 11) and the HTML pages. Times are milliseconds since 1970-01-01
  UTC. Ids are integers. Game ids have up to 16 digits but stay below 2^53, so a JSON number (a
  double) holds them exactly.
- Every answer carries `Cache-Control: no-store`, `X-Content-Type-Options: nosniff`,
  `Referrer-Policy: no-referrer`, `X-Frame-Options: DENY`,
  `Cross-Origin-Resource-Policy: same-origin` and a `Content-Security-Policy`
  (`default-src 'none'; frame-ancestors 'none'` for the API). With `TLS_MODE=native` it also
  carries `Strict-Transport-Security: max-age=31536000`.
- An answer must be read within 60 s of the moment the server has it ready, which only matters
  for the large ones (a GIF, the data export, a long PGN): the server closes the connection of a
  client that has not taken it all by then. The time the server takes to prepare an answer (a GIF
  render, an export) counts neither toward this nor toward the 30 s without a byte in or out
  after which a connection is closed.
- **Errors** have one shape:

  ```json
  { "error": "snake_case_code", "message": "An English sentence.", "retryAfter": 30 }
  ```

  `message` is meant for logs and as a fallback. A client chooses what to show from `error`.
  `retryAfter` (seconds) is present only on refusals that end with time. Some errors add fields:
  `field` (invalid input), `reason` (`weak_password`, `pow_required`), `pow` (`pow_required`) and
  `until` (`banned`). Answers that have `retryAfter` also carry a `Retry-After` header with the
  same value. The one exception is the 503 `busy` of the history, game, player and leaderboard
  reads, which has only the field.

Errors that any endpoint can give:

| Status | `error` | When |
|---|---|---|
| 400 | `invalid_request` | The body breaks the endpoint's schema (`field` says where), the request target or `Content-Length` is malformed, a path parameter is not valid URL encoding, or the body was cut off. |
| 400 | `invalid_json` | The body is not JSON. |
| 401 | `unauthorized` | No `Authorization` header on an endpoint that needs a session. |
| 401 | `invalid_token` | The token is malformed, expired, revoked or belongs to a deleted account. This also happens on endpoints where the session is optional. |
| 404 | `not_found` | No such endpoint. Some endpoints also use it: no such game, player or session. |
| 405 | `method_not_allowed` | The path exists for other methods (see `Allow`). |
| 408 | `request_timeout` | The body did not arrive within 10 s. |
| 413 | `payload_too_large` | The body exceeds `HTTP_BODY_LIMIT` (135,168 bytes for `POST /gif`). |
| 414 | `uri_too_long` | The request target exceeds 4096 characters. |
| 415 | `unsupported_media_type` | The body is not `application/json`, or its charset is not UTF-8. |
| 429 | `rate_limited` | A rate limit (section 1.5): `retryAfter` plus a `Retry-After` header. |
| 500 | `internal_error` | An unexpected failure. The server logs it. |
| 503 | `timeout` | The server did not answer within 30 s (60 s for the export, 45 s for the GIFs with the default settings). |

The read endpoints (sections 10 to 12) and the export answer 503 `busy` with `retryAfter: 1`
when the database stayed locked.

Endpoints that check or hash a password can also answer one of these:

- 503 `server_busy`: the worker's password hash queue (`PASSWORD_HASH_QUEUE_MAX`) is full, or the
  wait ran out.
- 429 `rate_limited`: once the queue is half full, this client (an IPv4 address or an IPv6 /48)
  already has `PASSWORD_HASH_WAITERS_PER_SOURCE` hashes waiting.

Both errors carry a random `retryAfter` of 5 to 15 s. Nothing was changed and no failed attempt
was counted, and a reset link stays valid. The 429 also gives back the rate-limit tokens that the
request took.

The HTML pages (section 14) answer their errors as HTML pages with the same status codes.

### 1.4 Authentication

The endpoints marked **session** need a bearer token:

```
Authorization: Bearer sct_L_8GDd7uzfQ3QQWtqrsWXDTsFWzRwIvJcwIGHhjWPS8
```

- **Getting a token.** A token (`sct_` followed by 43 base64url characters) comes from any of
  these:
  - `POST /auth/login`;
  - after it, `POST /auth/login/mfa` when two-step verification is on;
  - the Google sign-in poll (`POST /auth/sso/google/poll`);
  - `POST /auth/sso/complete`.

  Every one of them answers `{ token, expiresAt, user }`. The server stores only a SHA-256 of the
  token.
- **Where the session is required**, a missing header answers 401 `unauthorized` with
  `WWW-Authenticate: Bearer realm="scacelith"`. An invalid token answers 401 `invalid_token` with
  `WWW-Authenticate: Bearer realm="scacelith", error="invalid_token"`. A client should forget a
  token that gets `invalid_token` and sign in again.
- **Optional session.** On some endpoints the session is optional (`GET /games/:id`,
  `GET /games/:id/pgn`). Without the header they answer the public view. If a header is sent, its
  token must be valid.
- **Lifetime.** A session ends at the first of these:
  - `SESSION_MAX_DAYS` (90) after the sign-in: this is `expiresAt`;
  - `SESSION_IDLE_DAYS` (30) without use. Each use pushes the idle limit back; the server writes
    the new value at most every 5 minutes.

  An account keeps at most `MAX_SESSIONS_PER_USER` (10) sessions: a new sign-in revokes the
  oldest beyond that number.
- **Revocation.** These revoke sessions:
  - signing out (`POST /auth/logout`, `/auth/logout-all`, `DELETE /auth/sessions/:id`);
  - a password change, which revokes the other sessions;
  - a password reset and the deletion of the account, which revoke every session;
  - an administrator (`bin/admin.js`).

  A revocation from the API takes effect at once on every worker. Otherwise a worker may keep
  using its record of a valid session for up to 30 s.
- **Scope.** A token belongs to one server and opens its WebSocket too (`Hello.token`,
  [PROTOCOL.md](PROTOCOL.md)). Never send it to another server.

### 1.5 Rate limits and other throttles

Limits apply to one of two scopes:

- **Client:** an IPv4 address, or an IPv6 /64. In proxy mode, the address comes from
  `X-Forwarded-For` sent by a `TRUSTED_PROXIES` address. Some limits also count each IPv6 /48 as
  a whole, on top of each of its /64 networks: a /48 holds 65,536 of them, and one customer often
  gets a whole /48.
- **Player:** the signed-in account, whatever its address. A limit counted per player on an
  endpoint where the session is optional counts per client for a request without a token.

Each worker process checks a limit as a token bucket. The bucket holds `limit` requests and
refills continuously at `limit / window`, and `retryAfter` is the time until the next token.
Limits marked *shared* are also counted for the whole server by the primary process, over a
sliding window of the same length, so that they hold whatever worker a request reaches. If the
primary does not answer, the worker's own check still applies. When one of an endpoint's limits
refuses a request, the tokens that its other limits took for that request are given back.

There are three layers: a background ceiling per address that every request meets first, a
budget per signed-in account, and the limits of each endpoint (table below), among which the
stricter ones of the sign-in, registration and password recovery family and the quotas of the
GIFs.

**Per-address layer.** Before anything else, every request (any path and method, the health
endpoints and WebSocket upgrades included) takes one token of its client's request budget:
`HTTP_RATE_PER_IP` (600) per minute for the whole server, and `HTTP_RATE_PER_PREFIX` (default 4 x
`HTTP_RATE_PER_IP`) for an IPv6 /48 as a whole. Each worker process allows its share,
max(1, min(L, ceil(2 x L / `WORKERS`))) per minute (all of it with 1 or 2 workers, half of it
with 4), with a burst of half a minute of that share. A client may also have at most
`IP_MAX_INFLIGHT` (32) requests in progress in one worker. Beyond either: 429 `rate_limited`
with `retryAfter` (1 for the requests in progress). This is a ceiling against one address
saturating the server, loose enough for a class or a mobile operator's shared address, not a
quota. A client that keeps going after its refusals is blocked: `ABUSE_BLOCK_REFUSALS_PER_MIN`
(600) refusals in one minute, all workers together, block it for 1 minute, then 4, 16 and 60
minutes at each new block within 6 hours. These refusals are the 429s of this layer, the 429s
of the endpoint limits counted per client (a refusal of the `auth`, `auth_*` and `reauth`
limits counts 5; limits counted per player never count), connections refused before TLS and
malformed requests. A blocked client's requests on connections already open get 429
`rate_limited` with the time left and `Connection: close`, and its new connections are closed
before TLS (`TLS_MODE=native`); its WebSocket connections already open are kept. Addresses in
`ABUSE_EXEMPT` skip this layer, but not the account budget or the endpoint limits. Details: the
README, "Protection against abuse".

**Account budget.** Every request that carries a valid session token (on the endpoints marked
**session** or **optional** in section 2) also counts against its account: `USER_RATE_PER_MIN`
(120) requests per minute, all endpoints together, whatever the address. Each worker process
allows its share, max(1, min(`USER_RATE_PER_MIN`, ceil(2 x `USER_RATE_PER_MIN` / `WORKERS`))) per
minute (all of it with 1 or 2 workers, half of it with 4), with a burst of half a minute of that
share. It is counted in each worker only (no round trip to the primary per request), so a client
spread over every worker gets at most twice the rate. Beyond it: 429 `rate_limited`. The game's busiest use, paging
through the history, is about one request per second.

| Limit | Default | Counted per | Endpoints |
|---|---|---|---|
| per address | `HTTP_RATE_PER_IP` (600) / min for the whole server, each worker its share; `IP_MAX_INFLIGHT` (32) requests in progress per worker | client, and each IPv6 /48 (`HTTP_RATE_PER_PREFIX`, default 4 x `HTTP_RATE_PER_IP`) | Every request, the health endpoints and WebSocket upgrades included (see above). |
| account budget | `USER_RATE_PER_MIN` (120) / min, each worker its share | player | Every request that carries a valid session (see above). |
| `auth` | `AUTH_RATE_PER_IP` (20) / 10 min, shared | client, and each IPv6 /48 (`AUTH_RATE_PER_PREFIX`, default 5 x `AUTH_RATE_PER_IP`) | `POST /auth/register`, `/auth/login`, `/auth/login/mfa`, `/auth/verify-email/resend`, `/auth/password/forgot`, `/auth/password/reset`, `/auth/sso/complete`; `POST /verify-email`, `/reset-password`, `/confirm-email-change` |
| `auth_register` | `AUTH_REGISTER_PER_HOUR` (10) / hour, shared | client, and 3 times that per IPv6 /48 | `POST /auth/register` |
| `auth_mail` | `AUTH_MAIL_PER_HOUR` (10) / hour, shared | client, and 3 times that per IPv6 /48 | `POST /auth/verify-email/resend` |
| `auth_forgot` | `AUTH_FORGOT_PER_HOUR` (3) / hour, shared | client, and 3 times that per IPv6 /48 | `POST /auth/password/forgot` |
| `auth_forgot_day` | `AUTH_FORGOT_PER_DAY` (10) / 24 hours, shared | client, and 3 times that per IPv6 /48 | `POST /auth/password/forgot` |
| `auth_reset` | `AUTH_RESET_PER_HOUR` (10) / hour, shared | client, and 3 times that per IPv6 /48 | `POST /auth/password/reset`, `POST /reset-password` |
| `reauth` | the same numbers as `auth`, a bucket of its own, shared | client and IPv6 /48 | `POST /account/password`, `/account/mfa/totp/setup`, `/account/mfa/totp/enable`, `/account/mfa/totp/disable`, `/account/mfa/recovery-codes`, `/account/email`, `/account/export`, `/account/delete` |
| `reauth_user` | `AUTH_REAUTH_PER_USER` (10) / 10 min, shared | player | The same endpoints as `reauth`: a stolen session used from many addresses cannot guess the password faster. |
| `account` | 60 / min | player | `GET /account/me`, `PUT /account/preferences` |
| `account_games` | 60 / min | player | `GET /account/games` |
| `account_export` | 5 / hour, shared | player | `POST /account/export` (checked before `reauth`; every attempt counts) |
| `sessions` | 60 / min | player | `POST /auth/logout`, `/auth/logout-all`, `GET /auth/sessions`, `DELETE /auth/sessions/:id` |
| `public_read` | 60 / min | player (client without a token) | `GET /players/:username`, `/players/:username/games`, `/games/:id`, `/games/:id/pgn` (one bucket for the four) |
| `gif` | 30 / min | player | `GET /games/:id/gif`, `POST /gif` (one bucket for the two) |
| `gif_user_min`, `gif_user_hour` | `GIF_USER_RENDERS_PER_MIN` (4) / min and `GIF_USER_RENDERS_PER_HOUR` (30) / hour, shared | player | The same two, only when the GIF has to be made (not from the cache, section 11) |
| `gif_ip_min`, `gif_ip_hour` | `GIF_IP_RENDERS_PER_MIN` (12) / min and `GIF_IP_RENDERS_PER_HOUR` (120) / hour, shared | client (all its accounts together), and 3 times that per IPv6 /48 | The same, as above |
| `reports` | 30 / hour | player | `POST /reports` |
| `sso_start` | 30 / 10 min, shared | client, and 90 per IPv6 /48 | `POST /auth/sso/google/start` |
| `sso_poll` | 120 / min | client | `POST /auth/sso/google/poll` |
| `page` | 60 / min | client | `GET /verify-email`, `/reset-password`, `/confirm-email-change` |
| `sso_page` | 30 / min | client | `GET /auth/sso/google/callback` |

`GET /info` and `GET /leaderboard` have no limit of their own: only the per-address layer.

An endpoint with several limits checks them in the order of its row in section 2: on
`POST /auth/register`, `auth` then `auth_register`. The limits of the auth family (`auth`,
`auth_*`, `reauth`) are kept low because each request hashes a password or sends an e-mail.
**Password recovery** is the strictest: a client (an IPv4 address or an IPv6 /64) may ask for 3
reset e-mails per hour and 10 per 24 hours (3 times that per IPv6 /48), on top of `auth` and of
one e-mail per address every 5 minutes, whatever the address asked for; it may send 10 new
passwords per hour with reset links. A refusal is a 429, which says nothing about whether the
address has an account.

The server handles a request in this order:

1. the per-address layer (block, request budget, requests in progress), then the health
   endpoints;
2. endpoint match;
3. authentication;
4. the account budget, when the request carries a session;
5. the endpoint's limits;
6. body;
7. the endpoint itself. The GIF endpoints take their render limits here, only when they have a
   GIF to make.

So a request refused for its token spends none of the endpoint's tokens, and a request with an
invalid body does spend them. A limit answers 429 `rate_limited` with `retryAfter`.

Other throttles, answered by the endpoints themselves:

- **Failed sign-ins on one login name (user name or e-mail).** From `AUTH_FAILURES_PER_ACCOUNT`
  (5) failures on, each attempt must wait twice as long as the one before: 2 s, 4 s, and so on,
  up to 15 min. A refused attempt answers 429 `too_many_attempts` with `retryAfter`. The counter
  forgets after an hour without failures.
- **Failed second factors at the sign-in.** The same rule applies from the 5th wrong code of the
  account. One sign-in step accepts at most 5 wrong codes.
- **Second-factor codes of one account.** At most `AUTH_MFA_PER_ACCOUNT` (10) codes
  (authenticator or recovery codes, right or wrong) per 15 minutes for one account, for the whole
  server and from any address, at sign-in and in re-authentications. Beyond it the answer is 429
  `too_many_attempts` before the code is checked, so a recovery code is not used up.
- **Failed re-authentications of an account** (wrong password or code): the same rule (429
  `too_many_attempts`), shared by every endpoint of section 1.7.
- **E-mails.** One confirmation or reset mail per address every 5 minutes; the answer stays the
  same. One "someone tried to use your address" notice per address per hour.
- **Reports.** `REPORTS_PER_DAY` (5) per player in 24 hours: 429 `report_limit`.

### 1.6 Proof of work

`POST /auth/register` always needs a proof of work when `POW_REGISTER_BITS` is above 0 (18 by
default; `GET /info` gives it as `pow.register`). `POST /auth/login` needs one only for 5 minutes
after the server sees a wave of failed sign-ins (`POW_LOGIN_TRIGGER_PER_MIN`, then
`POW_LOGIN_BITS`). A client cannot know that in advance, and finds out from the answer.

1. The request without (or with a refused) proof answers 428:

   ```json
   { "error": "pow_required", "message": "Proof of work required.", "reason": "required",
     "pow": { "challenge": "eyJ2IjoxLCJo...In0.nBy1dpNb...4QQ", "bits": 18, "expiresAt": 1790882991200 } }
   ```

2. Find a nonce: a decimal string of at most 20 digits such that
   `SHA-256(challenge + ":" + nonce)` starts with `bits` zero bits (most significant bit of the
   first byte first). 18 bits take about 260,000 hashes on average.
3. Send the same request again with `"pow": { "challenge": "...", "nonce": "123456" }` in the body.

A challenge is valid for 2 minutes and only once, for one endpoint and one client network (an
IPv4 address or IPv6 /64). The challenge is signed, so the server keeps nothing about it until
it comes back with its answer. `reason` says why a proof was refused:
`required`, `malformed`, `signature`, `endpoint`, `network`, `expired`, `bits`, `work` or
`replayed`. In Node, `solvePow(challenge, bits)` of `src/client/pow.js` returns the nonce.

### 1.7 Re-authentication

Account changes ask for the password again, and, when two-step verification is on, for a second
factor:

- the password only: `POST /account/password` and `/account/mfa/totp/setup`;
- the password and an authenticator code, a recovery code being refused:
  `POST /account/mfa/recovery-codes`;
- the password and an authenticator code or a recovery code: `POST /account/mfa/totp/disable`,
  `/account/email`, `/account/export` and `/account/delete`.

In bodies, `code` holds a 6-digit authenticator code and `recoveryCode` a recovery code
(`xxxx-xxxx-xx`; case, spaces and dashes do not matter). A recovery code may also be sent in
`code` where recovery codes are accepted. Each code works once: a used authenticator code is
refused until the next 30-second step, and a recovery code is gone once it is used.

Errors of the re-authentication:

| Status | `error` | When |
|---|---|---|
| 403 | `invalid_password` | Wrong password. |
| 403 | `mfa_code_required` | Two-step verification is on and neither `code` nor `recoveryCode` was sent. |
| 403 | `invalid_code` | Wrong or already used code. |
| 400 | `password_not_set` | A Google-only account has no password yet ("Forgot password" sets one). |
| 429 | `too_many_attempts` | Too many failures, or too many codes tried, on this account (section 1.5). |
| 503 / 429 | `server_busy` / `rate_limited` | The password hash queue is busy (section 1.3). |

Failed passwords and codes count in the account's failure counter and are recorded as security
events.

## 2. Endpoint summary

Paths are under `/api/v1`, except the pages and the health endpoints at the end of the table
(sections 14 and 15). Every endpoint marked **session** or **optional** also counts in the account
budget when it gets a token, and every `reauth` endpoint in `reauth_user`; every request, the
health endpoints included, also takes a token of the per-address layer (section 1.5). In the
Auth column:

- **session**: a bearer token is required;
- **optional**: the public answer without a token, and more with one (the player endpoints of
  section 12 answer the same; a token makes their limit count per player);
- **none (page)**: an HTML page for a browser.

| Method and path | Auth | Limit | Purpose |
|---|---|---|---|
| `GET /info` | none | per address | Server name, versions, ports, sign-up rules |
| `POST /auth/register` | none | `auth`, `auth_register` | Create an account |
| `POST /auth/login` | none | `auth` | Sign in with a password |
| `POST /auth/login/mfa` | none | `auth` | Second step of a sign-in with two-step verification |
| `POST /auth/logout` | session | `sessions` | Sign out this session |
| `POST /auth/logout-all` | session | `sessions` | Sign out every session |
| `POST /auth/verify-email/resend` | none | `auth`, `auth_mail` | Send the confirmation link again |
| `POST /auth/password/forgot` | none | `auth`, `auth_forgot`, `auth_forgot_day` | Send a password reset link |
| `POST /auth/password/reset` | none | `auth`, `auth_reset` | Set a new password with a reset link's token |
| `POST /auth/sso/google/start` | none | `sso_start` | Start a Google sign-in |
| `POST /auth/sso/google/poll` | none | `sso_poll` | Collect the result of a Google sign-in |
| `POST /auth/sso/complete` | none | `auth` | Create the account of a first Google sign-in |
| `GET /auth/sessions` | session | `sessions` | List the signed-in devices |
| `DELETE /auth/sessions/:id` | session | `sessions` | Sign out one device |
| `GET /account/me` | session | `account` | The account, ratings, active sanctions |
| `PUT /account/preferences` | session | `account` | Accept or refuse direct challenges |
| `POST /account/password` | session | `reauth`, `reauth_user` | Change the password |
| `POST /account/mfa/totp/setup` | session | `reauth`, `reauth_user` | Start enabling two-step verification |
| `POST /account/mfa/totp/enable` | session | `reauth`, `reauth_user` | Finish enabling it, get recovery codes |
| `POST /account/mfa/totp/disable` | session | `reauth`, `reauth_user` | Turn two-step verification off |
| `POST /account/mfa/recovery-codes` | session | `reauth`, `reauth_user` | Replace the recovery codes |
| `POST /account/email` | session | `reauth`, `reauth_user` | Change the e-mail address |
| `POST /account/export` | session | `account_export`, `reauth`, `reauth_user` | Download the account's data (JSON) |
| `POST /account/delete` | session | `reauth`, `reauth_user` | Delete the account |
| `GET /account/games` | session | `account_games` | The player's game history, filtered and paged |
| `GET /games/:id` | optional | `public_read` | A game record with its moves and clocks |
| `GET /games/:id/pgn` | optional | `public_read` | The same game as a PGN file |
| `GET /games/:id/gif` | session | `gif`, render limits | The game as an animated GIF |
| `POST /gif` | session | `gif`, render limits | Any game sent as PGN, as an animated GIF |
| `GET /players/:username` | optional | `public_read` | A player's public profile |
| `GET /players/:username/games` | optional | `public_read` | A player's recent games |
| `GET /leaderboard` | none | per address | Top players of a category |
| `POST /reports` | session | `reports` | Report the opponent of a recent game |
| `GET`, `POST /verify-email` | none (page) | `page`, `auth` | E-mail confirmation link |
| `GET`, `POST /reset-password` | none (page) | `page`, `auth`, `auth_reset` | Password reset link |
| `GET`, `POST /confirm-email-change` | none (page) | `page`, `auth` | E-mail change link |
| `GET /auth/sso/google/callback` | none (page) | `sso_page` | Where Google sends the browser back |
| `GET /healthz`, `GET /readyz` | none | per address | Liveness and readiness (also under `/api/v1`) |

## 3. Server info

### GET /info

What a client needs before it signs in or connects. **Auth** none. **Limit** the per-address layer only (section 1.5).

```sh
curl -sS "$API/info"
```

```json
{
  "name": "Scacelith",
  "serverId": "07dd26af-672a-43af-a8af-34011c7e977b",
  "motd": "",
  "protocol": { "min": 2, "max": 2, "schema": 2006414980, "subprotocol": "scacelith.v1" },
  "wsPort": 443,
  "wsPath": "/ws",
  "registration": "open",
  "emailVerification": true,
  "sso": { "google": false },
  "mfa": true,
  "pow": { "register": 18 },
  "categories": [ { "id": "1+0", "baseSec": 60, "incSec": 0 }, { "id": "3+2", "baseSec": 180, "incSec": 2 } ],
  "limits": {
    "usernameMin": 3, "usernameMax": 20, "usernamePattern": "^[A-Za-z0-9][A-Za-z0-9_-]*$",
    "passwordMinLength": 10, "passwordMaxBytes": 256, "customTimeControls": true,
    "reportsPerDay": 5, "wsMaxMessageBytes": 512
  }
}
```

- `serverId`: a UUID that the database gets on its first start. It stays the same across
  restarts. It is `null` when the database cannot give it.
- `protocol`: the WebSocket protocol versions, schema hash and subprotocol (PROTOCOL.md).
- `wsPort`: the WebSocket port that players use: `PUBLIC_WS_PORT`, else `WS_PORT`, else
  `API_PORT`. Behind a proxy that publishes 443, set `PUBLIC_WS_PORT` as well as
  `PUBLIC_API_PORT`.
- `registration`: `open` or `closed`. `emailVerification`: whether new accounts confirm their
  address (`REQUIRE_EMAIL_VERIFICATION`).
- `sso.google`: whether Google sign-in is offered. `pow.register`: the proof-of-work bits that
  registration needs (0: none).
- `categories`: the official (rated) time controls (`RATED_CATEGORIES`, all of them; the example
  shortens the list). Any other time control is `custom`.
- `limits`: the rules a client can check before it sends a form. `customTimeControls`: whether
  challenges and private games may use custom time controls.

## 4. Registration and sign-in

### POST /auth/register

Creates an account. **Auth** none. **Limits** `auth` and `auth_register` (10 registrations per hour
per client). **Proof of work** when `POW_REGISTER_BITS` > 0.

| Field | Type | Notes |
|---|---|---|
| `username` | string, 1-64 | Then the server's rules: `USERNAME_MIN`-`USERNAME_MAX` characters (3-20), letters, digits, `_` and `-`, starting with a letter or a digit. Reserved names (`admin`, `moderator`, `deleted`, ...) and some prefixes are refused. Unique without regard to case. |
| `email` | string, 1-254 | Trimmed and stored in lower case. Plain ASCII, with a dotted domain. |
| `password` | string, 1-1024 | At least `PASSWORD_MIN_LENGTH` (10) characters and at most 256 bytes. It must not contain the user name or the e-mail's local part, and must not be a common password. |
| `pow` | object, optional | `{ challenge, nonce }` (section 1.6). |

Answers:

- **202 `{ "status": "verification_sent" }`** with e-mail confirmation (the default). A link valid
  for 24 h goes to the address. The answer is the same when another account already uses the
  address: no account is created then, and that account's owner gets a notice instead (at most
  one per hour).
- **201 `{ "status": "ready" }`** without e-mail confirmation (`REQUIRE_EMAIL_VERIFICATION=false`):
  the account can sign in at once.

Errors, checked in this order:

- 403 `registration_closed`;
- 400 `invalid_username`;
- 400 `invalid_email`;
- 400 `weak_password`, with `reason`: `too_short`, `too_long`, `contains_username`,
  `contains_email` or `too_common`;
- 409 `username_taken`;
- 428 `pow_required`;
- the hash queue errors (section 1.3);
- 409 `email_taken`, only without e-mail confirmation (with confirmation, the answer stays 202).

```sh
curl -sS "$API/auth/register" -H 'Content-Type: application/json' \
  -d '{"username":"alice","email":"alice@example.org","password":"correct horse battery"}'
# -> 428 pow_required: solve it (section 1.6), then send again with "pow":{"challenge":"...","nonce":"..."}
```

### POST /auth/login

Signs in with a password. **Auth** none. **Limit** `auth`. **Proof of work** only during a wave of
failed sign-ins.

| Field | Type | Notes |
|---|---|---|
| `login` | string, 1-254 | The user name, or the e-mail address (any text with `@`). |
| `password` | string, 1-1024 | |
| `clientLabel` | string, max 64, optional | Shown in the list of signed-in devices (e.g. `Scacelith 1.4 (Windows)`). |
| `pow` | object, optional | Section 1.6. |

There are two possible answers, both with status 200. A session:

```json
{
  "token": "sct_L_8GDd7uzfQ3QQWtqrsWXDTsFWzRwIvJcwIGHhjWPS8",
  "expiresAt": 1798658839708,
  "user": {
    "id": 1, "username": "alice", "email": "alice@example.org", "emailVerified": true,
    "mfaEnabled": false, "googleLinked": false, "hasPassword": true, "acceptChallenges": "all",
    "createdAt": 1790882839743, "lastLoginAt": 1790882839708, "pendingEmail": null
  }
}
```

`user` is the account view of `GET /account/me` (section 6). Or, when two-step verification is
on, a second step to complete within 5 minutes with `POST /auth/login/mfa`:

```json
{ "mfaRequired": true, "mfaToken": "mfa_m4wXAVhYCMWG0PX7MLcIpBeccBFg0oxPvoRujzGWjO8", "expiresIn": 300 }
```

Errors:

- 429 `too_many_attempts` (`retryAfter`);
- 428 `pow_required`;
- the hash queue errors;
- 401 `invalid_credentials`: the same answer, after the same time, for an unknown account, a wrong
  password and an account without a password;
- after a correct password only: 403 `banned`, with `until` (epoch ms, `null` for a permanent
  ban), and 403 `email_unverified` (the address is not confirmed yet).

```sh
TOKEN=$(curl -sS "$API/auth/login" -H 'Content-Type: application/json' \
  -d '{"login":"alice","password":"correct horse battery","clientLabel":"curl"}' | jq -r .token)
```

### POST /auth/login/mfa

The second step of a sign-in. **Auth** none. **Limit** `auth`, and at most
`AUTH_MFA_PER_ACCOUNT` (10) codes per 15 minutes for the account, from any address (section 1.5).

| Field | Type | Notes |
|---|---|---|
| `mfaToken` | string, 1-64 | From the login answer. |
| `code` | string, max 32, optional | A 6-digit authenticator code, or a recovery code. |
| `recoveryCode` | string, max 32, optional | A recovery code. |

Answer: 200 `{ token, expiresAt, user }`, as for `POST /auth/login`. A recovery code used here is
gone. Errors:

- 401 `invalid_mfa_token`: the step expired, was used, or ended after 5 wrong codes, or the
  password was reset or changed since the first step; sign in again;
- 429 `too_many_attempts`: the account's failure delay (section 1.5);
- 400 `invalid_request`: neither `code` nor `recoveryCode` was sent;
- 429 `too_many_attempts`: the account's `AUTH_MFA_PER_ACCOUNT` codes of the last 15 minutes are
  used up; the code was not checked, so a recovery code is not spent;
- 401 `invalid_code`;
- 403 `banned`, 403 `email_unverified`.

```sh
curl -sS "$API/auth/login/mfa" -H 'Content-Type: application/json' \
  -d '{"mfaToken":"mfa_m4wX...","code":"123456"}'
```

### POST /auth/logout and POST /auth/logout-all

**Auth** session. **Limit** `sessions`. No body. `logout` revokes the session of the token used,
and `logout-all` revokes every session of the account, this one included. Answer: 200
`{ "status": "logged_out" }`.

```sh
curl -sS -X POST "$API/auth/logout" -H "Authorization: Bearer $TOKEN"
```

### POST /auth/verify-email/resend

Sends the e-mail confirmation link again. **Auth** none. **Limits** `auth` and `auth_mail` (10
per hour per client). Body: `{ "email": string 1-254 }`. Answer:
**202 `{ "status": "accepted" }`**, always. A link is sent only to an active, unconfirmed account
with that address, at most once every 5 minutes per address.

### POST /auth/password/forgot

Sends a password reset link, valid for one hour, which opens the page `/reset-password` (section
14). **Auth** none. **Limits** `auth`, `auth_forgot` and `auth_forgot_day`. Body:
`{ "email": string 1-254 }`. Answer: **202 `{ "status": "accepted" }`**, always. A link goes only
to an active account, at most once every 5 minutes per address. A Google-only account sets its
first password this way.

Password recovery has the strictest limits of the API, all counted for the whole server:

- 3 requests per hour (`AUTH_FORGOT_PER_HOUR`) and 10 per 24 hours (`AUTH_FORGOT_PER_DAY`) per
  client, an IPv4 address or an IPv6 /64;
- 3 times those numbers per IPv6 /48;
- on top of the `auth` limit (20 per 10 minutes) and of the one mail per address every 5 minutes.

Beyond them the answer is 429 `rate_limited` with `retryAfter`, whatever the address: neither the
202 nor the 429 tells whether an account uses it. Setting the new password has its own limit
(`auth_reset`, below).

```sh
curl -sS "$API/auth/password/forgot" -H 'Content-Type: application/json' -d '{"email":"alice@example.org"}'
# -> 202 {"status":"accepted"}; a 4th request within the hour from the same address -> 429 rate_limited
```

### POST /auth/password/reset

Sets a new password with the token of a reset link (the page `/reset-password` does the same).
**Auth** none. **Limits** `auth` and `auth_reset` (10 per hour per client, 30 per IPv6 /48, shared
with the page: each attempt hashes a password).

| Field | Type | Notes |
|---|---|---|
| `token` | string, 1-128 | The `token` parameter of the link. |
| `newPassword` | string, 1-1024 | Password rules as at registration. |

Answer: 200 `{ "status": "password_reset" }`. The password reset has these effects:

- every session is revoked, and a pending e-mail change is cancelled;
- the address counts as confirmed (the link proved it);
- the owner gets a mail;
- two-step verification is not touched.

Errors: 400 `invalid_token` (link invalid, used or expired, or mailed to an address the account
no longer has), 400 `weak_password`, the hash queue errors, and 503 `server_busy` with
`retryAfter: 1` when the database stayed locked (nothing changed). After a hash queue error or a
503, the link stays valid.

### Google sign-in

Offered when `GET /info` says `sso.google: true`; otherwise every endpoint below answers 404
`sso_disabled`. The game signs in through the system browser with PKCE and never sees a Google
credential:

1. The client makes a PKCE pair: a `codeVerifier` of 43-128 characters `[A-Za-z0-9._~-]` and
   `codeChallenge = BASE64URL(SHA-256(codeVerifier))`, which has 43 characters and no padding.
2. `POST /auth/sso/google/start` with the challenge returns the Google URL, which the client opens
   in the browser.
3. Google sends the browser back to `/auth/sso/google/callback` (section 14). The page only says
   to go back to the game.
4. The client polls `POST /auth/sso/google/poll` with the attempt id and its `codeVerifier`. An
   attempt id is useless without the verifier.
5. The answer is a session, a two-step verification step (continue with `POST /auth/login/mfa`),
   or, for a new player, `needsUsername`: the client then calls `POST /auth/sso/complete` with a
   user name.

Which account the Google sign-in reaches:

- the account already linked to that Google account;
- otherwise, a local account with the same address: it is linked when both Google and the server
  have confirmed that address;
- otherwise, a new account.

#### POST /auth/sso/google/start

**Auth** none. **Limit** `sso_start`. Body: `{ "codeChallenge": string of exactly 43 [A-Za-z0-9_-] }`.
The game's first releases also send `"codeChallengeMethod": "S256"`: accepted, and the only value
accepted (S256 is the only method).
Answer:

```json
{ "attemptId": "sso_...", "authUrl": "https://accounts.google.com/...", "pollMs": 2000, "expiresIn": 600 }
```

#### POST /auth/sso/google/poll

**Auth** none. **Limit** `sso_poll`. Poll every `pollMs`.

| Field | Type | Notes |
|---|---|---|
| `attemptId` | string, 1-64 | From `start`. |
| `codeVerifier` | string, 43-128 `[A-Za-z0-9._~-]` | Its SHA-256 must match `start`'s challenge. |
| `clientLabel` | string, max 64, optional | As at sign-in. |

Answers (200). The result is given once:

- `{ "status": "pending" }`: the browser has not come back yet;
- `{ token, expiresAt, user }`: signed in;
- `{ "mfaRequired": true, "mfaToken": "...", "expiresIn": 300 }`: continue with
  `POST /auth/login/mfa`;
- `{ "needsUsername": true, "ssoTicket": "sso_...", "suggestedUsername": "alice" }`: a new
  account. `suggestedUsername` comes from the Google name or the address, and is `""` when
  nothing fits.

Errors:

- 410 `sso_expired`: an unknown, used or expired attempt;
- 403 `invalid_verifier`;
- 409 `sso_cancelled`;
- 502 `sso_failed`;
- 403 `sso_email_unverified`: Google has not confirmed the address;
- 409 `sso_account_unverified`: a local account uses the address without having confirmed it;
- 403 `registration_closed`;
- 403 `account_disabled`;
- 403 `banned`, 403 `email_unverified`.

#### POST /auth/sso/complete

Creates the account of a first Google sign-in. **Auth** none. **Limit** `auth` (with its IPv6 /48
count).

| Field | Type | Notes |
|---|---|---|
| `ssoTicket` | string, 1-64 | From the poll answer, valid for 10 minutes. |
| `username` | string, 1-64 | Username rules as at registration. |
| `clientLabel` | string, max 64, optional | |

Answer: 200 `{ token, expiresAt, user }`. The account has no password: `hasPassword` is `false`,
and "Forgot password" gives it one. Errors:

- 403 `registration_closed`;
- 400 `invalid_username`;
- 410 `sso_expired`;
- 409 `username_taken`;
- 409 `sso_already_linked`;
- 409 `email_taken`.

## 5. Sessions

### GET /auth/sessions

The account's active sessions (signed-in devices), the most recently used first. **Auth**
session. **Limit** `sessions`.

```sh
curl -sS "$API/auth/sessions" -H "Authorization: Bearer $TOKEN"
```

```json
{
  "sessions": [
    { "id": 3, "createdAt": 1790882839708, "lastSeenAt": 1790882839708, "expiresAt": 1798658839708,
      "clientLabel": "Laptop", "current": false },
    { "id": 1, "createdAt": 1790882839708, "lastSeenAt": 1790882839708, "expiresAt": 1798658839708,
      "clientLabel": "Scacelith 1.4 (Windows)", "current": true }
  ]
}
```

- `lastSeenAt` is updated at most every 5 minutes.
- `expiresAt` is the absolute end; the idle limit may end the session sooner.
- `clientLabel` is `null` when the sign-in sent none.
- `current` marks the session making the request.

### DELETE /auth/sessions/:id

Signs out one session of the account; the current one may be signed out too. **Auth** session.
**Limit** `sessions`. No body. Answer: 200 `{ "status": "revoked" }`. Error: 404 `not_found` (no
active session with this id on this account).

```sh
curl -sS -X DELETE "$API/auth/sessions/3" -H "Authorization: Bearer $TOKEN"
```

## 6. Account

### GET /account/me

The account as its player sees it. **Auth** session. **Limit** `account`.

```sh
curl -sS "$API/account/me" -H "Authorization: Bearer $TOKEN"
```

```json
{
  "user": {
    "id": 1, "username": "alice", "email": "alice@example.org", "emailVerified": true,
    "mfaEnabled": true, "googleLinked": false, "hasPassword": true, "acceptChallenges": "all",
    "createdAt": 1790882871478, "lastLoginAt": 1790882902200, "pendingEmail": "alice.new@example.org"
  },
  "ratings": [
    { "category": "3+2", "rating": 1510, "games": 2, "wins": 1, "draws": 1, "losses": 0, "peak": 1510, "provisional": true }
  ],
  "sanctions": [],
  "ban": null
}
```

- `user.hasPassword`: `false` for an account created through Google that has not set a password
  yet.
- `user.googleLinked`: whether a Google account is linked.
- `user.acceptChallenges`: `all` or `none` (see `PUT /account/preferences`).
- `user.lastLoginAt`: the last sign-in, or `null`.
- `user.pendingEmail`: the new address of an e-mail change waiting for its link (section 7), or
  `null`.
- `ratings`: one record per category the player has played rated games in. `provisional` is `true`
  while the rating is unrated or has fewer than `PROVISIONAL_GAMES` counted games (the game shows
  it as `1510?`).
- `sanctions`: the active ones, `[{ kind, reason, startsAt, endsAt }]`. `kind` is `ban`, `mm_block`
  or `warning`, and `endsAt` is `null` when the sanction is permanent.
- `ban`: `{ until }` while banned (`until` is `null` when permanent), else `null`.
- The anti-cheat's integrity level is never shown.

Error: 401 `invalid_token` (also when the account was deleted).

### PUT /account/preferences

**Auth** session; no re-authentication. **Limit** `account`. Body:
`{ "acceptChallenges": "all" | "none" }`. With `none`, direct challenges by name are refused: the
challenger is told that the player is unavailable. Answer: 200
`{ "preferences": { "acceptChallenges": "none" } }`.

```sh
curl -sS -X PUT "$API/account/preferences" -H "Authorization: Bearer $TOKEN" \
  -H 'Content-Type: application/json' -d '{"acceptChallenges":"none"}'
```

### POST /account/password

Changes the password. **Auth** session. **Limits** `reauth`, `reauth_user`. Body:
`{ "currentPassword": string 1-1024, "newPassword": string 1-1024 }`. The current password is
needed, but no second factor, even with two-step verification on.

Answer: 200 `{ "status": "password_changed" }`. The change has these effects:

- every other session is revoked, and this one stays signed in;
- a pending e-mail change is cancelled;
- the owner gets a mail.

Errors: the re-authentication errors (section 1.7) and 400 `weak_password` (checked after the
current password).

```sh
curl -sS "$API/account/password" -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
  -d '{"currentPassword":"correct horse battery","newPassword":"a much better passphrase"}'
```

### Two-step verification (TOTP)

Authenticator apps, RFC 6238: SHA-1, 6 digits, 30-second steps, one step of tolerance either way.

#### POST /account/mfa/totp/setup

**Auth** session. **Limits** `reauth`, `reauth_user`. Body: `{ "password": string }`. This
stores a new pending secret, which replaces any earlier pending one, and returns it for the
authenticator app:

```json
{
  "secret": "OCKJVMPMMKMPLBSIYLN6QQRMKIPC2VLH",
  "uri": "otpauth://totp/Scacelith:alice?secret=OCKJVMPMMKMPLBSIYLN6QQRMKIPC2VLH&issuer=Scacelith&algorithm=SHA1&digits=6&period=30",
  "algorithm": "SHA1", "digits": 6, "period": 30
}
```

Two-step verification is not on yet. Errors: 409 `mfa_already_enabled` (checked before the
password), and the re-authentication errors.

#### POST /account/mfa/totp/enable

**Auth** session. **Limits** `reauth`, `reauth_user`. Body: `{ "code": "123456" }`, a code from
the pending secret (exactly 6 digits). No password is asked here; the password was given at
setup. Answer: 200 `{ "status": "mfa_enabled", "recoveryCodes": [10 codes like "j7v5-3ezx-zn"] }`.
The recovery codes are shown this once. Errors:

- 409 `mfa_already_enabled`;
- 409 `mfa_setup_required`: no pending secret;
- 403 `invalid_code`;
- 429 `too_many_attempts`.

#### POST /account/mfa/totp/disable

**Auth** session. **Limits** `reauth`, `reauth_user`. Body:
`{ "password", "code"?, "recoveryCode"? }`. One of `code` or `recoveryCode` is required. Answer:
200 `{ "status": "mfa_disabled" }`. The secret and the recovery codes are deleted, and the owner
gets a mail. Errors:

- 409 `mfa_not_enabled`;
- 403 `mfa_code_required`;
- the re-authentication errors.

#### POST /account/mfa/recovery-codes

Replaces the recovery codes; the old ones stop working. **Auth** session. **Limits** `reauth`,
`reauth_user`. Body: `{ "password", "code" }`, where `code` is an authenticator code (a recovery
code is refused with 403 `invalid_code`). Answer: 200 `{ "recoveryCodes": [10 codes] }`. Errors:
409 `mfa_not_enabled`, and the re-authentication errors.

## 7. E-mail address change

### POST /account/email

**Auth** session. **Limits** `reauth`, `reauth_user`.

| Field | Type | Notes |
|---|---|---|
| `newEmail` | string, 1-254 | Trimmed and lower-cased, then checked like a registration address. |
| `password` | string, 1-1024 | |
| `code` | string, max 32, optional | Needed with two-step verification: an authenticator code or a recovery code. |
| `recoveryCode` | string, max 32, optional | |

**With e-mail confirmation (the default):** 202 `{ "status": "verification_sent" }`. The request
does this:

- A link valid for 24 hours goes to the new address. It opens `/confirm-email-change`
  (section 14), and the address changes only when the player presses that page's button.
- Until then, `GET /account/me` shows the new address as `pendingEmail`.
- A new request replaces the pending one. A password change or reset cancels it, and a request
  that a password change or reset overtakes while it is being checked gets 403
  `invalid_password`, with no link sent.
- At most one link goes to a given new address every 5 minutes, whoever asks: within that time a
  request mails no new link (a request for the change already pending keeps the link sent
  earlier, which stays valid). The answer is the same.
- The current address gets a notice that a change to a masked address (`a***@example.org`) was
  requested, with what to do if it was not its owner.

The answer and `pendingEmail` are the same when another account already uses the new address. No
link is sent then, so that change never completes; the owner of that address gets a notice (at
most one per hour) instead. When the link is confirmed:

- the address changes and counts as confirmed;
- the devices stay signed in;
- the links sent earlier (confirmation, password reset, other changes) stop working;
- the former address is told, with the new one masked.

**Without e-mail confirmation** (`REQUIRE_EMAIL_VERIFICATION=false`): the address changes at once:
200 `{ "status": "email_changed", "email": "alice.new@example.org" }`, and the former address is
told. If another account uses the address, the answer is 409 `email_taken`, and the owner of that
address gets the notice (at most one per hour).

Errors:

- 400 `invalid_email` and 400 `same_email` (the account's current address). Both are checked
  before the password, so they count no failure;
- 409 `email_taken`, only without e-mail confirmation;
- 503 `server_busy` with `retryAfter: 1` when the database stayed locked: nothing changed, and
  the same request can be sent again;
- the re-authentication errors (section 1.7). A Google-only account gets 400
  `password_not_set`.

```sh
curl -sS "$API/account/email" -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
  -d '{"newEmail":"alice.new@example.org","password":"correct horse battery","code":"123456"}'
# -> 202 {"status":"verification_sent"}; open the link sent to alice.new@example.org
```

## 8. Data export

### POST /account/export

Everything the server keeps about the account, as one JSON file to save. **Auth** session.
**Limits** `account_export` (5 per hour per player, for the whole server; every attempt counts,
failed ones included; checked first), `reauth` and `reauth_user`. Body:
`{ "password", "code"?, "recoveryCode"? }`, the re-authentication of section 1.7. Time limit
60 s.

Answer: 200, `Content-Type: application/json; charset=utf-8`, and
`Content-Disposition: attachment; filename="scacelith-account-<username>.json"`. Characters other
than letters, digits, `_`, `.` and `-` in the file name become `_`. The export records a security
event (`account_exported`).

```sh
curl -sS -o alice-export.json "$API/account/export" -H "Authorization: Bearer $TOKEN" \
  -H 'Content-Type: application/json' -d '{"password":"correct horse battery","code":"123456"}'
```

The document (`format` `scacelith-account-export`, `version` 1; times in epoch ms; the lists are
shortened here):

```json
{
  "format": "scacelith-account-export",
  "version": 1,
  "exportedAt": 1790882839708,
  "server": { "name": "Scacelith", "host": "caissa.scacelith.com" },
  "notes": ["This file holds the data Scacelith keeps about your account. ...", "Not included, ..."],
  "account": {
    "id": 1, "username": "alice", "email": "alice@example.org", "emailVerified": true, "pendingEmail": null,
    "mfaEnabled": false, "googleLinked": false, "googleEmail": null, "hasPassword": true,
    "acceptChallenges": "all", "createdAt": 1790882839743, "lastLoginAt": 1790882839708
  },
  "ratings": [
    { "category": "3+2", "rating": 1510, "games": 2, "wins": 1, "draws": 1, "losses": 0, "peak": 1510,
      "provisional": true, "rated": true, "countedGames": 2, "updatedAt": 1790882839809 }
  ],
  "ratingRefunds": [ { "day": 1790812800000, "category": "3+2", "points": 9 } ],
  "sessions": [
    { "id": 1, "createdAt": 1790882839708, "lastSeenAt": 1790882839708, "expiresAt": 1798658839708,
      "revokedAt": null, "clientLabel": "Scacelith 1.4 (Windows)", "ip": "203.0.113.7" }
  ],
  "securityEvents": [ { "kind": "login", "at": 1790882839708, "ip": "203.0.113.7", "detail": { "method": "password" } } ],
  "sanctions": [],
  "conduct": [ { "kind": "abort", "at": 1790800000000 } ],
  "reportsFiled": [
    { "gameId": 4100000000001, "reported": "bob", "category": "other", "comment": "rude", "createdAt": 1790882840000, "status": "open" }
  ],
  "games": {
    "total": 2,
    "list": [
      { "id": 4100000000001, "category": "3+2", "rated": true, "timeControl": "180+2",
        "white": { "name": "alice", "rating": 1500, "ratingAfter": 1510, "ratingDiff": 10 },
        "black": { "name": "bob", "rating": 1520, "ratingAfter": 1510, "ratingDiff": -10 },
        "color": "white", "status": 1, "reason": 2, "result": "1-0", "termination": "Resignation",
        "plies": 41, "startedAt": 1790620205000, "endedAt": 1790620611000, "baseMs": 180000, "incMs": 2000,
        "outcome": "win" }
    ]
  }
}
```

- `account`: the account view of `GET /account/me` (section 6). It adds `googleEmail`, the address
  of the linked Google account (or `null`).
- `ratings`: the full rating records:
  - `rated`: whether the player has left the unrated phase;
  - `countedGames`: the games that entered the rating;
  - `updatedAt`: when the record last changed.
- `ratingRefunds`: rating points given back to the player after an opponent was found cheating,
  added up per UTC day and category, newest first: `{ day, category, points }`, where `day` is
  00:00 UTC of that day (epoch ms). Neither the games nor the cheaters are named, so the export
  does not tell which opponent was sanctioned; it gives the points as the game's notice does (a
  total may still match the rating changes of some games in `games.list`).
- `sessions`: every stored session, newest first. There is no token in it. The retention purge
  deletes an expired session, and a signed-out one a day after the sign-out. `ip` is erased after
  `RETENTION_IP_DAYS`.
- `securityEvents`: newest first, kept `RETENTION_SECURITY_DAYS`, with `ip` erased after
  `RETENTION_IP_DAYS`. `ip` is given only for what was done while signed in, with the account's
  password (and second factor) or with a link mailed to its address: `register`,
  `email_verified`, `login`, `sso_login`, `sso_account_created`, `recovery_code_used`,
  `password_reset`, `password_changed`, `reauth_failed`, `mfa_setup_started`, `mfa_enabled`,
  `mfa_disabled`, `recovery_codes_regenerated`, `session_revoked`, `sessions_revoked_all`,
  `email_change_requested`, `email_changed`, `email_change_refused` and `account_exported`. Every
  other kind has `ip: null`: failed sign-ins (`login_failed`, `login_lockout`, `mfa_failed`) and
  the requests anyone can make by typing the account's name or address
  (`password_reset_requested`, `verification_resent`, `register_existing_email`) may come from
  another person. `detail` keeps only the fields that the export allows for the event's kind:
  - `login`: `method`;
  - `sso_login`, `sso_linked`, `sso_account_created`: `provider`;
  - `login_failed`: `failures`;
  - `login_lockout`: `retryAfterMs`;
  - `mfa_failed`: `attempts`;
  - `recovery_code_used`: `remaining`;
  - `reauth_failed`: `factor`;
  - `session_revoked` and `sessions_revoked_all`: `reason`;
  - `email_change_refused`: `reason`;
  - `sanction_auto`: `kind`, `gameId`, `until`.

  Every other kind has `detail: null`. A `moderator_action` event keeps only `{ action }`, for
  the actions `ban`, `unban`, `reset_mfa`, `verify_email` and `revoke_sessions`; the export leaves
  out the other moderator actions. The `rating_refund` events are left out: their points are in
  `ratingRefunds`.
- `sanctions`: every sanction, lifted ones included:
  `{ id, kind, reason, source ("auto" | "moderator"), gameId, startsAt, endsAt, createdAt, liftedAt }`.
  The moderator's name is never included.
- `conduct`: the conduct events recorded for the player's abandoned, aborted and no-show games
  (`kind`: `abandon`, `abort` or `noshow`), kept 30 days.
- `reportsFiled`: the reports the player made. `reported` is the reported player's current public
  name, and `status` is `open` or `closed` (whether the reported player was sanctioned is not
  said).
- `games`: `total` and every game, newest first, as summaries of `GET /account/games`
  (section 10). Moves are at `GET /games/:id` and the PGN at `GET /games/:id/pgn`.

**Never in the export:**

- the password hash, the two-step verification secret and the recovery codes;
- any session or link token, or a hash of one;
- the anti-cheat's data: integrity level and score, anomalies, the analysis of the games and its
  population statistics, the weight of a report;
- the reports other players made about the player;
- the identities of moderators;
- other players' private data: opponents appear by their public name and rating, nothing tells
  whether another player was sanctioned, and no IP address that may be another person's is
  included.

The `notes` array of the document says this to the player in plain English.

Errors:

- 429 `rate_limited` (`account_export` or `reauth`);
- the re-authentication errors;
- 503 `busy` (with a `Retry-After: 1` header);
- 503 `timeout`.

## 9. Account deletion

### POST /account/delete

Deletes the account; this cannot be undone. **Auth** session. **Limits** `reauth`,
`reauth_user`. Body: `{ "password", "code"?, "recoveryCode"? }`. With two-step verification, a
code or a recovery code is required. Answer: 200 `{ "status": "deleted" }`.

- Every session is revoked at once. The token gets 401 `invalid_token` from then on.
- The user name becomes `deleted#<id>`, in the account and in every game record.
- These are erased: the e-mail address, the password hash, the two-step secret and recovery codes,
  the sessions and link tokens, the Google link, the anti-cheat's integrity record, and the IP
  addresses stored with security events.
- Ratings and games are kept. Games stay readable under the anonymous name, and
  `GET /players/<former name>` answers 404.

Errors: the re-authentication errors.

```sh
curl -sS "$API/account/delete" -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
  -d '{"password":"correct horse battery","code":"123456"}'
```

## 10. Game history

### GET /account/games

The signed-in player's games, newest first, filtered and paged, with the number of games matching
the filter. **Auth** session. **Limit** `account_games`.

Query parameters, all optional; an empty value counts as absent:

| Parameter | Values |
|---|---|
| `before` | A game id: only older games (pass the previous page's `next`). |
| `limit` | 1-50, default 20. A larger number of up to 3 digits counts as 50. |
| `category` | An official category id (`3+2`, or `3%2B2`) or `custom`. |
| `rated` | `true` or `false`. |
| `result` | `win`, `loss` or `draw`, from the player's side. Aborted games appear only without this filter. |

```sh
curl -sS "$API/account/games?limit=20&rated=true&result=win" -H "Authorization: Bearer $TOKEN"
curl -sS "$API/account/games?limit=20&before=4100000000002" -H "Authorization: Bearer $TOKEN"
```

```json
{
  "games": [
    {
      "id": 4100000000001, "category": "3+2", "rated": true, "timeControl": "180+2",
      "white": { "name": "alice", "rating": 1500, "ratingAfter": 1510, "ratingDiff": 10 },
      "black": { "name": "bob", "rating": 1520, "ratingAfter": 1510, "ratingDiff": -10 },
      "color": "white", "status": 1, "reason": 2, "result": "1-0", "termination": "Resignation",
      "plies": 41, "startedAt": 1790620205000, "endedAt": 1790620611000,
      "baseMs": 180000, "incMs": 2000, "outcome": "win"
    }
  ],
  "next": null,
  "total": 1
}
```

A summary has these fields:

- `color`: the player's side.
- `outcome`: `win`, `loss`, `draw` or `aborted`, from the player's side.
- `white` / `black`:
  - `name`: the name in the game record (`deleted#<id>` for a deleted account);
  - `rating`: the rating at the start (`null` when unknown);
  - `ratingAfter` and `ratingDiff`: the rating after the game and the change. A rated game
    always has them, also when the rating rules leave a rating where it was (FIDE's zero score,
    an unrated opponent, the first games of an unrated player; [DESIGN.md](DESIGN.md) section
    6.6): then `ratingDiff` is `0`. They are `null` only for a game that does not count for the
    ratings (casual, custom, aborted).
- `timeControl`: in seconds (`180+2`). `baseMs` and `incMs` give it in milliseconds.
- `status`, `reason`, `result` and `termination`: see [Game codes](#game-codes).
- `plies`: the number of half-moves.

Paging:

- `next`: the id to pass as `before` for the next page, or `null` on the last page.
- `total`: the number of games matching the filter, across all pages.

Errors: 400 `invalid_cursor`, `invalid_limit` or `invalid_filter`, each with `field`; 503 `busy`.

## 11. Games, PGN and GIF

### GET /games/:id

One game record, with its moves and clocks. **Auth** optional. **Limit** `public_read`.

```sh
curl -sS "$API/games/4100000000001" -H "Authorization: Bearer $TOKEN"
```

```json
{
  "id": 4100000000001, "category": "3+2", "rated": true, "timeControl": "180+2",
  "white": { "name": "alice", "rating": 1500, "ratingAfter": 1510, "ratingDiff": 10 },
  "black": { "name": "bob", "rating": 1520, "ratingAfter": 1510, "ratingDiff": -10 },
  "status": 1, "reason": 2, "result": "1-0", "termination": "Resignation", "plies": 2,
  "startedAt": 1790620205000, "endedAt": 1790620611000, "baseMs": 180000, "incMs": 2000,
  "statusName": "WhiteWins", "rematchOf": null,
  "moves": [
    { "uci": "e2e4", "spentMs": 0, "clockMs": 180000 },
    { "uci": "e7e5", "spentMs": 1700, "clockMs": 180300 }
  ],
  "pgn": {
    "Event": "Scacelith rated 3+2", "Site": "caissa.scacelith.com", "Date": "2026.09.28", "Round": "-",
    "White": "alice", "Black": "bob", "Result": "1-0", "WhiteElo": 1500, "BlackElo": 1520,
    "TimeControl": "180+2", "Termination": "Resignation", "PlyCount": 2
  },
  "you": "white",
  "reportable": true
}
```

- The record has the summary fields of section 10 except `color` and `outcome`.
- `statusName`: the name of `status`.
- `rematchOf`: the id of the game that this one is a rematch of, or `null`.
- `moves`: one entry per ply.
  - `uci`: the move in UCI notation, with `n`, `b`, `r` or `q` added for a promotion.
  - `spentMs`: the time charged for the move.
  - `clockMs`: the mover's clock after the move.

  Both are `null` when the record does not have them.
- `pgn`: the main PGN tags, for display. `WhiteElo` and `BlackElo` are a number or `"-"`.
  `Termination` here is the end-reason name; the PGN file uses the PGN standard values.
- `you` and `reportable` are only present when the token is a player of this game:
  - `you`: `white` or `black`;
  - `reportable`: `true` when `POST /reports` would take a report of the opponent for this game
    now. The game must have ended within 7 days, the daily quota must not be used up, and the
    opponent must not already be reported for this game.

  Without a token, or with another player's, the answer is the public one.

Errors: 400 `invalid_game_id` (not a positive integer of at most 16 digits), 404 `not_found`,
401 `invalid_token` (a token that is not valid), 503 `busy`.

### GET /games/:id/pgn

The same game as a PGN file. **Auth** optional (the answer is the same). **Limit** `public_read`.

- Answer: 200, `Content-Type: application/x-chess-pgn; charset=utf-8`,
  `Content-Disposition: attachment; filename="scacelith-<id>.pgn"`.
- One game with `\n` line endings, and move text in lines under 80 columns.

```sh
curl -sS -OJ "$API/games/4100000000001/pgn"          # saves scacelith-4100000000001.pgn
```

```
[Event "Scacelith rated 3+2"]
[Site "caissa.scacelith.com"]
[Date "2026.09.28"]
[Round "-"]
[White "alice"]
[Black "bob"]
[Result "1-0"]
[UTCDate "2026.09.28"]
[UTCTime "18:30:05"]
[WhiteElo "1500"]
[BlackElo "1520"]
[WhiteRatingDiff "+10"]
[BlackRatingDiff "-10"]
[TimeControl "180+2"]
[Termination "normal"]
[PlyCount "2"]
[ScacelithGameId "4100000000001"]

1. e4 {[%clk 0:03:00.0] [%emt 0:00:00.0]} 1... e5 {[%clk 0:03:00.3]
[%emt 0:00:01.7]} {Resignation} 1-0
```

Tags, in this order:

- `Event`: `<SERVER_NAME> rated <category>` or `<SERVER_NAME> casual <category>`.
- `Site`: `SERVER_PUBLIC_HOST`.
- `Date`: the UTC start date.
- `Round`, `White`, `Black`, `Result`. The result is `*` for an aborted game.
- `UTCDate` and `UTCTime`: the start.
- `WhiteElo` and `BlackElo`: the ratings at the start, or `-`.
- `WhiteRatingDiff` and `BlackRatingDiff`: the changes (`"+10"`, `"-10"`), in every rated game,
  `"+0"` when the rating rules leave the rating where it was (as `ratingDiff` 0 in section 10).
  A casual, custom or aborted game has neither.
- `TimeControl`: in seconds.
- `Termination`, with a PGN standard value:
  - `normal`;
  - `time forfeit`: a flag fall, also when it ends in a draw;
  - `abandoned`;
  - `rules infraction`: a second illegal move or a fair-play forfeit;
  - `unterminated`: an aborted game.
- `PlyCount`.
- `ScacelithGameId`: the decimal id.

Each move carries `{[%clk h:mm:ss.f] [%emt h:mm:ss.f]}`: the mover's clock after the move and the
time charged for it, in tenths of a second (truncated). A value is left out when the record lacks
it. After the last move, the end reason follows in words, then the result. The game's PGN reader
reads `[%clk]` and `[%emt]` back (`tests/data/server-pgn/` holds server-made samples).

Errors:

- 400 `invalid_game_id`;
- 404 `not_found`;
- 500 `internal_error`: the stored moves cannot be replayed to the stored ending (the server logs
  it);
- 503 `busy`.

### GET /games/:id/gif

The game as an animated GIF, to keep or to share: the board seen from above, one frame per
position from the start to the final position, the players' names and ratings above the board,
the last move below it, and on the last frame the result and how the game ended. **Auth**
session: the quotas count per account. **Limits** `gif` on every request, and the render limits
when the GIF has to be made ([below](#cost-cache-and-quotas-of-the-gifs)).

| Query parameter | Values | Default |
|---|---|---|
| `size` | `small` (32 px squares: 284 x 350 pixels), `medium` (48 px: 424 x 515), `large` (72 px: 628 x 762) | `medium` |
| `orientation` | `white` or `black`: the side at the bottom of the board | `white` |
| `delay` | 100 to 3000: milliseconds per move | `500` |
| `coords` | `1` or `0`: the file letters and rank numbers around the board (without them: 268 x 342, 400 x 503, 600 x 748) | `1` |

- The start position stays at least 1 s (`delay` when it is longer), the final position 3 s, and
  the GIF loops. After the first frame, only the part of the picture that changes is stored.
- The names and ratings are those of the game record (section 11, `GET /games/:id`): the ratings
  at the start, a deleted account as `deleted#<id>`. The result and the ending (`Resignation`,
  `Loss on time`...) are those of the record too.
- Answer: 200, `Content-Type: image/gif`, `Content-Disposition: attachment;
  filename="scacelith-<id>.gif"`, `Content-Length`.
- Size of the file: about 135, 205 and 325 KiB for a 40-move game (small, medium, large), 0.5,
  0.8 and 1.2 MiB for 150 moves.

```sh
# Save it under a name of your choice ...
curl -sS -o alice-bob.gif "$API/games/4100000000001/gif?size=large&orientation=black&delay=800" \
  -H "Authorization: Bearer $TOKEN"
# ... or under the server's name (scacelith-4100000000001.gif), with the defaults.
curl -sS -OJ "$API/games/4100000000001/gif" -H "Authorization: Bearer $TOKEN"
```

Errors:

- 400 `invalid_option` with `field` (`size`, `orientation`, `delay` or `coords`): a value outside
  the table above;
- 400 `invalid_game_id`; 404 `not_found`;
- 422 `game_too_long`: the game has more than `GIF_MAX_PLIES` (600) half-moves;
- 429 `rate_limited` with `retryAfter`: the `gif` limit or a render limit;
- 503 `server_busy` with `retryAfter` (3 to 10 s) and `Retry-After`: the worker's rendering queue
  is full, or the GIF waited `GIF_QUEUE_TIMEOUT_MS` (10 s) for a free thread. The render limits
  that the request took are given back;
- 500 `render_failed`: the GIF could not be made (the server logs why);
- 503 `busy`: the database stayed locked;
- 404 `gif_disabled`: the server turned GIFs off (`GIF_ENABLED=false`).

### POST /gif

The same picture for any game sent as PGN text: a game saved by the game, an export of another
site, a game typed by hand. **Auth** session. **Limits** as for `GET /games/:id/gif`. Body:

| Field | Type | Notes |
|---|---|---|
| `pgn` | string, at most 65,536 bytes of UTF-8 | Only the first game of the text is used. |
| `size` | `small`, `medium` or `large`, optional | Default `medium`. |
| `orientation` | `white` or `black`, optional | Default `white`. |
| `delayMs` | integer 100-3000, optional | A JSON number. Default 500. |
| `coords` | boolean, optional | `true` or `false`. Default `true`. |

- The PGN reader takes what the game's own reader takes: every PGN that this server writes and
  the usual exports of other sites (comments, variations, NAGs and clock annotations are skipped;
  move numbers and SAN read leniently; a `FEN` tag gives the start position unless `SetUp` is
  `"0"`).
- The names and ratings come from the `White`, `Black`, `WhiteElo` and `BlackElo` tags. Accented
  letters lose their accents, other characters outside printable ASCII become `?`, and long names
  are cut. The result comes from the `Result` tag, else from the end of the move text. The
  `Termination` tag is shown unless it is `normal`: the final position then tells the story
  (checkmate, stalemate).
- Answer: as for `GET /games/:id/gif`, with `filename="scacelith-game.gif"`.

```sh
# A PGN file saved on disk (jq builds the JSON string).
jq -n --rawfile pgn my-game.pgn '{pgn: $pgn, size: "small", delayMs: 800}' |
  curl -sS -o my-game.gif "$API/gif" -H "Authorization: Bearer $TOKEN" \
    -H 'Content-Type: application/json' --data-binary @-
# A short game typed in.
curl -sS -o fools-mate.gif "$API/gif" -H "Authorization: Bearer $TOKEN" \
  -H 'Content-Type: application/json' -d '{"pgn":"1. f3 e5 2. g4 Qh4# 0-1","coords":false}'
```

Errors: those of `GET /games/:id/gif` (with `delayMs` as the `field` of the delay), except
`invalid_game_id`, `not_found` and `busy`, plus:

- 400 `invalid_request` with `field`: an unknown field, or `pgn` missing or not a string;
- 400 `invalid_option`: also `delayMs` that is not a number (`"500"`) or `coords` that is not a
  boolean;
- 400 `invalid_pgn` with `line` and `column` (from 1, columns in characters) and the reader's
  `message`: an illegal or ambiguous move, a broken tag, an unknown variant, more than 65,536
  bytes;
- 413 `payload_too_large`: a body above 135,168 bytes (the PGN as a JSON string, its escapes
  included).

```json
{ "error": "invalid_pgn", "message": "illegal move 'Ke3'", "line": 1, "column": 13 }
```

(the answer to `{"pgn":"1. e4 e5 2. Ke3 *"}`)

### Cost, cache and quotas of the GIFs

A GIF is made on a rendering thread of the worker process, never on the thread that runs the
games, and at the lowest CPU priority, so it only takes the CPU the games leave: `GIF_THREADS` (1)
per worker, started with the first GIF and stopped after a minute without one. Up to
`GIF_QUEUE_MAX` (4) GIFs wait for it, at most `GIF_QUEUE_TIMEOUT_MS` (10 s) each; a render may
last `GIF_RENDER_TIMEOUT_MS` (30 s). A 40-move game takes a few tens of milliseconds, the longest
ones up to about a second ([SIZING.md](SIZING.md#animated-gifs)). When the server is busy with
its games, the GIFs wait for them, and a request that waited too long gets 503 `server_busy`.

Each worker keeps the GIFs it made in a cache of `GIF_CACHE_MB` (32) MB, the least recently used
going first. A GIF from the cache, or one being made for another request, costs no render: asking
again for the same game with the same options is cheap (with several workers, a repeat that
reaches another worker may be made again). The cache keys on everything that changes the picture,
names included, so a deleted account never reappears from it.

Every request counts in `gif`, 30 per minute per player for both endpoints. A GIF that has to be
made also counts in the render limits, for the whole server:

- per player: `GIF_USER_RENDERS_PER_MIN` (4) per minute and `GIF_USER_RENDERS_PER_HOUR` (30) per
  hour;
- per client (an IPv4 address or an IPv6 /64), all its accounts together: `GIF_IP_RENDERS_PER_MIN`
  (12) per minute and `GIF_IP_RENDERS_PER_HOUR` (120) per hour, and 3 times that per IPv6 /48.

A client should keep the file it downloaded rather than ask for it again, and wait `retryAfter`
after a 429 or a 503.

### Game codes

`status`: 1 `WhiteWins`, 2 `BlackWins`, 3 `Draw`, 4 `Aborted`. Only finished games are stored.

`reason`, with its name (`termination`) and the words that end the PGN file's move text. Codes 7
and 21 are draws: the player who ran out of time or abandoned faced an opponent who could not
checkmate. The server never ends a game with codes 4 and 13; they are part of the shared list of
reasons.

| Code | `termination` | Words in the PGN |
|---|---|---|
| 1 | `Checkmate` | Checkmate |
| 2 | `Resignation` | Resignation |
| 3 | `Timeout` | Loss on time |
| 4 | `IllegalMoves` | Second illegal move (forfeit) |
| 5 | `Stalemate` | Stalemate |
| 6 | `InsufficientMaterial` | Dead position (insufficient material) |
| 7 | `TimeoutVsInsufficient` | Flag fall, but the opponent cannot checkmate |
| 8 | `FivefoldRepetition` | Fivefold repetition |
| 9 | `SeventyFiveMoves` | 75-move rule |
| 10 | `ThreefoldClaim` | Threefold repetition (claimed) |
| 11 | `FiftyMoveClaim` | 50-move rule (claimed) |
| 12 | `Agreement` | Draw by agreement |
| 13 | `IllegalMovesVsInsufficient` | Second illegal move, but the opponent cannot checkmate |
| 20 | `Abandonment` | Abandoned (disconnected for too long) |
| 21 | `AbandonmentVsInsufficient` | Abandoned, but the opponent cannot checkmate |
| 22 | `Aborted` | Game aborted |
| 23 | `NoShow` | Aborted: first move not played in time |
| 24 | `Forfeit` | Forfeit (fair play violation) |
| 25 | `ServerAborted` | Aborted by the server |
| 26 | `BothDisconnected` | Aborted: both players disconnected |

`result`: `1-0`, `0-1`, `1/2-1/2`, or `*` (aborted).

## 12. Players and leaderboard

Public data only: never an e-mail address, a session, a sanction or an integrity level. A deleted
account has no profile.

### GET /players/:username

**Auth** optional (the answer is the same; with a token the limit counts per player). **Limit**
`public_read`.

```sh
curl -sS "$API/players/alice"
```

```json
{
  "username": "alice",
  "createdAt": 1790882839743,
  "ratings": [
    { "category": "3+2", "rating": 1510, "provisional": true, "games": 2, "wins": 1, "draws": 1, "losses": 0, "peak": 1510 }
  ],
  "games": { "total": 3, "rated": 2, "wins": 1, "draws": 1, "losses": 0 }
}
```

- `ratings`: the official categories only, in the server's order.
- `games.total`: every stored game, casual and aborted ones included.
- `games.rated`, `wins`, `draws` and `losses`: summed over the rating records, so rated games only.

Errors: 400 `invalid_username` (2-24 characters of `[A-Za-z0-9_.-]`), 404 `not_found` (no such
player, or a deleted account), 503 `busy`.

### GET /players/:username/games

The player's recent games, newest first. **Auth** optional, as above. **Limit** `public_read`.
Query: `before` (a game id) and `limit` (1-50, default 20, as in section 10). Neither filters nor
a total are available here.

```sh
curl -sS "$API/players/alice/games?limit=1"
```

```json
{
  "username": "alice",
  "games": [
    { "id": 4100000000001, "category": "3+2", "rated": true, "timeControl": "180+2",
      "white": { "name": "alice", "rating": 1500, "ratingAfter": 1510, "ratingDiff": 10 },
      "black": { "name": "bob", "rating": 1520, "ratingAfter": 1510, "ratingDiff": -10 },
      "color": "white", "status": 1, "reason": 2, "result": "1-0", "termination": "Resignation",
      "plies": 41, "startedAt": 1790620205000, "endedAt": 1790620611000 }
  ],
  "next": 4100000000001
}
```

The summaries are those of section 10 without `baseMs`, `incMs` and `outcome`; `color` is the
side of this player. `next` is the last game's id whenever the page is full, so the next page may
be empty. Errors: 400 `invalid_username`, `invalid_cursor` or `invalid_limit`; 404 `not_found`;
503 `busy`.

### GET /leaderboard

**Auth** none. **Limit** the per-address layer only (section 1.5). Query: `category` (required: an official category, `3%2B2`
or `3+2`) and `limit` (1-100, default 100; a larger number of up to 3 digits counts as 100).

```sh
curl -sS "$API/leaderboard?category=3%2B2&limit=10"
```

```json
{
  "category": "3+2", "minGames": 30, "updatedAt": 1790882839828,
  "players": [ { "rank": 1, "username": "bob", "rating": 1874, "games": 212, "wins": 120, "draws": 30, "losses": 62, "peak": 1901 } ]
}
```

- The list holds the top 100 rated records with at least `minGames` (`PROVISIONAL_GAMES`) counted
  games. It leaves out deleted accounts and confirmed cheaters.
- Each worker computes it again at most every 10 seconds; `updatedAt` says when.

Errors: 400 `invalid_category`, 400 `invalid_limit`, 503 `busy`.

## 13. Reports

### POST /reports

Reports the opponent of one of the player's own games. A report never changes a rating, a
sanction or an integrity level by itself. It raises the review priority that moderators see,
and, except for `abuse`, it asks for the engine analysis of the game
([ANTICHEAT.md](ANTICHEAT.md)). **Auth** session. **Limit** `reports` (per player), plus
`REPORTS_PER_DAY` per player.

| Field | Type | Notes |
|---|---|---|
| `gameId` | integer, or a string of digits | The game. |
| `reported` | string, max 24 | The opponent's user name, as in the game record or as it is now, without regard to case. |
| `category` | `cheating`, `abuse` or `other` | |
| `comment` | string, optional | At most 500 characters once control characters are removed. |

Answer: **202 `{ "status": "received" }`**. A report of the same opponent for the same game gets
the same answer and changes nothing; the answer never tells anything about the reported account.
Errors:

- 400 `invalid_request`;
- 429 `report_limit` (`retryAfter: 3600`, with a `Retry-After` header);
- 403 `report_not_allowed`: not the opponent of the reporter in a game that ended within the last
  7 days.

`GET /games/:id` tells the game's players beforehand whether a report would be taken
(`reportable`).

```sh
curl -sS "$API/reports" -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
  -d '{"gameId":4100000000001,"reported":"bob","category":"cheating","comment":"engine-like play"}'
```

## 14. HTML pages outside /api

Pages for a browser, opened from the links of e-mails and by Google. Their links point to
`https://<SERVER_PUBLIC_HOST>` (with `:<PUBLIC_API_PORT>` when it is not 443).

- The pages run no JavaScript and load no external resource. They are served with
  `Content-Security-Policy: default-src 'none'; style-src 'unsafe-inline'; form-action 'self';
  frame-ancestors 'none'; base-uri 'none'`.
- A GET only shows a button or a form, so that a mail scanner opening the link does not use it up.
  The change happens on POST: a form sent as `application/x-www-form-urlencoded`, with the link's
  token in a hidden field.
- Errors (rate limits, invalid fields) are HTML pages too.

| Page | Answers |
|---|---|
| `GET /verify-email?token=` | 200: a "Confirm my e-mail address" button. 400: link invalid or expired. Limit `page`. |
| `POST /verify-email` (form `token`) | 200: address confirmed. 400: link invalid, used or expired. Limit `auth`. |
| `GET /reset-password?token=` | 200: the new password form (password twice). 400: link invalid (also when it was mailed to an address the account no longer has). Limit `page`. |
| `POST /reset-password` (form `token`, `newPassword`, `confirmPassword`) | 200: password changed, and every device signed out. 400: the form again with the error (the passwords differ, a weak password), or link invalid. 503 / 429: the form again with `Retry-After` when the server is busy (the password hash queue, or the database stayed locked); the link stays valid. Limits `auth` and `auth_reset`. |
| `GET /confirm-email-change?token=` | 200: shows the new address and the account's name, with a "Use this e-mail address" button. 400: link invalid or expired (also when the account's address changed since the request). Limit `page`. |
| `POST /confirm-email-change` (form `token`) | 200: address changed. 400: link invalid, used or expired. 409: another account took the address in the meantime. 503 (`Retry-After: 1`): the database stayed locked; nothing changed and the link still works. Limit `auth`. |
| `GET /auth/sso/google/callback?code=&state=` | 200: "You can go back to Scacelith" (or "choose your username"). 400: the sign-in failed, was cancelled or expired. Never shows a token. Limit `sso_page`. |

## 15. Health endpoints

On the API port, before authentication and every endpoint limit; like any request they take a
token of the per-address layer (section 1.5); a monitoring host can be listed in `ABUSE_EXEMPT`.
Both paths work for each endpoint:

| Endpoint | Answer |
|---|---|
| `GET` or `HEAD /healthz`, `/api/v1/healthz` | 200 `{ "status": "ok" }` while the process runs. |
| `GET` or `HEAD /readyz`, `/api/v1/readyz` | 200 `{ "status": "ready" }` when the worker accepts players; 503 `{ "status": "not_ready" }` while it starts or stops. |

On the metrics port (`METRICS_PORT`, 9464 on `METRICS_BIND`, 127.0.0.1 by default; plain HTTP,
keep it private):

- `GET /healthz`: `ok`.
- `GET /readyz`: `ready`, or 503 `not ready`, for the whole server (every shard ready, not shutting
  down).
- `GET /metrics`: the Prometheus metrics. With `METRICS_TOKEN` set, it needs
  `Authorization: Bearer <token>`.

```sh
curl -sS https://caissa.scacelith.com/healthz
curl -sS http://127.0.0.1:9464/readyz        # on the server itself
```
