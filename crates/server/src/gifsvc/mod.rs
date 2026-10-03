//! GIF service: rendering threads (nice 19), the result cache and the generation quotas used by
//! the two GIF routes (DESIGN 5.9, abuse design 3.6; the Node server's `src/gif/pool.js` and the
//! service half of `src/http/routes/gif.js`). See docs/RUST-PORT.md.
//!
//! ```ignore
//! let gifs = GifService::new(&config, Arc::new(GameRenderer::<ChessRules>::new()));
//! let job = GifJob { start_fen: None, moves, white, black, result, footer, options };
//! match gifs.render_with_quotas(job, || ctx.take_rates(&render_quotas(&config))).await {
//!     Ok(gif) => ..,                                            // 200 image/gif
//!     Err(DeliverError::Quota(e)) => ..,                        // the 429 of the quota
//!     Err(DeliverError::Gif(GifError::Busy(_))) => ..,          // 503 server_busy, refund all
//!     Err(DeliverError::Gif(GifError::RenderFailed(m))) => ..,  // log, 500 render_failed
//! }
//! ```
//!
//! * **Threads** ([`RenderPool`]): `GIF_THREADS` dedicated OS threads at nice 19, started on the
//!   first render and stopped after a minute without work, behind a FIFO queue of
//!   `GIF_QUEUE_MAX` jobs waiting `GIF_QUEUE_TIMEOUT_MS` at most; a render runs
//!   `GIF_RENDER_TIMEOUT_MS` at most. The pool is created on the first render.
//! * **Cache** ([`GifCache`]): an LRU of rendered GIFs (`GIF_CACHE_MB`) keyed by
//!   [`GifJob::cache_key`], a hash of everything the picture shows (never the PGN text), so a new
//!   name (an anonymized account) is a new picture.
//! * **In flight**: a request for a GIF being rendered waits for that render instead of starting
//!   another. A render counts as in flight *before* its quotas are taken, so two identical
//!   requests at once make one GIF and pay one quota; when the quotas refuse it, the requests
//!   waiting look again and the next one renders on its own quotas. Neither a cached nor a
//!   joined GIF costs a quota.
//! * **Metrics**: `scacelith_gif_renders_total{result}`, `scacelith_gif_render_duration_ms`,
//!   `scacelith_gif_cache_total{result}`, and the gauges `scacelith_gif_queue`,
//!   `scacelith_gif_renders_running`, `scacelith_gif_cache_bytes`.

mod cache;
mod job;
mod pool;
mod request;
mod rules;

use std::collections::HashMap;
use std::convert::Infallible;
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Weak};
use std::time::Instant;

use bytes::Bytes;
use parking_lot::Mutex;
use tokio::sync::watch;

pub use cache::GifCache;
pub use job::{GameRenderer, GifJob, JobPlayer};
pub use pool::{GIF_THREAD_NICE, GifError, IDLE_STOP, PoolSettings, PoolStats, RenderPool, Renderer};
pub use request::{
    GIF_BODY_FIELDS, GIF_BODY_LIMIT_BYTES, GIF_BUSY_RETRY_SEC, GIF_CONTENT_TYPE, GIF_DISABLED_MESSAGE,
    GIF_PGN_MAX_BYTES, GIF_ROUTE_RATE, InvalidOption, QueryOptions, QuotaSpec, RENDER_FAILED_MESSAGE,
    SERVER_BUSY_MESSAGE, busy_retry_after_secs, game_too_long_message, handler_timeout_ms, max_plies,
    options_from_body, options_from_query, render_quotas, tag_ending, tag_rating, tag_text,
};
pub use rules::ChessRules;

use crate::config::Config;
use crate::metrics::{self, Counter, Histogram};

/// The metrics of the GIF service (names, help texts and buckets of the Node server).
struct Metrics {
    ok: Counter,
    busy: Counter,
    failed: Counter,
    duration: Histogram,
    hit: Counter,
    miss: Counter,
}

