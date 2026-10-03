# Deploying the server with systemd

This guide installs the server as a systemd service on a Linux host, from the example files of
[`deploy/systemd/`](../deploy/systemd/). Every setting of the server is described in
[CONFIG.md](CONFIG.md); sizing a machine in [SIZING.md](SIZING.md).

| Example file | Installed as | Purpose |
|---|---|---|
| `scacelith-server.service` | `/etc/systemd/system/scacelith-server.service` | the service unit |
| `scacelith-server.env.example` | `/etc/scacelith/scacelith-server.env` | the settings an operator must give |
| `sysusers.d/scacelith-server.conf` | `/etc/sysusers.d/scacelith-server.conf` | the `scacelith` system account |
| `tmpfiles.d/scacelith-server.conf` | `/etc/tmpfiles.d/scacelith-server.conf` | its directories |
| `certbot-deploy-hook.sh` | `/etc/letsencrypt/renewal-hooks/deploy/scacelith-server` | installs a renewed certificate and reloads the service |

| Path | Owner and mode | Content |
|---|---|---|
| `/usr/local/bin/scacelith-server` | root, 0755 | the binary |
| `/etc/scacelith/` | root:scacelith, 0750 | the environment file and the secret files (0640) |
| `/etc/scacelith/tls/` | root:scacelith, 0750 | `fullchain.pem` (0644) and `privkey.pem` (0640) |
| `/var/lib/scacelith/` | scacelith, 0750 | `DATA_DIR`: the database `scacelith.db` (with its `-wal` and `-shm` files while it is open) and the game journal `journal/` |

Requirements: x86-64 Linux with systemd 253 or later (Debian 13, Ubuntu 24.04, Fedora 38 and
later); for systemd 252 (Debian 12, RHEL 9) see [Older systemd](#older-systemd). Optional: the
Stockfish 19 binary for the anti-cheat analysis (see the [README](../README.md) and
[ANTICHEAT.md](ANTICHEAT.md)) and an SMTP account for e-mail.

## 1. Build and install the binary

The release binary is static (`x86_64-unknown-linux-musl`) and runs on any x86-64 Linux,
whatever its C library. It needs Rust 1.99.0 with the musl target (`rust-toolchain.toml`; with
rustup: `rustup toolchain install 1.99.0 --target x86_64-unknown-linux-musl`) and a musl C
compiler for *ring* and the bundled SQLite (Debian and Ubuntu: `apt install musl-tools`):

```sh
cd dedicated-server
CC_x86_64_unknown_linux_musl=musl-gcc \
  cargo build --release --locked -p scacelith-server --target x86_64-unknown-linux-musl
# -> target/x86_64-unknown-linux-musl/release/scacelith-server
```

Without `--target`, the same command builds a binary linked to the build machine's glibc
(`target/release/scacelith-server`), which needs that glibc version or a later one on the host.
Copy the binary to the host, then:

```sh
sudo install -m 0755 scacelith-server /usr/local/bin/scacelith-server
scacelith-server version
```

## 2. Account and directories

```sh
sudo install -m 0644 deploy/systemd/sysusers.d/scacelith-server.conf /etc/sysusers.d/
sudo systemd-sysusers scacelith-server.conf
sudo install -m 0644 deploy/systemd/tmpfiles.d/scacelith-server.conf /etc/tmpfiles.d/
sudo systemd-tmpfiles --create scacelith-server.conf
```

The service runs as a static system account, `scacelith`, rather than with `DynamicUser=`. The
administration commands, migrations and backups open the same database as the running server,
and SQLite creates files next to it (`-wal`, `-shm`, a backup copy): they must run as the same
account, which a dynamic user does not make possible from a shell. The account's group also
gives the service read access to its TLS key and configuration, which root keeps writing. Run
every command that touches `/var/lib/scacelith` as `scacelith`, never as root: a file left there
owned by root can stop the server from opening its database.

## 3. Configuration

```sh
sudo install -m 0640 -o root -g scacelith deploy/systemd/scacelith-server.env.example \
  /etc/scacelith/scacelith-server.env
sudo sh -c 'umask 027 && scacelith-server gen-secret > /etc/scacelith/server-secret'
sudo chgrp scacelith /etc/scacelith/server-secret
sudo install -m 0640 -o root -g scacelith /dev/null /etc/scacelith/smtp-password
sudoedit /etc/scacelith/smtp-password             # the SMTP password alone
sudoedit /etc/scacelith/scacelith-server.env      # the values of your server
```

The environment file holds only the keys an operator must set; add any other key of
[CONFIG.md](CONFIG.md) to it. Secrets stay in their own files (`SERVER_SECRET_FILE`,
`SMTP_PASSWORD_FILE`; surrounding white space is ignored), out of the process environment. The
file is read twice, with two parsers that agree on plain `KEY=value` lines: by systemd for the
service (`EnvironmentFile=`), and by the server itself for the administration commands. Keep
comments on lines of their own and use no quotes, backslashes or `$`. The unit sets
`SCACELITH_ENV_FILE=` (empty) so that the service never reads a `.env` file from its working
directory: keep every setting in the environment file, not in `Environment=` lines of a drop-in,
so that the administration commands see the same configuration.

Run the administration commands as `scacelith` with that file. A shell function saves typing:

```sh
scs() { sudo -u scacelith env SCACELITH_ENV_FILE=/etc/scacelith/scacelith-server.env \
          /usr/local/bin/scacelith-server "$@"; }
scs check-config      # the effective configuration (secrets hidden), or every error; exit code 1 on errors
scs migrate           # applies the database migrations, then exits
scs admin --help      # moderation and maintenance commands
```

`check-config` prints warnings about risky settings on stderr, including keys of former versions
that no longer apply. `scacelith-server help` lists the commands. Changes to the environment
file take effect at the next `systemctl restart` (a reload only re-reads the certificate).

## 4. TLS certificate

The server terminates TLS itself (`TLS_MODE=native`, the default) with `TLS_CERT_FILE`, the
**full chain** (the server certificate, then the intermediates: the game refuses a chain without
them), and `TLS_KEY_FILE`. Both live in `/etc/scacelith/tls/`, written by root and read by the
service through its group. The server re-reads them on `systemctl reload`, and also checks both
files every 10 seconds and reloads them after a change. A certificate it cannot load (unreadable
file, bad PEM, a key that does not match) is refused and logged at error level, and the current
one stays in use. For a server among friends with a self-signed certificate, or behind a reverse
proxy (`TLS_MODE=proxy`), see the TLS section of the [README](../README.md).

With Let's Encrypt and certbot (`--standalone` needs port 80 free and open during issuance and
renewals; `--webroot` or a DNS challenge avoid that):

