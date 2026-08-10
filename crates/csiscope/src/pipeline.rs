//! Where the analysis actually runs: a bounded thread pool, and one shared
//! analysis per distinct view.
//!
//! ## The pool is deliberately smaller than the machine
//!
//! `csiscope` runs on the Pi that is capturing. `csid`'s RX thread has first
//! claim on that hardware, the unit file nices the console below it, and the
//! whole premise of the live path is that a slow consumer stalls only itself.
//! So the pool is [`ANALYSIS_THREADS`] wide on a 4-core node, not 4 wide:
//! enough to overlap the two heavy halves of a frame, with two cores left for
//! the capture and the kernel that feeds it.
//!
//! Small frames do not use the pool at all. Handing a 52-tone spectrum to
//! another thread costs more in scheduling than the arithmetic is worth, so
//! [`for_each_chunk_mut`] only fans out once there is real work to divide.
//!
//! ## One analysis per view, not per browser
//!
//! Everything a frame contains except the waterfall is a function of the ring
//! and the view settings, so two browsers on the same settings were computing
//! identical numbers twice. [`Pipeline`] keeps one task per distinct
//! [`ViewKey`], ticking at the fastest rate any of its subscribers asked for
//! and publishing each result on a `watch` channel. Clients read the latest
//! value at their own frame rate and add only their own waterfall.
//!
//! The task lives exactly as long as its subscribers: the last [`Subscription`]
//! to drop takes the analysis down with it.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use tokio::sync::watch;

use crate::analyze::Analysis;
use crate::frame::{SharedFrame, ViewKey, ViewSettings};
use crate::state::Hub;

/// Threads available to one frame's internal parallelism.
///
/// Two, on a 4-core Pi 5. The console's job is to be watchable while the node
/// captures; it is not the reason the node exists.
pub const ANALYSIS_THREADS: usize = 2;

/// Below this many elements, a chunked loop runs on the calling thread.
///
/// A 52-tone chain spectrum is 52 floats — the pool's own bookkeeping is
/// larger than the work. The threshold sits where measurement put the
/// crossover on the reference node.
const PARALLEL_THRESHOLD: usize = 4096;

fn pool() -> Option<&'static rayon::ThreadPool> {
    static POOL: OnceLock<Option<rayon::ThreadPool>> = OnceLock::new();
    POOL.get_or_init(|| {
        rayon::ThreadPoolBuilder::new()
            .num_threads(ANALYSIS_THREADS)
            .thread_name(|i| format!("csiscope-dsp{i}"))
            .build()
            .map_err(|e| tracing::warn!(error = %e, "no analysis pool; running single-threaded"))
            .ok()
    })
    .as_ref()
}

/// Fill `data` in fixed-size chunks, `f(chunk_index, chunk)`.
///
/// Each chunk is an independent output, so this is safe to run in parallel and
/// produces identical bytes either way.
pub fn for_each_chunk_mut<F>(data: &mut [f32], chunk: usize, f: F)
where
    F: Fn(usize, &mut [f32]) + Send + Sync,
{
    if chunk == 0 || data.is_empty() {
        return;
    }
    if data.len() < PARALLEL_THRESHOLD {
        for (i, out) in data.chunks_mut(chunk).enumerate() {
            f(i, out);
        }
        return;
    }
    match pool() {
        Some(p) => {
            use rayon::prelude::*;
            p.install(|| {
                data.par_chunks_mut(chunk)
                    .enumerate()
                    .for_each(|(i, out)| f(i, out))
            });
        }
        None => {
            for (i, out) in data.chunks_mut(chunk).enumerate() {
                f(i, out);
            }
        }
    }
}

/// Elements below which splitting a frame's two halves costs more than it saves.
///
/// Measured on the deployed console (monad02, 2026-07-28): against a 52-tone
/// capture with a 57-record window the two halves are ~15 µs of arithmetic, and
/// handing them to the pool twenty times a second put roughly a third of the
/// process into `sched_yield` and `crossbeam_deque::Stealer::steal` — rayon
/// workers spinning for work that had already finished. The threshold is set
/// so a 242-tone frame still parallelises and a legacy-52 one does not.
const JOIN_THRESHOLD: usize = 65_536;

