# Scacelith dedicated server

The online multiplayer server of Scacelith: accounts, rated matchmaking, challenges and private
games, authoritative games with server clocks (the robots press the clock by themselves unless
`AUTO_PRESS_CLOCK=false`), the opponent's live gestures (head, hand) relayed without being
stored, one Elo per official time control, anti-cheat and reports. It is one Rust program,
`scacelith-server`, a single binary with its database (SQLite) built in. The game (the Windows
binary) is only a client of it; anyone can run a community server, and players choose the server
in the game's Options.

- Official server: `caissa.scacelith.com`, TCP port `443` (HTTPS API and WSS on the same port).
- Licence: GPL-3.0-or-later, for the source and for the binaries built from it (see
  [License](#license)).

## Features

- **Accounts**: registration with e-mail confirmation, password login with an optional second
  factor (authenticator app and recovery codes), optional Google sign-in, signed-in devices,
  password reset, change of e-mail address, a download of the account's data and its deletion.
- **Games**: matchmaking per time control, direct challenges and private codes, rematches,
  server clocks, draw offers and claims, resignation and abort, reconnection within a grace
  period, games in progress kept across restarts and crashes (game journal).
- **Ratings**: one Elo per official time control, identical to the game's own rules, a
  leaderboard, game history with PGN downloads and animated GIFs of games.
- **Fair play**: protocol cheats sanctioned automatically, engine analysis of finished games
  (Stockfish) feeding an integrity level for moderators, player reports, rating refunds for the
  victims of a banned cheater, and command-line moderation (`scacelith-server admin`).
- **Operations**: native TLS with certificate reload, protection per network address, a
  Prometheus metrics endpoint, JSON logs, systemd integration (readiness, reload, watchdog),
  online database backups.

## Documentation

| Document | Content |
|---|---|
| [docs/DEPLOY.md](docs/DEPLOY.md) | installing the server as a systemd service: build, account, configuration, TLS, firewall, logs, upgrades, backups and restore |
| [docs/CONFIG.md](docs/CONFIG.md) | every setting, with its default and range (generated from the code) |
| [docs/API.md](docs/API.md) | the HTTPS API: every endpoint with its answers, errors, rate limits and curl examples |
| [docs/PROTOCOL.md](docs/PROTOCOL.md) | the realtime WebSocket protocol, version 1 (frozen) |
| [docs/DESIGN.md](docs/DESIGN.md) | how the server works: flows, contracts, game policies, security model |
| [docs/RUST-PORT.md](docs/RUST-PORT.md) | architecture and code layout: process model, crates and modules, interfaces |
| [docs/ANTICHEAT.md](docs/ANTICHEAT.md) | anomalies, sanctions, rating refunds, engine analysis, the statistical model, reports, moderation |
| [docs/SIZING.md](docs/SIZING.md) | sizing and hosting on a small VPS: capacity, memory, disk, settings |
| [docs/BENCHMARK.md](docs/BENCHMARK.md) | load tests and measured results |

## Requirements

- To build: Rust 1.99.0 (`rust-toolchain.toml` selects it with rustup) and a C compiler (for the
  bundled SQLite and *ring*). The static release binary also needs the
  `x86_64-unknown-linux-musl` target and a musl C compiler (Debian and Ubuntu: `musl-tools`).
- To run: Linux (the release binary is a static x86-64 executable that runs on any x86-64 Linux,
  whatever its C library); the example service unit needs systemd 253 or later
  ([docs/DEPLOY.md](docs/DEPLOY.md) covers older versions). One CPU core per game shard: by
  default one shard per core, up to 16.
- A TLS certificate for the server's public name (see [TLS certificates](#tls-certificates)).
- Optional: the official Stockfish 19 binary for the anti-cheat analysis, the engine it is
  calibrated for (`ANALYSIS_ENGINE_PATH`; see [Moderation and anti-cheat](#moderation-and-anti-cheat)),
  and an SMTP account for e-mail confirmation and password resets.

## Build

```sh
cd dedicated-server
cargo build --release --locked -p scacelith-server
# -> target/release/scacelith-server, linked to the build machine's glibc

CC_x86_64_unknown_linux_musl=musl-gcc \
  cargo build --release --locked -p scacelith-server --target x86_64-unknown-linux-musl
# -> target/x86_64-unknown-linux-musl/release/scacelith-server, static
```

The binary is all the server needs: the migrations, the GIF fonts and pieces and the list of
common passwords are built into it. [docs/DEPLOY.md](docs/DEPLOY.md) installs it as a service.

## Quick start

```sh
cd dedicated-server
cp .env.example .env                              # never commit .env
target/release/scacelith-server gen-secret        # paste the value as SERVER_SECRET in .env
# edit .env: SERVER_NAME, SERVER_PUBLIC_HOST, TLS_CERT_FILE, TLS_KEY_FILE, mail settings...
target/release/scacelith-server check-config      # prints the configuration (secrets hidden) or the errors
target/release/scacelith-server start
```

`start` applies the database migrations, replays the game journal (games that were running when
the server stopped come back), starts one game shard per core and listens on `API_PORT` (443 by
default; a port below 1024 needs a capability, see [Ports and firewall](#ports-and-firewall)).
`SIGTERM` or Ctrl-C stops it gracefully: players are warned, finished games are committed and
running games stay in the journal, to resume at the next start. `SIGHUP` reloads the TLS
certificate.

| Command | Effect |
|---|---|
| `scacelith-server [start]` | runs the server in the foreground (the default command) |
| `scacelith-server migrate` | applies the database migrations, prints a JSON report and exits |
| `scacelith-server check-config` | validates the configuration and prints it with the secrets hidden (warnings on stderr) |
| `scacelith-server gen-secret` | prints a new random value for `SERVER_SECRET` (48 random bytes, base64) |
| `scacelith-server gen-config-docs [--check]` | writes `.env.example` and `docs/CONFIG.md` from the configuration keys (run it in `dedicated-server/`) |
| `scacelith-server admin ...` | moderation and maintenance commands (`admin --help`) |
| `scacelith-server version`, `help` | the version, the list of commands |

Exit codes: 0 success, 1 failure (an invalid configuration included), 2 usage.

## Configuration

Configuration comes from the environment, then from the `.env` file of the working directory, or
from the file named by `SCACELITH_ENV_FILE` (set but empty: no file). `.env.example` lists every
key with its default and is the only configuration file in Git; [docs/CONFIG.md](docs/CONFIG.md)
describes each one. Secrets can also be read from files with the `_FILE` suffix
(`SERVER_SECRET_FILE=/etc/scacelith/server-secret`), which keeps them out of the environment.
Keys of the former Node.js server that no longer apply get a warning from `check-config`, never
an error.

## Ports and firewall

| Port | Default | Open to | Purpose |
|---|---|---|---|
| `API_PORT` | 443/tcp | the Internet | HTTPS API (`/api/v1/...`), the pages of e-mail links, and the game WebSocket (`wss://host/ws`) |
| `WS_PORT` | same as `API_PORT` | the Internet | set it only to put the WebSocket on its own port |
| `METRICS_PORT` | 9464/tcp on 127.0.0.1 | your monitoring only | Prometheus metrics, `/healthz`, `/readyz` |

When a NAT or a proxy publishes other port numbers than the ones the server listens on, set
`PUBLIC_API_PORT` / `PUBLIC_WS_PORT` to what the players must use; the server announces them in
`GET /api/v1/info`.

443 is the HTTPS port: firewalls and proxies of schools, companies and hotels let it through,
where they often block other ports. Any free port works for a community server (players then type
it in the game's Options with the host). On Linux, only a process with the `CAP_NET_BIND_SERVICE`
capability (root has it) may listen on a port below 1024. Do not run the server as root; give the
capability instead, in one of these ways:

- systemd (recommended): `AmbientCapabilities=CAP_NET_BIND_SERVICE` and
  `CapabilityBoundingSet=CAP_NET_BIND_SERVICE` in the unit, as the example unit does (see
  [Running as a service](#running-as-a-service));
- on the binary: `sudo setcap cap_net_bind_service=+ep /usr/local/bin/scacelith-server` (again
  after each upgrade of the binary);
- for the whole machine: `sysctl -w net.ipv4.ip_unprivileged_port_start=443` (and the same line in
  `/etc/sysctl.d/`), which lets every user bind 443 and above.

Without one of them the start fails with an error that lists these fixes (`Cannot listen on port
443 (EACCES): ...`) and exit code 1. A port of 1024 or above needs none of them.

## TLS certificates

Every connection between the game and a server is encrypted: HTTPS for the API and WSS for games.
The game refuses plain HTTP/WS except for a development server on the same machine (localhost).
There are three ways to provide the certificate.

### 1. A certificate from a public authority (recommended)

Use a certificate issued for the name players type in the game (`SERVER_PUBLIC_HOST`), for
example by Let's Encrypt, or the one your provider gives you. The game then trusts it through
Windows' certificate store with no setting on the player's side.

```ini
TLS_MODE=native
TLS_CERT_FILE=/etc/scacelith/tls/fullchain.pem
TLS_KEY_FILE=/etc/scacelith/tls/privkey.pem
TLS_MIN_VERSION=TLSv1.2
```

- `TLS_CERT_FILE` is the PEM **full chain**: the server certificate first, then the intermediate
  certificates (Let's Encrypt's `fullchain.pem`). A file holding only the server certificate
  works in browsers that fetch missing intermediates but fails in the game.
- `TLS_KEY_FILE` is the PEM private key (RSA 2048+ or ECDSA P-256/P-384). Keep it readable only by
  the account running the server (`chmod 600`, or `640` with the service group) and outside the
  Git checkout. `TLS_KEY_FILE_FILE` is refused: the key is already read from a file.
- Renewal needs no restart. The server checks both files every 10 seconds and reloads them after
  a change (it follows symbolic links, so certbot's swaps are seen), and reloads them on `SIGHUP`
  (`systemctl reload scacelith-server` with the example unit). A certificate it cannot load is
  refused and logged; the current one stays in use.
- Check it from another machine:
  `openssl s_client -connect caissa.scacelith.com:443 -servername caissa.scacelith.com </dev/null`
  must show the full chain and `Verify return code: 0 (ok)`, and
  `curl https://caissa.scacelith.com/api/v1/info` must answer without `-k` (add the port,
  `https://host:8443/...`, for a server on another port).

With Let's Encrypt and certbot, the files under `/etc/letsencrypt/live/` are readable by root
only: [docs/DEPLOY.md](docs/DEPLOY.md) (section 4) installs a deploy hook,
`deploy/systemd/certbot-deploy-hook.sh`, that copies them for the service account and reloads the
server after each renewal.

### 2. A self-signed certificate (private or LAN servers)

For a server among friends without a domain name, a self-signed certificate works if each player
pins it in the game:

```sh
openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes -days 825 \
  -subj "/CN=192.168.1.20" -addext "subjectAltName=IP:192.168.1.20" \
  -keyout privkey.pem -out fullchain.pem
chmod 600 privkey.pem
openssl x509 -in fullchain.pem -outform der | sha256sum     # the fingerprint to give to players
```

The name in `subjectAltName` must be what players type (`DNS:name` for a domain, `IP:address` for
an address). In the game, Options > Online server > custom server: host, port, and the
certificate fingerprint (the 64 hexadecimal characters printed above; the `AB:CD:...` form
printed by `openssl x509 -noout -fingerprint -sha256` is accepted too). The game then accepts
that exact certificate and nothing else for this server. A new certificate means a new
fingerprint that every player must enter again, so give it a long validity.

### 3. Behind a reverse proxy

When nginx, HAProxy, Caddy or a load balancer already terminates TLS, set `TLS_MODE=proxy`: the
server then listens in plain text and must only be reachable from the proxy (bind it to a private
address with `BIND_ADDRESS`, or firewall it). `TRUSTED_PROXIES` lists the proxy addresses whose
`X-Forwarded-For` header is believed; without it, every player would share the proxy's address
and the per-address limits would treat them as one client, and block them together (see
[Protection against abuse](#protection-against-abuse)). The proxy must pass WebSocket
upgrades on `/ws` and keep idle connections for more than a minute:

```nginx
location / {
    proxy_pass http://10.0.0.5:8443;   # API_PORT of the server (a port above 1024 needs no capability)
    proxy_http_version 1.1;
    proxy_set_header Upgrade $http_upgrade;
    proxy_set_header Connection $connection_upgrade;   # map $http_upgrade $connection_upgrade { default upgrade; '' close; }
    proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
    proxy_set_header X-Forwarded-Proto https;
    proxy_read_timeout 120s;
}
```

The proxy listens on 443 and the server behind it on any port: set `API_PORT` to that port
(8443 above) and `PUBLIC_API_PORT=443` (and `PUBLIC_WS_PORT=443`), so that the server announces the
port the players reach. The server then does no TLS work, so the handshake limits
(`MAX_PENDING_HANDSHAKES`, see [Kernel settings](#kernel-settings-linux)) do not apply, nor do
the per-address connection limits `IP_CONN_RATE` and `IP_MAX_CONNECTIONS`: limit the handshakes
and the connections per client on the proxy.

`TLS_MODE=off` exists for local development only and is refused unless `ALLOW_INSECURE_DEV=1`.

### The official server

`caissa.scacelith.com` (port 443) uses option 1 with a certificate provided by the server operator;
the game has this address built in as its default server. Its configuration sets at least
`SERVER_PUBLIC_HOST=caissa.scacelith.com`, `TLS_MODE=native`, `TLS_CERT_FILE` and `TLS_KEY_FILE`
(`API_PORT` keeps its default, 443). Its former port was 44664: the game moves a sign-in saved for
`caissa.scacelith.com:44664` to the new address by itself.

## Running as a service

[docs/DEPLOY.md](docs/DEPLOY.md) installs the server with the example files of
[`deploy/systemd/`](deploy/systemd/): a static `scacelith` account, `/etc/scacelith/` for the
configuration and the TLS files, `/var/lib/scacelith` as `DATA_DIR`, and a sandboxed
`Type=notify-reload` unit. With it, `systemctl start` returns once the server is ready (database
migrated, games recovered, ports bound), `systemctl reload` reloads the certificate,
`systemctl stop` drains the players, and the systemd watchdog restarts a server whose game hosts
or lobby stop answering. The unit gives the process `CAP_NET_BIND_SERVICE` and no other
capability, `LimitNOFILE=1048576` (it must exceed `MAX_CONNECTIONS`) and a private `/tmp`. Logs go
to the journal, one JSON object per line, with their priority.

## Kernel settings (Linux)

After a restart every client reconnects within a short time. New connections wait in a kernel
queue until the server accepts them; `LISTEN_BACKLOG` (2048 by default) sets its length, but the
kernel caps it at `net.core.somaxconn` (4096 since Linux 5.4, 128 on older kernels), and
connections still in their TCP handshake wait in a second queue bounded by
`net.ipv4.tcp_max_syn_backlog`. Raise both; on a server for more than about 50,000 players, use
8192 and 16384 with `LISTEN_BACKLOG=8192`:

```sh
# /etc/sysctl.d/90-scacelith.conf, applied with: sysctl --system
net.core.somaxconn = 4096
net.ipv4.tcp_max_syn_backlog = 8192
# Only for a server port inside 32768-60999 (here a custom API_PORT=44664):
# net.ipv4.ip_local_reserved_ports = 44664
```

With a stateful firewall on the machine (nftables or iptables rules on `ct state`, ufw,
firewalld), every player connection also takes an entry of the kernel's connection tracking
table, whose size `net.netfilter.nf_conntrack_max` scales with the RAM (65,536 on many 4 GB
machines), and a full table drops packets: set it to at least twice the connections you expect,
or exempt the game port from tracking (`notrack`). Without such a firewall there is nothing to
do. The server needs no other kernel setting ([docs/SIZING.md](docs/SIZING.md#system)).

The default port 443 needs nothing more. A custom port inside Linux's default range of ephemeral
ports (32768-60999, `net.ipv4.ip_local_port_range`) must also be reserved, as the commented line
shows: while the server is stopped, any outgoing connection of the machine (a DNS query, a
download, the SMTP relay) may get that port as its local port, and the restart then fails with
`EADDRINUSE`. Reserve `WS_PORT` as well when it differs from `API_PORT` and lies in that range (a
comma-separated list).

The server protects itself during such a reconnection storm. The game spreads the reconnections
of a restart over about half a minute (players with a game in progress within 8 s). With native
TLS, a new connection first has 3 s to send the start of its TLS handshake (the ClientHello), and
holds no handshake slot while it waits; a connection that stays silent is closed. The server then
performs at most `MAX_PENDING_HANDSHAKES` TLS handshakes at a time (128 × `WORKERS` by default), and
at most `MAX_PENDING_HANDSHAKES_PER_IP` for one address group, an IPv4 address or an IPv6 /48 (by
default a 32nd of `MAX_PENDING_HANDSHAKES`, at least 2). A connection beyond that is closed at
once, before any TLS work, and the game retries after a random delay. The CPU then completes the
handshakes in turn instead of starting all of them together and finishing none before the clients
give up. Raise `MAX_PENDING_HANDSHAKES_PER_IP` when many players share one address group (a school
or a company network), and `PASSWORD_HASH_WAITERS_PER_SOURCE` if they log in together while the
server is busy (see [Password hashing](#password-hashing)). These limits stop a few hosts from
blocking everyone, not a distributed attack. Games that were running when the server stopped come
back from the journal, and both players then have `RECOVERY_GRACE_MS` (90 s) to reconnect instead
of the normal grace. The clock of the side to move stays stopped until that player is back, for
`RECOVERY_CLOCK_HOLD_MS` (20 s) at most, so coming back within that time costs them nothing. After
it their clock runs again even while they are away, and the rest of their reconnection time is
charged to it.

`MAX_CONNECTIONS` (200,000) counts the signed-in players of the whole server; set it to what the
machine holds (see [Scaling](#scaling)). Beyond it, a newcomer
still completes the TLS handshake and the WebSocket upgrade, then is refused when it logs in
(`ServerFull`), and the game waits 60 to 120 s before it tries again. A player whose game is in
progress is still let in, so that the game can go on. So that such a player can reach the login,
the WebSocket upgrades may use a reserve of max(16, 2 %) connections beyond `MAX_CONNECTIONS`, and
the server does not shed load at `MAX_CONNECTIONS` itself, which would make them compete with the
newcomers to get through. Watch `scacelith_ws_hello_total{result="server_full"}` on the metrics
endpoint: it counts the newcomers refused at login, and it is the sign that the server is full.
The server sheds load only for up to 5 s after an upgrade was refused because the reserve was in
use as well (HTTP 503 `server_full`), or while it holds 1.2 times `MAX_CONNECTIONS`: with native
TLS the listener that carries the WebSocket upgrades then lets only half of
`MAX_PENDING_HANDSHAKES` new connections per second through and closes the other attempts before
any TLS work (`scacelith_tls_refused_total{reason="server_full"}`). On the default shared port the
API is slowed down with the upgrades; setting `WS_PORT` to another port keeps the API port outside
the limit, the better layout for a server that expects to be full.

## Accounts, e-mail and Google sign-in

- `REGISTRATION=open|closed`, `REQUIRE_EMAIL_VERIFICATION`, username and password rules: see
  [docs/CONFIG.md](docs/CONFIG.md). With e-mail confirmation (the default) an account is created
  only when the link sent to its address is used (within 24 h; the username is held meanwhile), so
  that registering never tells whether an address already has an account; without it the account
  is created at once.
- E-mail: `MAIL_TRANSPORT=smtp` with `SMTP_HOST`, `SMTP_PORT`, `SMTP_USER`, `SMTP_PASSWORD` and
  `SMTP_SECURITY` (`starttls` on 587 by default, `tls` for implicit TLS on 465; with `starttls`
  the upgrade is mandatory). `MAIL_TRANSPORT=log` (the default) writes the messages to the log
  instead (development), `none` disables e-mail (then turn e-mail confirmation off).
- Google sign-in is optional and off by default. The game opens Google's page in the system
  browser, and Google sends the browser back to the game itself, on 127.0.0.1 (no page of this
  server is involved). The client secret stays on the server: the game never sees it. Google
  sign-in never opens an existing account without its password: when a player's Google address
  is the address of an account with a password, the game asks that password (and the two-step
  code when it is on) once, then links Google to the account. To set it up, in the Google Cloud
  console:
  1. open Google Auth Platform (create a project first if needed);
  2. Branding: the application name, a support address and your domain;
  3. Audience: user type External, then "Publish app" so that its status is In production (in
     Testing, only the test users listed there can sign in);
  4. Data access: the scopes `openid`, `.../auth/userinfo.email` and `.../auth/userinfo.profile`
     only;
  5. Clients > Create client > application type **Desktop app**. It has no redirect URI to enter:
     the game's address on 127.0.0.1 is accepted for this type;
  6. set `SSO_GOOGLE_ENABLED=true`, `GOOGLE_CLIENT_ID` (the client ID) and the client secret in
     `GOOGLE_CLIENT_SECRET_FILE` (a file of mode 0600; `GOOGLE_CLIENT_SECRET` also works).

  `SERVER_PUBLIC_HOST` must be the name players type when they add the server (with
  `PUBLIC_API_PORT`, or `API_PORT`): Google sign-in works only for players who added it under
  exactly that name and port, and the game refuses it under another name or address.
- Every server is a separate trust boundary: the game keeps one login per server address and
  never sends a server the credentials or tokens of another one.
- Players manage their account from the game through the HTTPS API (every endpoint:
  [docs/API.md](docs/API.md)): game history and PGN downloads, signed-in devices, password,
  two-step verification, e-mail address, a download of their data and the deletion of the
  account. A change of e-mail address is confirmed through a link sent to the new address (with
  `REQUIRE_EMAIL_VERIFICATION`; without it the address changes at once), and the former address
  is told. The data download holds no password hash, two-step secret, token or anti-cheat data.

## Animated GIFs of games

Signed-in players can download any game of the server as an animated GIF
(`GET /api/v1/games/:id/gif`), and any game they have as PGN text (`POST /api/v1/gif`): a 2D
board seen from above, one frame per move, with the players' names and ratings and the result
([docs/API.md](docs/API.md#get-gamesidgif)). The server draws them itself, with no external
program, on rendering threads of their own (`GIF_THREADS`, `WORKERS` by default) at the lowest
CPU priority, so the games never wait for a GIF and a GIF only takes the CPU the games leave. It
keeps the recent ones in memory (`GIF_CACHE_MB`, 32 × `WORKERS` MB by default) and limits each
account to 4 GIFs a minute and 30 an hour (`GIF_USER_RENDERS_*`), each address to 12 and 120
(`GIF_IP_RENDERS_*`); a GIF from the cache does not count. `GIF_ENABLED=false` turns the feature
off. Every `GIF_*` setting: [docs/CONFIG.md](docs/CONFIG.md); memory and CPU:
[docs/SIZING.md](docs/SIZING.md#animated-gifs).

The pictures use two works shipped in `assets/` and built into the binary, under their own
licences:

- `assets/pieces/cburnett/`: the "cburnett" chess pieces by Colin M.L. Burnett, GPL version 2
  or later, as distributed with lichess (details in `assets/pieces/cburnett/LICENSE.md`). The
  server is GPL-3.0-or-later, so the combination is distributed under the GPL version 3 or later.
- `assets/fonts/scacelith-gif/`: "Scacelith GIF", bitmap subsets of Terminus Font 4.49.1 by
  Dimitar Toshkov Zhekov, modified (a zero without a slash) and renamed as the licence asks of
  modified versions; SIL Open Font License 1.1 (`assets/fonts/scacelith-gif/OFL.txt`). They were
  made from the Terminus Font 4.49.1 sources by `tools/gen-gif-font.js` of the former Node.js
  server (in Git history, commit 7531830).

## Password hashing

Every login, registration, password change or reset and every account change that asks for the
password computes an Argon2id hash (64 MiB, 3 passes), which takes one core for about 0.17 s on
a 2.1 GHz Xeon vCPU and about 0.3 s on a VPS vCore ([docs/SIZING.md](docs/SIZING.md#password-logins)),
on a thread of its own. The whole server runs at most `PASSWORD_HASH_CONCURRENCY` of them
at once (`WORKERS` by default); up to `PASSWORD_HASH_QUEUE_MAX` more wait their turn (32 ×
`WORKERS` by default). All the hashes of one request wait at most `PASSWORD_HASH_QUEUE_TIMEOUT_MS`
(10 s) together: a password change, which checks the current password and then hashes the new
one, does not wait twice. A request that finds the queue full, or whose wait ran out, is answered
HTTP 503 `server_busy` with a `Retry-After` of 5 to 15 seconds, and nothing changes on the
server: the player simply tries again a little later.

One client cannot take the whole queue. While less than half of the queue waits, one client may
queue as many hashes as it needs, so a class or a club that logs in at the same moment behind one
IPv4 address is served in turn. Once half of the queue waits, an IPv4 address, or an IPv6 /48,
may have at most `PASSWORD_HASH_WAITERS_PER_SOURCE` hashes waiting (2 × `WORKERS` by default), and
its next request is answered 429 `rate_limited` (the game shows its usual "Too many attempts"
message with the delay). Such a refusal does not use up one of the client's `AUTH_RATE_PER_IP`
attempts. The per-address limit of the password endpoints (`AUTH_RATE_PER_IP`, 20 per 10 minutes
for an IPv4 address or an IPv6 /64) is also applied to each IPv6 /48 as a whole
(`AUTH_RATE_PER_PREFIX`, 5 times as much by default), because a single customer often gets a /56
or a /48.

Some of these endpoints have stricter limits of their own: 10 registrations per hour per address
(`AUTH_REGISTER_PER_HOUR`: raise it before a class creates its accounts together), 3 password
reset e-mails per hour and 10 per day (`AUTH_FORGOT_PER_HOUR`, `AUTH_FORGOT_PER_DAY`), 10
confirmation e-mails sent again and 10 new passwords from reset links per hour, each with 3 times
as much per IPv6 /48; 10 two-step codes per 15 minutes for one account (`AUTH_MFA_PER_ACCOUNT`)
and 10 password re-checks per 10 minutes (`AUTH_REAUTH_PER_USER`), whatever the address. A
signed-in player also has a budget of its own, `USER_RATE_PER_MIN` (120) requests a minute across
the API from any address. Every limit:
[docs/API.md](docs/API.md#15-rate-limits-and-other-throttles).

A stored hash with weaker parameters is upgraded by the login with the password it just checked,
but only when a hash slot is free at once, and only while the stored hash did not change
meanwhile; a login whose password was replaced while it was being checked fails, so a password
reset always wins, also against a login that is waiting for its two-step verification code. A
failed login is held until it took as long as the slowest password check of the last 10 to 20
minutes, and at least as long as the check the server measured when it started (at most 2 s), so
that its time does not reveal whether the e-mail address or user name has an account.

- Raise `PASSWORD_HASH_CONCURRENCY` only when the machine has idle cores: each hash in flight
  keeps a core busy; `check-config` warns when it is above the number of cores.
- `PASSWORD_HASH_QUEUE_TIMEOUT_MS` is at most 13000: the game gives up after 15 s, so that a
  refused player gets the "busy" answer rather than a timeout.
- `POW_LOGIN_TRIGGER_PER_MIN` (30) turns the login proof of work on during a credential-stuffing
  wave. Each failed login costs a hash, so 30 a minute already keep about a sixth of a VPS vCore
  busy, and a trigger of a few hundred would take the whole CPU of a small machine.
- On the metrics endpoint, `scacelith_password_hash_queued`, `scacelith_password_hash_wait_ms`
  and `scacelith_password_hash_rejected_total` show the queue. Regular refusals with the reasons
  `queue_full` or `timeout` outside an attack mean the machine needs more cores, not a higher
  cap; the reason `source_limit` counts the clients held back to their
  `PASSWORD_HASH_WAITERS_PER_SOURCE` waiting hashes while the queue was at least half full.

## Protection against abuse

The server protects itself in two layers. The first, described here, works per network address:
it meets every request and every connection before anything else (before the API routes, before
the login, and with native TLS before any TLS work), and only stops one address from saturating
the server. The second works per signed-in account and per route (the limits in
[docs/API.md](docs/API.md)). An address is an IPv4 address or an IPv6 /64; an IPv6 address also
counts toward its /48 with 4 times each limit (all of them but `MAX_CONNECTIONS_PER_IP`, which
counts per /64 only), because one customer often gets a /56 or a /48 and could otherwise rotate
over its /64 networks. The limits are those of the whole server, counted exactly in its one
process.

| Setting | Default | Beyond it |
|---|---|---|
| `HTTP_RATE_PER_IP` | 600 requests per minute, any path (API, pages, health checks, unknown paths, WebSocket upgrades), with a burst of half a minute | 429 `rate_limited` with `Retry-After` |
| `HTTP_RATE_PER_PREFIX` | 4 × `HTTP_RATE_PER_IP` for an IPv6 /48 | the same |
| `IP_MAX_INFLIGHT` | 32 × `WORKERS` requests in progress | the same |
| `IP_CONN_RATE` | 10 new connections per second, with a burst of 4 seconds | the connection is reset before TLS |
| `IP_MAX_CONNECTIONS` | 128 open connections (TLS handshakes, API keep-alive and WebSockets together) | the same |
| `MAX_CONNECTIONS_PER_IP` | 64 WebSocket connections (per /64, no count per /48: the /48's upgrades still take its request budget, and with native TLS its connections the /48 count of `IP_MAX_CONNECTIONS`) | 429 `too_many_connections` at the upgrade |
| `ABUSE_BLOCK_REFUSALS_PER_MIN` | 600 refusals in a minute block the address (4 times that for a /48) | blocked for `ABUSE_BLOCK_BASE_SEC` (60 s), 4 times longer at each new block within 6 hours, up to `ABUSE_BLOCK_MAX_SEC` (1 h) |

The refusals that count toward a block are those that show a client ignoring the limits: the
429s of the request budget and the in-flight cap, the connections reset before TLS, failed TLS
handshakes and malformed HTTP, plus the 429s of the API's per-address route limits (a refused
login, registration, password reset or second-factor attempt counts 5). An address that reaches
the threshold within one second is blocked at once; otherwise the counts are added up over a
sliding minute, once a second. A blocked address has its new connections reset before any TLS
work, and its requests on connections already open get 429 with the time left and
`Connection: close`. WebSocket connections that are already open are never closed by a block, so
the players of a school or of a mobile operator who share the address with an abuser keep their
games; a player whose connection drops can only come back when the block ends.
`ABUSE_BLOCK_REFUSALS_PER_MIN=0` turns blocking off and keeps the limits.

Slow clients are cut as well: a request head must arrive within 10 s and a request body within
10 s of the start of its read (408), an idle kept-alive connection is closed after 6 s, and while
an answer is sent, 30 s without a byte in or out, or 60 s since the server produced it, close the
connection (game WebSockets have their own heartbeat instead). Malformed HTTP (400), oversized
heads (431) and head timeouts are counted in `scacelith_http_client_errors_total{reason}` and
toward a block of the address.

**Players who share one address.** A school, a club, a company network or a mobile operator's
carrier-grade NAT puts many players behind one IPv4 address. The defaults are sized for that: a
class of 30 browsing the menus at one request every 3 s each is 600 requests per minute, their
launch fits in the burst, and 50 players make about 150 connections. A real player over the budget
waits for the `Retry-After` the game shows, and does not come near 600 refusals a minute. For a
known network that plays together (a school's or a club's address, also your monitoring and a
load generator), list its address or subnet in `ABUSE_EXEMPT` (`203.0.113.7,2001:db8:12::/48`): it
then skips this whole layer, never blocked, while the login, registration and account limits
still apply to it. Raise `MAX_PENDING_HANDSHAKES_PER_IP`, `AUTH_RATE_PER_IP` and
`PASSWORD_HASH_WAITERS_PER_SOURCE` for it too if its players log in together. Keep
`IP_MAX_CONNECTIONS` at least twice `MAX_CONNECTIONS_PER_IP`; `check-config` warns otherwise.

**Behind a reverse proxy** (`TLS_MODE=proxy`) the request budget, the in-flight cap and the
blocks apply to the client named by `X-Forwarded-For`, and a block answers 429 without closing
the connection, which belongs to the proxy. The connection limits (`IP_CONN_RATE`,
`IP_MAX_CONNECTIONS`) and the reset before TLS cannot work there: limit the connections and
handshakes per client on the proxy (nginx: `limit_conn`, `limit_req`). Set `TRUSTED_PROXIES`
exactly: when the proxy is not trusted, every player shares its address, and the budget and a
block would hit them all together.

**Watching it.** The server logs each block at `warn` level: `ip blocked` with the address
(written as `LOG_IP` says), `scope` (`ip` or `prefix`), `blockLevel`, `ttlSec` and `refusals`.
On the metrics endpoint: `scacelith_http_rate_limited_total{limit}` (`ip`, `ip48`, `inflight`,
`blocked`), `scacelith_tls_refused_total{reason}` (`blocked`, `conn_rate`, `conn_open`),
`scacelith_abuse_blocks_total{scope,level}`, `scacelith_abuse_blocked{scope}`,
`scacelith_abuse_blocked_keys`, `scacelith_tls_connections_open` and `scacelith_http_inflight`.
Blocks at level 4 again and again from the same sources, or a flood from many addresses that no
per-address limit catches, belong to the provider's firewall:
[docs/SIZING.md](docs/SIZING.md#provider-firewall-the-ovh-edge-network-firewall) gives the OVH
Edge Network Firewall rules.

## Secrets

Nothing secret is ever committed: `.env`, keys and certificates are in `.gitignore`, and only
`.env.example` (empty secret values) is tracked. The secrets are:

| Key | What it protects |
|---|---|
| `SERVER_SECRET` | proof-of-work challenges, recovery codes, Google sign-in state, the hashed addresses of the logs (`LOG_IP=hashed`), and the TOTP secrets unless `MFA_ENCRYPTION_KEY` is set; at least 32 random bytes (`gen-secret`) |
| `MFA_ENCRYPTION_KEY` | TOTP secrets at rest (derived from `SERVER_SECRET` when empty; set it so that the secret can change without breaking two-factor logins) |
| `TLS_KEY_FILE` | the certificate's private key |
| `SMTP_PASSWORD`, `GOOGLE_CLIENT_SECRET` | mail account and Google OAuth client, used as written |
| `METRICS_TOKEN` | optional bearer token for the metrics endpoint |

Changing `SERVER_SECRET` logs nobody out, but it invalidates the recovery codes and, unless
`MFA_ENCRYPTION_KEY` is set, every enabled authenticator app (players would need an admin
`user reset-mfa`). Keep it stable and back it up, apart from the database copies. The TLS
session-ticket keys are random, kept in memory only and rotated by the TLS library, never
derived from `SERVER_SECRET`: after a restart each player makes one full handshake.

## Data, backups and upgrades

- `DATA_DIR` (default `./data`, created at start) holds the SQLite database (`scacelith.db`, WAL
  mode) and the game journal (`journal/`). Back up with
  `scacelith-server admin backup /path/to/new-file.db --verify`, run as the service account with
  the same configuration: it writes a consistent snapshot with `VACUUM INTO` while the server
  runs, creates the file with mode 600, refuses to overwrite one, and with `--verify` checks the
  copy. The copy holds e-mail addresses and recent IP addresses: encrypt it and move it off the
  host. Like every administration command, it refuses to run when `DB_PATH` (by default
  `DATA_DIR/scacelith.db`; a relative path is resolved from the current directory) is not an
  existing database, so a scheduled backup run from the wrong place fails instead of copying an
  empty one. [docs/DEPLOY.md](docs/DEPLOY.md) (section 10) has a backup timer and the restore
  procedure. Keep `SERVER_SECRET` and `MFA_ENCRYPTION_KEY` backed up too, but separately from the
  database copies (together they decrypt the players' TOTP secrets).
- Upgrading: stop the server, install the new binary, then `scacelith-server migrate` (or just
  `start`, which migrates first). Migrations are checksummed: the server refuses to start if an
  applied migration was modified or is unknown to its version (a downgrade).
- A crash loses at most the moves of the last `JOURNAL_FLUSH_MS` (50 ms) of the games in progress;
  finished games and rating changes are committed in database transactions.
- Players come back by themselves after a restart (see [Kernel settings](#kernel-settings-linux)).
- Retention: every `RETENTION_INTERVAL_MS` (one hour; the first run about a minute after the
  start) the server deletes expired and revoked sessions, expired tokens and pending signups,
  security events older than `RETENTION_SECURITY_DAYS` (90), non-certain anomalies of the same
  age, conduct events and failed analysis jobs older than 30 days, and erases stored IP addresses
  older than `RETENTION_IP_DAYS` (30). It works in small slices while the server runs, so that the
  games can write between them, and logs one `retention purge done` line with the counts. Games,
  ratings, analysed games, sanctions and reports are kept. The database overwrites deleted and
  erased data with zeros (`secure_delete`), so it does not stay readable in the file. SQLite reuses
  the freed pages; the file only shrinks after a `VACUUM` (server stopped). The full table is in
  the retention section of [docs/DESIGN.md](docs/DESIGN.md).
- The journal of each shard (`journal/shard-<n>/`) keeps about `JOURNAL_COMPACT_SEGMENTS + 1`
  segments of 16 MB (80 MB by default), however long the games last: a game still running after
  that many segments is rewritten as one snapshot record and its older segments are deleted. Plan
  about 100 MB of disk per shard for it; `scacelith_journal_disk_bytes` shows the actual size.
- When the journal cannot be written (a full disk, a failing volume),
  `scacelith_journal_errors_total` grows and the server logs `journal write failed`. Finished games
  still reach the database, rating changes included, after three failed journal flushes in a row:
  `scacelith_game_commit_unjournaled_total` counts them, with one error logged per episode. Free
  the space or fix the volume before a restart: a finished game whose end the journal lost would
  come back as a game in progress (the database keeps its first result).

## Moderation and anti-cheat

`scacelith-server admin` works on the database directly, also while the server runs: user lookup,
bans, MFA reset, session revocation, the integrity list and evidence, rating refunds, reports,
anomalies, statistics, backups, and the account of a pending signup whose confirmation mail never
arrived (`user verify-email <name>`, which does what the link would). Run it as the service
account with the server's configuration; `scacelith-server admin --help` lists the commands, and
[docs/ANTICHEAT.md](docs/ANTICHEAT.md) explains how suspicion levels are computed. Certain cheats
(a move for someone else's game, out of turn or illegal in a position both sides agree on, a
forged server message) forfeit the game and ban for `BAN_DURATION_HOURS` (24) automatically when
`AUTO_SANCTION_CERTAIN_CHEATS` is on; statistical suspicion never bans by itself. A ban for
cheating (that automatic one, or `integrity confirm` unless `--no-refund`) gives the cheater's
victims back the rating points they lost to them, games still in progress at the ban included; a
`user ban` is for anything else and refunds nothing (docs/ANTICHEAT.md, rating refunds).

The engine analysis is optional (an empty `ANALYSIS_ENGINE_PATH` turns it off). Use **Stockfish
19**, the official release: the anti-cheat is calibrated on it (its priors, its synthetic engine
profile and the default depths were measured with it), and its engines share one copy of their
evaluation network. Stockfish 16, or another UCI engine, still works, but the default depths and
the detection figures of docs/ANTICHEAT.md are not for it. The server starts the engines itself,
`ANALYSIS_WORKERS` of them, as child processes at the lowest CPU priority.

```sh
curl -LO https://github.com/official-stockfish/Stockfish/releases/download/sf_19/stockfish-linux-x86-64-universal.tar.gz
sha256sum stockfish-linux-x86-64-universal.tar.gz
# 9defc0d4e55d49c65a6d042f3e571a39fcea499ade6dbe741b53b8c65e03611f
tar xzf stockfish-linux-x86-64-universal.tar.gz
sudo install -m 755 stockfish/stockfish-linux-x86-64-universal /usr/local/bin/stockfish-19
stockfish-19 compiler | grep architecture      # the build it picked on this CPU
```

This one Linux binary holds every x86-64 build of Stockfish 19 and runs the best one for the CPU;
it needs glibc 2.35 or later. Compare the checksum with the digest the release page shows next to
the file too. Then set `ANALYSIS_ENGINE_PATH=/usr/local/bin/stockfish-19`, and `ANALYSIS_WORKERS`
as [docs/SIZING.md](docs/SIZING.md) suggests for your machine. Replace the binary with the server
stopped. The engines share their network through `/tmp/stockfish-<uid>`, so `/tmp` must be
writable and the same for all of them (the example unit's private `/tmp` is). Each engine start is
logged (`analysis engine started`, with `"network": "shared memory"`); an engine that could not
share while others run logs a warning instead. Details:
[docs/ANTICHEAT.md](docs/ANTICHEAT.md#engine).

One analysis engine (Stockfish 19 at the default depths 9/15) handles about 870 games a day on a
VPS vCore (540 to 1,260 depending on their length), far fewer than a busy server plays. Moderator
requests, games reported by credible players, and games of players already under suspicion
(integrity level, open credible report) or with a suspicious anomaly are analysed first, but one
engine claim in four still takes the oldest ordinary game, and at most 20 flagged games of one
player wait at a time (past that, a game with an anomaly of its own takes the place of a waiting
one that has none). Ordinary games are sampled (`ANALYSIS_SAMPLE_RATE`) and skipped while
`ANALYSIS_QUEUE_MAX` (5000) of them already wait; only they feed the population statistics the
players are compared with. If `scacelith_anticheat_analysis_queue_ordinary` stays at that cap, add
engines (`ANALYSIS_WORKERS`) or lower the sample rate. The statistics are kept per analysis
profile: changing the engine, its network, a depth or `ANALYSIS_HASH_MB` restarts them from the
priors (docs/ANTICHEAT.md, section 3).

## Monitoring

`http://127.0.0.1:9464/metrics` (Prometheus text format; `METRICS_TOKEN` adds a bearer token):
connections, messages, games, move latency, commits, journal, rate limits and the blocked
addresses ([Protection against abuse](#protection-against-abuse)), anti-cheat (including the
analysis backlog, the skipped games, and the analysis engines running and sharing their network:
`scacelith_anticheat_analysis_engines*`), the retention purge (`scacelith_retention_*`), the
process (`scacelith_process_*`: CPU, memory, file descriptors, threads), how late the async
runtime runs its timers (`scacelith_runtime_lateness_*`), the stalls of the game host actors and
the time given back to the players for them (`scacelith_game_stall_*`), and the gesture relay
(`scacelith_gestures_*`). `/healthz` answers while the process runs, `/readyz` while it accepts
players. The logs (stdout, one JSON object per line) are described in
[docs/DEPLOY.md](docs/DEPLOY.md) (section 7).

## Scaling

One process serves everything: `WORKERS` game shards (one per core by default, up to 16 with
`auto`, at most 64) share the threads of one async runtime, and the limits of the configuration
are those of the whole server. Several instances on one machine are independent servers, each
with its own accounts, data and ports ([docs/DEPLOY.md](docs/DEPLOY.md), section 11). Capacity
and memory: [docs/SIZING.md](docs/SIZING.md); load tests (`bench/run.sh`, the `scacelith-bench`
load generator) and their results: [docs/BENCHMARK.md](docs/BENCHMARK.md). Set
`MAX_CONNECTIONS` to what the machine really holds, so that a full server refuses newcomers at
login instead of running out of memory: its default of 200,000 needs about 5.3 GiB for the
connections alone, while a 2-vCore, 4 GB VPS holds about 60,000 and a 4-vCore, 8 GB one about
150,000. With the live gestures on, the CPU fills before the memory: the 2-vCore VPS holds about
9,300 games of calm players with the defaults, and about 26,000 with `GESTURE_RATE=0`
([docs/SIZING.md](docs/SIZING.md#recommended-settings)).

On a small machine, three settings are the idle cost you control, all announced to the game in
`Welcome`. `CLIENT_PING_INTERVAL_MS` (10 s) is how often the game pings the server for its ping
indicator and its estimate of the server clock; each ping costs server CPU for every connected
player. `GESTURE_RATE` (4 per second) bounds the live gestures (the head, the piece in hand and
where it is aimed) a client sends during a game, and the server relays each one to the opponent.
`GESTURE_IDLE_MS` (1 s) is the longest a client waits between two gestures while its player sits
still: these keepalives are most of the relay's work in a calm game. Raise `GESTURE_IDLE_MS` (up
to 10 s; the opponent's robot then notices later that the gestures stopped), lower
`GESTURE_RATE` to 2 or 1, or set it to 0 to turn the relay off, when the peak nears the machine's
capacity ([docs/SIZING.md](docs/SIZING.md#gestures)).

## Development

```sh
cd dedicated-server
cargo test --workspace                                   # unit, integration and end-to-end tests
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check
cargo doc --workspace --no-deps
cargo run -p scacelith-protocol --features gen --bin protogen -- --check   # generated protocol files
cargo run -p scacelith-server -- gen-config-docs --check                  # .env.example, docs/CONFIG.md
```

The workspace has five crates: `scacelith-server` (the server), `scacelith-protocol` (the realtime
protocol codec and its generator, `protogen`), `scacelith-chess` (rules and notation),
`scacelith-gif` (the GIF renderer) and `scacelith-client` (a Rust client used by the tests and the
load generator). The protocol schema, `protocol/scacelith-v1.json`, is the single source of the
Rust and C++ codecs: change it, run `protogen`, and commit the generated files with it. The
configuration keys live in `crates/server/src/config/keys.rs`: after a change, run
`gen-config-docs` (without `--check`). The real-engine analysis tests run when Stockfish is
installed where distributions put it, or named by `SCACELITH_TEST_ENGINE`, and return at once
otherwise.

Two tools check the server against its clients and its predecessor:

* `tools/live-check` (`scacelith-live-check`) runs the game's own C++ online client tests, Linux
  and Windows (Wine) builds, against a server started for each part, over real TLS
  ([its README](tools/live-check/README.md));
* `tools/rest-diff` (`scacelith-rest-diff`) replays the same scenarios against the former Node.js
  server and this one and reports every difference in the HTTPS API answers; the accepted ones,
  each with its reason, are in `tools/rest-diff/accepted.txt`
  ([its README](tools/rest-diff/README.md)).

## License

The dedicated server is free software under the GNU General Public License, version 3 or (at your
option) any later version (`GPL-3.0-or-later`, the `LICENSE` file at the root of the repository).
That covers its source and every binary built from it: whoever distributes a binary must also
offer its corresponding source under the same licence. The assets built into it keep their own
licences ([Animated GIFs of games](#animated-gifs-of-games)).
