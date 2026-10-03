#!/bin/sh
# Builds tools/gen-chess-crosscheck.cpp against the game's chess rules (src/chess/position.cpp,
# src/chess/game.cpp of a checkout of DarkCenobyte/scacelith-chess) and regenerates
# test/fixtures/chess-crosscheck.json.
# Usage: tools/gen-chess-crosscheck.sh GAME_CHECKOUT [output.json]   (CXX overrides g++)
set -eu
here=$(cd "$(dirname "$0")" && pwd)
server=$(dirname "$here")
if [ $# -lt 1 ] || [ ! -f "$1/src/chess/position.cpp" ]; then
    echo "usage: tools/gen-chess-crosscheck.sh GAME_CHECKOUT [output.json]" >&2
    exit 2
fi
root=$(cd "$1" && pwd)
out=${2:-"$server/test/fixtures/chess-crosscheck.json"}
cxx=${CXX:-g++}
build=$(mktemp -d "${TMPDIR:-/tmp}/chess-crosscheck.XXXXXX")
trap 'rm -rf "$build"' EXIT INT TERM
"$cxx" -std=c++17 -O2 -Wall -Wextra -I "$root/src" \
    "$here/gen-chess-crosscheck.cpp" "$root/src/chess/position.cpp" "$root/src/chess/game.cpp" \
    -o "$build/gen-chess-crosscheck"
"$build/gen-chess-crosscheck" "$root/assets/i18n/en.lang" "$out"