/// Run two independent halves of a frame, in parallel when there is enough of
/// them to be worth it.
///
/// `work` is a caller's estimate of the elements involved, in the same units
/// on both sides — it decides only whether to involve the pool, so it needs to
/// be cheap and roughly right, not exact.
///
/// Staying off the pool for small frames matters more than it looks: `pool()`
/// builds the threads lazily, so a console watching a narrow capture never
/// creates them at all.
pub fn join<A, B, RA, RB>(work: usize, a: A, b: B) -> (RA, RB)
where
    A: FnOnce() -> RA + Send,
    B: FnOnce() -> RB + Send,
    RA: Send,
    RB: Send,
{
    if work < JOIN_THRESHOLD {
        return (a(), b());
    }
    match pool() {
        Some(p) => p.install(|| rayon::join(a, b)),
        None => (a(), b()),
    }
}

// -- shared analyses ----------------------------------------------------------

/// A live shared analysis: the channel clients read, and the rates they asked
/// for.
struct View {
    rx: watch::Receiver<Option<Arc<SharedFrame>>>,
    /// One entry per subscriber, so the task can tick at the fastest rate
    /// anybody wants and slow down again when that client leaves. Shared with
    /// the task, which reads it each tick.
    rates: Arc<Mutex<Vec<(u64, f32)>>>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for View {
    fn drop(&mut self) {
        // The last subscriber has gone; stop analysing for nobody.
        self.task.abort();
    }
}

/// A client's handle on a shared analysis.
pub struct Subscription {
    view: Arc<View>,
    id: u64,
}

impl Subscription {
    /// The most recent shared analysis, if one has been produced yet.
    pub fn latest(&self) -> Option<Arc<SharedFrame>> {
        self.view.rx.borrow().clone()
    }

    /// Tell the shared task how fast this client wants frames.
    pub fn set_fps(&self, fps: f32) {
        if let Ok(mut rates) = self.view.rates.lock() {
            if let Some(e) = rates.iter_mut().find(|(id, _)| *id == self.id) {
                e.1 = fps;
            }
        }
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        if let Ok(mut rates) = self.view.rates.lock() {
            rates.retain(|(id, _)| *id != self.id);
        }
    }
}

/// The set of analyses currently running, one per distinct view.
pub struct Pipeline {
    hub: Arc<Hub>,
    views: Mutex<HashMap<ViewKey, std::sync::Weak<View>>>,
    next_id: AtomicU64,
}

impl Pipeline {
    pub fn new(hub: Arc<Hub>) -> Self {
        Pipeline {
            hub,
            views: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(0),
        }
    }

    /// Join the analysis for `settings`, starting it if nobody is watching yet.
    ///
    /// Must be called from a tokio runtime: the analysis lives in a task.
    pub fn subscribe(&self, settings: &ViewSettings) -> Subscription {
        let key = settings.view_key();
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);

        let mut views = match self.views.lock() {
            Ok(v) => v,
            // A poisoned registry would mean an analysis task panicked while
            // holding it. Recover rather than take the console down with it.
            Err(p) => p.into_inner(),
        };
        // Drop any views whose last subscriber has gone.
        views.retain(|_, w| w.strong_count() > 0);

        let view = match views.get(&key).and_then(|w| w.upgrade()) {
            Some(v) => v,
            None => {
                let v = Arc::new(spawn_view(self.hub.clone(), settings.clone()));
                views.insert(key, Arc::downgrade(&v));
                v
            }
        };
        if let Ok(mut rates) = view.rates.lock() {
            rates.push((id, settings.fps));
        }
        Subscription { view, id }
    }

