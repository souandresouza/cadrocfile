//! Opt-in performance tracing, enabled with `CADROCFILE_TRACE=1`.
//!
//! The important tool here is [`install_stall_detector`]: it measures the gap
//! between main-loop ticks, which is the only direct way to tell whether the UI
//! actually froze — as opposed to merely taking a while to finish work that was
//! correctly running elsewhere.

use std::{
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
    time::{Duration, Instant},
};

static ENABLED: AtomicBool = AtomicBool::new(false);
static INITIALISED: AtomicBool = AtomicBool::new(false);

/// Counts of interesting events, reported by [`summary`].
pub static THUMB_CACHE_HITS: AtomicU64 = AtomicU64::new(0);
pub static THUMB_DECODES: AtomicU64 = AtomicU64::new(0);
pub static THUMB_DROPPED: AtomicU64 = AtomicU64::new(0);

pub fn init() {
    let on = std::env::var("CADROCFILE_TRACE").is_ok_and(|v| v != "0" && !v.is_empty());
    ENABLED.store(on, Ordering::Relaxed);
    INITIALISED.store(true, Ordering::Relaxed);
}

pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// Logs a line when tracing is on. Prefer the [`trace!`] macro.
pub fn log(message: &str) {
    if enabled() {
        eprintln!("[trace {:>8.3}s] {message}", elapsed().as_secs_f64());
    }
}

fn start() -> Instant {
    use std::sync::OnceLock;
    static START: OnceLock<Instant> = OnceLock::new();
    *START.get_or_init(Instant::now)
}

pub fn elapsed() -> Duration {
    start().elapsed()
}

#[macro_export]
macro_rules! trace {
    ($($arg:tt)*) => {
        if $crate::trace::enabled() {
            $crate::trace::log(&format!($($arg)*));
        }
    };
}

/// Times a block and logs how long it took.
pub struct Span {
    label: String,
    started: Instant,
}

impl Span {
    pub fn new(label: impl Into<String>) -> Self {
        Self { label: label.into(), started: Instant::now() }
    }
}

impl Drop for Span {
    fn drop(&mut self) {
        if enabled() {
            log(&format!("{} took {:.1}ms", self.label, self.started.elapsed().as_secs_f64() * 1000.0));
        }
    }
}

/// Ticks the main loop frequently and reports any gap long enough to be seen as
/// a freeze.
///
/// A tick scheduled every 10ms that arrives 400ms late means the main thread
/// was blocked for ~390ms — the user sees exactly that as a hang.
pub fn install_stall_detector() {
    if !enabled() {
        return;
    }
    const TICK: Duration = Duration::from_millis(10);
    /// Roughly four dropped frames at 60Hz — below this nobody notices.
    const REPORT_ABOVE: Duration = Duration::from_millis(60);

    let mut last = Instant::now();
    let mut worst = Duration::ZERO;

    glib::timeout_add_local(TICK, move || {
        let now = Instant::now();
        let gap = now.duration_since(last).saturating_sub(TICK);
        last = now;

        if gap > REPORT_ABOVE {
            if gap > worst {
                worst = gap;
            }
            log(&format!(
                "MAIN THREAD STALLED {:.0}ms (worst so far {:.0}ms)",
                gap.as_secs_f64() * 1000.0,
                worst.as_secs_f64() * 1000.0
            ));
        }
        glib::ControlFlow::Continue
    });
}

/// Logs the event counters. Called on a timer so a short run still reports.
pub fn summary() {
    if !enabled() {
        return;
    }
    log(&format!(
        "thumbnails: {} from shared cache, {} decoded, {} dropped as stale",
        THUMB_CACHE_HITS.load(Ordering::Relaxed),
        THUMB_DECODES.load(Ordering::Relaxed),
        THUMB_DROPPED.load(Ordering::Relaxed),
    ));
}

use gtk::glib;
