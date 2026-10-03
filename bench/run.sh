#!/usr/bin/env bash
# Benchmark harness: the Rust server against the Node.js server (Node 24 and Node 26), same
# machine, same settings, same load generator (docs/BENCHMARK.md).
#
#   bench/run.sh [--targets rust,node24,node26] [--scenarios idle,connections,...] [--quick]
#
# For each target it prepares a fresh data directory (migrations, bench accounts with live
# sessions), then for each scenario starts the server pinned to the server CPUs, runs
# scacelith-bench pinned to the load CPUs, and stops the server. Results go to
# bench/results/<date>/: one JSON report per target and scenario, the logs, and summary.md (the
# Markdown tables, the only file kept by git).
#
# Never point it at a production server: it creates accounts and raises every rate limit.
set -euo pipefail

BENCH_DIR=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
SERVER_DIR=$(dirname "$BENCH_DIR")

usage() {
    cat <<'EOF'
Usage: bench/run.sh [options] [-- extra scacelith-bench options]

  --targets LIST      rust,node24,node26                                  [all three]
  --scenarios LIST    idle,connections,games,matchmaking,rest,login       [all]
  --quick             small sizes and short windows (checks the set-up in a few minutes)
  --node24 PATH       Node.js 24 binary                                   [$NODE24]
  --node26 PATH       Node.js 26 binary                                   [$NODE26]
  --node-tree DIR     dedicated-server/ of a checkout of the Node.js server, with its
                      node_modules installed (git worktree add ../node-ref 7531830;
                      cd ../node-ref/dedicated-server && npm ci)          [$SCACELITH_NODE_TREE]
  --server-cpus LIST  CPUs of the server (taskset -c)                     [0,1]
  --load-cpus LIST    CPUs of the load generator                          [2,3]
  --port N            API and WebSocket port (both servers)               [18443]
  --metrics-port N    metrics listener port                               [19464]
  --out DIR           results directory                                   [bench/results/<date>]
  --no-build          use the release binaries already built
  --bench PATH        scacelith-bench binary        [target/release/scacelith-bench]
  --rust-server PATH  Rust server binary            [target/release/scacelith-server]
  -h, --help
EOF
}

TARGETS=rust,node24,node26
SCENARIOS=idle,connections,games,matchmaking,rest,login
QUICK=0
NODE24=${NODE24:-}
NODE26=${NODE26:-}
NODE_TREE=${SCACELITH_NODE_TREE:-}
SERVER_CPUS=0,1
LOAD_CPUS=2,3
PORT=18443
METRICS_PORT=19464
OUT=
BUILD=1
BENCH=$SERVER_DIR/target/release/scacelith-bench
RUST_SERVER=$SERVER_DIR/target/release/scacelith-server
EXTRA=()

while [[ $# -gt 0 ]]; do
    case $1 in
        --targets) TARGETS=$2; shift 2 ;;
        --scenarios) SCENARIOS=$2; shift 2 ;;
        --quick) QUICK=1; shift ;;
        --node24) NODE24=$2; shift 2 ;;
        --node26) NODE26=$2; shift 2 ;;
        --node-tree) NODE_TREE=$2; shift 2 ;;
        --server-cpus) SERVER_CPUS=$2; shift 2 ;;
        --load-cpus) LOAD_CPUS=$2; shift 2 ;;
        --port) PORT=$2; shift 2 ;;
        --metrics-port) METRICS_PORT=$2; shift 2 ;;
        --out) OUT=$2; shift 2 ;;
        --no-build) BUILD=0; shift ;;
        --bench) BENCH=$2; shift 2 ;;
        --rust-server) RUST_SERVER=$2; shift 2 ;;
        -h|--help) usage; exit 0 ;;
        --) shift; EXTRA=("$@"); break ;;
        *) echo "run.sh: unknown option $1" >&2; usage >&2; exit 2 ;;
    esac
done

