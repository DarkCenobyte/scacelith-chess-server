//! SQLite store: schema and migrations, the single writer thread (FIFO, `BEGIN IMMEDIATE`), the
//! reader pool, and the typed API of every table (users, sessions, tokens, signups, ratings,
//! games, conduct, sanctions, anomalies, analysis queue, integrity, reports, refunds), plus the
//! retention purge. Owner: store. See docs/RUST-PORT.md.
