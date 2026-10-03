//! Helpers of the store tests: temporary stores, users, game records, a simple rating function.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::config::Config;
use crate::store::{
    GameOutcome, GameRecord, NewUser, RatingFn, RatingRecord, SideOutcome, Store, StoreOptions,
};

/// A directory removed when dropped.
pub struct TempDir(PathBuf);

impl TempDir {
    pub fn new(tag: &str) -> TempDir {
        static N: AtomicU64 = AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "scacelith-{tag}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        TempDir(dir)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }

    pub fn file(&self, name: &str) -> String {
        self.0.join(name).display().to_string()
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The test configuration.
pub fn config() -> Config {
    Config::for_tests()
}

/// A rating function moving each side by `(score - 0.5) * 20` (K 20), every game counted.
pub fn simple_rating() -> RatingFn {
    Arc::new(|w: &RatingRecord, b: &RatingRecord, score: f64| {
        let side = |r: &RatingRecord, s: f64| {
            let after = r.rating + ((s - 0.5) * 20.0).round() as i64;
            let mut rec = *r;
            rec.rating = after;
            rec.games += 1;
            rec.counted_games += 1;
            rec.rated = true;
            rec.peak = rec.peak.max(after);
            if s == 1.0 {
                rec.wins += 1;
            } else if s == 0.0 {
                rec.losses += 1;
            } else {
                rec.draws += 1;
            }
            SideOutcome { before: r.rating, after, k: Some(20), record: rec }
        };
        GameOutcome { white: side(w, score), black: side(b, 1.0 - score) }
    })
}

/// Options with the simple rating function.
pub fn options(path: Option<String>) -> StoreOptions {
    StoreOptions { path, rating: Some(simple_rating()), ..StoreOptions::default() }
}

/// A migrated in-memory store.
pub async fn memory_store() -> Store {
    let store = Store::open(&config(), options(Some(":memory:".into()))).await.unwrap();
    store.migrate().await.unwrap();
    store
}

/// A migrated store with a configuration and options (in memory unless `opts.path` says).
pub async fn store_with(config: &Config, mut opts: StoreOptions) -> Store {
    if opts.path.is_none() {
        opts.path = Some(":memory:".into());
    }
    let store = Store::open(config, opts).await.unwrap();
    store.migrate().await.unwrap();
    store
}

/// Keeps the log lines in memory while alive (one capture at a time in the test binary).
pub struct LogCapture {
    capture: crate::log::Capture,
}

impl LogCapture {
    /// Captures every record (debug level and up) until dropped; one capture at a time across
    /// the whole test binary.
    pub fn start() -> LogCapture {
        LogCapture { capture: crate::log::capture_logs(crate::log::Level::Debug) }
    }

    /// The captured records of a logger component, parsed.
    pub fn records(&self, component: &str) -> Vec<serde_json::Value> {
        self.capture.records().into_iter().filter(|v| v["c"] == component).collect()
    }
}

/// A migrated store on a file of `dir`.
pub async fn file_store(dir: &TempDir) -> Store {
    file_store_with(dir, &config(), options(None)).await
}

/// A migrated store on a file of `dir`, with a configuration and options.
pub async fn file_store_with(dir: &TempDir, config: &Config, mut opts: StoreOptions) -> Store {
    if opts.path.is_none() {
        opts.path = Some(dir.file("scacelith.db"));
    }
    let store = Store::open(config, opts).await.unwrap();
    store.migrate().await.unwrap();
    store
}

/// A new account.
pub fn new_user(name: &str, email: Option<&str>) -> NewUser {
    NewUser {
        username: name.into(),
        email: email.map(Into::into),
        password_hash: Some("hash".into()),
        email_verified: false,
        accept_challenges: true,
        created_at: 1_000,
    }
}

/// SHA-256 of `s`, lowercase hex (the token hashes the auth module stores).
pub fn sha(s: &str) -> String {
    use sha2::Digest;
    hex::encode(sha2::Sha256::digest(s.as_bytes()))
}

/// A finished game record (white wins by default, 40 plies, rated, 3+2).
pub fn record(id: u64, white: u32, black: u32) -> GameRecord {
    GameRecord {
        id,
        category: "3+2".into(),
        rated: true,
        base_ms: 180_000,
        inc_ms: 2_000,
        white_id: white,
        black_id: black,
        white_name: "W".into(),
        black_name: "B".into(),
        white_rating: Some(1500),
        black_rating: Some(1500),
        started_at: Some(1_000_000),
        ended_at: Some(1_600_000),
        status: crate::store::status::WHITE_WINS,
        reason: 1,
        rematch_of: None,
        flags: 1,
        moves: vec![0x0102; 40],
        spent_ms: Some(vec![900; 40]),
        clock_ms: Some(vec![170_000; 40]),
    }
}
