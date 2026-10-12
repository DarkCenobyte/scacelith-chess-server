# Scacelith dedicated server

[![CI](https://img.shields.io/github/actions/workflow/status/DarkCenobyte/scacelith-chess-server/ci.yml?branch=master&event=push&label=CI&logo=githubactions&logoColor=white)](https://github.com/DarkCenobyte/scacelith-chess-server/actions/workflows/ci.yml)
[![Release build](https://img.shields.io/github/actions/workflow/status/DarkCenobyte/scacelith-chess-server/release.yml?event=push&label=release%20build&logo=githubactions&logoColor=white)](https://github.com/DarkCenobyte/scacelith-chess-server/actions/workflows/release.yml)
[![CodeQL](https://img.shields.io/github/actions/workflow/status/DarkCenobyte/scacelith-chess-server/codeql.yml?branch=master&event=push&label=CodeQL&logo=github)](https://github.com/DarkCenobyte/scacelith-chess-server/actions/workflows/codeql.yml)
[![Coverage](https://img.shields.io/endpoint?url=https%3A%2F%2Fraw.githubusercontent.com%2FDarkCenobyte%2Fscacelith-chess-server%2Fbadges%2Fcoverage.json)](https://github.com/DarkCenobyte/scacelith-chess-server/actions/workflows/coverage.yml)
[![Latest release](https://img.shields.io/github/v/release/DarkCenobyte/scacelith-chess-server?sort=semver&display_name=tag&logo=github)](https://github.com/DarkCenobyte/scacelith-chess-server/releases/latest)
[![Downloads](https://img.shields.io/github/downloads/DarkCenobyte/scacelith-chess-server/total?logo=github)](https://github.com/DarkCenobyte/scacelith-chess-server/releases)
[![Licence](https://img.shields.io/github/license/DarkCenobyte/scacelith-chess-server?color=blue)](LICENSE)
[![Last commit](https://img.shields.io/github/last-commit/DarkCenobyte/scacelith-chess-server/master)](https://github.com/DarkCenobyte/scacelith-chess-server/commits/master)

[![Top language](https://img.shields.io/github/languages/top/DarkCenobyte/scacelith-chess-server?logo=rust)](https://github.com/DarkCenobyte/scacelith-chess-server)
[![Languages](https://img.shields.io/github/languages/count/DarkCenobyte/scacelith-chess-server)](https://github.com/DarkCenobyte/scacelith-chess-server)
[![Rust toolchain](https://img.shields.io/badge/dynamic/toml?url=https%3A%2F%2Fraw.githubusercontent.com%2FDarkCenobyte%2Fscacelith-chess-server%2Fmaster%2Frust-toolchain.toml&query=%24.toolchain.channel&label=Rust&logo=rust&color=B7410E)](rust-toolchain.toml)
[![Online protocol](https://img.shields.io/badge/dynamic/json?url=https%3A%2F%2Fraw.githubusercontent.com%2FDarkCenobyte%2Fscacelith-chess-server%2Fmaster%2Fprotocol%2Fscacelith-v1.json&query=%24.minor&prefix=v1.&label=online%20protocol&color=6E4C9E)](docs/PROTOCOL.md)
![Linux x86-64 (static, musl)](https://img.shields.io/badge/Linux-x86--64%20%28static%2C%20musl%29-FCC624?logo=linux&logoColor=black)
![SQLite](https://img.shields.io/badge/database-SQLite-003B57?logo=sqlite&logoColor=white)

[![Website](https://img.shields.io/website?url=https%3A%2F%2Fscacelith.com&label=scacelith.com&up_message=online&down_message=offline)](https://scacelith.com)
[![Official server](https://img.shields.io/website?url=https%3A%2F%2Fcaissa.scacelith.com%2Freadyz&label=official%20server&up_message=online&down_message=offline)](https://scacelith.com)
[![API reference](https://img.shields.io/website?url=https%3A%2F%2Fdarkcenobyte.github.io%2Fscacelith-chess-server%2F&label=API%20reference&up_message=OpenAPI%203.1&down_message=offline&up_color=6BA539&logo=openapiinitiative&logoColor=white)](https://darkcenobyte.github.io/scacelith-chess-server/)
[![Game client](https://img.shields.io/github/v/release/DarkCenobyte/scacelith-chess?sort=semver&display_name=tag&label=game%20client)](https://github.com/DarkCenobyte/scacelith-chess)
[![Built with Claude Code](https://img.shields.io/badge/built%20with-Claude%20Code-D97757?logo=claude&logoColor=white)](https://claude.com/claude-code)

The online multiplayer server of [Scacelith](https://github.com/DarkCenobyte/scacelith-chess), a
photorealistic chess game played in first person: accounts, rated matchmaking, challenges and private
games, authoritative games with server clocks (the robots press the clock by themselves unless
`AUTO_PRESS_CLOCK=false`), the opponent's live gestures (head, hand) and stance (standing up to
look at the board) relayed without being stored, one Elo per official time control, anti-cheat
and reports. It is one Rust program, `scacelith-server`, a single binary with its database
(SQLite) built in. The game (for Windows, Linux and macOS, developed in
[DarkCenobyte/scacelith-chess](https://github.com/DarkCenobyte/scacelith-chess)) is only a client
of it; anyone can run a community server, and players choose the server in the game's Options.

- Official server: `caissa.scacelith.com`, TCP port `443` (HTTPS API and WSS on the same port).
- Licence: GPL-3.0-or-later, for the source and for the binaries built from it (see
  [Licence and third parties](#licence-and-third-parties)).

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


## Installing

Each release on the [Releases page](https://github.com/DarkCenobyte/scacelith-chess-server/releases)
has the static x86-64 Linux binary, alone (`scacelith-server-<version>-x86_64-linux-musl`) and in
an archive with this README, the deployment, administration and configuration docs,
`.env.example` and the systemd files (`scacelith-server-<version>-x86_64-linux-musl.tar.gz`), its
software bill of materials (CycloneDX, `scacelith-server-<version>.cdx.json`) and `SHA256SUMS`.
The release workflow builds them from the tag, and their build provenance and the binary's SBOM
are attested; to check a download with the GitHub CLI:

```sh
gh attestation verify scacelith-server-1.1.1-x86_64-linux-musl.tar.gz --repo DarkCenobyte/scacelith-chess-server
```

The binary runs on any x86-64 Linux, whatever its C library, and is all the server needs: the
migrations, the GIF fonts and pieces and the list of common passwords are built into it. It also
needs a TLS certificate for the server's public name, and optionally the official Stockfish 19
binary for the anti-cheat analysis and an SMTP account for e-mail confirmation and password
resets. One CPU core per game shard: by default one shard per core, up to 16.

```sh
cp .env.example .env                              # never commit .env
./scacelith-server gen-secret                     # paste the value as SERVER_SECRET in .env
# edit .env: SERVER_NAME, SERVER_PUBLIC_HOST, TLS_CERT_FILE, TLS_KEY_FILE, mail settings...
./scacelith-server check-config                   # prints the configuration (secrets hidden) or the errors
./scacelith-server start
```

`start` applies the database migrations, replays the game journal (games that were running when
the server stopped come back), starts one game shard per core and listens on `API_PORT` (443 by
default; a port below 1024 needs a capability, see
[Ports and firewall](docs/ADMINISTRATION.md#ports-and-firewall)). `SIGTERM` or Ctrl-C stops it
gracefully: players are warned, finished games are committed and running games stay in the
journal, to resume at the next start. `SIGHUP` reloads the TLS certificate.

[docs/DEPLOY.md](docs/DEPLOY.md) installs the server as a systemd service (systemd 253 or later
for the example unit; older versions are covered too), and
[docs/ADMINISTRATION.md](docs/ADMINISTRATION.md) covers the rest of an operator's work: TLS
certificates, ports, kernel settings, accounts and e-mail, protection against abuse, secrets,
backups, moderation, monitoring and scaling.

## Building

You need Rust 1.99.0 (`rust-toolchain.toml` selects it with rustup) and a C compiler (for the
bundled SQLite and *ring*); the static release binary also needs the `x86_64-unknown-linux-musl`
target and a musl C compiler (Debian and Ubuntu: `musl-tools`).

```sh
cargo build --release --locked -p scacelith-server
# -> target/release/scacelith-server, linked to the build machine's glibc

CC_x86_64_unknown_linux_musl=musl-gcc \
  cargo build --release --locked -p scacelith-server --target x86_64-unknown-linux-musl
# -> target/x86_64-unknown-linux-musl/release/scacelith-server, static

cargo test --workspace                            # unit, integration and end-to-end tests
```

[docs/DEVELOPMENT.md](docs/DEVELOPMENT.md) has the other checks, the contract shared with the
game, the test coverage, the continuous integration and the releases.

## Commands and options

| Command | Effect |
|---|---|
| `scacelith-server [start]` | runs the server in the foreground (the default command) |
| `scacelith-server migrate` | applies the database migrations, prints a JSON report and exits |
| `scacelith-server check-config` | validates the configuration and prints it with the secrets hidden (warnings on stderr) |
| `scacelith-server gen-secret` | prints a new random value for `SERVER_SECRET` (48 random bytes, base64) |
| `scacelith-server gen-config-docs [--check]` | writes `.env.example` and `docs/CONFIG.md` from the configuration keys (run it at the root of the repository) |
| `scacelith-server admin ...` | moderation and maintenance commands (`admin --help`): users, bans, the integrity list, reports, backups |
| `scacelith-server version`, `help` | the version, the list of commands |

Exit codes: 0 success, 1 failure (an invalid configuration included), 2 usage.

Configuration comes from the environment, then from the `.env` file of the working directory, or
from the file named by `SCACELITH_ENV_FILE` (set but empty: no file). `.env.example` lists every
key with its default and is the only configuration file in Git; [docs/CONFIG.md](docs/CONFIG.md)
describes each one. Secrets can also be read from files with the `_FILE` suffix
(`SERVER_SECRET_FILE=/etc/scacelith/server-secret`), which keeps them out of the environment.
Keys of the former Node.js server that no longer apply get a warning from `check-config`, never
an error. The main ones:

| Key | Default | Effect |
|---|---|---|
| `SERVER_SECRET` | required | the master secret, at least 32 random bytes (`gen-secret`) |
| `SERVER_NAME` | `Scacelith Community Server` | the name shown to players |
| `SERVER_PUBLIC_HOST` | `localhost` | the public DNS name players type, used in e-mail links and for Google sign-in |
| `API_PORT`, `BIND_ADDRESS` | `443`, `0.0.0.0` | where the HTTPS API and the game WebSocket listen |
| `TLS_MODE` | `native` | `native` (`TLS_CERT_FILE`, `TLS_KEY_FILE`), `proxy` behind a reverse proxy, or `off` for local development |
| `DATA_DIR` | `./data` | the database and the game journal |
| `WORKERS` | `auto` | game shards and runtime threads: one per core, at most 16 |
| `MAX_CONNECTIONS` | `200000` | signed-in players, whole server: set it to what the machine holds |
| `REGISTRATION` | `open` | whether new accounts can be created from the game |
| `MAIL_TRANSPORT` | `log` | `smtp` (the `SMTP_*` keys), `log` or `none` |
| `ANALYSIS_ENGINE_PATH` | (empty) | the Stockfish 19 binary of the anti-cheat analysis (empty: no analysis) |
| `METRICS_PORT` | `9464` | Prometheus metrics, `/healthz` and `/readyz` on 127.0.0.1 (0: off) |
| `LOG_LEVEL` | `info` | `debug`, `info`, `warn` or `error` |

## Documentation

| Document | Content |
|---|---|
| [docs/DEPLOY.md](docs/DEPLOY.md) | installing the server as a systemd service: build, account, configuration, TLS, firewall, logs, upgrades, backups and restore |
| [docs/ADMINISTRATION.md](docs/ADMINISTRATION.md) | running a server: ports, TLS certificates, the service, kernel settings, accounts and e-mail, GIFs, password hashing, protection against abuse, secrets, data, moderation, monitoring, scaling |
| [docs/CONFIG.md](docs/CONFIG.md) | every setting, with its default and range (generated from the code) |
| [docs/API.md](docs/API.md) | the HTTPS API: every endpoint with its answers, errors, rate limits and curl examples |
| [OpenAPI reference](https://darkcenobyte.github.io/scacelith-chess-server/) | the same API as an OpenAPI 3.1 description, in the ten languages of the game (`gh-pages` branch) |
| [docs/PROTOCOL.md](docs/PROTOCOL.md) | the realtime WebSocket protocol, version 1 (frozen); its schema is `protocol/scacelith-v1.json`, the released wire `protocol/frozen/` |
| [docs/DESIGN.md](docs/DESIGN.md) | how the server works: flows, contracts, game policies, security model |
| [docs/RUST-PORT.md](docs/RUST-PORT.md) | architecture and code layout: process model, crates and modules, interfaces |
| [docs/ANTICHEAT.md](docs/ANTICHEAT.md) | anomalies, sanctions, rating refunds, engine analysis, the statistical model, reports, moderation |
| [docs/SIZING.md](docs/SIZING.md) | sizing and hosting on a small VPS: capacity, memory, disk, settings |
| [docs/BENCHMARK.md](docs/BENCHMARK.md) | load tests and measured results |
| [docs/DEVELOPMENT.md](docs/DEVELOPMENT.md) | checks, the contract with the game, test coverage, continuous integration and releases |

## Licence and third parties

The dedicated server is free software under the GNU General Public License, version 3 or (at your
option) any later version (`GPL-3.0-or-later`, the `LICENSE` file at the root of the repository).
That covers its source and every binary built from it: whoever distributes a binary must also
offer its corresponding source under the same licence.

Built into the binary:

- **Rust crates** from crates.io (`Cargo.lock`), among them tokio, hyper, rustls and *ring*,
  rusqlite, argon2, serde_json and the RustCrypto crates, under licences compatible with the GPL
  (Apache-2.0, MIT, BSD-3-Clause, ISC, Zlib, Unlicense, Unicode-3.0, and CDLA-Permissive-2.0 for
  the Mozilla root certificates of webpki-roots); `cargo deny check` enforces that list
  (`deny.toml`), and each release's SBOM (`scacelith-server-<version>.cdx.json`) names every crate
  with its version and licence.
- **[SQLite](https://sqlite.org)**, compiled in by rusqlite's `bundled` feature: public domain.
- **The "cburnett" chess pieces** of the GIFs (`assets/pieces/cburnett/`) by Colin M.L. Burnett,
  GPL version 2 or later, as distributed with lichess (`assets/pieces/cburnett/LICENSE.md`); the
  combination is distributed under the GPL version 3 or later.
- **"Scacelith GIF"**, the GIFs' bitmap font (`assets/fonts/scacelith-gif/`): subsets of Terminus
  Font 4.49.1 by Dimitar Toshkov Zhekov, modified and renamed, SIL Open Font License 1.1
  (`assets/fonts/scacelith-gif/OFL.txt`).
- **The list of common passwords** refused at registration
  (`crates/server/src/security/password/common-passwords.txt`), drawn from widely published
  lists, SecLists' among them (MIT).

Used, not shipped: [Stockfish](https://stockfishchess.org) 19 (GPL-3.0-or-later), an optional
separate program that the server starts for the anti-cheat analysis.

## Links

- Official website: [scacelith.com](https://scacelith.com)
- The game client, for Windows, Linux and macOS:
  [DarkCenobyte/scacelith-chess](https://github.com/DarkCenobyte/scacelith-chess)
- The API reference: [darkcenobyte.github.io/scacelith-chess-server](https://darkcenobyte.github.io/scacelith-chess-server/)
