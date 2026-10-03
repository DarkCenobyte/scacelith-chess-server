# scacelith-live-check

Live interoperability check: the game's C++ online client (`net::OnlineClient`, the opt-in
`net_live_*` tests of `build/scacelith_tests`) against this server, over real TLS. Each part starts
a server of its own and drives the C++ test that goes with it; the C++ test talks to the server
like the game does: native TLS with a pinned certificate, the HTTPS API and the WebSocket on one
port.

## Build and run

```sh
# The server and the harness (debug builds are enough)
cd dedicated-server && cargo build -p scacelith-server -p scacelith-live-check && cd ..

# The C++ tests (see the main README): Linux, and Windows cross-compiled
cmake -B build -G Ninja -DCMAKE_BUILD_TYPE=Release && ninja -C build scacelith_tests
cmake -B build-win -G Ninja -DCMAKE_TOOLCHAIN_FILE=cmake/mingw-w64-x86_64.cmake -DCMAKE_BUILD_TYPE=Release
ninja -C build-win scacelith_tests

# From the root of the source tree the tests were built from (they write their temporary
# credential files there)
dedicated-server/target/debug/scacelith-live-check                       # build/scacelith_tests
LC_ALL=C.UTF-8 WINEDEBUG=-all \
  dedicated-server/target/debug/scacelith-live-check wine build-win/scacelith_tests.exe   # WinHTTP
```

```text
scacelith-live-check [--only=game,account,sso] [--server=PATH] [--sso-http] [COMMAND...]
```

| Option | Meaning |
| --- | --- |
| `COMMAND...` | Runs the C++ tests (default `build/scacelith_tests`, relative to the current directory). The name of the C++ test of a part is appended to it. |
| `--only=LIST` | The parts to run, comma-separated (default `game,account,sso`, in that order). |
| `--server=PATH` | The `scacelith-server` binary of the game and account parts (default: the one next to the harness, `target/debug/scacelith-server`). |
| `--sso-http` | The sso part in plain HTTP on the loopback (the C++ client is then a development one, `insecureDev`) instead of TLS with a pinned certificate. |
| `LIVE_HOST` (environment) | The host name the C++ client connects to (default `localhost`). The test certificate names `localhost` and `127.0.0.1`, but Wine's WinHTTP only matches DNS names. |

It prints what each part does (`[game]`, `[account]`, `[sso]`, `[control]` for every request of
a C++ test to its control server), the C++ tests' output, then `== <part>: passed` or
`== <part>: FAILED (exit code N)`. Exit code: 0 when every part passed, else the first failing
part's (the C++ test's exit code, 1 when the harness itself failed, 127 when the command could
not run).

Under Wine, run in a UTF-8 locale (`LC_ALL=C.UTF-8`, see the main README); a message such as
`wine: failed to open L"C:\\windows\\syswow64\\rundll32.exe"` comes from Wine setting up its prefix
and does not matter.

## The parts

### game

A server with two shards, a self-signed certificate, proof of work on registration
(`POW_REGISTER_BITS=12`), no e-mail confirmation, no GIFs (`GIF_ENABLED=false`) and a gesture
keepalive of 2.5 s (`GESTURE_IDLE_MS=2500`). A bot of the client SDK (`scacelith_client::bot`)
registers `livebot`, queues in rated 3+2 and plays random legal moves. Then:

- `net_live_server_game` registers `cppplayer` (solving the proof of work), signs in, checks the
  keepalive the client read in `Welcome` (2500 ms), queues, plays 12 plies against the bot,
  resigns and checks the rating update;
- `net_live_account_server_settings` changes the player's address (refused for the bot's address:
  `email_taken`, then applied at once: `email_changed`) and asks for GIFs (`gif_disabled`). The
  harness checks the notice mailed to the former address in the server's log.

### account

The account API (`docs/API.md`) end to end: a server with one shard (every request reaches the
same GIF cache) and e-mail confirmation on, the mails read from its log (`MAIL_TRANSPORT=log`).
The harness registers and confirms three accounts (`cpp_account`, `rival_live`, `third_live`) as a
browser would, then plays through the realtime protocol, as the C++ player's account against the
rival: a rated win and a rated loss, a casual draw by agreement, a custom time control (7+1) won
by checkmate, a promotion (b7xa8=Q), an aborted game, and one game between the two other players.
Then `net_live_account_api` signs in with the C++ client and calls `fetchMyGames` (filters,
paging), `fetchGame`, `downloadPgn`, `downloadGameGif` / `renderPgnGif` (decoded; the per-account
render quota and a cache hit past it), `setAcceptChallenges`, `changeEmail`, `exportAccount`,
`fetchSessions` / `revokeSession`, the expired and revoked sessions, two-factor re-authentication
(a recovery code, an authenticator code) and `deleteAccount`.

