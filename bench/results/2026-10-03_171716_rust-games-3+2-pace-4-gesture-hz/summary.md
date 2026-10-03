# Benchmark run 2026-10-03_171716

- Machine: 4 CPUs (Intel(R) Xeon(R) Processor @ 2.10GHz), 15.7 GiB RAM, kernel 6.18.44-fc-v64
- Server CPUs: 0,1, load generator CPUs: 2,3, WORKERS=2, TLS (ECDSA P-256, no resumption)
- rust: scacelith-server b02f03b; password hash argon2id m=65536 t=3 p=4, PASSWORD_HASH_CONCURRENCY=2 (whole server)
- Games at a realistic 3+2 pace: `bench/run.sh --targets rust --scenarios games -- --steps 1000,4000,8000 --move-interval-ms 5000 --gesture-hz 4` (one move every 5 s per player, 4 gesture(s) per second per player)

### games

| server | step | live games | moves/s | move relay p50 ms | move relay p99 ms | move confirm p99 ms | gestures/s | gesture p50 ms | gesture p99 ms | errors | CPU % | RSS MiB | load CPU % |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| rust | 1000 games | 1000 | 205 | 0.20 | 1.04 | 0.95 | 8000 | 0.19 | 1.04 | 0 | 30.6 | 97.0 | 29.9 |
| rust | 4000 games | 4000 | 808 | 0.40 | 5.06 | 5.31 | 32005 | 0.43 | 5.18 | 0 | 91.8 | 231 | 85.6 |
| rust | 8000 games | 8000 | 1608 | 1.26 | 114 | 110 | 63996 | 1.30 | 114 | 0 | 171 | 408 | 156 |

