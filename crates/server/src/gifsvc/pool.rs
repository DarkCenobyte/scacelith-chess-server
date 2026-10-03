//! The rendering threads: a FIFO queue of at most `queue_max` jobs in front of at most `threads`
//! dedicated OS threads at nice 19, each rendering one job at a time (the Node server's
//! `src/gif/pool.js`). The runtime threads never render: they post the job and await the GIF.
//!
//! * Threads start on first use and stop after `idle` without work.
//! * A job is refused as [`GifError::Busy`] when the queue is full (admission: it cannot start at
//!   once and `queue_max` jobs already wait), when it waits longer than `queue_timeout` for a
//!   thread, or when the pool is closed (which also refuses the jobs waiting or running).
//! * It fails as [`GifError::RenderFailed`] when the renderer returns an error (invalid FEN,
//!   illegal move...), when it runs longer than `render_timeout`, when the render panics, or when
//!   no thread can start. A thread cannot be killed: on a render timeout the job is answered at
//!   once, its thread is told to stop (the renderer checks the flag between frames) and leaves the
//!   pool, and the next job gets a new thread.
//!
//! The timers of a job run in a small tokio task, so they hold even when the caller stops
//! waiting.

use std::collections::VecDeque;
use std::fmt;
use std::io;
use std::panic::{self, AssertUnwindSafe};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::{Duration, Instant};

use bytes::Bytes;
use parking_lot::Mutex;
use tokio::sync::{Notify, oneshot};

use super::job::GifJob;
use crate::config::Config;

/// Nice value of the rendering threads: the lowest scheduling priority, so a render only takes
/// the CPU time the games leave.
pub const GIF_THREAD_NICE: i32 = 19;

/// How long an idle rendering thread waits for work before it stops.
pub const IDLE_STOP: Duration = Duration::from_secs(60);

/// Why a job produced no GIF. The message is the Node server's (it goes to the log, never to the
/// client).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GifError {
    /// No thread was free in time, or the renderer is closed: 503 `server_busy`, every token of
    /// the request given back. Messages: "GIF renderer busy (queue full)", "GIF renderer busy (no
    /// thread free within N ms)", "GIF renderer closed".
    Busy(String),
    /// The job could not be rendered: 500 `render_failed`, quotas kept. The message starts with
    /// "GIF render failed: ".
    RenderFailed(String),
}

impl GifError {
    pub(crate) fn closed() -> GifError {
        GifError::Busy("GIF renderer closed".into())
    }

    /// The error code of the Node server: `busy` or `render_failed`.
    pub fn code(&self) -> &'static str {
        match self {
            GifError::Busy(_) => "busy",
            GifError::RenderFailed(_) => "render_failed",
        }
    }

    /// The message (for the log).
    pub fn message(&self) -> &str {
        match self {
            GifError::Busy(m) | GifError::RenderFailed(m) => m,
        }
    }
}

impl fmt::Display for GifError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.message())
    }
}

impl std::error::Error for GifError {}

/// Renders a job on a rendering thread: [`GameRenderer`](super::GameRenderer) in the server,
/// stand-ins in the tests.
pub trait Renderer: Send + Sync + 'static {
    /// Renders `job` into the bytes of a GIF. Returns early (with any error) once `cancel` is
    /// set. An error is the reason of a `render_failed` ("illegal move at ply 3").
    fn render(&self, job: &GifJob, cancel: &AtomicBool) -> Result<Vec<u8>, String>;
}

/// The limits of a pool.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PoolSettings {
    /// Renders at the same time (at least 1).
    pub threads: usize,
    /// Jobs waiting for a thread at most (0: a job is taken only when it can start at once).
    pub queue_max: usize,
    /// Longest wait in the queue.
    pub queue_timeout: Duration,
    /// Longest render.
    pub render_timeout: Duration,
    /// Idle time after which a thread stops (`None`: never).
    pub idle: Option<Duration>,
}