```sh
sudo certbot certonly --standalone -d play.example.org
sudo install -m 0755 deploy/systemd/certbot-deploy-hook.sh \
  /etc/letsencrypt/renewal-hooks/deploy/scacelith-server
sudoedit /etc/letsencrypt/renewal-hooks/deploy/scacelith-server      # CERT_NAME=play.example.org
# certbot runs the hook after each renewal; run it once now for the first certificate:
sudo RENEWED_LINEAGE=/etc/letsencrypt/live/play.example.org \
  /etc/letsencrypt/renewal-hooks/deploy/scacelith-server
```

The hook copies `fullchain.pem` and `privkey.pem` from `/etc/letsencrypt/live/<name>/` (readable
by root only) to `/etc/scacelith/tls/`, root-owned with group `scacelith` (the key 0640), replacing
each file in one rename, then runs `systemctl reload scacelith-server`, which returns once the
server has reloaded. Any other ACME client works the same way: write both files there with those
owners and modes, then reload. Check the result from another machine:

```sh
openssl s_client -connect play.example.org:443 -servername play.example.org </dev/null
# the full chain, and "Verify return code: 0 (ok)"
journalctl -u scacelith-server -o cat --since "10 min ago" | jq -cR 'fromjson? | select(.msg | startswith("certificate"))'
```

`LoadCredential=` would let systemd read the root-only certbot files itself, but it copies them
when the service starts: a reload after a renewal would then still serve the old certificate on
systemd versions that do not refresh credentials on reload. The copy made by the hook works with
every version.

## 5. Firewall

| Port | Open to | Purpose |
|---|---|---|
| 443/tcp (`API_PORT`) | the Internet | the HTTPS API and the game WebSocket (`wss://host/ws`) |
| `WS_PORT`/tcp | the Internet | only when it is set to another port than `API_PORT` |
| 80/tcp | the Internet, during issuance and renewals | only for certbot's HTTP-01 challenge |
| 9464/tcp (`METRICS_PORT`) | no one (bound to 127.0.0.1) | Prometheus metrics, `/healthz`, `/readyz`; if `METRICS_BIND` is an internal address, open it to your monitoring only |

