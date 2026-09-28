#!/bin/sh
# Builds tools/gen-chess-crosscheck.cpp against the game's chess rules (src/chess/position.cpp,
# src/chess/game.cpp) and regenerates test/fixtures/chess-crosscheck.json.
# Usage: dedicated-server/tools/gen-chess-crosscheck.sh [output.json]   (CXX overrides g++)
set -eu
here=$(cd "$(dirname "$0")" && pwd)
server=$(dirname "$here")
root=$(dirname "$server")
out=${1:-"$server/test/fixtures/chess-crosscheck.json"}
cxx=${CXX:-g++}
build=$(mktemp -d "${TMPDIR:-/tmp}/chess-crosscheck.XXXXXX")
trap 'rm -rf "$build"' EXIT INT TERM
"$cxx" -std=c++17 -O2 -Wall -Wextra -I "$root/src" \
    "$here/gen-chess-crosscheck.cpp" "$root/src/chess/position.cpp" "$root/src/chess/game.cpp" \
    -o "$build/gen-chess-crosscheck"
"$build/gen-chess-crosscheck" "$root/assets/i18n/en.lang" "$out"
