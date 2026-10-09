#!/bin/sh
# Checks the contract between this server and a checkout of the game (DarkCenobyte/scacelith-chess),
# everything but the live tests (tools/live-check):
#   1. protogen --check --client: the game's C++ codec and its copy of the protocol (protocol/)
#   2. the rating vectors, the mating-material positions and the server's PGN files, of which the
#      game keeps a copy
#   3. the chess cross-check vectors, regenerated from the game's own rules
#   4. the server tests that read the game's sources (SCACELITH_CLIENT_DIR)
# Usage: tools/interop/check-game.sh GAME_CHECKOUT   (from anywhere; CXX overrides g++)
set -eu
here=$(cd "$(dirname "$0")" && pwd)
server=$(dirname "$(dirname "$here")")
if [ $# -ne 1 ] || [ ! -d "$1/src/net" ]; then
    echo "usage: tools/interop/check-game.sh GAME_CHECKOUT" >&2
    exit 2
fi
game=$(cd "$1" && pwd)
cd "$server"
status=0

echo "== protocol: the game's C++ codec and protocol/"
cargo run --quiet --locked -p scacelith-protocol --features gen --bin protogen -- --check --client "$game" || status=1

echo "== shared fixtures"
cmp test/fixtures/elo-vectors.json "$game/tests/data/elo-vectors.json" || status=1
cmp test/fixtures/mating-material.json "$game/tests/data/mating-material.json" || status=1
diff -r test/fixtures/server-pgn "$game/tests/data/server-pgn" || status=1

echo "== chess cross-check vectors from the game's rules"
work=$(mktemp -d "${TMPDIR:-/tmp}/check-game.XXXXXX")
trap 'rm -rf "$work"' EXIT INT TERM
sh tools/gen-chess-crosscheck.sh "$game" "$work/chess-crosscheck.json"
cmp "$work/chess-crosscheck.json" test/fixtures/chess-crosscheck.json || status=1

echo "== server tests that read the game's sources"
SCACELITH_CLIENT_DIR="$game" cargo test --quiet --locked -p scacelith-protocol --lib generated_files_are_fresh || status=1
SCACELITH_CLIENT_DIR="$game" cargo test --quiet --locked -p scacelith-server --lib heartbeat_interval_gives_the_game_clients || status=1

if [ "$status" -eq 0 ]; then
    echo "== the game's checkout matches this server's contract"
else
    echo "== the game's checkout does not match this server's contract (see above)" >&2
fi
exit "$status"