die() { echo "run.sh: $*" >&2; exit 1; }
log() { echo "[run.sh $(date +%H:%M:%S)] $*" >&2; }

for tool in taskset openssl curl; do
    command -v "$tool" >/dev/null || die "$tool is required"
done
IFS=, read -r -a TARGET_LIST <<<"$TARGETS"
IFS=, read -r -a SCENARIO_LIST <<<"$SCENARIOS"

# ---- sizes ------------------------------------------------------------------------------------
# Full run: the sizes of the brief. Quick run: the same code paths with small sizes.
if [[ $QUICK == 1 ]]; then
    ACCOUNTS=600
    IDLE_ARGS=(--warmup-s 2 --duration-s 5)
    CONN_ARGS=(--steps 100,300 --warmup-s 2 --duration-s 5)
    GAMES_ARGS=(--steps 20,50 --warmup-s 3 --duration-s 8)
    MATCH_ARGS=(--steps 40,100 --rounds 2)
    REST_ARGS=(--concurrency 8 --setup-games 4 --warmup-s 2 --duration-s 4)
    LOGIN_ARGS=(--accounts 4 --concurrency 2 --warmup-s 2 --duration-s 6)
else
    ACCOUNTS=20000
    IDLE_ARGS=(--warmup-s 5 --duration-s 20)
    CONN_ARGS=(--steps 1000,5000,10000 --warmup-s 5 --duration-s 15)
    GAMES_ARGS=(--steps 100,500,1000 --warmup-s 10 --duration-s 30)
    MATCH_ARGS=(--steps 200,1000 --rounds 3)
    REST_ARGS=(--concurrency 32 --setup-games 16 --warmup-s 5 --duration-s 15)
    LOGIN_ARGS=(--accounts 16 --concurrency 8 --warmup-s 5 --duration-s 20)
fi

# ---- binaries -----------------------------------------------------------------------------------
if [[ $BUILD == 1 ]]; then
    log "building the release binaries"
    (cd "$SERVER_DIR" && cargo build --release -j "${CARGO_BUILD_JOBS:-2}" -p scacelith-server -p scacelith-client)
fi
[[ -x $BENCH ]] || die "missing $BENCH (build it, or drop --no-build)"

node_bin() {
    case $1 in
        node24) echo "$NODE24" ;;
        node26) echo "$NODE26" ;;
    esac
}

needs_node_tree=0
for t in "${TARGET_LIST[@]}"; do
    case $t in
        rust) [[ -x $RUST_SERVER ]] || die "missing $RUST_SERVER" ;;
        node24|node26)
            bin=$(node_bin "$t")
            [[ -n $bin && -x $bin ]] || die "$t: give the Node.js binary with --$t or \$${t^^}"
            needs_node_tree=1 ;;
        *) die "unknown target $t" ;;
    esac
done
if [[ $needs_node_tree == 1 ]]; then
    [[ -f $NODE_TREE/bin/scacelith-server.js ]] || die "--node-tree must be the dedicated-server/ of the Node.js server"
fi

# ---- results directory, certificate, shared settings ----------------------------------------------
STAMP=$(date +%Y-%m-%d_%H%M%S)
OUT=${OUT:-$BENCH_DIR/results/$STAMP}
WORK=$OUT/work
mkdir -p "$WORK"
CERT=$WORK/cert.pem
KEY=$WORK/key.pem
if [[ ! -f $CERT ]]; then
    # One self-signed ECDSA P-256 certificate for every server: same handshake cost. It is an
    # end-entity certificate (CA:FALSE), which the clients pin as their only trust anchor.
    openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes -days 7 \
        -subj /CN=localhost -addext "subjectAltName=DNS:localhost,IP:127.0.0.1" \
        -addext "basicConstraints=critical,CA:FALSE" -addext "keyUsage=critical,digitalSignature" \
        -addext "extendedKeyUsage=serverAuth" \
        -keyout "$KEY" -out "$CERT" 2>/dev/null
    chmod 600 "$KEY"
