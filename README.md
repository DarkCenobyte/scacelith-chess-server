# Scacelith dedicated server

The online multiplayer server of Scacelith: accounts, rated matchmaking, challenges and private
games, authoritative games with server clocks (the robots press the clock by themselves unless
`AUTO_PRESS_CLOCK=false`), the opponent's live gestures (head, hand) relayed without being
stored, one Elo per official time control, anti-cheat and reports. It is a plain Node.js program
with no npm dependency. The game (the Windows binary) is only a client of it; anyone can run a
community server, and players choose the server in the game's Options.

- Official server: `caissa.scacelith.com`, TCP port `443` (HTTPS API and WSS on the same port).
- Design and contracts: [docs/DESIGN.md](docs/DESIGN.md). Every setting: [docs/CONFIG.md](docs/CONFIG.md).
  Anti-cheat: [docs/ANTICHEAT.md](docs/ANTICHEAT.md).
- HTTPS API, every endpoint with its answers, errors, rate limits and curl examples (sign-up and
  sign-in, two-step verification, account, game history, PGN, animated GIFs, data export,
  deletion):
  [docs/API.md](docs/API.md). The realtime WebSocket protocol: [docs/PROTOCOL.md](docs/PROTOCOL.md).
- Sizing and hosting on a small VPS (capacity, memory, disk, restarts, settings): [docs/SIZING.md](docs/SIZING.md).

## Requirements

- Node.js 22.13 or later (it uses the built-in `node:sqlite`). Node 24.7+ adds Argon2id password
  hashing; older versions use scrypt.
- A TLS certificate for the server's public name (see [TLS certificates](#tls-certificates)).
- Linux is the reference platform (Windows works for tests). One CPU core per game shard; the
  default is one shard per core, up to 16.