    /// How many distinct analyses are running. Diagnostics, and the thing the
    /// fan-out claim is actually about.
    pub fn active_views(&self) -> usize {
        match self.views.lock() {
            Ok(mut v) => {
                v.retain(|_, w| w.strong_count() > 0);
                v.len()
            }
            Err(_) => 0,
        }
    }
}

/// How long a view may reuse its last analysis while no record arrives.
///
/// The guard below skips work when the ring has not advanced, but it must not
/// skip forever: several fields of a frame are functions of the CLOCK rather
/// than of the ring — how long since the last record, the delivered rate, the
/// drop counters. Freezing those is precisely the failure IP-123 was written
/// about, where a console showed a healthy-looking picture of a capture that
/// had stopped. One second is far below the point a human reads a display as
/// stalled, and still 20x less work than a 20 fps view doing it every tick.
const IDLE_REFRESH: std::time::Duration = std::time::Duration::from_secs(1);

/// Whether this tick has to run the analysis again.
///
/// Everything a frame contains except the clock-derived fields is a function of
/// the ring, so an unchanged `Hub::total()` means an identical result. On a
/// quiet channel that is the common case by a wide margin: monad02's console
/// feed delivered 0.4 Hz on 2026-07-28 while the view ticked at 20 fps, so
/// roughly 49 of every 50 analyses recomputed bytes that were already on the
/// wire — on the node that had the least thermal headroom in the fleet.
fn should_recompute(total: u64, last_total: u64, since_emit: std::time::Duration) -> bool {
    total != last_total || since_emit >= IDLE_REFRESH
}