fi
SECRET=$(openssl rand -base64 48 | tr -d '\n')
NOFILE=$(ulimit -Hn)
[[ $NOFILE == unlimited ]] && NOFILE=1048576

# The settings both servers get. Per-worker limits of the Node.js server and whole-server limits
# of the Rust server have the same totals by default with the same WORKERS (password hashes, GIF
# threads, queue and cache, TLS handshake slots), so they stay at their defaults.
common_env() {
    local data=$1
    printf '%s\n' \
        "SCACELITH_ENV_FILE=" \
        "SERVER_NAME=Scacelith Bench" \
        "SERVER_PUBLIC_HOST=localhost" \
        "SERVER_SECRET=$SECRET" \
        "BIND_ADDRESS=127.0.0.1" \
        "API_PORT=$PORT" \
        "WS_PORT=$PORT" \
        "METRICS_PORT=$METRICS_PORT" \
        "METRICS_BIND=127.0.0.1" \
        "TLS_MODE=native" \
        "TLS_CERT_FILE=$CERT" \
        "TLS_KEY_FILE=$KEY" \
        "DATA_DIR=$data" \
        "WORKERS=2" \
        "MAIL_TRANSPORT=log" \
        "REQUIRE_EMAIL_VERIFICATION=false" \
        "POW_REGISTER_BITS=0" \
        "POW_LOGIN_BITS=0" \
        "LOG_LEVEL=warn" \
        "SHUTDOWN_GRACE_MS=200" \
        "ANALYSIS_WORKERS=0" \
        "ABUSE_EXEMPT=127.0.0.0/8,::1" \
        "HTTP_RATE_PER_IP=1000000" \
        "AUTH_RATE_PER_IP=1000000" \
        "AUTH_REGISTER_PER_HOUR=1000000" \
        "AUTH_FAILURES_PER_ACCOUNT=1000000" \
        "USER_RATE_PER_MIN=1000000" \
        "MAX_CONNECTIONS=1000000" \
        "MAX_CONNECTIONS_PER_IP=1000000" \
        "WS_MSG_RATE=100" \
        "WS_MSG_BURST=200" \
        "GESTURE_RATE=20" \
        "GESTURE_BURST=40" \
        "CHALLENGE_UNPLAYED_PER_MIN=1000000" \
        "MATCH_REPEAT_LIMIT=1000000" \
        "CONDUCT_ABANDON_LIMIT=1000000" \
        "GIF_USER_RENDERS_PER_MIN=1000000" \
        "GIF_USER_RENDERS_PER_HOUR=1000000" \
        "GIF_IP_RENDERS_PER_MIN=1000000" \
        "GIF_IP_RENDERS_PER_HOUR=1000000"
}

# Settings whose unit differs: every TLS handshake slot may serve the one load address
# (Node.js: 128 slots per worker; Rust: 128 x WORKERS for the whole server).
target_env() {
    case $1 in
        rust) echo "MAX_PENDING_HANDSHAKES_PER_IP=255" ;;
        node*) echo "MAX_PENDING_HANDSHAKES_PER_IP=127" ;;
    esac
}

# The command of a target's server, one argument per line ("start" or "migrate" appended).
server_cmd() {
    case $1 in
        rust) echo "$RUST_SERVER" ;;
        node*) printf '%s\n' "$(node_bin "$1")" "$NODE_TREE/bin/scacelith-server.js" ;;
    esac
}

with_env() {
    local target=$1 data=$2
    shift 2
    local vars=()
    mapfile -t vars < <(common_env "$data"; target_env "$target")
    env "${vars[@]}" "$@"
}

