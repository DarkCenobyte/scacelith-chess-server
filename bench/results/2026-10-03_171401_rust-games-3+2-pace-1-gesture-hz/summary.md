# Benchmark run 2026-10-03_171401

- Machine: 4 CPUs (Intel(R) Xeon(R) Processor @ 2.10GHz), 15.7 GiB RAM, kernel 6.18.44-fc-v64
- Server CPUs: 0,1, load generator CPUs: 2,3, WORKERS=2, TLS (ECDSA P-256, no resumption)
- rust: scacelith-server b02f03b; password hash argon2id m=65536 t=3 p=4, PASSWORD_HASH_CONCURRENCY=2 (whole server)
- Games at a realistic 3+2 pace: `bench/run.sh --targets rust --scenarios games -- --steps 1000,4000,8000 --move-interval-ms 5000 --gesture-hz 1` (the side to move thinks 5 s +-50 %, so one ply every 5 s per game; 1 gesture(s) per second per player)

### games

| server | step | live games | moves/s | move relay p50 ms | move relay p99 ms | move confirm p99 ms | gestures/s | gesture p50 ms | gesture p99 ms | errors | CPU % | RSS MiB | load CPU % |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| rust | 1000 games | 1000 | 205 | 0.19 | 0.98 | 0.95 | 2000 | 0.18 | 0.92 | 0 | 14.7 | 91.6 | 13.3 |
| rust | 4000 games | 4000 | 808 | 0.21 | 0.82 | 0.87 | 7999 | 0.22 | 0.98 | 0 | 38.3 | 230 | 35.8 |
| rust | 8000 games | 8000 | 1610 | 0.29 | 1.39 | 1.39 | 16001 | 0.31 | 1.46 | 0 | 64.1 | 409 | 58.7 |