Its control server (plain HTTP on 127.0.0.1, JSON answers):

| Route | Answer |
| --- | --- |
| `GET /state` | The accounts, password, the games played (`id`, `color`, `outcome`, `rated`, `category`, `plies`, `result`, `status`, `reason`, `termination`, `promotion`), the other game, the render quota. |
| `GET /mail?to=&subject=&after=` | The latest mail to `to` whose subject contains `subject`, once more than `after` were written (5 s at most): `{to, subject, text, count}`, else 404 `no_mail`. |
| `POST /confirm-email-change?to=` | The confirmation link mailed to `to` opened (GET) and its button pressed (POST): `{link, getStatus, status, changed, eventSaved}`. It answers once the change is saved among the security events (saved in batches a second after the first, `docs/DESIGN.md` section 7), so that the export that follows holds it. |
| `POST /expire-session` | The newest active session of the C++ player that the harness did not open expires now (in the database): `{sessionId, clientLabel}`. |
| `POST /revoke-session?id=` | That session revoked from another device (a new sign-in of the harness): `{status, body}` of `DELETE /auth/sessions/:id`. |
| `POST /challenge` | The rival challenges the C++ player: `{result}`, `delivered` (then declined), the refusal's error code (`UserUnavailable`), or `acked_not_delivered`. |
| `GET /totp?secret=` | A TOTP code the server has not seen used: `{code, step}`. |
| `GET /metric?name=` | One sample of the server's metrics, labels included: `{value}`. |

### sso

Google sign-in (`docs/API.md`, "Google sign-in"). The whole server runs in the harness's process
(`scacelith_server::embedded`, a hook of this workspace's tools) because Google's token and key
endpoints must be a local fake provider that signs the ID tokens with a test key, and no setting
may name them. Its public host is `LIVE_HOST` and its API port a fixed one, so that the origin tag
is the one the game computes for the server it connects to; e-mail confirmation is on. Its log
goes to a capture: the mails and the warnings are printed at the end (a failed Google sign-in
gives its reason only there).

`net_live_sso` signs in through the game's 127.0.0.1 listener; its browser opener asks the control
server for Google's answer. Scenarios: a new Google account (`SsoNeedsUsername`, then
`completeSso`), a login by Google subject, a link to a password account (a wrong password, then the
right one), a link with two-factor (a wrong code, then the right one), `sso_account_exists`, and a
stranger's link opened against the game's listener.

| Route | Answer |
| --- | --- |
| `GET /state` | `{host, apiPort, tag, clientId}`. |
| `GET /fake-authorize?url=&sub=&email=&name=&verified=&error=` | Google's answer to the authorization URL `url` for that account (`verified=false`: an address Google has not confirmed; `error=access_denied`: the user refused): a 302 to the redirect URI with `code`, `state`, `iss` and the parameters Google adds, its `Location` also as `{location}`. |
| `POST /seed-password-account` `{username, email, password?, mfa?}` | An account whose address is confirmed (without a password, one like a Google-made account); with `mfa`, two-step verification turned on through the API: `{userId, totpSecret?}`. |
| `GET /totp?username=` or `?secret=` | A TOTP code of an account seeded with `mfa`, or of a secret: `{code, step}`. |
| `GET /links` | The Google links stored, oldest first: `{links: [{provider, subject, userId, username, email, createdAt}]}`. |

## The C++ tests' environment

Each C++ test is skipped when its variable is not set, so a plain run of `scacelith_tests` never
needs a server. The pin is the lower-case hexadecimal SHA-256 of the server certificate (DER).

| Variable | Value | Test |
| --- | --- | --- |
| `SCACELITH_NET_LIVE` | `host:port:pin:username:password` | `net_live_server_game` |
| `SCACELITH_NET_LIVE_SETTINGS` | `host:port:pin:username:password:address in use:game id` | `net_live_account_server_settings` |
| `SCACELITH_NET_LIVE_ACCOUNT` | `host:port:pin:control port` | `net_live_account_api` |
| `SCACELITH_NET_LIVE_SSO` | `host:port:control port` | `net_live_sso` |
| `SCACELITH_NET_LIVE_SSO_PIN` | `pin` (unset: plain HTTP, `insecureDev`) | `net_live_sso` |

## Code

`src/main.rs` (command line, the parts, the C++ runs), `src/game.rs`, `src/account.rs`,
`src/sso.rs` (one part each), `src/control.rs` (the control server), `src/google.rs` (the fake
Google). The server of the game and account parts is the integration tests' harness
(`crates/server/tests/support`, included by path): the `scacelith-server` binary in a child
process, its JSON log collected.
