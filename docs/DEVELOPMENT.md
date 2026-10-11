# Developing the Scacelith server

The workspace, its checks, the contract shared with the game, the continuous integration and
the releases. [RUST-PORT.md](RUST-PORT.md) describes the architecture and the code layout,
[DESIGN.md](DESIGN.md) how the server works.

## Checks

```sh
cargo test --workspace                                   # unit, integration and end-to-end tests
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check
cargo doc --workspace --no-deps
cargo run -p scacelith-protocol --features gen --bin protogen -- --check   # generated protocol files
cargo run -p scacelith-server -- gen-config-docs --check                  # .env.example, docs/CONFIG.md
cargo deny check                                         # advisories, licences, sources (deny.toml)
```

The workspace has five crates: `scacelith-server` (the server), `scacelith-protocol` (the realtime
protocol codec and its generator, `protogen`), `scacelith-chess` (rules and notation),
`scacelith-gif` (the GIF renderer) and `scacelith-client` (a Rust client used by the tests and the
load generator). The protocol schema, `protocol/scacelith-v1.json`, is the single source of the
Rust and C++ codecs: change it, run `protogen`, and commit the generated files with it.

## The contract with the game

The game and the server share a contract, kept on both sides: the realtime protocol (the schema,
the frozen manifests, [PROTOCOL.md](PROTOCOL.md) and the golden vectors, of which the
game keeps a copy in its `protocol/` folder), the rating vectors (`test/fixtures/elo-vectors.json`,
the game's `tests/data/elo-vectors.json`), the positions of who can still mate
(`test/fixtures/mating-material.json`, the game's `tests/data/mating-material.json`: the material
that decides whether a resignation, a flag fall or a forfeit is a loss or a draw) and the server's
PGN files (`test/fixtures/server-pgn/`, the game's `tests/data/server-pgn/`). `protogen -- --client ../scacelith-chess` writes the game's
C++ codec and its copy of the protocol in a checkout of the game, and
`tools/interop/check-game.sh ../scacelith-chess` checks the whole contract against one. The
configuration keys live in `crates/server/src/config/keys.rs`: after a change, run
`gen-config-docs` (without `--check`). The real-engine analysis tests run when Stockfish is
installed where distributions put it, or named by `SCACELITH_TEST_ENGINE`, and return at once
otherwise.

## Continuous integration and releases

GitHub Actions runs all the checks above on every push to master and every pull request
(`.github/workflows/ci.yml`), with the real-engine tests (Ubuntu's Stockfish), the static release
build, a dependency review of pull requests, the contract with the game and its live tests (against
the game's commit pinned in `tools/interop/game-revision`, so that a run of a server commit always
tests the same pair; a manual run of the workflow takes another branch of the game as
`game-revision`, for a change made on both sides) and a lint of the workflows (actionlint, zizmor); a
weekly run catches new advisories and, against the game's master, changes on the game's side. Move
the pin forward with every change of the contract; the game pins this server the same way (its
`tools/interop/server-revision`) and runs the same contract and live tests in its own CI. CodeQL
(`.github/workflows/codeql.yml`) scans the Rust code and the workflows. Dependabot
(`.github/dependabot.yml`) proposes the updates of the crates, the Rust toolchain and the actions,
each a week after its release. The Coverage workflow ([Test coverage](#test-coverage)) is not one
of the checks master requires.

A release is published by pushing the tag `v` + the workspace version of `Cargo.toml`
(`v1.0.0`): the release workflow (`.github/workflows/release.yml`) checks that they match, runs the
CI again on the tagged commit, builds the static binary from scratch, writes its SBOM, attests both
and publishes the GitHub release (a pre-release when the version has a suffix, such as `-rc.1`).
Started by hand, it builds the same files and keeps them as workflow artifacts without publishing
anything, unless it runs on a tag with "Publish" ticked.

## Test coverage

[cargo-llvm-cov](https://github.com/taiki-e/cargo-llvm-cov) measures what the tests cover, for
the crates (not the test tools of `tools/`):

```sh
rustup component add llvm-tools
cargo install cargo-llvm-cov --locked
cargo llvm-cov --workspace --exclude-from-report scacelith-live-check \
  --exclude-from-report scacelith-rest-diff --html          # -> target/llvm-cov/html/index.html
```

The Coverage workflow (`.github/workflows/coverage.yml`) does the same on every pull request and
every push to master, with the real-engine tests, and shows the line, function and region coverage
in the run's summary. On master it also writes the line coverage to `coverage.json` on the
`badges` branch (one commit, replaced at each run), which the README's coverage badge reads
through shields.io.

## Comparison tools

Two tools check the server against its clients and its predecessor:

* `tools/live-check` (`scacelith-live-check`) runs the game's own C++ online client tests, Linux
  and Windows (Wine) builds, against a server started for each part, over real TLS
  ([its README](../tools/live-check/README.md));
* `tools/rest-diff` (`scacelith-rest-diff`) replays the same scenarios against the former Node.js
  server and this one and reports every difference in the HTTPS API answers; the accepted ones,
  each with its reason, are in `tools/rest-diff/accepted.txt`
  ([its README](../tools/rest-diff/README.md)).