SERVER_PID=
stop_server() {
    [[ -n $SERVER_PID ]] || return 0
    if kill -0 "$SERVER_PID" 2>/dev/null; then
        kill -TERM "$SERVER_PID" 2>/dev/null || true
        for _ in $(seq 1 300); do
            kill -0 "$SERVER_PID" 2>/dev/null || break
            sleep 0.1
        done
        kill -KILL "$SERVER_PID" 2>/dev/null || true
    fi
    wait "$SERVER_PID" 2>/dev/null || true
    SERVER_PID=
}
trap stop_server EXIT INT TERM

port_free() {
    ! (exec 3<>"/dev/tcp/127.0.0.1/$1") 2>/dev/null
}

# Starts the server of $1 (data directory $2, log $3); sets SERVER_PID and SPAWNED_MS.
start_server() {
    local target=$1 data=$2 logfile=$3 cmd=()
    port_free "$PORT" || die "port $PORT is in use"
    port_free "$METRICS_PORT" || die "port $METRICS_PORT is in use"
    mapfile -t cmd < <(server_cmd "$target")
    SPAWNED_MS=$(date +%s%3N)
    # The subshell becomes the server (exec keeps its pid): SERVER_PID is the root of its tree.
    (
        ulimit -n "$NOFILE"
        cd "$data"
        mapfile -t vars < <(common_env "$data"; target_env "$target")
        exec env "${vars[@]}" taskset -c "$SERVER_CPUS" "${cmd[@]}" start
    ) >"$logfile" 2>&1 &
    SERVER_PID=$!
}

wait_ready() {
    local deadline=$((SECONDS + 120))
    while ((SECONDS < deadline)); do
        kill -0 "$SERVER_PID" 2>/dev/null || return 1
        if curl -sf -o /dev/null --max-time 2 --cacert "$CERT" "https://localhost:$PORT/api/v1/readyz" \
            --resolve "localhost:$PORT:127.0.0.1" &&
            curl -sf -o /dev/null --max-time 2 "http://127.0.0.1:$METRICS_PORT/readyz"; then
            return 0
        fi
        sleep 0.2
    done
    return 1
}

# Creates the bench accounts of a target in its data directory, with the server's own admin
# command (the same one on both: verified accounts with a live session each, no password hash).
make_accounts() {
    local target=$1 data=$2 tokens=$3 admin=()
    case $target in
        rust) admin=("$RUST_SERVER" admin) ;;
        node*) admin=("$(node_bin "$target")" "$NODE_TREE/bin/admin.js") ;;
    esac
    with_env "$target" "$data" "${admin[@]}" bench-accounts --count "$ACCOUNTS" --prefix bench \
        --out "$tokens" --format tsv --i-know-this-is-a-test-server >"$data/accounts.log" 2>&1 ||
        die "$target: account creation failed (see $data/accounts.log)"
}

version_of() {
    case $1 in
        rust) echo "scacelith-server $(git -C "$SERVER_DIR" rev-parse --short HEAD 2>/dev/null || echo unknown)" ;;
        node*) echo "node $("$(node_bin "$1")" --version)" ;;
    esac
}

hash_of() {
    case $1 in
        rust) echo "argon2id m=65536 t=3 p=4, PASSWORD_HASH_CONCURRENCY=2 (whole server)" ;;
        node*)
            if [[ $("$(node_bin "$1")" -e "process.stdout.write(typeof require('crypto').argon2)") == function ]]; then
                echo "argon2id m=65536 t=3 p=4, PASSWORD_HASH_CONCURRENCY=1 per worker x 2"
            else
                echo "scrypt N=2^17 r=8 p=1, PASSWORD_HASH_CONCURRENCY=1 per worker x 2"
            fi
            ;;
    esac
}

scenario_args() {
    case $1 in
        idle) echo "${IDLE_ARGS[@]}" ;;
        connections) echo "${CONN_ARGS[@]}" ;;
        games) echo "${GAMES_ARGS[@]}" ;;
        matchmaking) echo "${MATCH_ARGS[@]}" ;;
        rest) echo "${REST_ARGS[@]}" ;;
        login) echo "${LOGIN_ARGS[@]}" ;;
        *) die "unknown scenario $1" ;;
    esac
}

