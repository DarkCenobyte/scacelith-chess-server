# Benchmark run 2026-10-03_165735

- Machine: 4 CPUs (Intel(R) Xeon(R) Processor @ 2.10GHz), 15.7 GiB RAM, kernel 6.18.44-fc-v64
- Server CPUs: 0,1, load generator CPUs: 2,3, WORKERS=2, TLS (ECDSA P-256, no resumption)
- rust: scacelith-server b02f03b; password hash argon2id m=65536 t=3 p=4, PASSWORD_HASH_CONCURRENCY=2 (whole server)
- node26: node v26.10.0; password hash argon2id m=65536 t=3 p=4, PASSWORD_HASH_CONCURRENCY=1 per worker x 2

### idle

| server | step | ready ms | idle CPU % | RSS MiB | PSS MiB | processes |
|---|---|---:|---:|---:|---:|---:|
| rust | idle | 24 | 1.10 | 12.9 | 11.2 | 1 |
| node26 | idle | 434 | 2.15 | 242 | 137 | 3 |

### connections

| server | step | open | handshakes/s | handshake p50 ms | handshake p99 ms | hello p99 ms | failed | dropped | idle CPU % | RSS MiB | KiB/conn RSS | KiB/conn PSS | ramp load CPU % |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| rust | 1000 conns | 1000 | 3444 | 36.4 | 95.2 | 59.9 | 0 | 0 | 1.33 | 61.6 | 49.6 | 33.2 | 142 |
| rust | 5000 conns | 5000 | 5031 | 28.9 | 49.7 | 25.3 | 0 | 0 | 2.40 | 159 | 29.9 | 25.0 | 186 |
| rust | 10000 conns | 10000 | 4253 | 33.3 | 68.6 | 32.5 | 0 | 0 | 3.66 | 258 | 25.0 | 22.5 | 189 |
| node26 | 1000 conns | 1000 | 1107 | 162 | 236 | 20.2 | 0 | 0 | 3.06 | 306 | 68.9 | 60.2 | 56.9 |
| node26 | 5000 conns | 5000 | 1467 | 128 | 162 | 16.0 | 0 | 0 | 4.73 | 476 | 48.6 | 46.6 | 70.4 |
| node26 | 10000 conns | 10000 | 1448 | 126 | 186 | 19.2 | 0 | 0 | 6.86 | 643 | 41.4 | 40.4 | 77.8 |

### games

| server | step | live games | moves/s | move relay p50 ms | move relay p99 ms | move confirm p99 ms | gestures/s | gesture p50 ms | gesture p99 ms | errors | CPU % | RSS MiB | load CPU % |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| rust | 100 games | 100 | 103 | 0.19 | 0.54 | 0.52 | 2000 | 0.18 | 0.57 | 0 | 11.7 | 19.5 | 11.8 |
| rust | 500 games | 500 | 507 | 0.20 | 0.89 | 0.82 | 9999 | 0.20 | 0.78 | 0 | 32.9 | 53.1 | 36.0 |
| rust | 1000 games | 1000 | 1019 | 0.27 | 1.74 | 1.78 | 19997 | 0.29 | 1.62 | 0 | 55.9 | 82.1 | 59.8 |
| node26 | 100 games | 100 | 102 | 0.37 | 8.32 | 8.13 | 1999 | 0.31 | 9.34 | 0 | 40.9 | 305 | 13.0 |
| node26 | 500 games | 500 | 505 | 0.44 | 15.7 | 17.2 | 9995 | 0.47 | 16.3 | 0 | 90.4 | 374 | 36.2 |
| node26 | 1000 games | 1000 | 1013 | 1.00 | 36.4 | 41.5 | 20006 | 1.10 | 36.4 | 0 | 129 | 508 | 64.6 |

### matchmaking

| server | step | connected | match p50 ms | match p90 ms | match p99 ms | match max ms | makespan ms | unmatched | CPU % |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|
| rust | 200 players | 200 | 240 | 248 | 248 | 249 | 246 | 0 | 5.60 |
| rust | 1000 players | 1000 | 219 | 227 | 231 | 237 | 248 | 0 | 21.9 |
| node26 | 200 players | 200 | 219 | 252 | 252 | 268 | 225 | 0 | 13.0 |
| node26 | 1000 players | 1000 | 207 | 266 | 274 | 296 | 254 | 0 | 29.4 |

### rest

| server | step | conns | req/s | p50 ms | p90 ms | p99 ms | max ms | errors | CPU % | RSS MiB | load CPU % |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| rust | info | 32 | 50391 | 0.60 | 0.87 | 1.30 | 24.6 | 0 | 186 | 21.4 | 86.2 |
| rust | leaderboard | 32 | 73675 | 0.41 | 0.60 | 0.89 | 34.2 | 0 | 182 | 22.8 | 115 |
| rust | pgn | 32 | 7410 | 3.49 | 8.83 | 11.6 | 20.8 | 0 | 178 | 44.2 | 18.3 |
| rust | gif-cold | 4 | 213 | 18.2 | 22.3 | 26.4 | 36.9 | 0 | 197 | 153 | 3.33 |
| rust | gif-cached | 32 | 12130 | 2.53 | 3.30 | 4.67 | 22.8 | 0 | 192 | 183 | 86.8 |
| node26 | info | 32 | 38012 | 0.76 | 1.17 | 2.03 | 30.1 | 0 | 197 | 417 | 71.8 |
| node26 | leaderboard | 32 | 39680 | 0.73 | 1.10 | 1.97 | 23.5 | 0 | 196 | 420 | 71.8 |
| node26 | pgn | 32 | 3480 | 5.06 | 20.2 | 53.8 | 210 | 0 | 130 | 477 | 10.2 |
| node26 | gif-cold | 4 | 97.6 | 39.4 | 49.7 | 68.6 | 93.6 | 0 | 198 | 670 | 1.73 |
| node26 | gif-cached | 32 | 9389 | 3.10 | 4.93 | 8.00 | 39.3 | 0 | 197 | 746 | 63.3 |

### login

| server | step | conns | req/s | p50 ms | p90 ms | p99 ms | max ms | errors | CPU % | RSS MiB | load CPU % | register s |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| rust | login | 8 | 11.7 | 680 | 713 | 745 | 748 | 0 | 197 | 97.9 | 0.25 | 1.36 |
| node26 | login | 8 | 8.70 | 827 | 1196 | 1327 | 1370 | 0 | 195 | 388 | 0.15 | 2.45 |

