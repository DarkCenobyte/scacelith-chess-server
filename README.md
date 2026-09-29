# Scacelith dedicated server

The online multiplayer server of Scacelith: accounts, rated matchmaking, challenges and private
games, authoritative games with server clocks, one Elo per official time control, anti-cheat and
reports. It is a plain Node.js program with no npm dependency. The game (the Windows binary) is
only a client of it; anyone can run a community server, and players choose the server in the
game's Options.

- Official server: `caissa.scacelith.com`, TCP port `44664` (HTTPS API and WSS on the same port).
- Design and contracts: [docs/DESIGN.md](docs/DESIGN.md). Every setting: [docs/CONFIG.md](docs/CONFIG.md).
  Anti-cheat: [docs/ANTICHEAT.md](docs/ANTICHEAT.md).

## Requirements

- Node.js 22.13 or later (it uses the built-in `node:sqlite`). Node 24.7+ adds Argon2id password
  hashing; older versions use scrypt.
- A TLS certificate for the server's public name (see [TLS certificates](#tls-certificates)).
- Linux is the reference platform (Windows works for tests). One CPU core per game shard; the
  default is one shard per core, up to 16.
- Optional: a Stockfish binary for the anti-cheat analysis (`ANALYSIS_ENGINE_PATH`), and an SMTP
  account for e-mail confirmation and password resets.

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
the server stopped come back), starts one worker per shard and listens on `API_PORT` (44664 by
default). `SIGTERM` or Ctrl-C stops it gracefully: running games are journaled and resume at the
next start. `npm test` runs the unit and integration tests.

Configuration comes from the environment, then from `.env` next to `package.json` (or the file
named by `SCACELITH_ENV_FILE`). `.env.example` lists every key with its default and is the only
configuration file in Git. Secrets can also be read from files with the `_FILE` suffix
(`SERVER_SECRET_FILE=/run/secrets/scacelith_secret`), which keeps them out of the environment.

## Ports and firewall

| Port | Default | Open to | Purpose |
|---|---|---|---|
| `API_PORT` | 44664/tcp | the Internet | HTTPS API (`/api/v1/...`), e-mail and Google sign-in pages, and the game WebSocket (`wss://host:44664/ws`) |
| `WS_PORT` | same as `API_PORT` | the Internet | set it only to put the WebSocket on its own port |
| `METRICS_PORT` | 9464/tcp on 127.0.0.1 | your monitoring only | Prometheus metrics, `/healthz`, `/readyz` |

When a NAT or a proxy publishes other port numbers than the ones the server listens on, set
`PUBLIC_API_PORT` / `PUBLIC_WS_PORT` to what the players must use; the server announces them in
`GET /api/v1/info`.

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
  Git checkout. `TLS_KEY_FILE_FILE` is not needed: the key is already read from a file.
- Renewal needs no restart. The server re-reads both files when they change (it also follows
  certbot's symlink swaps) and on `SIGHUP` (`systemctl reload scacelith` with the unit below).
  A broken new certificate is refused and logged; the previous one stays in use.
- Check it from another machine:
  `openssl s_client -connect caissa.scacelith.com:44664 -servername caissa.scacelith.com </dev/null`
  must show the full chain and `Verify return code: 0 (ok)`, and
  `curl https://caissa.scacelith.com:44664/api/v1/info` must answer without `-k`.

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
and the per-address limits would treat them as one client. The proxy must pass WebSocket
upgrades on `/ws` and keep idle connections for more than a minute:

```nginx
location / {
    proxy_pass http://10.0.0.5:44664;
    proxy_http_version 1.1;
    proxy_set_header Upgrade $http_upgrade;
    proxy_set_header Connection $connection_upgrade;   # map $http_upgrade $connection_upgrade { default upgrade; '' close; }
    proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
    proxy_set_header X-Forwarded-Proto https;
    proxy_read_timeout 120s;
}
```

`TLS_MODE=off` exists for local development only and is refused unless `ALLOW_INSECURE_DEV=1`.

### The official server

`caissa.scacelith.com:44664` uses option 1 with a certificate provided by the server operator;
the game has this address built in as its default server. Its `.env` sets at least
`SERVER_PUBLIC_HOST=caissa.scacelith.com`, `API_PORT=44664`, `TLS_MODE=native`, `TLS_CERT_FILE`
and `TLS_KEY_FILE`.

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
TimeoutStopSec=30
Restart=on-failure
LimitNOFILE=1048576
NoNewPrivileges=true
ProtectSystem=strict
ReadWritePaths=/var/lib/scacelith
PrivateTmp=true

[Install]
WantedBy=multi-user.target
```

With `DATA_DIR=/var/lib/scacelith` in the environment file. `LimitNOFILE` must exceed
`MAX_CONNECTIONS`. This unit is an example: adapt the paths to your installation.

## Accounts, e-mail and Google sign-in

- `REGISTRATION=open|closed`, `REQUIRE_EMAIL_VERIFICATION`, username and password rules: see
  [docs/CONFIG.md](docs/CONFIG.md).
- E-mail: `MAIL_TRANSPORT=smtp` with `SMTP_HOST`, `SMTP_PORT`, `SMTP_USER`, `SMTP_PASSWORD` and
  `SMTP_SECURITY` (`starttls` on 587 by default, `tls` for implicit TLS on 465; with `starttls`
  the upgrade is mandatory). `MAIL_TRANSPORT=log` writes the messages
  to the log instead (tests), `none` disables e-mail (then turn e-mail confirmation off).
- Google sign-in is optional and off by default. Create an OAuth client of type "Web application"
  in the Google Cloud console, add the redirect URI
  `https://<SERVER_PUBLIC_HOST>:<port>/auth/sso/google/callback` (the value `check-config`
  prints as `googleRedirectUri`), then set `SSO_GOOGLE_ENABLED=true`, `GOOGLE_CLIENT_ID` and
  `GOOGLE_CLIENT_SECRET` (or `GOOGLE_CLIENT_SECRET_FILE`). The client secret stays on the server:
  the game signs in through the system browser with PKCE and never sees it.