FAILED=()
for target in "${TARGET_LIST[@]}"; do
    data=$WORK/$target
    rm -rf "$data"
    mkdir -p "$data"
    log "$target: migrations and $ACCOUNTS accounts"
    mapfile -t cmd < <(server_cmd "$target")
    with_env "$target" "$data" "${cmd[@]}" migrate >"$data/migrate.log" 2>&1 ||
        die "$target: migrate failed (see $data/migrate.log)"
    tokens=$data/tokens.tsv
    make_accounts "$target" "$data" "$tokens"
    case $target in rust) proto=rust ;; *) proto=node ;; esac
    version=$(version_of "$target")
    hash=$(hash_of "$target")

    for scenario in "${SCENARIO_LIST[@]}"; do
        read -r -a args <<<"$(scenario_args "$scenario")"
        log "$target: $scenario"
        start_server "$target" "$data" "$data/server-$scenario.log"
        if [[ $scenario == idle ]]; then
            args+=(--spawned-at-ms "$SPAWNED_MS")
        elif ! wait_ready; then
            log "$target: the server did not become ready (see $data/server-$scenario.log)"
            stop_server
            FAILED+=("$target/$scenario")
            continue
        fi
        if ! taskset -c "$LOAD_CPUS" bash -c 'ulimit -n "$1"; shift; exec "$@"' _ "$NOFILE" \
            "$BENCH" "$scenario" "${args[@]}" \
            --target "$proto" --label "$target" \
            --addr "127.0.0.1:$PORT" --host localhost --ca "$CERT" --tokens "$tokens" \
            --server-pid "$SERVER_PID" --metrics-addr "127.0.0.1:$METRICS_PORT" --seed 1 \
            --meta "server=$version" --meta "passwordHash=$hash" \
            --meta "serverCpus=$SERVER_CPUS" --meta "loadCpus=$LOAD_CPUS" --meta "workers=2" \
            --out "$OUT/$target-$scenario.json" "${EXTRA[@]}" \
            >/dev/null 2> >(tee "$data/bench-$scenario.log" >&2); then
            log "$target: $scenario failed (see $data/bench-$scenario.log)"
            FAILED+=("$target/$scenario")
        fi
        stop_server
        sleep 1
    done
done

# ---- summary --------------------------------------------------------------------------------------
reports=()
for scenario in "${SCENARIO_LIST[@]}"; do
    for target in "${TARGET_LIST[@]}"; do
        [[ -f $OUT/$target-$scenario.json ]] && reports+=("$OUT/$target-$scenario.json")
    done
done
{
    echo "# Benchmark run $STAMP"
    echo
    echo "- Machine: $(nproc) CPUs ($(grep -m1 'model name' /proc/cpuinfo 2>/dev/null | cut -d: -f2- | sed 's/^ //'))," \
        "$(awk '/MemTotal/ {printf "%.1f GiB", $2 / 1048576}' /proc/meminfo 2>/dev/null) RAM, kernel $(uname -r)"
    echo "- Server CPUs: $SERVER_CPUS, load generator CPUs: $LOAD_CPUS, WORKERS=2, TLS (ECDSA P-256, no resumption)"
    for target in "${TARGET_LIST[@]}"; do
        echo "- $target: $(version_of "$target"); password hash $(hash_of "$target")"
    done
    if [[ $QUICK == 1 ]]; then
        echo "- Quick run: small sizes, short windows (a check of the set-up, not a result)"
    fi
    if ((${#FAILED[@]})); then
        echo "- Failed: ${FAILED[*]}"
    fi
    echo
    if ((${#reports[@]})); then
        "$BENCH" table "${reports[@]}"
    fi
} >"$OUT/summary.md"
log "done: $OUT/summary.md"
((${#FAILED[@]} == 0))