impl PoolSettings {
    /// The settings `GIF_THREADS`, `GIF_QUEUE_MAX`, `GIF_QUEUE_TIMEOUT_MS` and
    /// `GIF_RENDER_TIMEOUT_MS`; idle threads stop after a minute.
    pub fn from_config(config: &Config) -> PoolSettings {
        let ms = |v: i64| Duration::from_millis(u64::try_from(v).unwrap_or(0));
        PoolSettings {
            threads: usize::try_from(config.gif_threads).unwrap_or(0).max(1),
            queue_max: usize::try_from(config.gif_queue_max).unwrap_or(0),
            queue_timeout: ms(config.gif_queue_timeout_ms),
            render_timeout: ms(config.gif_render_timeout_ms),
            idle: Some(IDLE_STOP),
        }
    }
}

/// Counters and gauges of a pool (the Node `stats()`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PoolStats {
    /// The thread limit.
    pub threads: usize,
    /// Threads started and not stopped.
    pub live: usize,
    /// Jobs rendering.
    pub running: usize,
    /// Jobs waiting for a thread.
    pub queued: usize,
    /// GIFs rendered.
    pub completed: u64,
    /// Jobs failed (render errors, timeouts, panics, no thread).
    pub failed: u64,
    /// Jobs refused because the queue was full.
    pub rejected_full: u64,
    /// Jobs refused after waiting `queue_timeout`.
    pub rejected_wait: u64,
    /// Renders stopped after `render_timeout`.
    pub render_timeouts: u64,
    /// Threads ever started.
    pub threads_started: u64,
    /// Sum of the render times of the GIFs rendered, in milliseconds.
    pub render_ms_total: f64,
    /// Longest render time, in milliseconds.
    pub render_ms_max: f64,
    /// Render time of the last GIF, in milliseconds.
    pub last_render_ms: f64,
    /// Bytes of the GIFs rendered.
    pub bytes_total: u64,
}

type Reply = oneshot::Sender<Result<Bytes, GifError>>;

/// A job waiting for a thread.
struct Entry {
    id: u64,
    job: GifJob,
    reply: Reply,
    /// Told when the job starts (its render timer begins).
    started: Arc<Notify>,
}

/// A job accepted by the pool.
struct Submitted {
    id: u64,
    started: Arc<Notify>,
    answer: oneshot::Receiver<Result<Bytes, GifError>>,
}

/// What a thread receives.
struct Task {
    id: u64,
    job: GifJob,
    cancel: Arc<AtomicBool>,
}

/// The job a thread is rendering.
struct Running {
    id: u64,
    reply: Reply,
    cancel: Arc<AtomicBool>,
    started_at: Instant,
}

/// A rendering thread, as the pool sees it. Removing the slot drops the sender: the thread stops
/// as soon as it is idle.
struct Slot {
    id: u64,
    tx: mpsc::Sender<Task>,
    task: Option<Running>,
}

#[derive(Default)]
struct State {
    queue: VecDeque<Entry>,
    slots: Vec<Slot>,
    next_job: u64,
    next_slot: u64,
    closed: bool,
    stats: PoolStats,
}

struct Shared {
    settings: PoolSettings,
    renderer: Arc<dyn Renderer>,
    state: Mutex<State>,
    /// Makes every thread start fail (tests of that path).
    #[cfg(test)]
    spawn_fails: AtomicBool,
}

/// A pool of GIF rendering threads (cheap to clone; the clones share the pool).
#[derive(Clone)]
pub struct RenderPool {
    shared: Arc<Shared>,
}

impl RenderPool {
    /// A pool rendering with `renderer`; no thread starts before the first job.
    pub fn new(settings: PoolSettings, renderer: Arc<dyn Renderer>) -> RenderPool {
        let settings = PoolSettings { threads: settings.threads.max(1), ..settings };
        RenderPool {
            shared: Arc::new(Shared {
                settings,
                renderer,
                state: Mutex::new(State::default()),
                #[cfg(test)]
                spawn_fails: AtomicBool::new(false),
            }),
        }
    }