static METRICS: LazyLock<Metrics> = LazyLock::new(|| {
    let renders = metrics::counter_vec(
        "scacelith_gif_renders_total",
        "GIF renders by result (ok, busy: refused by a full queue or a wait timeout, failed)",
        &["result"],
    );
    let (ok, busy, failed) = (renders.with(&["ok"]), renders.with(&["busy"]), renders.with(&["failed"]));
    let duration = metrics::histogram(
        "scacelith_gif_render_duration_ms",
        "Time to get a GIF rendered (queue wait and render)",
        &[25.0, 50.0, 100.0, 250.0, 500.0, 1000.0, 2500.0, 5000.0, 10000.0, 30000.0],
    );
    let cache = metrics::counter_vec(
        "scacelith_gif_cache_total",
        "GIF requests served without a render (hit: cached or being rendered) or rendered (miss)",
        &["result"],
    );
    let (hit, miss) = (cache.with(&["hit"]), cache.with(&["miss"]));
    metrics::gauge_fn("scacelith_gif_queue", "GIF renders waiting for a free rendering thread", || {
        sum_live(|s| s.queued)
    });
    metrics::gauge_fn("scacelith_gif_renders_running", "GIF renders in progress", || sum_live(|s| s.running));
    metrics::gauge_fn("scacelith_gif_cache_bytes", "Bytes of rendered GIFs in the cache", || {
        sum_live(|s| s.cache_bytes)
    });
    Metrics { ok, busy, failed, duration, hit, miss }
});

/// The services not closed (the gauges sum over them).
static LIVE: Mutex<Vec<Weak<Inner>>> = Mutex::new(Vec::new());

fn sum_live(f: impl Fn(&ServiceStats) -> usize) -> f64 {
    let live: Vec<Arc<Inner>> = LIVE.lock().iter().filter_map(Weak::upgrade).collect();
    live.iter().map(|inner| f(&inner.stats())).sum::<usize>() as f64
}

/// The bytes of `GIF_CACHE_MB`.
fn cache_bytes(config: &Config) -> usize {
    usize::try_from(config.gif_cache_mb).unwrap_or(0).saturating_mul(1024 * 1024)
}

/// Counters of a service.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ServiceStats {
    /// Renders waiting for a thread.
    pub queued: usize,
    /// Renders in progress.
    pub running: usize,
    /// Rendering threads alive.
    pub live: usize,
    /// GIFs in the cache.
    pub cached: usize,
    /// Bytes of the GIFs in the cache.
    pub cache_bytes: usize,
    /// Requests served without a render of their own (cached, or joined to a render in flight).
    pub hits: u64,
    /// Requests that claimed a render.
    pub misses: u64,
    /// Renders that produced a GIF.
    pub rendered: u64,
    /// Renders refused as busy (full queue, wait timeout, closed).
    pub busy: u64,
    /// Renders that failed.
    pub failed: u64,
}

/// What happened to a request or a render, counted in the process metrics and in the service's
/// own [`ServiceStats`] (the metrics are shared by every service of the process; the stats are
/// not).
#[derive(Clone, Copy)]
enum Event {
    Hit,
    Miss,
    Rendered,
    Busy,
    Failed,
}

/// The per-service counters behind [`ServiceStats`].
#[derive(Default)]
struct Tally {
    hits: AtomicU64,
    misses: AtomicU64,
    rendered: AtomicU64,
    busy: AtomicU64,
    failed: AtomicU64,
}

/// Why [`GifService::render_with_quotas`] answered no GIF.
#[derive(Debug, PartialEq, Eq)]
pub enum DeliverError<E> {
    /// The render quotas refused the render (the caller's error: a 429).
    Quota(E),
    /// The render (this request's or the one it waited for) failed.
    Gif(GifError),
}

impl<E: std::fmt::Display> std::fmt::Display for DeliverError<E> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DeliverError::Quota(e) => e.fmt(f),
            DeliverError::Gif(e) => e.fmt(f),
        }
    }
}

impl<E: std::fmt::Debug + std::fmt::Display> std::error::Error for DeliverError<E> {}

/// The state of a render in flight, as the requests waiting for it see it.
#[derive(Clone, Debug)]
enum Flight {
    Pending,
    Done(Result<Bytes, GifError>),
    /// Nothing will be made (the quotas refused it): look again.
    Cancelled,
}