fn spawn_view(hub: Arc<Hub>, settings: ViewSettings) -> View {
    let (tx, rx) = watch::channel(None);
    let rates: Arc<Mutex<Vec<(u64, f32)>>> = Arc::new(Mutex::new(Vec::new()));
    let task_rates: Arc<Mutex<Vec<(u64, f32)>>> = rates.clone();

    let task = tokio::spawn(async move {
        let mut analysis = Analysis::new();
        let mut fps = settings.fps;
        let mut ticker = tokio::time::interval(settings.interval());
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // `None` until the first analysis, so a view always produces one frame
        // immediately rather than waiting out an idle interval.
        let mut last_total: Option<u64> = None;
        let mut last_emit = tokio::time::Instant::now();

        loop {
            ticker.tick().await;
            if tx.receiver_count() == 0 {
                // Nobody is reading. The `Drop` on `View` aborts us too, but
                // this covers the window before the registry notices.
                return;
            }

            let total = hub.total();
            if let Some(seen) = last_total {
                if !should_recompute(total, seen, last_emit.elapsed()) {
                    continue;
                }
            }

            let want = task_rates
                .lock()
                .map(|r| r.iter().map(|(_, f)| *f).fold(1.0f32, f32::max))
                .unwrap_or(fps);
            if (want - fps).abs() > f32::EPSILON {
                fps = want;
                let mut s = settings.clone();
                s.fps = fps;
                ticker = tokio::time::interval(s.interval());
                ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            }

            // The analysis is synchronous and can be tens of milliseconds on a
            // 996-tone window; keep it off the async worker so it cannot stall
            // the HTTP surface.
            let hub = hub.clone();
            let view = settings.clone();
            let handle = tokio::task::spawn_blocking(move || {
                let mut a = analysis;
                let f = a.compute(&hub, &view);
                (f, a)
            });
            match handle.await {
                Ok((frame, a)) => {
                    analysis = a;
                    // Recorded whether or not a frame came back: a `None` means
                    // the ring had nothing to analyse, and retrying that at the
                    // full frame rate is the same wasted work the guard exists
                    // to stop.
                    last_total = Some(total);
                    last_emit = tokio::time::Instant::now();
                    if let Some(f) = frame {
                        let _ = tx.send(Some(Arc::new(f)));
                    }
                }
                Err(e) => {
                    tracing::error!(error = %e, "analysis task failed");
                    return;
                }
            }
        }
    });

    View { rx, rates, task }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunked_fills_agree_whether_or_not_they_are_parallel() {
        // Either side of the threshold, and with a chunk count that does not
        // divide the pool width.
        for chunks in [3usize, 8, 129] {
            let chunk = 64usize;
            let mut serial = vec![0.0f32; chunks * chunk];
            let mut chunked = serial.clone();
            let fill = |i: usize, out: &mut [f32]| {
                for (j, o) in out.iter_mut().enumerate() {
                    *o = (i * 1000 + j) as f32;
                }
            };
            for (i, out) in serial.chunks_mut(chunk).enumerate() {
                fill(i, out);
            }
            for_each_chunk_mut(&mut chunked, chunk, fill);
            assert_eq!(serial, chunked, "{chunks} chunks");
        }
    }

    #[test]
    fn join_returns_both_halves_on_either_side_of_the_threshold() {
        for work in [0, JOIN_THRESHOLD - 1, JOIN_THRESHOLD, JOIN_THRESHOLD * 4] {
            let (a, b) = join(work, || 6 * 7, || "answer");
            assert_eq!(a, 42, "work={work}");
            assert_eq!(b, "answer", "work={work}");
        }
    }

    /// A narrow capture must never involve the pool. This is the regression
    /// guard for the deployed finding: an ungated `join` at 20 Hz put a third
    /// of the process into rayon's idle work-stealing.
    #[test]
    fn a_legacy_52_tone_frame_stays_off_the_pool() {
        // 52 tones, a 256-record window, the full 128-column bundle — the
        // largest a legacy capture gets.
        const { assert!(52 * (128 + 256) < JOIN_THRESHOLD) };
        // ...while an HE20 frame is worth splitting.
        const { assert!(242 * (128 + 256) >= JOIN_THRESHOLD) };
    }

    /// The idle guard must save work on a quiet channel without ever letting a
    /// view go stale — those are the two ways it can be wrong, in both
    /// directions.
    #[test]
    fn an_idle_view_reuses_its_frame_but_never_freezes() {
        use std::time::Duration;

        // A new record always forces a recompute, however recently we emitted.
        assert!(should_recompute(101, 100, Duration::ZERO));
        // No new record and we published a moment ago: skip.
        assert!(!should_recompute(100, 100, Duration::from_millis(50)));
        // No new record, but the clock-derived fields are now a second old:
        // recompute anyway, so a dead feed shows as dead.
        assert!(should_recompute(100, 100, IDLE_REFRESH));
        assert!(should_recompute(100, 100, Duration::from_secs(30)));
    }

    /// The guard is only worth its complexity if it actually removes most of
    /// the work on the feed that motivated it.
    #[test]
    fn the_guard_removes_most_ticks_on_the_console_feed() {
        use std::time::Duration;

        // monad02's console view: 20 fps over a 0.4 Hz capture.
        let tick = Duration::from_millis(50);
        let mut total = 0u64;
        let mut last_total = 0u64;
        let mut since_emit = Duration::ZERO;
        let mut computed = 0usize;
        let ticks = 20 * 60; // one minute
        for i in 1..=ticks {
            // A record every 2.5 s = every 50th tick.
            if i % 50 == 0 {
                total += 1;
            }
            since_emit += tick;
            if should_recompute(total, last_total, since_emit) {
                computed += 1;
                last_total = total;
                since_emit = Duration::ZERO;
            }
        }
        // Without the guard this is 1200 analyses a minute. With it, the
        // 1 Hz idle refresh dominates: ~60, plus one per arriving record.
        assert!(computed <= 90, "{computed} analyses/min is not a saving");
        assert!(computed >= 24, "{computed} analyses/min is too stale");
    }

    #[test]
    fn the_pool_is_bounded_below_the_machine() {
        const { assert!(ANALYSIS_THREADS < 4, "the capture path needs cores too") };
        if let Some(p) = pool() {
            assert_eq!(p.current_num_threads(), ANALYSIS_THREADS);
        }
    }
}