    /// Renders `job` on a pool thread. Must be called within a tokio runtime (the job's timers
    /// run there).
    pub async fn render(&self, job: GifJob) -> Result<Bytes, GifError> {
        let Submitted { id, started, answer } = self.shared.submit(job)?;
        tokio::spawn(watchdog(self.shared.clone(), id, started));
        answer.await.unwrap_or_else(|_| Err(GifError::closed()))
    }

    /// Counters and gauges.
    pub fn stats(&self) -> PoolStats {
        let st = self.shared.state.lock();
        PoolStats {
            threads: self.shared.settings.threads,
            live: st.slots.len(),
            running: st.slots.iter().filter(|s| s.task.is_some()).count(),
            queued: st.queue.len(),
            ..st.stats.clone()
        }
    }

    /// Stops the pool: the jobs waiting or running are refused as busy ("GIF renderer closed"),
    /// and so is every later job. The threads stop once their render returns. Idempotent.
    pub fn close(&self) {
        let mut st = self.shared.state.lock();
        if st.closed {
            return;
        }
        st.closed = true;
        for entry in st.queue.drain(..) {
            let _ = entry.reply.send(Err(GifError::closed()));
        }
        for slot in st.slots.drain(..) {
            if let Some(run) = slot.task {
                run.cancel.store(true, Ordering::Relaxed);
                let _ = run.reply.send(Err(GifError::closed()));
            }
        }
    }

    #[cfg(test)]
    fn fail_spawns(&self) {
        self.shared.spawn_fails.store(true, Ordering::Relaxed);
    }
}

/// The timers of job `id`: the wait in the queue, then the render.
async fn watchdog(shared: Arc<Shared>, id: u64, started: Arc<Notify>) {
    let queue_deadline = tokio::time::Instant::now() + shared.settings.queue_timeout;
    tokio::select! {
        () = started.notified() => {}
        () = tokio::time::sleep_until(queue_deadline) => {
            if shared.expire_queued(id) {
                return;
            }
        }
    }
    let Some(started_at) = shared.running_since(id) else { return };
    tokio::time::sleep_until(tokio::time::Instant::from_std(started_at + shared.settings.render_timeout))
        .await;
    shared.expire_running(id);
}

fn millis(d: Duration) -> u128 {
    d.as_millis()
}

impl Shared {
    /// Queues a job (and starts it when a thread is free).
    fn submit(self: &Arc<Self>, job: GifJob) -> Result<Submitted, GifError> {
        let mut st = self.state.lock();
        if st.closed {
            return Err(GifError::closed());
        }
        let running = st.slots.iter().filter(|s| s.task.is_some()).count();
        let can_start = st.queue.is_empty() && running < self.settings.threads;
        if !can_start && st.queue.len() >= self.settings.queue_max {
            st.stats.rejected_full += 1;
            return Err(GifError::Busy("GIF renderer busy (queue full)".into()));
        }
        st.next_job += 1;
        let id = st.next_job;
        let (reply, rx) = oneshot::channel();
        let started = Arc::new(Notify::new());
        st.queue.push_back(Entry { id, job, reply, started: started.clone() });
        self.pump(&mut st);
        Ok(Submitted { id, started, answer: rx })
    }