- Every server is a separate trust boundary: the game keeps one login per server address and
  never sends a server the credentials or tokens of another one.

## Secrets

Nothing secret is ever committed: `.env`, keys and certificates are in `.gitignore`, and only
`.env.example` (empty secret values) is tracked. The secrets are:

| Key | What it protects |
|---|---|
| `SERVER_SECRET` | session tickets, proof-of-work challenges, recovery codes, address hashing; at least 32 random bytes (`gen-secret`) |
| `MFA_ENCRYPTION_KEY` | TOTP secrets at rest (derived from `SERVER_SECRET` when empty; set it so that the secret can change without breaking two-factor logins) |
| `TLS_KEY_FILE` | the certificate's private key |
| `SMTP_PASSWORD`, `GOOGLE_CLIENT_SECRET` | mail account and Google OAuth client, used as written |
| `METRICS_TOKEN` | optional bearer token for the metrics endpoint |

Changing `SERVER_SECRET` logs nobody out, but it invalidates the recovery codes and, unless
`MFA_ENCRYPTION_KEY` is set, every enabled authenticator app (players would need an admin
`user reset-mfa`). Keep it stable and back it up with the database.

## Data, backups and upgrades

- `DATA_DIR` (default `./data`) holds the SQLite database (`scacelith.db`, WAL mode) and the
  game journal (`journal/`). Back up with `sqlite3 data/scacelith.db ".backup backup.db"` (safe
  while running) and keep the secrets with it.
- Upgrading: stop the server, update the code, `node bin/scacelith-server.js migrate` (or just
  `start`, which migrates first). Migrations are checksummed: the server refuses to start if an
  applied migration was modified or is unknown to its version (a downgrade).
- A crash loses at most the last journal flush (`JOURNAL_FLUSH_MS`, 50 ms) of moves in progress;
  finished games and rating changes are committed in database transactions.
- Retention: every `RETENTION_INTERVAL_MS` (one hour; the first run about a minute after the
  start) the server deletes expired and revoked sessions, expired tokens, security events older
  than `RETENTION_SECURITY_DAYS` (90), non-certain anomalies of the same age, conduct events and
  failed analysis jobs older than 30 days, and erases stored IP addresses older than
  `RETENTION_IP_DAYS` (30). It works in small slices while the server runs and logs one
  `retention purge done` line with the counts. Games, ratings, analysed games, sanctions and
  reports are kept. SQLite reuses the freed pages; the file only shrinks after a `VACUUM`
  (server stopped). The full table is in the retention section of [docs/DESIGN.md](docs/DESIGN.md).

## Moderation

`node bin/admin.js` (also `npm exec scacelith-admin`) works on the database directly: user
lookup, bans, MFA reset, session revocation, the integrity list and evidence, reports. Run it
with the same `.env`. See `node bin/admin.js` for the commands and
[docs/ANTICHEAT.md](docs/ANTICHEAT.md) for how suspicion levels are computed. Certain cheats
(a move for someone else's game, out of turn or illegal in a position both sides agree on, a
forged server message) forfeit the game and ban for `BAN_DURATION_HOURS` (24) automatically when
`AUTO_SANCTION_CERTAIN_CHEATS` is on; statistical suspicion never bans by itself.

One analysis engine handles roughly 1,000 to 12,000 games a day, fewer than a busy server
plays. Reported games, games of players already under suspicion (integrity level, open
report) or with a suspicious anomaly, and moderator requests are always analysed first. Ordinary games are sampled
(`ANALYSIS_SAMPLE_RATE`) and skipped while `ANALYSIS_QUEUE_MAX` (5000) of them already wait.
If `scacelith_anticheat_analysis_queue_ordinary` stays at that cap, add engines
(`ANALYSIS_WORKERS`), lower `ANALYSIS_DEPTH_DEEP`, or lower the sample rate.

## Monitoring

`http://127.0.0.1:9464/metrics` (Prometheus text format; `METRICS_TOKEN` adds a bearer token):
connections, messages, games, move latency, commits, journal, rate limits, anti-cheat (including
the analysis backlog and the skipped games), the retention purge (`scacelith_retention_*`), process
memory and event-loop lag per shard. `/healthz` answers when the process runs, `/readyz` when it
accepts players.

## Scaling

One machine: `WORKERS` shards (one per core by default); games live on one shard and the others
relay to it over a local socket bus. Several machines behind a load balancer: see the scaling
section of [docs/DESIGN.md](docs/DESIGN.md) (shard ranges with `SHARD_BASE`, a TCP bus, a shared
database). Load tests: `npm run bench`; measured results and capacity estimate in [docs/BENCHMARK.md](docs/BENCHMARK.md).