```sh
sudo ufw allow 443/tcp && sudo ufw allow 80/tcp                   # ufw
sudo firewall-cmd --permanent --add-service=https --add-service=http && sudo firewall-cmd --reload   # firewalld
# nftables, in the input chain: tcp dport { 80, 443 } accept
```

After a restart every player reconnects within a short time; raise `net.core.somaxconn` with
`LISTEN_BACKLOG` as [CONFIG.md](CONFIG.md) explains.

## 6. Starting the service

```sh
sudo install -m 0644 deploy/systemd/scacelith-server.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now scacelith-server
systemctl status scacelith-server
```

| Command | Effect |
|---|---|
| `systemctl start scacelith-server` | starts the server and returns once it reports ready: database migrated, games in progress recovered from the journal, ports bound (or fails) |
| `systemctl stop scacelith-server` | graceful stop: players are warned, connections drain, final database commits |
| `systemctl restart scacelith-server` | stop, then start; games in progress survive and players reconnect by themselves |
| `systemctl reload scacelith-server` | re-reads the TLS certificate and key (SIGHUP) and returns once done |
| `systemctl status scacelith-server` | state, the server's status line, the latest log lines |
| `systemctl edit scacelith-server` | a drop-in that overrides settings of the unit |
| `systemctl show scacelith-server -p StatusText -p MainPID -p NRestarts` | selected properties |
| `systemctl reset-failed scacelith-server` | clears the failure counter after the start limit was reached |
| `systemd-analyze security scacelith-server` | the sandbox review (1.5, "OK", for the example unit with systemd 255) |

Prefer drop-ins to editing the unit, for example to give more time to the stop:

```ini
# systemctl edit scacelith-server
[Service]
TimeoutStopSec=75s
```

With `API_PORT` (and `WS_PORT`) at 1024 or above, the capability is not needed: add
`AmbientCapabilities=` and `CapabilityBoundingSet=` (both empty) to a drop-in.

## 7. Logs

The server writes one JSON object per line on its standard output, which systemd sends to the
journal; there are no log files, and retention is journald's (`Storage=`, `SystemMaxUse=` in
`journald.conf`). Under journald every line starts with a syslog priority, `<6>` for example,
which journald removes and stores as the entry's priority. When standard output is not the
journal (a file, a terminal), lines have no prefix. `LOG_FORMAT=pretty` prints readable text
instead of JSON; `LOG_LEVEL` sets the threshold. A panic is logged as an `error` record (component
`panic`, with its thread, location and message). Standard error goes to the journal as well, but
holds only what comes before logging starts (an invalid configuration) and, with
`RUST_BACKTRACE=1`, the backtrace of a panic, in plain text that journald stores with priority 6
(`info`): look for those without `-p`.

| `level` | Priority | `journalctl -p` name |
|---|---|---|
| `error` | 3 | `err` |
| `warn` | 4 | `warning` |
| `security` | 5 | `notice` |
| `info` | 6 | `info` |
| `debug` | 7 | `debug` |

`security` records (failed logins, blocks, anomalies, sanctions) have priority 5: `-p warning`
shows errors and warnings only, `-p notice` adds the security records.

```sh
journalctl -u scacelith-server -f                              # follow the service
journalctl -u scacelith-server -b -p warning                   # errors and warnings since boot
journalctl -u scacelith-server -p notice..notice --since today # security records only
journalctl -u scacelith-server -o cat -n 50                    # the records alone, without journald's prefix
```

Every record starts with the same fields, followed by its own (`userId`, `ip`, `err`...):

| Field | Content |
|---|---|
| `t` | time, UTC ISO 8601 with milliseconds |
| `level` | `debug`, `info`, `warn`, `security` or `error` |
| `c` | the component that logged it, dotted (`auth`, for example) |
| `msg` | the message: a fixed text per kind of event |
| `inst` | `INSTANCE_ID` (the host name by default) |
| `proc` | `server` (`admin` for the administration commands) |

Fields named after passwords, tokens, secrets or codes are replaced by `"[redacted]"`, tokens
found inside texts are masked, and client addresses are truncated to their /24 or /48 unless
`LOG_IP` says otherwise. Query the records with `jq`;
`fromjson?` skips the lines that are not JSON (for example the plain-text errors printed when the
configuration is invalid):