#[derive(Default)]
struct Store {
    cache: GifCache,
    /// Renders in flight by cache key, with their claim number.
    inflight: HashMap<String, (u64, watch::Receiver<Flight>)>,
    next_claim: u64,
}

impl Store {
    /// Forgets the flight of `key` if it is still claim `id`'s.
    fn end_flight(&mut self, key: &str, id: u64) {
        if self.inflight.get(key).is_some_and(|(owner, _)| *owner == id) {
            self.inflight.remove(key);
        }
    }
}

#[derive(Default)]
struct Lifecycle {
    closed: bool,
    pool: Option<RenderPool>,
}

struct Inner {
    settings: PoolSettings,
    renderer: Arc<dyn Renderer>,
    store: Mutex<Store>,
    life: Mutex<Lifecycle>,
    tally: Tally,
}

impl Inner {
    /// The pool, created on first use; `None` once closed.
    fn pool(&self) -> Option<RenderPool> {
        let mut life = self.life.lock();
        if life.closed {
            return None;
        }
        let pool =
            life.pool.get_or_insert_with(|| RenderPool::new(self.settings.clone(), self.renderer.clone()));
        Some(pool.clone())
    }

    fn stats(&self) -> ServiceStats {
        let pool = self.life.lock().pool.as_ref().map(RenderPool::stats).unwrap_or_default();
        let store = self.store.lock();
        ServiceStats {
            queued: pool.queued,
            running: pool.running,
            live: pool.live,
            cached: store.cache.len(),
            cache_bytes: store.cache.bytes(),
            hits: self.tally.hits.load(Ordering::Relaxed),
            misses: self.tally.misses.load(Ordering::Relaxed),
            rendered: self.tally.rendered.load(Ordering::Relaxed),
            busy: self.tally.busy.load(Ordering::Relaxed),
            failed: self.tally.failed.load(Ordering::Relaxed),
        }
    }

    fn count(&self, event: Event) {
        let m = &*METRICS;
        let (metric, own) = match event {
            Event::Hit => (&m.hit, &self.tally.hits),
            Event::Miss => (&m.miss, &self.tally.misses),
            Event::Rendered => (&m.ok, &self.tally.rendered),
            Event::Busy => (&m.busy, &self.tally.busy),
            Event::Failed => (&m.failed, &self.tally.failed),
        };
        metric.inc();
        own.fetch_add(1, Ordering::Relaxed);
    }
}

impl Drop for Inner {
    fn drop(&mut self) {
        if let Some(pool) = self.life.get_mut().pool.take() {
            pool.close();
        }
    }
}

/// What a request finds for its key.
enum Lookup {
    Cached(Bytes),
    Joined(watch::Receiver<Flight>),
    Claimed(Claim),
}

/// The right to render the GIF of a key: registered as in flight until it renders or is
/// dropped (then the requests waiting look again).
struct Claim {
    inner: Arc<Inner>,
    key: String,
    id: u64,
    tx: Option<watch::Sender<Flight>>,
}

impl Claim {
    /// Renders `job` on the pool and caches it. The render runs in its own task, so it finishes
    /// (and serves the requests waiting for it) even if this request stops waiting.
    async fn render(mut self, job: GifJob) -> Result<Bytes, GifError> {
        let tx = self.tx.take().expect("a claim renders once");
        let (inner, key, id) = (self.inner.clone(), std::mem::take(&mut self.key), self.id);
        let task = tokio::spawn(async move {
            let t0 = Instant::now();
            let result = match inner.pool() {
                Some(pool) => pool.render(job).await,
                None => Err(GifError::closed()),
            };
            match &result {
                Ok(_) => {
                    inner.count(Event::Rendered);
                    METRICS.duration.observe(t0.elapsed().as_secs_f64() * 1000.0);
                }
                Err(GifError::Busy(_)) => inner.count(Event::Busy),
                Err(GifError::RenderFailed(_)) => inner.count(Event::Failed),
            }
            {
                let mut store = inner.store.lock();
                if let Ok(gif) = &result {
                    store.cache.set(&key, gif.clone());
                }
                store.end_flight(&key, id);
            }
            tx.send_replace(Flight::Done(result.clone()));
            result
        });
        task.await.unwrap_or_else(|_| Err(GifError::closed()))
    }
}

