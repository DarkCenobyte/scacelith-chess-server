//! The analysis engines seen from outside, on the real server: the anti-cheat starts
//! `ANALYSIS_WORKERS` engines, logs each start with where the engine keeps its network, and its
//! gauges reach `/metrics`.
//!
//! Port of the Node.js `test/integration/analysis-engines.test.js`, with the real engine instead of
//! a fake one: `SCACELITH_TEST_ENGINE` names a Stockfish 19 binary (the engine the anti-cheat is
//! calibrated for, which reports its network the same way). Without it the test is skipped.

mod support;

use std::path::PathBuf;
use std::time::Duration;

use support::*;

/// The engine of the test, when `SCACELITH_TEST_ENGINE` names an existing file.
fn engine() -> Option<PathBuf> {
    let path = PathBuf::from(std::env::var_os("SCACELITH_TEST_ENGINE")?);
    path.is_file().then_some(path)
}

#[tokio::test]
async fn every_engine_start_is_logged_with_its_network_memory_and_metrics_count_the_shared_engines() {
    let Some(engine) = engine() else {
        eprintln!("skipped: SCACELITH_TEST_ENGINE does not name a Stockfish binary");
        return;
    };
    let srv = TestServer::options()
        .env("ANALYSIS_ENGINE_PATH", &engine.display().to_string())
        .env("ANALYSIS_WORKERS", "2")
        .start()
        .await;
    // An engine loads its network (about 100 MiB) before its first report.
    let limit = Duration::from_secs(60);
    for i in [0, 1] {
        let start = srv
            .logs
            .wait(limit, |l| l["msg"] == "analysis engine started" && l["engine"] == i)
            .await
            .unwrap_or_else(|| panic!("engine {i} started:\n{}", srv.logs.tail(20)));
        assert_eq!(start["level"], "info");
        let name = start["name"].as_str().unwrap_or_default();
        assert!(name.starts_with("Stockfish"), "the engine's name: {start}");
        let net = start["net"].as_str().unwrap_or_default();
        assert!(net.starts_with("nn-") && net.ends_with(".nnue"), "the network file: {start}");
        assert_eq!(start["network"], "shared memory", "{start}");
        assert!(start["pid"].as_u64().is_some_and(|pid| pid > 0), "{start}");
    }
    let body = eventually(Duration::from_secs(10), "both engines sharing their network", || async {
        let body = srv.metrics().await;
        (metric_value(&body, "scacelith_anticheat_analysis_engines_shared") == 2.0).then_some(body)
    })
    .await;
    assert_eq!(metric_value(&body, "scacelith_anticheat_analysis_engines"), 2.0);
    assert!(
        body.contains("scacelith_anticheat_analysis_games_total"),
        "the anti-cheat's other metrics come along"
    );
}