```sh
# Security records of the day
journalctl -u scacelith-server -o cat --since today | jq -cR 'fromjson? | select(.level == "security")'
# The records of one component
journalctl -u scacelith-server -o cat --since "1 hour ago" | jq -cR 'fromjson? | select(.c == "auth")'
# Most frequent messages of the last hour
journalctl -u scacelith-server -o cat --since "1 hour ago" | jq -rR 'fromjson? | .msg' | sort | uniq -c | sort -rn | head
# With journald's own fields: -o json carries the record in MESSAGE and the level in PRIORITY
journalctl -u scacelith-server -o json -n 100 | jq -c '{prio: .PRIORITY, pid: ._PID, rec: (.MESSAGE | fromjson?)}'
```

Log collectors that read the journal (systemd-journal-upload, Vector, Fluent Bit...) receive the
same `MESSAGE` and `PRIORITY`. journald limits how many lines a service may log (by default 10,000
per 30 seconds); with `LOG_LEVEL=debug`, which logs every request, raise it in a drop-in with
`LogRateLimitIntervalSec=` and `LogRateLimitBurst=`, or journald drops lines and says so.

## 8. Start, stop, watchdog and restarts

* **Start.** `systemctl start` waits for the server's `READY=1`, sent once the database is
  migrated, the games in progress are recovered and the ports are bound; `TimeoutStartSec=5min`
  bounds that (a long migration counts: see [Upgrades](#9-upgrades-and-migrations)). A server
  that cannot start (invalid configuration, port already in use, database error) exits with
  code 1; `journalctl -u scacelith-server -o cat -n 20` shows why (an invalid configuration is
  reported in plain text, before logging starts).
* **Stop.** `SIGTERM` (`systemctl stop`) starts a graceful stop: `STOPPING=1`, the players get the
  shutdown notice, connections drain for `SHUTDOWN_GRACE_MS` (3 s by default), then the final
  database commits; the server exits with code 0. A second `SIGTERM` or `SIGINT` makes it exit at
  once with code 1. `TimeoutStopSec=45s` must cover `SHUTDOWN_GRACE_MS` and the final commits:
  raise it by as much as you raise `SHUTDOWN_GRACE_MS`. If it runs out, systemd kills the
  server, and the games in progress come back from the journal at the next start anyway.
  `KillMode=mixed` sends `SIGTERM` to the server alone and kills what is left in the unit (the
  analysis engines) once it has exited.
* **Restarts.** Games in progress survive a restart or a crash: they are recovered from the
  journal, and their players have `RECOVERY_GRACE_MS` (90 s) to reconnect, which the game does by
  itself. `Restart=on-failure` restarts the server 5 s after a crash, an exit with code 1, a
  watchdog timeout or a start timeout, not after a clean stop. After 10 starts within 5 minutes
  (manual starts count too) systemd gives up until `systemctl reset-failed scacelith-server`.
  Exit codes: 0 success, 1 failure, 2 usage (a command-line mistake).
* **Watchdog.** With `WatchdogSec=30s` the server sends `WATCHDOG=1` every 15 s (half the
  interval, which systemd passes in `WATCHDOG_USEC`: nothing to configure on the server). A server
  that stops sending is killed with `SIGABRT` and restarted. Change the interval in a drop-in;
  `WatchdogSec=0` turns it off.
* **Health.** `systemctl status` shows the status line the server sends (`STATUS=`). On the
  metrics port, `/healthz` answers while the process runs, `/readyz` only while the server is
  ready and not shutting down, and `/metrics` serves Prometheus metrics ([SIZING.md](SIZING.md)
  lists the useful ones):

```sh
curl -fsS http://127.0.0.1:9464/readyz
```

## 9. Upgrades and migrations

Make a backup first ([section 10](#10-backups-and-restore)), then:

```sh
sudo install -m 0755 scacelith-server /usr/local/bin/scacelith-server.new
sudo systemctl stop scacelith-server
sudo mv /usr/local/bin/scacelith-server.new /usr/local/bin/scacelith-server
scs migrate              # optional: applies the new migrations now and prints what it did
scs check-config         # warnings about settings the new version changed
sudo systemctl start scacelith-server
```

`start` applies pending migrations itself before it reports ready; running `migrate` first keeps
a long migration out of `TimeoutStartSec` and shows its result. Each migration runs in its own
transaction and is recorded with a checksum. A server refuses to start on a database that holds
a migration it does not know (written by a newer version) or one whose file changed: going back
to an older version means restoring the backup made before the upgrade. Players see a shutdown
notice, reconnect by themselves, and their games in progress resume.

## 10. Backups and restore

The database is one SQLite file in WAL mode, `/var/lib/scacelith/scacelith.db`, with its `-wal`
and `-shm` files while the server runs: copying the files of a running server does not give a
consistent database. Take copies with the administration command, as `scacelith` and with the
environment file (the `scs` function of [section 3](#3-configuration)):

```sh
sudo install -d -m 0700 -o scacelith -g scacelith /var/lib/scacelith/backup
scs admin backup /var/lib/scacelith/backup/scacelith-$(date -u +%Y%m%dT%H%M%SZ).db --verify
# Backup written to /var/lib/scacelith/backup/scacelith-20261003T043000Z.db (35.2 MB in 412 ms, quick_check ok).
```

`admin backup <file>` writes a consistent snapshot of the database in one pass with SQLite's
`VACUUM INTO`, while the server keeps running and writing (the server's WAL grows until the copy
ends: take it at a quiet hour). The file must not exist yet; it is created with mode 0600, and a
relative path is resolved from the current directory. `--verify` then runs `PRAGMA quick_check`
on the copy, and the command fails when the check does not answer `ok`; `--json` prints the
result as JSON (`file`, `bytes`, `ms`, `verified`). The command refuses to copy anything when
`DB_PATH` is not an existing Scacelith database (no such file, or no applied migration), so a
backup run with the wrong configuration fails instead of copying an empty database. Exit codes:
0 written, 1 refused or failed (`error: ...` or `admin: ...` on standard error), 2 usage.

The copy is a single file. It holds e-mail addresses and the IP addresses of the last
`RETENTION_IP_DAYS`: encrypt it, move it off the machine, and delete the local copy. Back up
`/etc/scacelith/` (the secret files in particular) separately, never next to the database copies:
`SERVER_SECRET` and `MFA_ENCRYPTION_KEY` together decrypt the players' authenticator secrets, and
without them a restored database has broken recovery codes and authenticator enrolments.

A daily snapshot with a timer, overwriting `backup/scacelith.db` for your off-site backup tool to
collect:

```ini
# /etc/systemd/system/scacelith-server-backup.service
[Unit]
Description=Snapshot of the Scacelith database

[Service]
Type=oneshot
User=scacelith
Group=scacelith
UMask=0077
Nice=10
Environment=SCACELITH_ENV_FILE=/etc/scacelith/scacelith-server.env
ProtectSystem=strict
ReadWritePaths=/var/lib/scacelith
PrivateTmp=yes
NoNewPrivileges=yes
ExecStartPre=rm -f /var/lib/scacelith/backup/scacelith.db.new
ExecStart=/usr/local/bin/scacelith-server admin backup /var/lib/scacelith/backup/scacelith.db.new --verify
ExecStartPost=mv -f /var/lib/scacelith/backup/scacelith.db.new /var/lib/scacelith/backup/scacelith.db

# /etc/systemd/system/scacelith-server-backup.timer
[Unit]
Description=Daily snapshot of the Scacelith database

[Timer]
OnCalendar=*-*-* 04:30:00
RandomizedDelaySec=15min
Persistent=true

[Install]
WantedBy=timers.target
```

Enable it with `sudo systemctl enable --now scacelith-server-backup.timer`; a failed snapshot
leaves the unit failed (`systemctl status scacelith-server-backup`) and keeps the previous copy.

The `sqlite3` command (SQLite 3.27 or later) can make the same snapshot through a read-only
connection, without the checks of the command above:

```sh
sudo -u scacelith sqlite3 -readonly -cmd '.timeout 10000' /var/lib/scacelith/scacelith.db \
  "VACUUM INTO '/var/lib/scacelith/backup/scacelith-manual.db'"
sudo -u scacelith sqlite3 -readonly /var/lib/scacelith/backup/scacelith-manual.db 'PRAGMA quick_check'   # prints: ok
```

Do not use the `.backup` command of `sqlite3` on a running server: it copies the database a few
pages at a time and starts over whenever the server writes, so on a busy server it may never
finish.

**Restoring a copy.** Stop the server, move the current database (with its `-wal` and `-shm`
files) **and the game journal** aside, install the copy, and start:

```sh
sudo systemctl stop scacelith-server
sudo -u scacelith sh -c 'cd /var/lib/scacelith && mkdir before-restore && mv scacelith.db* journal before-restore/'
sudo install -m 0600 -o scacelith -g scacelith scacelith-20261003T043000Z.db /var/lib/scacelith/scacelith.db
sudo systemctl start scacelith-server
```

The journal must not be replayed against an older copy. It holds the games that were in progress
when the server stopped, and the server replays each of them at the start without looking at the
database, then commits it to the database when it ends. A game whose player registered after the
copy was made can never be committed (the copy has no such account): its commit fails and is
retried for ever, at least every 10 seconds and with an error in the log each time; the other
finished games of its shard can wait up to 10 seconds for their own commit; its other player
stays "in a game" for matchmaking and challenges until the next restart; and the journal keeps
it, so every later start replays it again. The journal cannot bring back the games that ended between the copy and the
stop either: once they are in the database, the journal forgets them. So the games in progress at
the stop are lost with a restore, and their players find no game when they reconnect. Keep the
journal only with a copy made after the server stopped (it then matches the journal exactly), for
example when moving the server to another machine: stop it, take the copy, and move the copy and
`journal/` together.

## 11. Several instances

Each instance is an independent server, with its own accounts, ratings and games, its own
environment file, data directory, ports and certificate. Never point two instances at the same
`DATA_DIR`, `DB_PATH` or `JOURNAL_DIR`: a server assumes it is the only process writing its
database and journal. Independent instances can all keep `SHARD_BASE` at its default. A template
unit, `scacelith-server@<name>.service`, derived from the example:

```sh
sed -e 's|^Description=.*|Description=Scacelith dedicated server (%i)|' \
    -e 's|/etc/scacelith/scacelith-server.env|/etc/scacelith/%i.env|' \
    -e 's|^StateDirectory=scacelith$|StateDirectory=scacelith/%i|' \
    -e 's|/var/lib/scacelith|/var/lib/scacelith/%i|g' \
    deploy/systemd/scacelith-server.service | sudo tee /etc/systemd/system/scacelith-server@.service >/dev/null
sudo systemctl daemon-reload
```

For an instance named `staging`, create `/etc/scacelith/staging.env` from the example with
`DATA_DIR=/var/lib/scacelith/staging`, its own `SERVER_PUBLIC_HOST`, certificate files (for
example in `/etc/scacelith/tls/staging/`, created like `/etc/scacelith/tls/`), `INSTANCE_ID=staging`,
and ports no other instance uses on the same address: `API_PORT` (and `WS_PORT`) and
`METRICS_PORT` (to use port 443 for every instance, give each one an address of its own with
`BIND_ADDRESS`, the first instance included). Then:

```sh
sudo systemctl enable --now scacelith-server@staging
sudo -u scacelith env SCACELITH_ENV_FILE=/etc/scacelith/staging.env scacelith-server check-config
```

Give each instance its own copy of the certbot hook, with its `CERT_NAME`, `DEST` and
`UNIT=scacelith-server@staging.service`. All instances run as `scacelith` and can read each
other's data; for a stronger separation, give an instance an account of its own (a line in the
sysusers file, and `User=` and `Group=` in a drop-in of that instance).

## Older systemd

`Type=notify-reload` needs systemd 253. With systemd 252 or older, override two settings in a
drop-in (`systemctl edit scacelith-server`):

```ini
[Service]
Type=notify
ExecReload=/bin/kill -HUP $MAINPID
```

`systemctl reload` then sends `SIGHUP` through `ExecReload=`, and the server reloads its
certificate the same way. Older systemd versions ignore the sandbox settings they do not know,
with a warning in the journal: check `systemctl status` after the first start.

## What the unit expects from the server

* `scacelith-server start` runs in the foreground, takes its configuration from the environment,
  and sends `READY=1` (sd_notify) once the database is migrated, the games in progress are
  recovered and the ports are bound; `STATUS=` lines while it runs.
* `SIGHUP`: `RELOADING=1` (with `MONOTONIC_USEC`), the TLS certificate and key are re-read, then
  `READY=1`.
* `SIGTERM` or `SIGINT`: `STOPPING=1`, drain for `SHUTDOWN_GRACE_MS`, final commits, exit 0; a
  second signal exits 1. Any failure exits 1.
* `WATCHDOG=1` every half `WatchdogSec` while `WATCHDOG_USEC` is set, until the process exits.
* Log lines on standard output, prefixed with their syslog priority when it is the journal
  (`JOURNAL_STREAM`).
* Needs `CAP_NET_BIND_SERVICE` for a port below 1024, read access to the TLS files, write access
  to `DATA_DIR` only, and `/tmp` shared by the server and its analysis engines.