    /// Starts queued jobs while a thread is free or may start (FIFO).
    fn pump(self: &Arc<Self>, st: &mut State) {
        while !st.closed && !st.queue.is_empty() {
            let index = match st.slots.iter().position(|s| s.task.is_none()) {
                Some(i) => i,
                None if st.slots.len() >= self.settings.threads => return,
                None => match self.spawn(st) {
                    Ok(i) => i,
                    Err(e) => {
                        // The busy threads take the queue when they finish; with none left, the
                        // waiting jobs fail now.
                        if !st.slots.is_empty() {
                            return;
                        }
                        let message = format!("GIF render failed: cannot start a rendering thread ({e})");
                        for entry in st.queue.drain(..) {
                            st.stats.failed += 1;
                            let _ = entry.reply.send(Err(GifError::RenderFailed(message.clone())));
                        }
                        return;
                    }
                },
            };
            let entry = st.queue.pop_front().expect("the queue is not empty");
            let cancel = Arc::new(AtomicBool::new(false));
            let task = Task { id: entry.id, job: entry.job, cancel: cancel.clone() };
            if st.slots[index].tx.send(task).is_err() {
                // The thread is gone without leaving the pool (cannot happen: a thread leaves
                // its slot before it ends). Fail the job rather than lose it.
                st.slots.remove(index);
                st.stats.failed += 1;
                let message = "GIF render failed: the rendering thread stopped (gone)".to_string();
                let _ = entry.reply.send(Err(GifError::RenderFailed(message)));
                continue;
            }
            st.slots[index].task =
                Some(Running { id: entry.id, reply: entry.reply, cancel, started_at: Instant::now() });
            entry.started.notify_one();
        }
    }

    /// Starts a thread; returns its slot index.
    fn spawn(self: &Arc<Self>, st: &mut State) -> io::Result<usize> {
        #[cfg(test)]
        if self.spawn_fails.load(Ordering::Relaxed) {
            return Err(io::Error::other("thread limit reached"));
        }
        let (tx, rx) = mpsc::channel();
        st.next_slot += 1;
        let id = st.next_slot;
        let shared = self.clone();
        std::thread::Builder::new().name("gif-render".into()).spawn(move || thread_main(shared, id, rx))?;
        st.stats.threads_started += 1;
        st.slots.push(Slot { id, tx, task: None });
        Ok(st.slots.len() - 1)
    }

    /// Refuses job `id` if it is still waiting; returns whether it was.
    fn expire_queued(&self, id: u64) -> bool {
        let mut st = self.state.lock();
        let Some(i) = st.queue.iter().position(|e| e.id == id) else { return false };
        let entry = st.queue.remove(i).expect("the index is valid");
        st.stats.rejected_wait += 1;
        let message =
            format!("GIF renderer busy (no thread free within {} ms)", millis(self.settings.queue_timeout));
        let _ = entry.reply.send(Err(GifError::Busy(message)));
        true
    }

    /// When job `id` started, if it is rendering.
    fn running_since(&self, id: u64) -> Option<Instant> {
        let st = self.state.lock();
        st.slots.iter().find_map(|s| s.task.as_ref().filter(|r| r.id == id).map(|r| r.started_at))
    }

    /// Fails job `id` if it is still rendering: its thread is told to stop and leaves the pool.
    fn expire_running(self: &Arc<Self>, id: u64) {
        let mut st = self.state.lock();
        let Some(i) = st.slots.iter().position(|s| s.task.as_ref().is_some_and(|r| r.id == id)) else {
            return;
        };
        let slot = st.slots.remove(i);
        let run = slot.task.expect("the slot is rendering");
        run.cancel.store(true, Ordering::Relaxed);
        st.stats.render_timeouts += 1;
        st.stats.failed += 1;
        let message = format!("GIF render failed: longer than {} ms", millis(self.settings.render_timeout));
        let _ = run.reply.send(Err(GifError::RenderFailed(message)));
        self.pump(&mut st);
    }

    /// The thread of `slot_id` finished job `task_id`. Returns whether the thread stays (false:
    /// it left the pool meanwhile, after a render timeout or a close).
    fn finish(
        self: &Arc<Self>,
        slot_id: u64,
        task_id: u64,
        outcome: Result<Vec<u8>, String>,
        ms: f64,
    ) -> bool {
        let mut st = self.state.lock();
        let Some(slot) = st.slots.iter_mut().find(|s| s.id == slot_id) else { return false };
        let Some(run) = slot.task.take_if(|r| r.id == task_id) else { return true };
        match outcome {
            Ok(gif) => {
                let s = &mut st.stats;
                s.completed += 1;
                s.last_render_ms = ms;
                s.render_ms_total += ms;
                s.render_ms_max = s.render_ms_max.max(ms);
                s.bytes_total += gif.len() as u64;
                let _ = run.reply.send(Ok(Bytes::from(gif)));
            }
            Err(message) => {
                st.stats.failed += 1;
                let _ = run.reply.send(Err(GifError::RenderFailed(format!("GIF render failed: {message}"))));
            }
        }
        self.pump(&mut st);
        true
    }

