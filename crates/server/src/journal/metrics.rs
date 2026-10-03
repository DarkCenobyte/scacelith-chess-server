//! Metrics of the journal (names, help texts and buckets of the Node server). The two gauges carry
//! a `shard` label, as the Node supervisor added to these per-worker gauges.

use std::sync::LazyLock;

use crate::metrics::{self, Counter, GaugeVec, Histogram};

pub(super) static FLUSH_MS: LazyLock<Histogram> = LazyLock::new(|| {
    metrics::histogram(
        "scacelith_journal_flush_ms",
        "Journal write (+ fsync) duration per flush",
        &[0.25, 0.5, 1.0, 2.0, 5.0, 10.0, 25.0, 50.0, 100.0, 250.0],
    )
});

pub(super) static BYTES: LazyLock<Counter> =
    LazyLock::new(|| metrics::counter("scacelith_journal_bytes_total", "Bytes written to the game journal"));

pub(super) static RECORDS: LazyLock<Counter> = LazyLock::new(|| {
    metrics::counter("scacelith_journal_records_total", "Records appended to the game journal")
});

pub(super) static ERRORS: LazyLock<Counter> =
    LazyLock::new(|| metrics::counter("scacelith_journal_errors_total", "Failed journal writes"));

pub(super) static SEGMENTS_DELETED: LazyLock<Counter> = LazyLock::new(|| {
    metrics::counter(
        "scacelith_journal_segments_deleted_total",
        "Journal segments deleted once no game needed them",
    )
});

pub(super) static SNAPSHOTS: LazyLock<Counter> = LazyLock::new(|| {
    metrics::counter(
        "scacelith_journal_snapshots_total",
        "Game snapshots written to the journal (compaction of long games)",
    )
});

pub(super) static SEGMENTS: LazyLock<GaugeVec> = LazyLock::new(|| {
    metrics::gauge_vec("scacelith_journal_segments", "Journal segments on disk", &["shard"])
});

pub(super) static DISK_BYTES: LazyLock<GaugeVec> = LazyLock::new(|| {
    metrics::gauge_vec("scacelith_journal_disk_bytes", "Size of the journal segments on disk", &["shard"])
});
