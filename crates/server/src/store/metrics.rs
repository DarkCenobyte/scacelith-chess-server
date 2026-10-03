//! Metrics of the store (names, help texts and buckets of the Node server, docs/SIZING.md).

use std::sync::LazyLock;

use crate::metrics::{self, Counter, CounterVec, Histogram};

/// Waiting `signal` analysis jobs per player: a flagged player's further games are not queued
/// while this many of their games wait (the scoring reads their 30 latest analysed games, so these
/// renew most of that window). A game with an anomaly of its own replaces a waiting one without.
pub const SIGNAL_JOBS_PER_PLAYER: i64 = 20;

/// Lock timeouts (`busy` errors), wherever they happen.
pub(crate) static BUSY: LazyLock<Counter> = LazyLock::new(|| {
    metrics::counter(
        "scacelith_store_busy_total",
        "Store operations that gave up waiting for the database lock",
    )
});

static COMMIT_BATCH_MS: LazyLock<Histogram> = LazyLock::new(|| {
    metrics::histogram(
        "scacelith_store_commit_batch_ms",
        "Duration of one finished-games commit transaction",
        &[1.0, 2.0, 5.0, 10.0, 25.0, 50.0, 100.0, 250.0, 1000.0],
    )
});

static GAMES_COMMITTED: LazyLock<Counter> = LazyLock::new(|| {
    metrics::counter("scacelith_store_games_committed_total", "Finished games written to the database")
});

static ANALYSIS_SKIPPED: LazyLock<CounterVec> = LazyLock::new(|| {
    metrics::counter_vec(
        "scacelith_anticheat_analysis_skipped_total",
        "Finished rated games not queued for engine analysis (sample: ANALYSIS_SAMPLE_RATE, backlog: \
         ANALYSIS_QUEUE_MAX reached, player: 20 flagged games of a player already waiting, displaced: a \
         waiting flagged game without an anomaly of its own gave its place to a game with one)",
        &["reason"],
    )
});

/// Counts one committed batch: its duration, the games written (duplicates aside) and the games
/// left out of the analysis queue or taken out of it.
pub(crate) fn count_commit(entries: &[super::CommitEntry], ms: f64) {
    COMMIT_BATCH_MS.observe(ms);
    GAMES_COMMITTED.add(entries.iter().filter(|e| !e.duplicate).count() as u64);
    for e in entries {
        if let Some(reason) = e.analysis_skipped {
            ANALYSIS_SKIPPED.with(&[reason.as_str()]).inc();
        }
        if !e.analysis_displaced.is_empty() {
            ANALYSIS_SKIPPED.with(&["displaced"]).add(e.analysis_displaced.len() as u64);
        }
    }
}