impl Drop for Claim {
    fn drop(&mut self) {
        if let Some(tx) = self.tx.take() {
            self.inner.store.lock().end_flight(&self.key, self.id);
            tx.send_replace(Flight::Cancelled);
        }
    }
}

/// Waits for a render in flight; a claim dropped without rendering counts as cancelled.
async fn wait(mut rx: watch::Receiver<Flight>) -> Flight {
    match rx.wait_for(|f| !matches!(f, Flight::Pending)).await {
        Ok(flight) => flight.clone(),
        Err(_) => Flight::Cancelled,
    }
}

/// The GIF renderer of the server: the thread pool (created on the first render), the cache and
/// the renders in flight. Cheap to clone; the clones share everything.
#[derive(Clone)]
pub struct GifService {
    inner: Arc<Inner>,
}

impl GifService {
    /// A service with the `GIF_*` settings of `config`, rendering with `renderer` (in the server
    /// [`GameRenderer`] over the chess rules).
    pub fn new(config: &Config, renderer: Arc<dyn Renderer>) -> GifService {
        GifService::with_settings(PoolSettings::from_config(config), cache_bytes(config), renderer)
    }

    /// A service with explicit pool settings and cache size in bytes (0: no cache).
    pub fn with_settings(
        settings: PoolSettings,
        cache_bytes: usize,
        renderer: Arc<dyn Renderer>,
    ) -> GifService {
        LazyLock::force(&METRICS);
        let inner = Arc::new(Inner {
            settings,
            renderer,
            store: Mutex::new(Store { cache: GifCache::new(cache_bytes), ..Store::default() }),
            life: Mutex::new(Lifecycle::default()),
            tally: Tally::default(),
        });
        let mut live = LIVE.lock();
        live.retain(|w| w.strong_count() > 0);
        live.push(Arc::downgrade(&inner));
        GifService { inner }
    }

    /// Whether the rendering threads' pool exists (it is created by the first render).
    pub fn started(&self) -> bool {
        self.inner.life.lock().pool.is_some()
    }

    /// The GIF of `job`: cached, or the render in flight of the same GIF, or a new render.
    pub async fn render(&self, job: GifJob) -> Result<Bytes, GifError> {
        match self.render_with_quotas(job, || async { Ok::<(), Infallible>(()) }).await {
            Ok(gif) => Ok(gif),
            Err(DeliverError::Gif(e)) => Err(e),
            Err(DeliverError::Quota(never)) => match never {},
        }
    }

    /// The GIF of `job`, as the routes deliver it: cached or in flight (a cache hit, no quota),
    /// else a new render (a miss) registered as in flight *before* `take_quotas` runs, and
    /// started only when it succeeds. A refusal of the quotas is returned as
    /// [`DeliverError::Quota`] and lets the requests waiting for this render look again.
    ///
    /// A request that waited for another's render gets that render's error. On
    /// [`GifError::Busy`] the route gives back every token of the request (route rate and the
    /// render quotas it took); on [`GifError::RenderFailed`] the quotas stay spent.
    pub async fn render_with_quotas<F, Fut, E>(
        &self,
        job: GifJob,
        take_quotas: F,
    ) -> Result<Bytes, DeliverError<E>>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<(), E>>,
    {
        let key = job.cache_key();
        loop {
            match self.lookup_or_claim(&key) {
                Lookup::Cached(gif) => {
                    self.inner.count(Event::Hit);
                    return Ok(gif);
                }
                Lookup::Joined(rx) => match wait(rx).await {
                    Flight::Done(Ok(gif)) => {
                        self.inner.count(Event::Hit);
                        return Ok(gif);
                    }
                    Flight::Done(Err(e)) => return Err(DeliverError::Gif(e)),
                    Flight::Pending | Flight::Cancelled => {}
                },
                Lookup::Claimed(claim) => {
                    self.inner.count(Event::Miss);
                    if let Err(e) = take_quotas().await {
                        drop(claim);
                        return Err(DeliverError::Quota(e));
                    }
                    return claim.render(job).await.map_err(DeliverError::Gif);
                }
            }
        }
    }