    /// The render of job `task_id` panicked: the thread leaves the pool and ends.
    fn died(self: &Arc<Self>, slot_id: u64, task_id: u64, why: &str) {
        let mut st = self.state.lock();
        let Some(i) = st.slots.iter().position(|s| s.id == slot_id) else { return };
        let slot = st.slots.remove(i);
        if let Some(run) = slot.task.filter(|r| r.id == task_id) {
            st.stats.failed += 1;
            let message = format!("GIF render failed: the rendering thread stopped ({why})");
            let _ = run.reply.send(Err(GifError::RenderFailed(message)));
        }
        self.pump(&mut st);
    }

    /// The thread of `slot_id` has been idle for the idle time: it leaves the pool unless a job
    /// was given to it meanwhile. Returns whether it leaves.
    fn retire(&self, slot_id: u64) -> bool {
        let mut st = self.state.lock();
        match st.slots.iter().position(|s| s.id == slot_id) {
            None => true,
            Some(i) if st.slots[i].task.is_none() => {
                st.slots.remove(i);
                true
            }
            Some(_) => false,
        }
    }
}

/// The text of a panic payload.
fn panic_message(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "panic".to_string()
    }
}

/// A rendering thread: lowest priority, then one job at a time until it is idle too long or
/// leaves the pool.
fn thread_main(shared: Arc<Shared>, slot_id: u64, rx: mpsc::Receiver<Task>) {
    // Linux applies the nice value to this thread alone; on failure it renders at the process's
    // priority.
    let _ = crate::sys::set_current_thread_nice(GIF_THREAD_NICE);
    loop {
        let next = match shared.settings.idle {
            Some(idle) => rx.recv_timeout(idle),
            None => rx.recv().map_err(|_| RecvTimeoutError::Disconnected),
        };
        let task = match next {
            Ok(task) => task,
            Err(RecvTimeoutError::Timeout) if shared.retire(slot_id) => return,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => return,
        };
        let t0 = Instant::now();
        let outcome =
            panic::catch_unwind(AssertUnwindSafe(|| shared.renderer.render(&task.job, &task.cancel)));
        let ms = t0.elapsed().as_secs_f64() * 1000.0;
        match outcome {
            Ok(outcome) => {
                if !shared.finish(slot_id, task.id, outcome, ms) {
                    return;
                }
            }
            Err(payload) => {
                shared.died(slot_id, task.id, &panic_message(payload.as_ref()));
                return;
            }
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::gifsvc::job::JobPlayer;
    use scacelith_gif::{GameResult, Options};

    /// A stand-in for the renderer (the Node tests' gif-slow-worker.js), driven by the white
    /// player's name, directives separated by '|': "sleep:<ms>" blocks the thread like a long
    /// render (stopping early when cancelled), "fail:<message>" fails, "crash" panics,
    /// "bytes:<n>" answers n bytes, "nice" answers the thread's nice value; the answer is
    /// otherwise a small fake GIF.
    pub struct FakeRenderer;

    impl Renderer for FakeRenderer {
        fn render(&self, job: &GifJob, cancel: &AtomicBool) -> Result<Vec<u8>, String> {
            let mut n = 8;
            for directive in job.white.name.split('|') {
                if let Some(ms) = directive.strip_prefix("sleep:") {
                    let until =
                        Instant::now() + Duration::from_millis(ms.parse().expect("sleep milliseconds"));
                    while Instant::now() < until {
                        if cancel.load(Ordering::Relaxed) {
                            return Err("cancelled".into());
                        }
                        std::thread::sleep(Duration::from_millis(2));
                    }
                } else if let Some(message) = directive.strip_prefix("fail:") {
                    return Err(message.to_string());
                } else if directive == "crash" {
                    panic!("the renderer crashed");
                } else if directive == "nice" {
                    return Ok(thread_nice().to_string().into_bytes());
                } else if let Some(bytes) = directive.strip_prefix("bytes:") {
                    n = bytes.parse().expect("byte count");
                }
            }
            let mut gif = vec![0u8; n];
            let len = n.min(6);
            gif[..len].copy_from_slice(&b"GIF89a"[..len]);
            Ok(gif)
        }
    }

    /// The nice value of the calling thread (Linux).
    pub fn thread_nice() -> i64 {
        let stat = std::fs::read_to_string("/proc/thread-self/stat").expect("procfs");
        let after = &stat[stat.rfind(')').expect("stat format") + 2..];
        after.split(' ').nth(16).expect("nice field").parse().expect("number")
    }

    /// A job for the fake renderer.
    pub fn job(what: &str) -> GifJob {
        GifJob {
            start_fen: None,
            moves: Vec::new(),
            white: JobPlayer { name: what.to_string(), rating: None },
            black: JobPlayer { name: String::new(), rating: None },
            result: GameResult::Unfinished,
            footer: None,
            options: Options::default(),
        }
    }

    fn pool(
        threads: usize,
        queue_max: usize,
        queue_ms: u64,
        render_ms: u64,
        idle_ms: Option<u64>,
    ) -> RenderPool {
        RenderPool::new(
            PoolSettings {
                threads,
                queue_max,
                queue_timeout: Duration::from_millis(queue_ms),
                render_timeout: Duration::from_millis(render_ms),
                idle: idle_ms.map(Duration::from_millis),
            },
            Arc::new(FakeRenderer),
        )
    }

    async fn settle() {
        tokio::time::sleep(Duration::from_millis(30)).await;
    }

    fn is_busy(r: &Result<Bytes, GifError>, text: &str) -> bool {
        matches!(r, Err(GifError::Busy(m)) if m.contains(text))
    }

    fn is_failed(r: &Result<Bytes, GifError>, text: &str) -> bool {
        matches!(r, Err(GifError::RenderFailed(m)) if m.starts_with("GIF render failed: ") && m.contains(text))
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn renders_on_a_thread_render_failed_for_a_bad_job_threads_restart_after_idling() {
        let p = pool(1, 4, 15_000, 30_000, Some(50));
        let gif = p.render(job("bytes:10")).await.unwrap();
        assert_eq!(&gif[..6], b"GIF89a");
        assert!(is_failed(&p.render(job("fail:illegal move at ply 1")).await, "illegal move at ply 1"));
        assert!(is_failed(
            &p.render(job("fail:invalid start position (FEN)")).await,
            "invalid start position"
        ));
        let s = p.stats();
        assert_eq!((s.completed, s.failed, s.live, s.threads_started, s.bytes_total), (1, 2, 1, 1, 10));
        assert!(s.render_ms_max >= 0.0 && s.render_ms_total >= s.last_render_ms);
        // Idle: the thread stops, and the next job starts a new one.
        tokio::time::sleep(Duration::from_millis(250)).await;
        assert_eq!(p.stats().live, 0);
        p.render(job("")).await.unwrap();
        let s = p.stats();
        assert_eq!((s.threads_started, s.completed), (2, 2));
        p.close();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn busy_when_the_queue_is_full() {
        let p = pool(1, 2, 5000, 30_000, None);
        let running = tokio::spawn({
            let p = p.clone();
            async move { p.render(job("sleep:300")).await }
        });
        settle().await;
        let queued: Vec<_> = (0..2)
            .map(|_| {
                let p = p.clone();
                tokio::spawn(async move { p.render(job("")).await })
            })
            .collect();
        settle().await;
        assert!(is_busy(&p.render(job("")).await, "queue full"));
        let s = p.stats();
        assert_eq!((s.running, s.queued, s.rejected_full), (1, 2, 1));
        assert_eq!(&running.await.unwrap().unwrap()[..6], b"GIF89a");
        for q in queued {
            assert_eq!(&q.await.unwrap().unwrap()[..6], b"GIF89a");
        }
        assert_eq!(p.stats().completed, 3);
        p.close();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn busy_after_waiting_longer_than_the_queue_timeout_jobs_run_in_order() {
        let p = pool(1, 4, 100, 30_000, None);
        let order = Arc::new(Mutex::new(Vec::new()));
        let first = tokio::spawn({
            let (p, order) = (p.clone(), order.clone());
            async move {
                let r = p.render(job("sleep:400")).await;
                order.lock().push("first");
                r
            }
        });
        settle().await;
        assert!(is_busy(&p.render(job("")).await, "no thread free within 100 ms"));
        first.await.unwrap().unwrap();
        p.render(job("")).await.unwrap();
        order.lock().push("second");
        assert_eq!(*order.lock(), ["first", "second"]);
        assert_eq!(p.stats().rejected_wait, 1);
        p.close();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn render_failed_for_a_render_too_long_a_panicking_render_and_an_error() {
        let p = pool(1, 4, 15_000, 150, None);
        let t0 = Instant::now();
        assert!(is_failed(&p.render(job("sleep:2000")).await, "longer than 150 ms"));
        assert!(t0.elapsed() < Duration::from_millis(1500), "answered at the timeout");
        assert_eq!(p.stats().render_timeouts, 1);
        assert_eq!(p.render(job("bytes:16")).await.unwrap().len(), 16);
        assert!(is_failed(
            &p.render(job("crash")).await,
            "the rendering thread stopped (the renderer crashed)"
        ));
        assert!(is_failed(&p.render(job("fail:bad job")).await, "bad job"));
        assert_eq!(p.render(job("")).await.unwrap().len(), 8);
        let s = p.stats();
        assert_eq!((s.threads_started, s.failed, s.completed), (3, 3, 2));
        p.close();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn render_failed_when_no_thread_can_start_nothing_stays_queued() {
        let p = pool(1, 4, 15_000, 30_000, None);
        p.fail_spawns();
        assert!(is_failed(
            &p.render(job("")).await,
            "cannot start a rendering thread (thread limit reached)"
        ));
        let s = p.stats();
        assert_eq!((s.queued, s.live, s.failed), (0, 0, 1));
        p.close();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn two_threads_render_at_the_same_time() {
        let p = pool(2, 0, 15_000, 30_000, None);
        let both: Vec<_> = (0..2)
            .map(|_| {
                let p = p.clone();
                tokio::spawn(async move { p.render(job("sleep:250")).await })
            })
            .collect();
        settle().await;
        assert_eq!(p.stats().running, 2);
        assert!(is_busy(&p.render(job("")).await, "queue full"));
        for b in both {
            b.await.unwrap().unwrap();
        }
        let s = p.stats();
        assert_eq!((s.live, s.completed), (2, 2));
        p.close();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn close_refuses_running_and_waiting_jobs_as_busy_and_new_ones() {
        let p = pool(1, 4, 15_000, 30_000, None);
        let jobs: Vec<_> = ["sleep:500", ""]
            .into_iter()
            .map(|what| {
                let p = p.clone();
                tokio::spawn(async move { p.render(job(what)).await })
            })
            .collect();
        settle().await;
        p.close();
        for j in jobs {
            assert!(is_busy(&j.await.unwrap(), "closed"));
        }
        assert!(is_busy(&p.render(job("")).await, "closed"));
        assert_eq!(p.stats().live, 0);
        p.close();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_rendering_thread_runs_at_nice_19_and_the_caller_keeps_its_priority() {
        let before = thread_nice();
        let p = pool(1, 4, 15_000, 30_000, None);
        let nice = p.render(job("nice")).await.unwrap();
        assert_eq!(std::str::from_utf8(&nice).unwrap(), GIF_THREAD_NICE.to_string());
        assert_eq!(thread_nice(), before);
        p.close();
    }
}