- Optional: the official Stockfish 19 binary for the anti-cheat analysis, the engine it is
  calibrated for (`ANALYSIS_ENGINE_PATH`; see [Anti-cheat engine](#anti-cheat-engine)), and an
  SMTP account for e-mail confirmation and password resets.

## Quick start

```sh
cd dedicated-server
cp .env.example .env                      # never commit .env
node bin/scacelith-server.js gen-secret   # paste the value as SERVER_SECRET in .env
# edit .env: SERVER_NAME, SERVER_PUBLIC_HOST, TLS_CERT_FILE, TLS_KEY_FILE, mail settings...
node bin/scacelith-server.js check-config # prints the configuration (secrets hidden) or the errors
node bin/scacelith-server.js start
```

`start` applies the database migrations, replays the game journal (games that were running when
the server stopped come back), starts one worker per shard and listens on `API_PORT` (443 by
default; a port below 1024 needs a capability, see [Ports and firewall](#ports-and-firewall)). `SIGTERM` or Ctrl-C stops it gracefully: running games are journaled and resume at the
next start. `npm test` runs the unit and integration tests.

Configuration comes from the environment, then from `.env` next to `package.json` (or the file
named by `SCACELITH_ENV_FILE`). `.env.example` lists every key with its default and is the only
configuration file in Git. Secrets can also be read from files with the `_FILE` suffix
(`SERVER_SECRET_FILE=/run/secrets/scacelith_secret`), which keeps them out of the environment.

## Ports and firewall

| Port | Default | Open to | Purpose |
|---|---|---|---|
| `API_PORT` | 443/tcp | the Internet | HTTPS API (`/api/v1/...`), e-mail and Google sign-in pages, and the game WebSocket (`wss://host/ws`) |
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
  `CapabilityBoundingSet=CAP_NET_BIND_SERVICE` in the unit, as in
  [Running as a service](#running-as-a-service-systemd-example);
- on the Node.js binary: `sudo setcap cap_net_bind_service=+ep "$(readlink -f "$(command -v node)")"`
  (every program run with that binary gets it, and a Node.js upgrade drops it: run it again);
- for the whole machine: `sysctl -w net.ipv4.ip_unprivileged_port_start=443` (and the same line in
  `/etc/sysctl.d/`), which lets every user bind 443 and above.

Without one of them the start fails, and each worker logs the error with these fixes (`Cannot listen
on port 443 (EACCES) ...`) and exits with a non-zero status. A port of 1024 or above needs none of
them.

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
- Renewal needs no restart. The server re-reads both files when they change (it also follows
  certbot's symlink swaps) and on `SIGHUP` (`systemctl reload scacelith` with the unit below).
  A broken new certificate is refused and logged; the previous one stays in use.
- Check it from another machine:
  `openssl s_client -connect caissa.scacelith.com:443 -servername caissa.scacelith.com </dev/null`
  must show the full chain and `Verify return code: 0 (ok)`, and
  `curl https://caissa.scacelith.com/api/v1/info` must answer without `-k` (add the port,
  `https://host:8443/...`, for a server on another port).

With Let's Encrypt and certbot, the files under `/etc/letsencrypt/live/` are readable by root
only. Either run a deploy hook that copies them for the service account:

```sh
certbot certonly --standalone -d play.example.org \
  --deploy-hook 'install -m 644 "$RENEWED_LINEAGE/fullchain.pem" /etc/scacelith/tls/fullchain.pem &&
                 install -m 640 -g scacelith "$RENEWED_LINEAGE/privkey.pem" /etc/scacelith/tls/privkey.pem'
```

or point `TLS_CERT_FILE` / `TLS_KEY_FILE` at the `live/` paths and give the service account read
access to them. `--standalone` needs port 80 free during the renewal; use `--webroot` or a DNS
challenge otherwise.

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
port the players reach. The server then does no TLS work, so the handshake limit
`MAX_PENDING_HANDSHAKES` (see [Kernel settings](#kernel-settings-linux)) does not apply, nor do
the per-address connection limits `IP_CONN_RATE` and `IP_MAX_CONNECTIONS`: limit the handshakes
and the connections per client on the proxy.

`TLS_MODE=off` exists for local development only and is refused unless `ALLOW_INSECURE_DEV=1`.

### The official server

`caissa.scacelith.com` (port 443) uses option 1 with a certificate provided by the server operator;
the game has this address built in as its default server. Its `.env` sets at least
`SERVER_PUBLIC_HOST=caissa.scacelith.com`, `TLS_MODE=native`, `TLS_CERT_FILE` and `TLS_KEY_FILE`
(`API_PORT` keeps its default, 443). Its former port was 44664: the game moves a sign-in saved for
`caissa.scacelith.com:44664` to the new address by itself.

## Running as a service (systemd example)

```ini
# /etc/systemd/system/scacelith.service
[Unit]
Description=Scacelith dedicated server
After=network-online.target
Wants=network-online.target

[Service]
User=scacelith
Group=scacelith
WorkingDirectory=/opt/scacelith/dedicated-server
EnvironmentFile=/etc/scacelith/scacelith.env
ExecStart=/usr/bin/node bin/scacelith-server.js start
ExecReload=/bin/kill -HUP $MAINPID
KillSignal=SIGTERM
TimeoutStopSec=45
Restart=on-failure
LimitNOFILE=1048576
# Port 443 (below 1024) without running as root: this capability only, nothing else.
AmbientCapabilities=CAP_NET_BIND_SERVICE
CapabilityBoundingSet=CAP_NET_BIND_SERVICE
NoNewPrivileges=true
ProtectSystem=strict
ReadWritePaths=/var/lib/scacelith
PrivateTmp=true

[Install]
WantedBy=multi-user.target
```

With `DATA_DIR=/var/lib/scacelith` in the environment file. `LimitNOFILE` must exceed
`MAX_CONNECTIONS`. `AmbientCapabilities=CAP_NET_BIND_SERVICE` lets the `scacelith` account listen
on 443; `CapabilityBoundingSet` keeps every other capability away from the process, and
`NoNewPrivileges` still applies (the capability is given at the start, not gained later). Leave
both lines out when `API_PORT` is 1024 or above. `TimeoutStopSec` must be at least
`SHUTDOWN_GRACE_MS` + 15 s (what the server gives its workers for their last database writes before
it kills them) + a few seconds to close the database: 45 s covers the default `SHUTDOWN_GRACE_MS`
(3 s) with room to spare; raise it with `SHUTDOWN_GRACE_MS`, or systemd kills the server before
its stop is over. This unit is an example: adapt the paths to your installation.

### Anti-cheat engine

The engine analysis of the anti-cheat is optional (an empty `ANALYSIS_ENGINE_PATH` turns it off).
Use **Stockfish 19**, the official release. The anti-cheat is calibrated on it: its priors, its
synthetic engine profile and the default depths were measured with Stockfish 19. At those depths
it needs less CPU per game than Stockfish 16 did at its former defaults, and its analysis engines
share one copy of their evaluation network, where each Stockfish 16 engine loads its own.
Stockfish 16, or another UCI engine, still works, but the default depths and the detection
figures of docs/ANTICHEAT.md are not for it.

```sh
curl -LO https://github.com/official-stockfish/Stockfish/releases/download/sf_19/stockfish-linux-x86-64-universal.tar.gz
sha256sum stockfish-linux-x86-64-universal.tar.gz
# 9defc0d4e55d49c65a6d042f3e571a39fcea499ade6dbe741b53b8c65e03611f
tar xzf stockfish-linux-x86-64-universal.tar.gz
sudo install -m 755 stockfish/stockfish-linux-x86-64-universal /usr/local/bin/stockfish-19
stockfish-19 compiler | grep architecture      # x86-64-bmi2 on an OVH vCore (Haswell)
```

This one Linux binary holds every x86-64 build of Stockfish 19 and runs the best one for the CPU
(the release has no separate Linux file per CPU); it needs glibc 2.35 or later. Compare the
checksum with the digest the release page shows next to the file too. Then set
`ANALYSIS_ENGINE_PATH=/usr/local/bin/stockfish-19`, and `ANALYSIS_WORKERS` as
[docs/SIZING.md](docs/SIZING.md) suggests for your machine. Replace the binary with the server
stopped.

The engines share their network through a directory they create in `/tmp` (`/tmp/stockfish-<uid>`),
so `/tmp` must be writable and the same for all of them. The unit above gives the service a
private, writable `/tmp` (`PrivateTmp=true`); with `ProtectSystem=strict` and no `PrivateTmp`, add
`ReadWritePaths=/tmp`. Each engine start is logged (`analysis engine started`, with `"network":
"shared memory"`). An engine that could not share while others run logs a warning instead
(`analysis engine started with its own copy of the network`, with Stockfish's reason), and then
takes about 110 MB more. In the metrics, `scacelith_anticheat_analysis_engines_shared` equals
`scacelith_anticheat_analysis_engines` when every engine shares. Details:
[docs/ANTICHEAT.md](docs/ANTICHEAT.md#engine).

### Kernel settings (Linux)

After a restart every client reconnects within a few seconds. New connections wait in a kernel
queue until the server accepts them; `LISTEN_BACKLOG` (2048 by default) sets its length, but the
kernel caps it at `net.core.somaxconn` (4096 since Linux 5.4, 128 on older kernels), and
connections still in their TCP handshake wait in a second queue bounded by
`net.ipv4.tcp_max_syn_backlog`. Raise both:

```sh
# /etc/sysctl.d/90-scacelith.conf, applied with: sysctl --system
net.core.somaxconn = 4096
net.ipv4.tcp_max_syn_backlog = 8192
# Only for a server port inside 32768-60999 (here a custom API_PORT=44664):
# net.ipv4.ip_local_reserved_ports = 44664
```

The default port 443 needs nothing more. A custom port inside Linux's default range of ephemeral
ports (32768-60999, `net.ipv4.ip_local_port_range`) must also be reserved, as the commented line
shows: while the server is stopped, any outgoing connection of the machine (a DNS query, a
download, the SMTP relay) may get that port as its local port, and the restart then fails with
`EADDRINUSE`. Reserve `WS_PORT` as well when it differs from `API_PORT` and lies in that range (a
comma-separated list).

The server protects itself during such a reconnection storm. With native TLS, a new connection first
has 3 s to send the start of its TLS handshake (the ClientHello), and holds no handshake slot while
it waits; a connection that stays silent is closed. Each worker then performs at most
`MAX_PENDING_HANDSHAKES` (128) TLS handshakes at a time, and at most `MAX_PENDING_HANDSHAKES_PER_IP`
(4 by default) for one address group, an IPv4 address or an IPv6 /48. A connection beyond that is
closed at once, before any TLS work, and the game retries after a random delay. The CPU then
completes the handshakes in turn instead of starting all of them together and finishing none before
the clients give up. Raise `MAX_PENDING_HANDSHAKES_PER_IP` when many players share one address group
(a school or a company network), and `PASSWORD_HASH_WAITERS_PER_SOURCE` if they log in together
while the server is busy (see [Password hashing on a small
server](#password-hashing-on-a-small-server)). These limits stop a few hosts from blocking everyone,
not a distributed attack: see the connection storms part of section 5.8 in
[docs/DESIGN.md](docs/DESIGN.md). Games that were running when the server stopped come back from the
journal, and both players then have `RECOVERY_GRACE_MS` (90 s) to reconnect instead of the normal
grace. The clock of the side to move stays stopped until that player is back, for
`RECOVERY_CLOCK_HOLD_MS` (20 s) at most, so coming back within that time costs them nothing. After
it their clock runs again even while they are away, and the rest of their reconnection time is
charged to it. A model of the reconnection wave brings everyone back in time with 10,000 players on
2 cores. With 100,000 players on 4 cores the wave takes about a minute, and more than half of the
players to move lose part of their reconnection time on their clock, while their games are kept for
the 90 s (capacity section of
[docs/BENCHMARK.md](docs/BENCHMARK.md#capacity-of-a-dedicated-machine)).

`MAX_CONNECTIONS` (200,000) counts the signed-in players of the whole server. Beyond it, a newcomer
still completes the TLS handshake and the WebSocket upgrade, then is refused when it logs in
(`ServerFull`), and the game waits 60 to 120 s before it tries again. A player whose game is in
progress is still let in, so that the game can go on. So that such a player can reach the login, the
WebSocket upgrades may use a reserve of max(16, 2 %) connections beyond `MAX_CONNECTIONS`, and the
server does not shed load at `MAX_CONNECTIONS` itself, which would make them compete with the
newcomers to get through: each attempt of a newcomer then costs a TLS handshake, the upgrade and the
login check. Watch `scacelith_ws_hello_total{result="server_full"}` on the metrics endpoint: it
counts the newcomers refused at login, and it is the sign that the server is full. A worker sheds
load only for up to 5 s after the upgrade itself was refused (HTTP 503, once the reserve is in use
as well), or while it holds 1.2 times its share of `MAX_CONNECTIONS`: with native TLS it then lets
only `MAX_PENDING_HANDSHAKES / 2` new TLS connections per second through and closes the other
attempts before any TLS work (`scacelith_tls_refused_total{reason="server_full"}`). The game learns
that the server is full only from the connections let through: the HTTP 503 answer to its WebSocket
upgrade, or the `ServerFull` answer when it logs in (`GET /api/v1/info` does not say it). While a
worker sheds on the default shared port, new API connections are let through at that same rate, so
the API keeps working, more slowly; setting `WS_PORT` to another port keeps the API outside the
limit, the better layout for a server that expects to be full.

## Accounts, e-mail and Google sign-in

- `REGISTRATION=open|closed`, `REQUIRE_EMAIL_VERIFICATION`, username and password rules: see
  [docs/CONFIG.md](docs/CONFIG.md). With e-mail confirmation (the default) an account is created
  only when the link sent to its address is used (within 24 h; the username is held meanwhile), so
  that registering never tells whether an address already has an account; without it the
  account is created at once.
- E-mail: `MAIL_TRANSPORT=smtp` with `SMTP_HOST`, `SMTP_PORT`, `SMTP_USER`, `SMTP_PASSWORD` and
  `SMTP_SECURITY` (`starttls` on 587 by default, `tls` for implicit TLS on 465; with `starttls`
  the upgrade is mandatory). `MAIL_TRANSPORT=log` writes the messages
  to the log instead (tests), `none` disables e-mail (then turn e-mail confirmation off).
- Google sign-in is optional and off by default. Create an OAuth client of type "Web application"
  in the Google Cloud console, add the redirect URI
  `https://<SERVER_PUBLIC_HOST>/auth/sso/google/callback` (`https://<SERVER_PUBLIC_HOST>:<port>/...`
  when the public port is not 443; the value `check-config` prints as `googleRedirectUri`), then set `SSO_GOOGLE_ENABLED=true`, `GOOGLE_CLIENT_ID` and
  `GOOGLE_CLIENT_SECRET` (or `GOOGLE_CLIENT_SECRET_FILE`). The client secret stays on the server:
  the game signs in through the system browser with PKCE and never sees it.
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
program, on a rendering thread of each worker process (`GIF_THREADS`, 1) at the lowest CPU
priority, so the games never wait for a GIF and a GIF only takes the CPU the games leave. It keeps
the recent ones in memory (`GIF_CACHE_MB`, 32 per worker) and limits each account to 4 GIFs a
minute and 30 an hour (`GIF_USER_RENDERS_*`), each address to 12 and 120 (`GIF_IP_RENDERS_*`); a
GIF from the cache does not count. A GIF takes from a few tens of milliseconds to about a second
of one core, and a rendering thread up to about 125 MiB while it lives
([docs/SIZING.md](docs/SIZING.md#animated-gifs)). `GIF_ENABLED=false` turns the feature off.
Every `GIF_*` setting: [docs/CONFIG.md](docs/CONFIG.md).

The pictures use two works shipped in `assets/`, under their own licences:

- `assets/pieces/cburnett/`: the "cburnett" chess pieces by Colin M.L. Burnett, GPL version 2
  or later, as distributed with lichess (details in `assets/pieces/cburnett/LICENSE.md`). The
  server is GPL-3.0-or-later, so the combination is distributed under the GPL version 3 or later.
- `assets/fonts/scacelith-gif/`: "Scacelith GIF", bitmap subsets of Terminus Font 4.49.1 by
  Dimitar Toshkov Zhekov, modified (a zero without a slash) and renamed as the licence asks of
  modified versions; SIL Open Font License 1.1 (`assets/fonts/scacelith-gif/OFL.txt`).
  `tools/gen-gif-font.js` makes them again from the Terminus Font 4.49.1 sources.

## Password hashing on a small server

Every login, registration, password change or reset and every account change that asks for the
password computes a password hash: about 0.5-0.6 s of CPU and 64-128 MiB of memory (scrypt, or
Argon2id on Node 24.7+) in Node's libuv thread pool. Each worker process runs at most
`PASSWORD_HASH_CONCURRENCY` (1) of them at once; up to `PASSWORD_HASH_QUEUE_MAX` (32) more wait
their turn. All the hashes of one request wait at most `PASSWORD_HASH_QUEUE_TIMEOUT_MS` (10 s)
together: a password change, which checks the current password and then hashes the new one,
does not wait twice. A request that finds the queue full, or whose wait ran out, is answered
HTTP 503 `server_busy` with a `Retry-After` of 5 to 15 seconds, and nothing changes on the
server: the player simply tries again a little later. With one hash per worker, a login burst on
a 2-core VPS still leaves each shard at least half a core for its games, and the extra memory
stays at about 128 MiB per worker.

One client cannot take the whole queue. While less than half of the queue waits, one client may
queue as many hashes as it needs, so a class or a club that logs in at the same moment behind one
IPv4 address is served in turn. Once half of the queue waits, an IPv4 address, or an IPv6 /48,
may have at most `PASSWORD_HASH_WAITERS_PER_SOURCE` (2) hashes waiting in a worker, and its next
request is answered 429 `rate_limited` (the game shows its usual "Too many attempts" message with
the delay). Such a refusal does not use up one of the client's `AUTH_RATE_PER_IP` attempts. For a
school or a company network whose players log in together while the server is busy, raise
`PASSWORD_HASH_WAITERS_PER_SOURCE`, together with `MAX_PENDING_HANDSHAKES_PER_IP` and
`AUTH_RATE_PER_IP`. The per-address limit of the password endpoints (`AUTH_RATE_PER_IP`, 20 per
10 minutes for an IPv4 address or an IPv6 /64) is also applied to each IPv6 /48 as a whole
(`AUTH_RATE_PER_PREFIX`, 5 times as much by default), because a single customer often gets a /56
(256 /64 networks) or a /48 (65536).

Some of these endpoints have stricter limits of their own, counted for the whole server: 10
registrations per hour per address (`AUTH_REGISTER_PER_HOUR`: raise it before a class creates its
accounts together), 3 password reset e-mails per hour and 10 per day (`AUTH_FORGOT_PER_HOUR`,
`AUTH_FORGOT_PER_DAY`), 10 confirmation e-mails sent again and 10 new passwords from reset links
per hour, each with 3 times as much per IPv6 /48; 10 two-step codes per 15 minutes for one
account (`AUTH_MFA_PER_ACCOUNT`) and 10 password re-checks per 10 minutes
(`AUTH_REAUTH_PER_USER`), whatever the address. A signed-in player also has a budget of its own,
`USER_RATE_PER_MIN` (120) requests a minute across the API from any address, and the endpoints
that read games, file reports or make GIFs count per account rather than per address. Every
limit: [docs/API.md](docs/API.md#15-rate-limits-and-other-throttles).

When a stored hash is outdated (for example scrypt after an upgrade to Node 24.7, where new
hashes use Argon2id), the login upgrades it with the password it just checked, but only when a
hash slot is free at once; otherwise the next login does it. That new hash, like the one of a
password change, is only written when the stored hash did not change meanwhile, and a login whose
password was replaced while it was being checked fails: a password reset always wins, also
against a login that is waiting for its two-step verification code. A failed login is held until
it took as long as the slowest password check of the last 10 to 20 minutes, and at least as long
as the slowest kind of check the worker measured when it started (at most 2 s, after the hash
slot is freed), so that its time does not reveal whether the e-mail address or user name has an
account, whatever algorithm its hash uses. On Node 24.7 or later, where new hashes use Argon2id
but older accounts keep their scrypt hash until they log in, a failed login therefore takes about
as long as a scrypt check (0.5 s).

- Raise `PASSWORD_HASH_CONCURRENCY` only when the machine has idle cores: each hash in flight
  keeps a whole core busy for half a second.
- Keep `UV_THREADPOOL_SIZE` (an environment variable Node reads at start, 4 threads per process
  by default) at 4 or more, and above `PASSWORD_HASH_CONCURRENCY`: the same threads write and
  fsync the game journal and resolve host names for SMTP and Google sign-in, which would
  otherwise wait behind the hashes. Set it in the process environment (for example the systemd
  `EnvironmentFile`), not in the server's `.env` file: the server reads its own settings from it,
  but the thread pool only sees the real environment. The server logs a warning at start (and
  `check-config` prints it) when `PASSWORD_HASH_CONCURRENCY` is not below the pool size. Beware
  of an empty or non-numeric value (a bare `UV_THREADPOOL_SIZE=` line): libuv reads it, and 0, as
  a pool of 1 thread, and the warning says so.
- `PASSWORD_HASH_QUEUE_TIMEOUT_MS` is at most 13000: the game gives up after 15 s, and the hash
  itself takes a second or two, so that a refused player gets the "busy" answer rather than a
  timeout.
- `POW_LOGIN_TRIGGER_PER_MIN` (30) turns the login proof of work on during a credential-stuffing
  wave. Each failed login costs a hash, so a much higher trigger could never be reached on a
  small machine.
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
over its /64 networks. The limits are for the whole server: each worker
allows its share (all of it with 1 or 2 workers, half of it with 4), so a client spread over the
workers gets at most twice as much, and no request waits for the other processes.

| Setting | Default | Beyond it |
|---|---|---|
| `HTTP_RATE_PER_IP` | 600 requests per minute, any path (API, pages, health checks, unknown paths, WebSocket upgrades), with a burst of half a minute | 429 `rate_limited` with `Retry-After` |
| `HTTP_RATE_PER_PREFIX` | 4 × `HTTP_RATE_PER_IP` for an IPv6 /48 | the same |
| `IP_MAX_INFLIGHT` | 32 requests in progress per worker | the same, with `Retry-After: 1` |
| `IP_CONN_RATE` | 10 new connections per second, with a burst of 4 seconds | the connection is reset before TLS |
| `IP_MAX_CONNECTIONS` | 128 open connections (TLS handshakes, API keep-alive and WebSockets together) | the same |
| `MAX_CONNECTIONS_PER_IP` | 64 WebSocket connections, counted exactly over the workers (per /64, no count per /48: the /48's upgrades still take its request budget, and with native TLS its connections the /48 count of `IP_MAX_CONNECTIONS`) | 429 `too_many_connections` at the upgrade |
| `ABUSE_BLOCK_REFUSALS_PER_MIN` | 600 refusals in a minute block the address (4 times that for a /48) | blocked for `ABUSE_BLOCK_BASE_SEC` (60 s), 4 times longer at each new block within 6 hours, up to `ABUSE_BLOCK_MAX_SEC` (1 h) |

The refusals that count toward a block are those that show a client ignoring the limits: the
429s of the request budget and the in-flight cap, the connections reset before TLS, failed TLS
handshakes and malformed HTTP, plus the 429s of the API's per-address route limits (a refused
login, registration, password reset or second-factor attempt counts 5). A worker that sees 600
of them from one address within a second blocks it at once; otherwise the primary adds up the
workers' counts, reported once a second, and blocks the address everywhere within a second or
two. A blocked address has its new connections reset before any TLS work, and its
requests on connections already open get 429 with the time left and `Connection: close`.
WebSocket connections that are already open are never closed by a block, so the players of a
school or of a mobile operator who share the address with an abuser keep their games; a player
whose connection drops can only come back when the block ends. `ABUSE_BLOCK_REFUSALS_PER_MIN=0`
turns blocking off and keeps the limits.

Slow clients are cut as well: the request headers must arrive within 10 s and the whole request
within 30 s (both checked every second), a connection with no byte in or out for 30 s is closed
unless the server is still preparing its answer (game WebSockets have their own heartbeat
instead), and an answer that the client has not read 60 s after the server finished it is
dropped with its connection. A header or request timeout is
answered 408 and, like malformed HTTP (400) and oversized headers (431), counted in
`scacelith_http_client_errors_total{reason}` and toward a block of the address.

**Players who share one address.** A school, a club, a company network or a mobile operator's
carrier-grade NAT puts many players behind one IPv4 address. The defaults are sized for that: a
class of 30 browsing the menus at one request every 3 s each is 600 requests per minute, their
launch fits in the burst, and 50 players make about 150 connections. A real player over the budget
waits for the `Retry-After` the game shows, and does not come near 600 refusals a minute. For a
known network that plays together (a school's or a club's address, also your monitoring and a
load generator), list its address or subnet in `ABUSE_EXEMPT` (`203.0.113.7,2001:db8:12::/48`): it
then skips this whole layer, never blocked, while the login, registration and account limits
still apply to it. Raise `MAX_PENDING_HANDSHAKES_PER_IP`, `AUTH_RATE_PER_IP` and
`PASSWORD_HASH_WAITERS_PER_SOURCE` for it too if its players log in together (see [Kernel
settings](#kernel-settings-linux) and [Password hashing on a small
server](#password-hashing-on-a-small-server)). Keep `IP_MAX_CONNECTIONS` at least twice
`MAX_CONNECTIONS_PER_IP`; `check-config` warns otherwise.

**Behind a reverse proxy** (`TLS_MODE=proxy`) the request budget, the in-flight cap and the
blocks apply to the client named by `X-Forwarded-For`, and a block answers 429 without closing
the connection, which belongs to the proxy. The connection limits (`IP_CONN_RATE`,
`IP_MAX_CONNECTIONS`) and the reset before TLS cannot work there: limit the connections and
handshakes per client on the proxy (nginx: `limit_conn`, `limit_req`). Set `TRUSTED_PROXIES`
exactly: when the proxy is not trusted, every player shares its address, and the budget and a
block would hit them all together.

**Watching it.** The primary logs each block at `warn` level: `ip blocked` with the address
(truncated as `LOG_IP` says), `scope` (`ip` or `prefix`), `blockLevel`, `ttlSec` and `refusals`.
On the metrics endpoint: `scacelith_http_rate_limited_total{limit}` (`ip`, `ip48`, `inflight`,
`blocked`), `scacelith_tls_refused_total{reason}` (`blocked`, `conn_rate`, `conn_open`),
`scacelith_abuse_blocks_total{scope,level}` and `scacelith_abuse_blocked{scope}` in the primary,
`scacelith_abuse_blocked_keys`, `scacelith_tls_connections_open` and `scacelith_http_inflight`
per worker. Blocks at level 4 again and again from the same sources, or a flood from many
addresses that no per-address limit catches, belong to the provider's firewall:
[docs/SIZING.md](docs/SIZING.md#provider-firewall-the-ovh-edge-network-firewall) gives the OVH
Edge Network Firewall rules and what this layer can still cost.

## Secrets

Nothing secret is ever committed: `.env`, keys and certificates are in `.gitignore`, and only
`.env.example` (empty secret values) is tracked. The secrets are:

| Key | What it protects |
|---|---|
| `SERVER_SECRET` | proof-of-work challenges, recovery codes, address hashing; at least 32 random bytes (`gen-secret`) |
| `MFA_ENCRYPTION_KEY` | TOTP secrets at rest (derived from `SERVER_SECRET` when empty; set it so that the secret can change without breaking two-factor logins) |
| `TLS_KEY_FILE` | the certificate's private key |
| `SMTP_PASSWORD`, `GOOGLE_CLIENT_SECRET` | mail account and Google OAuth client, used as written |
| `METRICS_TOKEN` | optional bearer token for the metrics endpoint |

Changing `SERVER_SECRET` logs nobody out, but it invalidates the recovery codes and, unless
`MFA_ENCRYPTION_KEY` is set, every enabled authenticator app (players would need an admin
`user reset-mfa`). Keep it stable and back it up, apart from the database copies.

## Data, backups and upgrades

- `DATA_DIR` (default `./data`) holds the SQLite database (`scacelith.db`, WAL mode) and the
  game journal (`journal/`). Back up with `node bin/admin.js backup /path/to/new-file.db --verify`
  (run it as the service user, with the same `.env`). It uses `VACUUM INTO`, which writes a
  consistent snapshot in one pass while the server runs; do not use the `sqlite3` shell's
  `.backup`, which copies 100 pages at a time, starts over whenever the server writes, and may
  never finish on a busy server. The copy is created with mode 600 and holds e-mail addresses and
  recent IPs: encrypt it and move it off the host. `bin/admin.js` refuses to run when `DB_PATH`
  (by default `DATA_DIR/scacelith.db`, and a relative `DATA_DIR` is resolved from the current
  directory) is not an existing Scacelith database, so a scheduled backup run from the wrong
  place fails instead of copying an empty database. Keep `SERVER_SECRET` and `MFA_ENCRYPTION_KEY`
  backed up too, but separately from the database copies (together they decrypt the players'
  TOTP secrets).
- Upgrading: stop the server, update the code, `node bin/scacelith-server.js migrate` (or just
  `start`, which migrates first). Migrations are checksummed: the server refuses to start if an
  applied migration was modified or is unknown to its version (a downgrade).
- A crash loses at most the last journal flush (`JOURNAL_FLUSH_MS`, 50 ms) of moves in progress;
  finished games and rating changes are committed in database transactions.
- Players come back by themselves after a restart. The game spreads their reconnections over
  about half a minute (players with a game in progress within 8 s, since the side to move's
  clock runs again once the game is restored), so a restart does not turn into a burst of TLS
  handshakes. After a full restart each of them does one full handshake: the TLS session-ticket
  keys the workers share are drawn at random at the start, replaced every day (UTC) by a one-way
  step and kept in memory only, so that neither `SERVER_SECRET` nor a later memory dump decrypts
  recorded sessions of the past days. A restarted worker gets the current keys from the primary.
- Retention: every `RETENTION_INTERVAL_MS` (one hour; the first run about a minute after the
  start) the server deletes expired and revoked sessions, expired tokens and pending signups,
  security events older than `RETENTION_SECURITY_DAYS` (90), non-certain anomalies of the same
  age, conduct events and failed analysis jobs older than 30 days, and erases stored IP addresses
  older than `RETENTION_IP_DAYS` (30). It works in small slices while the server runs, pausing
  between them so that the workers can write, and logs one `retention purge done` line with the
  counts.
  Games, ratings, analysed games, sanctions and reports are kept. The database overwrites deleted
  and erased data with zeros (`secure_delete`), so it does not stay readable in the file. SQLite
  reuses the freed pages; the file only shrinks after a `VACUUM` (server stopped). The full table
  is in the retention section of [docs/DESIGN.md](docs/DESIGN.md).
- The journal of each worker (`journal/shard-<n>/`) keeps about `JOURNAL_COMPACT_SEGMENTS + 1`
  segments of 16 MB (80 MB by default), however long the games last: a game still running after
  that many segments is rewritten as one snapshot record and its older segments are deleted. Plan
  about 100 MB of disk per worker for it; `scacelith_journal_disk_bytes` shows the actual size.
- When the journal cannot be written (a full disk, a failing volume),
  `scacelith_journal_errors_total` grows and the workers log `journal write failed`. Finished
  games still reach the database, rating changes included, after three failed journal flushes in
  a row (about 0.3 s): `scacelith_game_commit_unjournaled_total` counts them, with one error
  logged per episode. Free the space or fix the volume before a restart: a finished game whose
  end the journal lost would come back as a game in progress (the database keeps its first
  result).

## Moderation

`node bin/admin.js` (also `npm exec scacelith-admin`) works on the database directly: user
lookup, bans, MFA reset, session revocation, the integrity list and evidence, reports. Run it
with the same `.env`. See `node bin/admin.js` for the commands and
[docs/ANTICHEAT.md](docs/ANTICHEAT.md) for how suspicion levels are computed. Certain cheats
(a move for someone else's game, out of turn or illegal in a position both sides agree on, a
forged server message) forfeit the game and ban for `BAN_DURATION_HOURS` (24) automatically when
`AUTO_SANCTION_CERTAIN_CHEATS` is on; statistical suspicion never bans by itself. A ban for
cheating (that automatic one, or `integrity confirm` unless `--no-refund`) gives the cheater's
victims back the rating points they lost to them, games still in progress at the ban included; a
`user ban` is for anything else and refunds nothing (docs/ANTICHEAT.md, rating refunds).

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
addresses ([Protection against abuse](#protection-against-abuse)), anti-cheat (including
the analysis backlog, the skipped games, and the analysis engines running and sharing their
network: `scacelith_anticheat_analysis_engines*`), the retention purge (`scacelith_retention_*`),
process memory and event-loop lag per shard, the stalls of a shard's event loop and the time given
back to the players for them (`scacelith_game_stall_*`), and the gesture relay
(`scacelith_gestures_*`).
`/healthz` answers when the process runs, `/readyz` when it accepts players.

## Scaling

One machine: `WORKERS` shards (one per core by default); games live on one shard and the others
relay to it over a local socket bus. Several machines behind a load balancer: see the scaling
section of [docs/DESIGN.md](docs/DESIGN.md) (shard ranges with `SHARD_BASE`, a TCP bus, a shared
database). Load tests: `npm run bench`; measured results and capacity estimate in [docs/BENCHMARK.md](docs/BENCHMARK.md).

On a small machine, `CLIENT_PING_INTERVAL_MS` is the idle cost you control: the game pings the
server at that interval (announced in `Welcome`) for its ping indicator and its estimate of the
server clock, and each ping costs server CPU for every connected player. The default, 10 s, keeps
it at about half of an idle player's cost; 2 s would make it a third or more of the cost of a
player in a 3+2 game.

`GESTURE_RATE` is the other one. During a game the client sends its player's live gestures (the
head, the piece in hand and where it is aimed) whenever they change and at least once a second,
even while its player sits still, at most that many per second (4 by default, announced in
`Welcome`), and the server relays each one to the opponent. A gesture costs about as much server
CPU as a client ping, so with the relay on every player in a game costs at least one relay a
second, which alone halves the capacity of a server with the default 10 s ping, and a player who
keeps moving costs several times their moves: [docs/SIZING.md](docs/SIZING.md#gestures) gives the
capacity for each rate. Lower it to 2 or 1, or to 0 to turn the relay off, when the peak nears
that capacity.