    /// The cached GIF of `key`, or the render in flight, or a new claim (atomically).
    fn lookup_or_claim(&self, key: &str) -> Lookup {
        let mut store = self.inner.store.lock();
        if let Some(gif) = store.cache.get(key) {
            return Lookup::Cached(gif);
        }
        if let Some((_, rx)) = store.inflight.get(key) {
            return Lookup::Joined(rx.clone());
        }
        store.next_claim += 1;
        let id = store.next_claim;
        let (tx, rx) = watch::channel(Flight::Pending);
        store.inflight.insert(key.to_string(), (id, rx));
        Lookup::Claimed(Claim { inner: self.inner.clone(), key: key.to_string(), id, tx: Some(tx) })
    }

    /// Counters of the pool and the cache.
    pub fn stats(&self) -> ServiceStats {
        self.inner.stats()
    }

    /// The pool's own counters (`None` before the first render).
    pub fn pool_stats(&self) -> Option<PoolStats> {
        self.inner.life.lock().pool.as_ref().map(RenderPool::stats)
    }

    /// Stops the service at shutdown: the renders waiting or running and every later one are
    /// refused as busy, the cache is emptied, the threads stop. Idempotent.
    pub fn close(&self) {
        let pool = {
            let mut life = self.inner.life.lock();
            life.closed = true;
            life.pool.take()
        };
        self.inner.store.lock().cache.clear();
        LIVE.lock().retain(|w| w.strong_count() > 0 && !std::ptr::eq(w.as_ptr(), Arc::as_ptr(&self.inner)));
        if let Some(pool) = pool {
            pool.close();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::time::Duration;

    use super::job::tests::{StartOnly, opera_job};
    use super::pool::tests::{FakeRenderer, job};
    use scacelith_gif::{Options, Orientation, Size};

    /// The tests depend on timing: they run one at a time.
    static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    /// The fake renderer, counting its renders and numbering its GIFs.
    #[derive(Default)]
    struct Counting {
        renders: AtomicUsize,
    }

    impl Renderer for Counting {
        fn render(&self, job: &GifJob, cancel: &AtomicBool) -> Result<Vec<u8>, String> {
            let n = self.renders.fetch_add(1, Ordering::SeqCst) + 1;
            let mut gif = FakeRenderer.render(job, cancel)?;
            gif.extend_from_slice(format!("#{n}").as_bytes());
            Ok(gif)
        }
    }

    fn service(threads: usize, queue_max: usize, cache_bytes: usize) -> (GifService, Arc<Counting>) {
        let renderer = Arc::new(Counting::default());
        let settings = PoolSettings {
            threads,
            queue_max,
            queue_timeout: Duration::from_millis(5000),
            render_timeout: Duration::from_millis(5000),
            idle: None,
        };
        (GifService::with_settings(settings, cache_bytes, renderer.clone()), renderer)
    }

    /// The service's own counters: hit, miss, rendered, busy, failed.
    fn counts(gifs: &GifService) -> [u64; 5] {
        let s = gifs.stats();
        [s.hits, s.misses, s.rendered, s.busy, s.failed]
    }

    fn delta(gifs: &GifService, before: [u64; 5]) -> [u64; 5] {
        let now = counts(gifs);
        std::array::from_fn(|i| now[i] - before[i])
    }

    fn with_delay(what: &str, delay_ms: u32) -> GifJob {
        GifJob { options: Options { delay_ms, ..Options::default() }, ..job(what) }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cached_gifs_cost_no_render_and_no_quota_other_options_and_names_are_new_pictures() {
        let _serial = SERIAL.lock().await;
        let (gifs, renderer) = service(1, 4, 1 << 20);
        let quotas = AtomicUsize::new(0);
        let counter = &quotas;
        let take = move || async move {
            counter.fetch_add(1, Ordering::SeqCst);
            Ok::<(), ()>(())
        };
        let before = counts(&gifs);
        assert!(!gifs.started());
        let a = gifs.render_with_quotas(job(""), take).await.unwrap();
        let b = gifs.render_with_quotas(job(""), take).await.unwrap();
        assert!(gifs.started());
        assert_eq!(a, b);
        assert_eq!(renderer.renders.load(Ordering::SeqCst), 1);
        assert_eq!(quotas.load(Ordering::SeqCst), 1, "the second request took no render quota");
        assert_eq!(delta(&gifs, before), [1, 1, 1, 0, 0]);
        let black =
            GifJob { options: Options { orientation: Orientation::Black, ..Options::default() }, ..job("") };
        gifs.render(black).await.unwrap();
        assert_eq!(renderer.renders.load(Ordering::SeqCst), 2, "other options: another picture");
        let renamed = GifJob { black: JobPlayer { name: "deleted#99".into(), rating: None }, ..job("") };
        let c = gifs.render(renamed).await.unwrap();
        assert_ne!(c, a);
        assert_eq!(renderer.renders.load(Ordering::SeqCst), 3, "a new name: a new picture");
        let s = gifs.stats();
        assert_eq!((s.cached, s.live, s.queued, s.running), (3, 1, 0, 0));
        assert_eq!(s.cache_bytes, a.len() + c.len() * 2);
        gifs.close();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn two_requests_at_once_make_one_render_and_pay_one_quota() {
        let _serial = SERIAL.lock().await;
        let (gifs, renderer) = service(1, 4, 1 << 20);
        let quotas = Arc::new(AtomicUsize::new(0));
        // The quotas take a little while (the Node primary's IPC round trip).
        let request = |what: &'static str| {
            let (gifs, quotas) = (gifs.clone(), quotas.clone());
            tokio::spawn(async move {
                gifs.render_with_quotas(with_delay(what, 700), || async move {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    quotas.fetch_add(1, Ordering::SeqCst);
                    Ok::<(), ()>(())
                })
                .await
            })
        };
        let before = counts(&gifs);
        let (r1, r2) = (request("sleep:100"), request("sleep:100"));
        let (r1, r2) = (r1.await.unwrap().unwrap(), r2.await.unwrap().unwrap());
        assert_eq!(r1, r2);
        assert_eq!(renderer.renders.load(Ordering::SeqCst), 1, "one render");
        assert_eq!(quotas.load(Ordering::SeqCst), 1, "one render quota");
        assert_eq!(delta(&gifs, before), [1, 1, 1, 0, 0], "the second is a hit");
        gifs.close();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_refused_claim_lets_the_waiting_request_render_on_its_own_quota() {
        let _serial = SERIAL.lock().await;
        let (gifs, renderer) = service(1, 4, 1 << 20);
        let (release, gate) = tokio::sync::oneshot::channel::<()>();
        let alice = tokio::spawn({
            let gifs = gifs.clone();
            async move {
                gifs.render_with_quotas(job(""), || async move {
                    let _ = gate.await;
                    Err::<(), &str>("alice's quota is spent")
                })
                .await
            }
        });
        tokio::time::sleep(Duration::from_millis(30)).await;
        let bob_quota = Arc::new(AtomicUsize::new(0));
        let bob = tokio::spawn({
            let (gifs, bob_quota) = (gifs.clone(), bob_quota.clone());
            async move {
                gifs.render_with_quotas(job(""), || async move {
                    bob_quota.fetch_add(1, Ordering::SeqCst);
                    Ok::<(), &str>(())
                })
                .await
            }
        });
        tokio::time::sleep(Duration::from_millis(30)).await;
        assert_eq!(bob_quota.load(Ordering::SeqCst), 0, "bob waits for the render alice started");
        release.send(()).unwrap();
        assert_eq!(alice.await.unwrap(), Err(DeliverError::Quota("alice's quota is spent")));
        let gif = bob.await.unwrap().unwrap();
        assert_eq!(bob_quota.load(Ordering::SeqCst), 1, "bob renders on his own quota");
        assert_eq!(renderer.renders.load(Ordering::SeqCst), 1);
        assert_eq!(gifs.render(job("")).await.unwrap(), gif, "then cached for everyone");
        assert_eq!(renderer.renders.load(Ordering::SeqCst), 1);
        gifs.close();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_error_of_a_render_goes_to_every_request_waiting_for_it_and_nothing_is_cached() {
        let _serial = SERIAL.lock().await;
        let (gifs, renderer) = service(1, 0, 1 << 20);
        let before = counts(&gifs);
        let fail = with_delay("sleep:100|fail:illegal move at ply 3", 600);
        let (a, b) = (gifs.render(fail.clone()), gifs.render(fail.clone()));
        let (a, b) = tokio::join!(a, b);
        assert_eq!(a, Err(GifError::RenderFailed("GIF render failed: illegal move at ply 3".into())));
        assert_eq!(a, b);
        assert_eq!(delta(&gifs, before), [0, 1, 0, 0, 1]);
        // Busy: the only thread renders something else and no job may wait.
        let long = tokio::spawn({
            let gifs = gifs.clone();
            async move { gifs.render(job("sleep:300")).await }
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        let busy = gifs.render(job("")).await;
        assert_eq!(busy, Err(GifError::Busy("GIF renderer busy (queue full)".into())));
        assert_eq!(busy.unwrap_err().code(), "busy");
        long.await.unwrap().unwrap();
        assert_eq!(delta(&gifs, before), [0, 3, 1, 1, 1]);
        // Nothing failed was cached: the same jobs render again.
        assert_eq!(gifs.render(fail).await.unwrap_err().code(), "render_failed");
        gifs.render(job("")).await.unwrap();
        assert_eq!(renderer.renders.load(Ordering::SeqCst), 4);
        gifs.close();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn no_pool_for_a_refused_request_no_cache_when_disabled_and_close() {
        let _serial = SERIAL.lock().await;
        let (gifs, renderer) = service(1, 4, 0);
        let refused = gifs.render_with_quotas(job(""), || async { Err::<(), u8>(7) }).await;
        assert_eq!(refused, Err(DeliverError::Quota(7)));
        assert!(!gifs.started(), "not for a refused request");
        gifs.render(job("")).await.unwrap();
        gifs.render(job("")).await.unwrap();
        assert_eq!(renderer.renders.load(Ordering::SeqCst), 2, "every request renders without a cache");
        assert_eq!(gifs.stats().cached, 0);
        assert_eq!(gifs.pool_stats().map(|s| s.completed), Some(2));
        gifs.close();
        gifs.close();
        assert_eq!(gifs.render(job("bytes:9")).await, Err(GifError::Busy("GIF renderer closed".into())));
        assert!(!gifs.started());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn renders_and_cache_show_in_the_metrics_and_a_real_game_renders_on_a_niced_thread() {
        let _serial = SERIAL.lock().await;
        let settings = PoolSettings::from_config(&Config::for_tests());
        assert_eq!((settings.threads, settings.idle), (1, Some(IDLE_STOP)));
        let gifs = GifService::with_settings(settings, 1 << 20, Arc::new(GameRenderer::<StartOnly>::new()));
        let small = GifJob {
            moves: Vec::new(),
            options: Options { size: Size::Small, ..Options::default() },
            ..opera_job()
        };
        let gif = gifs.render(small).await.unwrap();
        assert_eq!(&gif[..6], b"GIF89a");
        assert_eq!(
            gifs.render(opera_job()).await,
            Err(GifError::RenderFailed("GIF render failed: illegal move at ply 1".into()))
        );
        let text = crate::metrics::registry().render();
        for line in [
            "# HELP scacelith_gif_renders_total GIF renders by result (ok, busy: refused by a full queue or a wait timeout, failed)",
            "# TYPE scacelith_gif_render_duration_ms histogram",
            "scacelith_gif_render_duration_ms_bucket{le=\"30000\"}",
            "# HELP scacelith_gif_cache_total GIF requests served without a render (hit: cached or being rendered) or rendered (miss)",
            "scacelith_gif_cache_total{result=\"hit\"}",
            "# HELP scacelith_gif_queue GIF renders waiting for a free rendering thread",
            "# HELP scacelith_gif_renders_running GIF renders in progress",
            "# HELP scacelith_gif_cache_bytes Bytes of rendered GIFs in the cache",
        ] {
            assert!(text.contains(line), "{line}");
        }
        assert!(
            text.contains(&format!("scacelith_gif_cache_bytes {}\n", gif.len())),
            "only this service is live"
        );
        gifs.close();
        assert!(crate::metrics::registry().render().contains("scacelith_gif_cache_bytes 0\n"));
    }
}
